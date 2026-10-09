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

use crate::repository::{Neo4jClient, TODO_STATUSES};
use crate::services::context_builder::ContextBuilderService;
use crate::services::items;
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

/// Items listed at most; `count` always says how many matched.
const LIST_CAP: usize = 50;

pub struct TodoSkill {
    neo4j: Neo4jClient,
    context_builder: Option<Arc<ContextBuilderService>>,
}

impl TodoSkill {
    pub fn new(neo4j: Neo4jClient, context_builder: Option<Arc<ContextBuilderService>>) -> Self {
        Self {
            neo4j,
            context_builder,
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
                action=complete: mark it done. Items you create are assigned to the user; \
                only the user assigns an item to an agent. To ask the user something that \
                needs an answer, prefer notify_user with needs: \"action\", which also makes \
                the item."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "upsert", "complete"] },
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
                    "title": { "type": "string" },
                    "description": { "type": "string" },
                    "priority": { "type": "integer", "minimum": 0, "maximum": 3 },
                    "due_at": { "type": "string", "description": "RFC 3339 time, or empty to clear." },
                    "prompt": { "type": "string", "description": "What an agent would be told; defaults to title + description." },
                    "session_id": { "type": "string", "description": "The item's chat thread." }
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
            other => ToolCallResult::error(format!(
                "Unknown action '{other}'; use list, get, upsert or complete"
            )),
        }
    }

    async fn require_id(&self, args: &Value) -> Result<crate::repository::Todo, ToolCallResult> {
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
        let text = |k: &str| args[k].as_str().map(str::trim).filter(|s| !s.is_empty());
        let opt = |k: &str| -> Option<Option<&str>> {
            args.get(k)
                .map(|v| v.as_str().map(str::trim).filter(|s| !s.is_empty()))
        };

        let saved = match text("id") {
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

#[async_trait]
impl Skill for TodoSkill {
    fn name(&self) -> &str {
        "Todos"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "manage_todo" => Some(self.handle(arguments.unwrap_or(json!({}))).await),
            _ => None,
        }
    }
}
