//! `manage_program` (PROGRAMS_PLAN.md Phase 1): the brain's programs —
//! departments, projects, investigations, cases and shared services — as a
//! tool. The rules are `services/programs.rs`, shared with REST.

use async_trait::async_trait;
use serde_json::{Value, json};

use agent_brain_protocol::{Skill, ToolCallResult, ToolDefinition};
use agent_brain_repository::Neo4jClient;
use agent_brain_work::programs::{self, PROGRAM_KINDS, PROGRAM_STATUSES};

pub struct ProgramSkill {
    neo4j: Neo4jClient,
}

impl ProgramSkill {
    pub fn new(neo4j: Neo4jClient) -> Self {
        Self { neo4j }
    }

    fn def() -> ToolDefinition {
        ToolDefinition {
            name: "manage_program".to_string(),
            description: "Programs organize the brain's work: a department (standing work), \
                project (a deliverable), investigation or case (a question that closes with a \
                finding), or services (on-demand workflows every program uses). Programs nest \
                (`parent`); each owns workflows (one program per workflow), names the topics \
                it covers and the agents and APIs it relies on, and has a charter and success \
                criteria. USE THIS to answer how a line of work is organized or doing. \
                action=list: the tree, with each program's issues and the enabled workflows in \
                no program. action=get: one program in full (`name`): workflows' state, \
                children, open todos. action=upsert: create or edit `name`; keys left out keep \
                their values, null resets one. A paused or closed program pauses its workflows \
                (except `keep_running`) and active again resumes exactly those. Closing an \
                investigation or case needs a `finding`; a closed program is a cold case the \
                owner reopens. Editing a seeded program makes it runtime-owned. \
                action=delete: remove `name` (refused while programs sit under it). \
                action=reset: put a seeded program's file back. action=audit: only what is \
                wrong (issues, unassigned workflows). The program-review workflow's steps: \
                action=review_context (`name`): what a review is given; action=record_review \
                (`name`, `answer`: the program-manager's JSON answer): files its proposals as \
                todos tagged with the program and returns the review note; \
                action=start_due_reviews: start a review run for every program that is due."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "upsert", "delete", "reset", "audit", "review_context", "record_review", "start_due_reviews"] },
                    "answer": { "type": "string", "description": "record_review: the program-manager's answer, a JSON object {verdict, summary, findings, proposals}." },
                    "name": { "type": "string", "description": "The program's slug." },
                    "label": { "type": "string" },
                    "kind": { "type": "string", "enum": PROGRAM_KINDS },
                    "parent": { "type": ["string", "null"], "description": "The program above it; null = top level." },
                    "status": { "type": "string", "enum": PROGRAM_STATUSES },
                    "charter": { "type": "string", "description": "What it is for, in a few sentences." },
                    "success": { "type": "string", "description": "What good output looks like." },
                    "topics": { "type": "array", "items": { "type": "string" } },
                    "workflows": { "type": "array", "items": { "type": "string" }, "description": "The whole list (it replaces the stored one)." },
                    "keep_running": { "type": "array", "items": { "type": "string" }, "description": "Its workflows a pause leaves on." },
                    "agents": { "type": "array", "items": { "type": "string" } },
                    "apis": { "type": "array", "items": { "type": "string" } },
                    "review": { "type": ["object", "null"], "properties": { "every": { "type": "string" } } },
                    "budget": { "type": ["object", "null"], "properties": { "monthly_usd": { "type": "number" } } },
                    "finding": { "type": ["string", "null"], "description": "What a closed investigation or case found." }
                },
                "required": ["action"]
            }),
        }
    }

    async fn handle(&self, args: Value) -> ToolCallResult {
        let name = args["name"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let need_name = || ToolCallResult::error("`name` is required for this action".to_string());
        let saved = |r: Result<programs::Saved, programs::SaveError>| match r {
            Ok(s) => ToolCallResult::success_json(json!(s)),
            Err(e) => ToolCallResult::error(e.to_string()),
        };
        match args["action"].as_str().unwrap_or("") {
            "list" => match programs::tree(&self.neo4j).await {
                Ok(t) => ToolCallResult::success_json(t),
                Err(e) => ToolCallResult::error(format!("Failed to list programs: {e}")),
            },
            "audit" => match programs::tree(&self.neo4j).await {
                Ok(mut t) => {
                    let rows: Vec<Value> = t["programs"]
                        .as_array()
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|r| r["issues"].as_array().is_some_and(|i| !i.is_empty()))
                        .map(|r| json!({"name": r["name"], "issues": r["issues"]}))
                        .collect();
                    t["programs"] = json!(rows);
                    t["count"] = json!(rows.len());
                    ToolCallResult::success_json(t)
                }
                Err(e) => ToolCallResult::error(format!("Failed to audit programs: {e}")),
            },
            "get" => {
                let Some(name) = name else { return need_name() };
                match programs::detail(&self.neo4j, name).await {
                    Ok(Some(p)) => ToolCallResult::success_json(p),
                    Ok(None) => {
                        let names: Vec<String> = programs::load_all(&self.neo4j)
                            .await
                            .unwrap_or_default()
                            .into_iter()
                            .map(|s| s.def.name)
                            .collect();
                        ToolCallResult::error(format!(
                            "No program '{name}'. Programs: {}",
                            names.join(", ")
                        ))
                    }
                    Err(e) => ToolCallResult::error(e.to_string()),
                }
            }
            "upsert" => {
                if name.is_none() {
                    return need_name();
                }
                saved(programs::save(&self.neo4j, &args, false).await)
            }
            "delete" => {
                let Some(name) = name else { return need_name() };
                match programs::delete(&self.neo4j, name).await {
                    Ok(changes) => {
                        ToolCallResult::success_json(json!({"deleted": name, "changes": changes}))
                    }
                    Err(e) => ToolCallResult::error(e.to_string()),
                }
            }
            "reset" => {
                let Some(name) = name else { return need_name() };
                let plugins = agent_brain_graph::bundles::Plugins::global();
                saved(programs::reset(&self.neo4j, &plugins, name).await)
            }
            "review_context" => {
                let Some(name) = name else { return need_name() };
                match programs::review_context(&self.neo4j, name).await {
                    Ok(Some(c)) => ToolCallResult::success_json(c),
                    Ok(None) => ToolCallResult::error(format!("No program '{name}'")),
                    Err(e) => ToolCallResult::error(e.to_string()),
                }
            }
            "record_review" => {
                let Some(name) = name else { return need_name() };
                self.record_review(name, args["answer"].as_str().unwrap_or(""))
                    .await
            }
            "start_due_reviews" => match programs::start_due_reviews(&self.neo4j).await {
                Ok(started) => ToolCallResult::success_json(json!({
                    "started": started.len(),
                    "runs": started.iter().map(|(p, t)| json!({"program": p, "task_id": t})).collect::<Vec<_>>(),
                    "note": if started.is_empty() { "No program's review is due." } else { "Each run is dispatched on a scheduler tick." },
                })),
                Err(e) => ToolCallResult::error(format!("Failed to start reviews: {e}")),
            },
            other => ToolCallResult::error(format!(
                "Unknown action '{other}': use list, get, upsert, delete, reset, audit, \
                 review_context, record_review or start_due_reviews."
            )),
        }
    }
}

impl ProgramSkill {
    /// File a review: each proposal becomes a todo for the owner, tagged with
    /// the program (one already open with the same title is not filed again),
    /// the program is stamped with the verdict, and the note text is returned
    /// for the workflow's `store_note` step. The manager proposes; nothing
    /// else changes.
    async fn record_review(&self, name: &str, text: &str) -> ToolCallResult {
        use agent_brain_scheduler::items;
        let review = match programs::parse_review(text) {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Review not recorded: {e}")),
        };
        let Ok(Some(detail)) = programs::detail(&self.neo4j, name).await else {
            return ToolCallResult::error(format!("No program '{name}'"));
        };
        let label = detail["label"].as_str().unwrap_or(name).to_string();
        let open: Vec<String> = detail["open_todos"]
            .as_array()
            .map(|a| {
                a.iter()
                    .filter_map(|t| t["title"].as_str().map(|s| s.trim().to_lowercase()))
                    .collect()
            })
            .unwrap_or_default();
        let date = agent_brain_graph::clock::today();
        let mut created = Vec::new();
        let mut skipped = Vec::new();
        for (i, p) in review.proposals.iter().enumerate() {
            let title = p.title.trim().to_string();
            if i >= programs::MAX_PROPOSALS {
                skipped.push(json!({"title": title, "why": "over the per-review limit"}));
                continue;
            }
            if open.contains(&title.to_lowercase()) {
                skipped.push(json!({"title": title, "why": "already open"}));
                continue;
            }
            let ask = if p.ask.trim().is_empty() {
                format!("Approve or drop this proposal from the {label} review.")
            } else {
                p.ask.trim().to_string()
            };
            let description = match p.target.as_deref().filter(|t| !t.trim().is_empty()) {
                Some(t) => format!(
                    "{}\n\nAbout: {t}. From the {label} program review of {date}.",
                    p.why.trim()
                ),
                None => format!(
                    "{}\n\nFrom the {label} program review of {date}.",
                    p.why.trim()
                ),
            };
            let item = items::NewItem {
                title: title.clone(),
                description: Some(description),
                priority: Some(2),
                due_at: None,
                patch: agent_brain_repository::TodoItemPatch {
                    assignee: Some(Some("user".into())),
                    origin_kind: Some(Some("agent".into())),
                    origin_ref: Some(Some(format!("program-review:{name}:{date}:{i}"))),
                    ask: Some(Some(ask)),
                    program: Some(Some(name.to_string())),
                    source: Some(Some(format!(
                        "agent program-manager, reviewing program '{name}'"
                    ))),
                    ..Default::default()
                },
            };
            match items::create_item(&self.neo4j, item).await {
                Ok((t, true)) => created.push(json!({"id": t.id, "title": t.title})),
                Ok((t, false)) => {
                    skipped.push(json!({"title": t.title, "why": "filed already by this review"}))
                }
                Err(e) => skipped.push(json!({"title": title, "why": format!("not filed: {e}")})),
            }
        }
        if let Err(e) = programs::mark_reviewed(&self.neo4j, name, &review).await {
            return ToolCallResult::error(format!(
                "Proposals filed, but the program was not stamped: {e}"
            ));
        }
        let filed = if created.is_empty() {
            vec![]
        } else {
            vec![format!(
                "Filed {} proposal(s) as todos for the owner.",
                created.len()
            )]
        };
        ToolCallResult::success_json(json!({
            "program": name,
            "verdict": review.verdict,
            "created": created,
            "skipped": skipped,
            "note": programs::review_note(&label, name, &date, &review, &filed),
        }))
    }
}

#[async_trait]
impl Skill for ProgramSkill {
    fn name(&self) -> &str {
        "program"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "manage_program" => Some(self.handle(arguments.unwrap_or_default()).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The review workflow's `manage_program` steps fit the tool's own schema
    /// (a duplicate `review` key once made `record_review`'s argument an object).
    #[test]
    fn the_review_workflow_fits_the_tools_schema() {
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../plugins/core/workflows/program-review.yaml");
        let (wf, _) = agent_brain_work::workflow_seeder::load_file(&path, &[]).unwrap();
        // The whole chain (its step references), checked against this tool's schema.
        let steps: Vec<Value> = serde_json::from_str(&wf.steps).unwrap();
        let ours = steps
            .iter()
            .filter(|s| s["tool_name"] == "manage_program")
            .count();
        assert_eq!(ours, 2);
        let def = ProgramSkill::def();
        let schemas = std::collections::HashMap::from([(def.name.clone(), def.input_schema)]);
        let errors = agent_brain_work::validation::validate_step_array(&steps, &[], &schemas);
        assert!(
            errors.is_empty(),
            "{:?}",
            errors.iter().map(|e| e.render()).collect::<Vec<_>>()
        );
    }
}
