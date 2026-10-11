//! Items: `:Todo` as the one record for work that wants doing (HBI plan §9.2).
//!
//! The rules every writer shares (REST, `manage_todo`, `notify_user`, the
//! proposal writer):
//! - a status is one of [`TODO_STATUSES`], an origin one of [`TODO_ORIGINS`];
//! - an assignee is `user`, `agent:auto`, or `agent:<profile>` naming a
//!   *spawnable* profile, checked against the live roster when it is written,
//!   so a typo fails while a person is looking, not silently at dispatch;
//! - **only the user assigns an agent** (plan §9.9 decision 5): the brain's own
//!   work (anything under a `ROUTE_CONTEXT`: a queued job, a chat turn, a
//!   sub-agent) may assign `user` or nobody, never an agent.
//!
//! Nothing here dispatches an agent. That is Phase 13.

use chrono::DateTime;
use serde_json::Value;

use agent_brain_repository::{Neo4jClient, TODO_ORIGINS, TODO_STATUSES, Todo, TodoItemPatch};

/// Whether this call is the brain's own work rather than a person's: a queued
/// job, a chat turn or a sub-agent, all of which scope a `ROUTE_CONTEXT`.
pub fn written_by_brain() -> bool {
    agent_brain_llm::routing::ROUTE_CONTEXT
        .try_with(|_| ())
        .is_ok()
}

/// A JSON field as a patch value: absent → leave it, null or "" → clear it.
fn field(body: &Value, key: &str) -> Option<Option<String>> {
    let v = body.get(key)?;
    Some(
        v.as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
    )
}

/// The item fields a create or update body carries. `origin` may be given as
/// `{kind, ref}` or as flat `origin_kind` / `origin_ref`.
pub fn patch_from_json(body: &Value) -> TodoItemPatch {
    let origin = body.get("origin");
    TodoItemPatch {
        assignee: field(body, "assignee"),
        origin_kind: origin
            .and_then(|o| field(o, "kind"))
            .or_else(|| field(body, "origin_kind")),
        origin_ref: origin
            .and_then(|o| field(o, "ref"))
            .or_else(|| field(body, "origin_ref")),
        prompt: field(body, "prompt"),
        session_id: field(body, "session_id"),
        snoozed_until: field(body, "snoozed_until"),
        result_note_id: field(body, "result_note_id"),
        model: field(body, "model"),
    }
}

/// Check a status and a patch. `roster` is the spawnable agents' names;
/// `by_brain` is [`written_by_brain`]. `Err` names every problem.
pub fn validate(
    status: Option<&str>,
    patch: &TodoItemPatch,
    roster: &[String],
    by_brain: bool,
) -> Result<(), String> {
    let mut errors = Vec::new();
    if let Some(s) = status.map(str::trim).filter(|s| !s.is_empty())
        && !TODO_STATUSES.contains(&s)
    {
        errors.push(format!(
            "status '{s}' is not one of {}",
            TODO_STATUSES.join(", ")
        ));
    }
    if let Some(Some(a)) = &patch.assignee {
        if let Some(agent) = a.strip_prefix("agent:") {
            if by_brain {
                errors.push(
                    "only the user assigns an item to an agent; the brain may assign 'user' \
                     or leave it unassigned"
                        .to_string(),
                );
            } else if agent != "auto" && !roster.iter().any(|r| r == agent) {
                errors.push(format!(
                    "'{agent}' is not a spawnable agent (spawnable: {})",
                    roster.join(", ")
                ));
            }
        } else if a != "user" {
            errors.push(format!(
                "assignee '{a}' must be 'user', 'agent:auto' or 'agent:<spawnable profile>'"
            ));
        }
    }
    if let Some(Some(k)) = &patch.origin_kind
        && !TODO_ORIGINS.contains(&k.as_str())
    {
        errors.push(format!(
            "origin kind '{k}' is not one of {}",
            TODO_ORIGINS.join(", ")
        ));
    }
    if let Some(Some(t)) = &patch.snoozed_until
        && DateTime::parse_from_rfc3339(t).is_err()
    {
        errors.push(format!(
            "snoozed_until '{t}' is not an RFC 3339 time (e.g. 2026-10-10T09:00:00Z)"
        ));
    }
    if errors.is_empty() {
        Ok(())
    } else {
        Err(errors.join("; "))
    }
}

/// The spawnable agents' names, for [`validate`].
pub async fn roster(
    cb: Option<&agent_brain_work::context_builder::ContextBuilderService>,
) -> Vec<String> {
    match cb {
        Some(cb) => cb
            .spawnable_profiles()
            .await
            .into_iter()
            .map(|p| p.name)
            .collect(),
        None => Vec::new(),
    }
}

/// What a new item needs. Unset fields stay unset.
#[derive(Debug, Clone, Default)]
pub struct NewItem {
    pub title: String,
    pub description: Option<String>,
    pub priority: Option<i64>,
    pub due_at: Option<String>,
    pub patch: TodoItemPatch,
}

/// Create an item, or return the one already made for the same origin, so a
/// retried notification or a re-run backfill never makes two.
pub async fn create_item(neo4j: &Neo4jClient, item: NewItem) -> anyhow::Result<(Todo, bool)> {
    if let (Some(Some(kind)), Some(Some(r))) = (&item.patch.origin_kind, &item.patch.origin_ref)
        && let Some(existing) = neo4j.find_todo_by_origin(kind, r).await?
    {
        return Ok((existing, false));
    }
    let todo = neo4j
        .create_todo(
            &item.title,
            item.description.as_deref(),
            None,
            item.priority,
            None,
            item.due_at.as_deref(),
        )
        .await?;
    let todo = neo4j
        .patch_todo_item(&todo.id, &item.patch)
        .await?
        .unwrap_or(todo);
    Ok((todo, true))
}

/// A short title from a longer message: its first line, at most 120 chars.
pub fn title_from(message: &str) -> String {
    let line = message
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("");
    let line = line.trim_start_matches('#').trim();
    if line.chars().count() <= 120 {
        line.to_string()
    } else {
        format!("{}…", line.chars().take(119).collect::<String>())
    }
}

/// The item as the HBI and `manage_todo` show it: the record plus `origin`
/// as `{kind, ref}` and whether it is the user's to act on.
pub fn item_json(t: &Todo) -> Value {
    let mut v = serde_json::to_value(t).unwrap_or(Value::Null);
    v["origin"] = match &t.origin_kind {
        Some(kind) => serde_json::json!({ "kind": kind, "ref": t.origin_ref }),
        None => Value::Null,
    };
    v["for_user"] = Value::Bool(for_user(t));
    v
}

/// Whether the user is the one to act on it now: open and theirs (assigned
/// to `user`, or to no one, as every todo before 2026-10-09 is), or waiting
/// on their approval or review whoever it is assigned to.
pub fn for_user(t: &Todo) -> bool {
    match t.status.as_str() {
        "done" => false,
        "awaiting_approval" | "awaiting_review" => true,
        _ => t.assignee.as_deref().is_none_or(|a| a == "user"),
    }
}

/// Link a schedule drafted from an item: `(:Todo)-[:BECAME]->(:Workflow)`.
pub async fn link_became(
    neo4j: &Neo4jClient,
    item_id: &str,
    schedule_id: &str,
) -> anyhow::Result<()> {
    neo4j
        .run(
            neo4rs::query(
                "MATCH (t:Todo {id: $item}), (s:Workflow {id: $sched}) \
                 MERGE (t)-[r:BECAME]->(s) SET r.created_at = datetime()",
            )
            .param("item", item_id)
            .param("sched", schedule_id),
        )
        .await?;
    Ok(())
}

/// A schedule was turned on: every open item it was drafted from is done,
/// as "became a schedule". Returns the items closed.
pub async fn close_items_that_became(neo4j: &Neo4jClient, schedule_id: &str) -> Vec<String> {
    neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Todo)-[:BECAME]->(s:Workflow {id: $sched}) WHERE t.status <> 'done' \
                 SET t.status = 'done', t.closed_reason = 'became a schedule', t.updated_at = datetime() \
                 RETURN t.id AS id",
            )
            .param("sched", schedule_id),
        )
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.get::<String>("id").ok())
        .collect()
}

/// Tools that act outside the knowledge graph: an HTTP call that can POST, a
/// call into another MCP server (aidevboard's `apply_to_job` submits a real
/// application), a file written to disk, a proposal, a drafted plugin bundle. An
/// agent holding any of them asks before an item runs.
pub const EXTERNAL_EFFECT_TOOLS: &[&str] = &[
    "http_request",
    "mcp_call",
    "write_workspace_file",
    "write_proposal",
    "manage_plugin",
];

/// Whether an agent's item runs on its own or waits for the user.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SideEffects {
    /// Writes nothing outside the graph: runs when due.
    ReadOnly,
    /// May act outside the graph: waits for the user's approval. The reason
    /// names what it holds.
    AsksFirst(String),
}

/// An agent's side effects: its profile's explicit `side_effects`, else
/// derived from its tools. Fails safe: an unknown value, an unknown agent or
/// `agent:auto` (who runs is not known until it runs) asks first.
pub fn side_effects(
    profile: Option<&agent_brain_work::context_builder::ContextProfile>,
) -> SideEffects {
    let Some(p) = profile else {
        return SideEffects::AsksFirst("which agent runs is chosen when it runs".to_string());
    };
    match p.side_effects.as_deref().map(str::trim) {
        Some("read_only") => return SideEffects::ReadOnly,
        Some("writes") => {
            return SideEffects::AsksFirst(format!(
                "`{}` is declared side_effects: writes",
                p.name
            ));
        }
        Some(other) => {
            return SideEffects::AsksFirst(format!(
                "`{}` declares side_effects: {other}, which is neither read_only nor writes",
                p.name
            ));
        }
        None => {}
    }
    let held: Vec<&str> = p
        .tools
        .iter()
        .map(String::as_str)
        .filter(|t| EXTERNAL_EFFECT_TOOLS.contains(t))
        .collect();
    if held.is_empty() {
        SideEffects::ReadOnly
    } else {
        SideEffects::AsksFirst(format!(
            "`{}` can act outside the brain ({})",
            p.name,
            held.join(", ")
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roster() -> Vec<String> {
        vec!["researcher".into(), "knowledge-worker".into()]
    }

    #[test]
    fn an_assignee_is_checked_against_the_roster_and_who_writes_it() {
        let p = patch_from_json(&json!({ "assignee": "agent:researcher" }));
        assert!(validate(None, &p, &roster(), false).is_ok());
        let err = validate(None, &p, &roster(), true).unwrap_err();
        assert!(err.contains("only the user assigns"), "{err}");
        let typo = patch_from_json(&json!({ "assignee": "agent:researchr" }));
        let err = validate(None, &typo, &roster(), false).unwrap_err();
        assert!(
            err.contains("'researchr' is not a spawnable agent") && err.contains("researcher"),
            "{err}"
        );
        assert!(
            validate(
                None,
                &patch_from_json(&json!({ "assignee": "agent:auto" })),
                &roster(),
                false
            )
            .is_ok()
        );
        assert!(
            validate(
                None,
                &patch_from_json(&json!({ "assignee": "user" })),
                &roster(),
                true
            )
            .is_ok()
        );
        assert!(
            validate(
                None,
                &patch_from_json(&json!({ "assignee": "bob" })),
                &roster(),
                false
            )
            .is_err()
        );
    }

    #[test]
    fn status_origin_and_snooze_are_checked() {
        let none = TodoItemPatch::default();
        assert!(validate(Some("awaiting_review"), &none, &[], false).is_ok());
        assert!(
            validate(Some("waiting"), &none, &[], false)
                .unwrap_err()
                .contains("status 'waiting'")
        );
        let p = patch_from_json(
            &json!({ "origin": { "kind": "robot", "ref": "x" }, "snoozed_until": "tomorrow" }),
        );
        let err = validate(None, &p, &[], false).unwrap_err();
        assert!(
            err.contains("origin kind 'robot'") && err.contains("snoozed_until 'tomorrow'"),
            "{err}"
        );
    }

    #[test]
    fn a_patch_reads_present_keys_only_and_clears_on_null() {
        let p = patch_from_json(
            &json!({ "prompt": "do x", "session_id": null, "origin_kind": "agent", "origin_ref": "n1" }),
        );
        assert_eq!(p.prompt, Some(Some("do x".into())));
        assert_eq!(p.session_id, Some(None));
        assert_eq!(p.assignee, None);
        assert_eq!(p.origin_kind, Some(Some("agent".into())));
        assert!(TodoItemPatch::default().is_empty());
    }

    fn profile(
        name: &str,
        tools: &[&str],
        side: Option<&str>,
    ) -> agent_brain_work::context_builder::ContextProfile {
        serde_json::from_value(json!({
            "name": name, "description": "", "tools": tools, "side_effects": side,
        }))
        .unwrap()
    }

    #[test]
    fn side_effects_are_derived_from_tools_and_fail_safe() {
        assert_eq!(
            side_effects(Some(&profile(
                "knowledge-worker",
                &["search_notes", "store_note"],
                None
            ))),
            SideEffects::ReadOnly
        );
        // researcher holds http_request and mcp_call (2026-10-09): it asks.
        let r = side_effects(Some(&profile(
            "researcher",
            &["search_web", "http_request", "mcp_call"],
            None,
        )));
        assert!(
            matches!(&r, SideEffects::AsksFirst(why) if why.contains("http_request, mcp_call")),
            "{r:?}"
        );
        // An explicit declaration wins; an unknown one asks.
        assert_eq!(
            side_effects(Some(&profile("x", &["http_request"], Some("read_only")))),
            SideEffects::ReadOnly
        );
        assert!(matches!(
            side_effects(Some(&profile("x", &[], Some("maybe")))),
            SideEffects::AsksFirst(_)
        ));
        assert!(matches!(side_effects(None), SideEffects::AsksFirst(_)));
    }

    #[test]
    fn titles_come_from_the_first_line() {
        assert_eq!(
            title_from("\n## API key needed\nMore detail"),
            "API key needed"
        );
        assert_eq!(title_from(&"x".repeat(200)).chars().count(), 120);
    }
}
