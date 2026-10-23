//! `manage_todo`: the brain's own view of the user's items (HBI plan §9.2,
//! `TODO_DELAYED_PROMPT_PLAN.md` Phase 1).
//!
//! Until this tool, todos were REST-only: the brain read them through raw
//! Cypher in three schedules and could write nothing back. The rules are the
//! ones every writer shares (`services/items.rs`): checked statuses and
//! origins, an assignee checked against the live spawnable roster, and only
//! the user assigns an agent. Nothing here dispatches an agent (Phase 13).

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::{Value, json};

use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use agent_brain_repository::{Neo4jClient, TODO_STATUSES};
use agent_brain_scheduler::items;
use agent_brain_work::context_builder::ContextBuilderService;

/// Items listed at most; `count` always says how many matched.
const LIST_CAP: usize = 50;

pub struct TodoSkill {
    neo4j: Neo4jClient,
    context_builder: Option<Arc<ContextBuilderService>>,
    /// Starts agent runs (`dispatch_due_todos`); `None` leaves that tool off.
    queue: Option<Arc<agent_brain_work::queue::QueueService>>,
    /// Stores a delivered result as a note.
    knowledge: Option<Arc<dyn agent_brain_knowledge::KnowledgeStore>>,
}

impl TodoSkill {
    pub fn new(
        neo4j: Neo4jClient,
        context_builder: Option<Arc<ContextBuilderService>>,
        queue: Option<Arc<agent_brain_work::queue::QueueService>>,
        knowledge: Option<Arc<dyn agent_brain_knowledge::KnowledgeStore>>,
    ) -> Self {
        Self {
            neo4j,
            context_builder,
            queue,
            knowledge,
        }
    }

    fn dispatch_def() -> ToolDefinition {
        ToolDefinition {
            name: "dispatch_due_todos".to_string(),
            description: "Run the user's items that are assigned to an agent and due. An agent that \
                writes nothing outside the brain starts at once; one that can (http_request, \
                mcp_call, files, …) waits for the user's approval in the Feed. At most 3 runs \
                start per call and 3 per item; a run that failed is retried or handed to the user. \
                Called by the item-dispatch schedule; returns what it started, asked about and held."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn def() -> ToolDefinition {
        ToolDefinition {
            name: "manage_todo".to_string(),
            description: "Read and update the user's items (todos): the one list of work that \
                wants doing. action=list: open items, filterable by status and assignee \
                (`count` is every match; at most 50 are listed). action=get: one item by id. \
                action=upsert: create (no id) or update (id) an item: title, description, \
                priority (0 urgent … 3 low), due_at, prompt, session_id, status. \
                action=complete: mark it done. An item you create with the title of one still open \
                 updates that one instead (`merged_into_open_item`). Items you create are assigned to the user and \
                need `ask`: what exactly the user must do or decide, and what is blocked \
                until they do; the item records where it came from (this run, workflow or \
                chat) by itself. Only the user assigns an item to an agent. To ask the user something that \
                needs an answer, prefer notify_user with needs: \"action\", which also makes \
                the item."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "upsert", "complete", "deliver"] },
                    "result": { "type": "string", "description": "deliver: an agent run's answer for the item (its chain's last step)." },
                    "id": { "type": "string", "description": "get / upsert (update) / complete: the item id." },
                    "status": {
                        "type": "string",
                        "enum": TODO_STATUSES,
                        "description": "list: only this status (default: every status but done). upsert: set it."
                    },
                    "assignee": {
                        "type": "string",
                        "description": "list: only items assigned to this ('user', 'agent:<profile>', 'agent:auto'). upsert: 'user' or empty."
                    },
                    "include_done": { "type": "boolean", "description": "list: include done items (default false)." },
                    "program": { "type": "string", "description": "list: only this program's items. upsert: the program it belongs to (manage_program list); a workflow run's items get theirs on their own." },
                    "title": { "type": "string" },
                    "description": { "type": "string" },
                    "priority": { "type": "integer", "minimum": 0, "maximum": 3 },
                    "due_at": { "type": "string", "description": "RFC 3339 time, or empty to clear." },
                    "prompt": { "type": "string", "description": "What an agent would be told; defaults to title + description." },
                    "session_id": { "type": "string", "description": "The item's chat thread." },
                    "ask": { "type": "string", "description": "upsert: what the user must do or decide, and what waits on it, in one or two sentences (required to create). E.g. 'Get a key at https://x.dev/signup and add X_API_KEY to docker-compose.yml; the weather workflow cannot run until then.'" }
                },
                "required": ["action"]
            }),
        }
    }

    async fn handle(&self, args: Value) -> ToolCallResult {
        match args["action"].as_str().unwrap_or("") {
            "list" => self.list(&args).await,
            "get" => match self.require_id(&args).await {
                Ok(t) => ToolCallResult::success_json(items::item_json(&t)),
                Err(e) => e,
            },
            "upsert" => self.upsert(&args).await,
            "complete" => match self.require_id(&args).await {
                Ok(t) => match self
                    .neo4j
                    .update_todo(&t.id, None, None, Some("done"), None, None, None)
                    .await
                {
                    Ok(Some(t)) => ToolCallResult::success_json(
                        json!({ "completed": true, "item": items::item_json(&t) }),
                    ),
                    Ok(None) => ToolCallResult::error(format!("No item with id '{}'", t.id)),
                    Err(e) => ToolCallResult::error(format!("Failed to complete the item: {e}")),
                },
                Err(e) => e,
            },
            "deliver" => {
                let (Some(id), Some(result)) = (args["id"].as_str(), args["result"].as_str())
                else {
                    return ToolCallResult::error("deliver needs `id` and `result`");
                };
                let Some(knowledge) = self.knowledge.as_deref() else {
                    return ToolCallResult::error("deliver is unavailable: no knowledge store");
                };
                match agent_brain_scheduler::item_dispatch::deliver(
                    &self.neo4j,
                    knowledge,
                    id,
                    result,
                )
                .await
                {
                    Ok(v) => ToolCallResult::success_json(v),
                    Err(e) => ToolCallResult::error(format!("Not delivered: {e}")),
                }
            }
            other => ToolCallResult::error(format!(
                "Unknown action '{other}'; use list, get, upsert, complete or deliver"
            )),
        }
    }

    async fn require_id(
        &self,
        args: &Value,
    ) -> Result<agent_brain_repository::Todo, ToolCallResult> {
        let Some(id) = args["id"].as_str().filter(|s| !s.trim().is_empty()) else {
            return Err(ToolCallResult::error("`id` is required for this action"));
        };
        match self.neo4j.get_todo(id).await {
            Ok(Some(t)) => Ok(t),
            Ok(None) => Err(ToolCallResult::error(format!(
                "No item with id '{id}'. action=list shows the open ones."
            ))),
            Err(e) => Err(ToolCallResult::error(format!(
                "Failed to read the item: {e}"
            ))),
        }
    }

    async fn list(&self, args: &Value) -> ToolCallResult {
        let status = args["status"].as_str().filter(|s| !s.is_empty());
        if let Some(s) = status
            && !TODO_STATUSES.contains(&s)
        {
            return ToolCallResult::error(format!(
                "status '{s}' is not one of {}",
                TODO_STATUSES.join(", ")
            ));
        }
        let include_done = args["include_done"].as_bool().unwrap_or(false);
        let assignee = args["assignee"].as_str().filter(|s| !s.is_empty());
        let program = args["program"].as_str().filter(|s| !s.is_empty());
        let all = match self.neo4j.list_todos(status).await {
            Ok(t) => t,
            Err(e) => return ToolCallResult::error(format!("Failed to list items: {e}")),
        };
        let total = all.len();
        let matched: Vec<_> = all
            .into_iter()
            .filter(|t| status.is_some() || include_done || t.status != "done")
            .filter(|t| match assignee {
                None => true,
                // An unassigned item is the user's.
                Some("user") => t.assignee.as_deref().is_none_or(|a| a == "user"),
                Some(a) => t.assignee.as_deref() == Some(a),
            })
            .filter(|t| program.is_none_or(|p| t.program.as_deref() == Some(p)))
            .collect();
        let shown: Vec<Value> = matched
            .iter()
            .take(LIST_CAP)
            .map(items::item_json)
            .collect();
        let mut out = json!({
            "count": matched.len(),
            "returned": shown.len(),
            "complete": shown.len() == matched.len(),
            "items": shown,
        });
        if matched.is_empty() {
            // Say what does exist, so an empty answer is not read as "no items".
            out["note"] = json!(format!(
                "No item matches. {total} item(s) exist{}; try include_done or another status.",
                status
                    .map(|s| format!(" with status '{s}'"))
                    .unwrap_or_default()
            ));
        }
        ToolCallResult::success_json(out)
    }

    async fn upsert(&self, args: &Value) -> ToolCallResult {
        let mut patch = items::patch_from_json(args);
        // The tool sets these; a caller cannot claim another origin or a result.
        patch.origin_kind = None;
        patch.origin_ref = None;
        patch.result_note_id = None;
        let roster = items::roster(self.context_builder.as_deref()).await;
        if let Err(e) = items::validate(
            args["status"].as_str(),
            &patch,
            &roster,
            items::written_by_brain(),
        ) {
            return ToolCallResult::error(format!("Item rejected — nothing was saved: {e}"));
        }
        if let Err(e) = items::check_program(&self.neo4j, &patch).await {
            return ToolCallResult::error(format!("Item rejected — nothing was saved: {e}"));
        }
        let text = |k: &str| args[k].as_str().map(str::trim).filter(|s| !s.is_empty());
        let opt = |k: &str| -> Option<Option<&str>> {
            args.get(k)
                .map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
        };

        // An item the brain creates with the title of one still open is that
        // item again: it is updated, not repeated. On 2026-10-23 three
        // harness-improve runs filed three "Adopt harness edit for intake".
        let merged_into = match (text("id"), text("title")) {
            (None, Some(title)) if items::written_by_brain() => {
                open_item_titled(&self.neo4j, title).await
            }
            _ => None,
        };
        let saved = match text("id").or(merged_into.as_deref()) {
            Some(id) => self
                .neo4j
                .update_todo(
                    id,
                    text("title"),
                    opt("description"),
                    text("status"),
                    args["priority"].as_i64(),
                    None,
                    opt("due_at"),
                )
                .await
                .map(|t| t.map(|t| (t, false))),
            None => {
                let Some(title) = text("title") else {
                    return ToolCallResult::error("`title` is required to create an item");
                };
                if text("ask").is_none() {
                    return ToolCallResult::error(
                        "`ask` is required to create an item: say what the user must do or \
                         decide, and what is blocked until they do. Nothing was saved.",
                    );
                }
                items::stamp_source(&self.neo4j, &mut patch).await;
                // What the brain creates is the user's to act on, and says so.
                patch.assignee = patch.assignee.or(Some(Some("user".to_string())));
                patch.origin_kind = Some(Some(
                    if items::written_by_brain() {
                        "agent"
                    } else {
                        "user"
                    }
                    .to_string(),
                ));
                self.neo4j
                    .create_todo(
                        title,
                        text("description"),
                        text("status"),
                        args["priority"].as_i64(),
                        None,
                        text("due_at"),
                    )
                    .await
                    .map(|t| Some((t, true)))
            }
        };
        match saved {
            Ok(Some((t, created))) => match self.neo4j.patch_todo_item(&t.id, &patch).await {
                Ok(Some(t)) => ToolCallResult::success_json(json!({
                    "created": created,
                    "updated": !created,
                    "merged_into_open_item": merged_into.is_some(),
                    "item": items::item_json(&t),
                })),
                Ok(None) => ToolCallResult::error("The item vanished while saving"),
                Err(e) => {
                    ToolCallResult::error(format!("Saved, but its item fields were not: {e}"))
                }
            },
            Ok(None) => ToolCallResult::error(format!(
                "No item with id '{}'",
                text("id").unwrap_or_default()
            )),
            Err(e) => ToolCallResult::error(format!("Failed to save the item: {e}")),
        }
    }
}

/// The id of an open item (not done) whose title is `title`, ignoring case
/// and surrounding space: where an item the brain creates again goes.
async fn open_item_titled(neo4j: &Neo4jClient, title: &str) -> Option<String> {
    let todos = neo4j.list_todos(None).await.ok()?;
    pick_open_titled(&todos, title)
}

fn pick_open_titled(todos: &[agent_brain_repository::Todo], title: &str) -> Option<String> {
    todos
        .iter()
        .find(|t| t.status != "done" && t.title.trim().eq_ignore_ascii_case(title.trim()))
        .map(|t| t.id.clone())
}

#[async_trait]
impl Skill for TodoSkill {
    fn name(&self) -> &str {
        "Todos"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        let mut t = vec![Self::def()];
        if self.queue.is_some() {
            t.push(Self::dispatch_def());
        }
        t
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "manage_todo" => Some(self.handle(arguments.unwrap_or(json!({}))).await),
            "dispatch_due_todos" => {
                let Some(queue) = self.queue.clone() else {
                    return Some(ToolCallResult::error(
                        "dispatch_due_todos is unavailable: no job queue",
                    ));
                };
                let d = agent_brain_scheduler::item_dispatch::Dispatcher::new(
                    self.neo4j.clone(),
                    queue,
                    self.context_builder.clone(),
                );
                Some(ToolCallResult::success_json(d.poll().await))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(id: &str, title: &str, status: &str) -> agent_brain_repository::Todo {
        serde_json::from_value(json!({
            "id": id, "title": title, "description": null, "status": status, "priority": 2,
            "tags": [], "due_at": null, "created_at": "", "updated_at": ""
        }))
        .unwrap()
    }

    #[test]
    fn a_repeated_title_goes_to_the_open_item_only() {
        let todos = vec![
            todo("a", "Adopt harness edit for intake", "done"),
            todo("b", "Adopt harness edit for intake ", "pending"),
            todo("c", "Something else", "pending"),
        ];
        assert_eq!(
            pick_open_titled(&todos, "adopt harness edit for INTAKE").as_deref(),
            Some("b")
        );
        assert_eq!(
            pick_open_titled(&todos[..1], "Adopt harness edit for intake"),
            None
        );
        assert_eq!(pick_open_titled(&todos, "New thing"), None);
    }
}
