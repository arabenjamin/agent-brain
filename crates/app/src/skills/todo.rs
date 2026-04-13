//! Todo Skill — CRUD tools for the LLM to manage todo items stored in DuckDB.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::repository::TelemetryClient;
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

pub struct TodoSkill {
    store: Arc<TelemetryClient>,
}

impl TodoSkill {
    pub fn new(store: Arc<TelemetryClient>) -> Self {
        Self { store }
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn create_todo_def() -> ToolDefinition {
        ToolDefinition {
            name: "create_todo".to_string(),
            description: "Create a new todo item.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short title for the todo" },
                    "description": { "type": "string", "description": "Optional longer description" },
                    "status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "done"],
                        "description": "Initial status (default: pending)"
                    },
                    "priority": {
                        "type": "integer",
                        "description": "0=urgent, 1=high, 2=normal, 3=low (default: 2)"
                    },
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags"
                    },
                    "due_at": { "type": "string", "description": "Optional ISO-8601 due date" }
                },
                "required": ["title"]
            }),
        }
    }

    fn list_todos_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_todos".to_string(),
            description: "List todo items, optionally filtered by status.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "done"],
                        "description": "Filter by status. Omit to list all todos."
                    }
                }
            }),
        }
    }

    fn get_todo_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_todo".to_string(),
            description: "Fetch a single todo item by its ID.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Todo UUID" }
                },
                "required": ["id"]
            }),
        }
    }

    fn update_todo_def() -> ToolDefinition {
        ToolDefinition {
            name: "update_todo".to_string(),
            description: "Update a todo item. Only provided fields are changed.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Todo UUID" },
                    "title": { "type": "string" },
                    "description": { "type": ["string", "null"] },
                    "status": {
                        "type": "string",
                        "enum": ["pending", "in_progress", "done"]
                    },
                    "priority": { "type": "integer" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "due_at": { "type": ["string", "null"] }
                },
                "required": ["id"]
            }),
        }
    }

    fn delete_todo_def() -> ToolDefinition {
        ToolDefinition {
            name: "delete_todo".to_string(),
            description: "Permanently delete a todo item.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Todo UUID" }
                },
                "required": ["id"]
            }),
        }
    }

    // ========================================================================
    // Handlers
    // ========================================================================

    async fn handle_create_todo(&self, arguments: Option<Value>) -> ToolCallResult {
        let args: CreateTodoInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let tags_json = serde_json::to_string(&args.tags.unwrap_or_default()).unwrap_or_default();
        match self.store.create_todo(
            &args.title,
            args.description.as_deref(),
            args.status.as_deref(),
            args.priority,
            Some(&tags_json),
            args.due_at.as_deref(),
        ) {
            Ok(todo) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&todo).unwrap_or_default(),
            ),
            Err(e) => ToolCallResult::error(format!("Failed to create todo: {e}")),
        }
    }

    async fn handle_list_todos(&self, arguments: Option<Value>) -> ToolCallResult {
        let args: ListTodosInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        match self.store.list_todos(args.status.as_deref()) {
            Ok(todos) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&todos).unwrap_or_default(),
            ),
            Err(e) => ToolCallResult::error(format!("Failed to list todos: {e}")),
        }
    }

    async fn handle_get_todo(&self, arguments: Option<Value>) -> ToolCallResult {
        let args: IdInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        match self.store.get_todo(&args.id) {
            Ok(Some(todo)) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&todo).unwrap_or_default(),
            ),
            Ok(None) => ToolCallResult::error(format!("Todo '{}' not found", args.id)),
            Err(e) => ToolCallResult::error(format!("Failed to fetch todo: {e}")),
        }
    }

    async fn handle_update_todo(&self, arguments: Option<Value>) -> ToolCallResult {
        let args: UpdateTodoInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let tags_json = args.tags.as_ref().map(|t| {
            serde_json::to_string(t).unwrap_or_else(|_| "[]".to_string())
        });

        let description_ref = args.description.as_ref().map(|d| d.as_deref());
        let due_at_ref = args.due_at.as_ref().map(|d| d.as_deref());

        match self.store.update_todo(
            &args.id,
            args.title.as_deref(),
            description_ref,
            args.status.as_deref(),
            args.priority,
            tags_json.as_deref(),
            due_at_ref,
        ) {
            Ok(Some(todo)) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&todo).unwrap_or_default(),
            ),
            Ok(None) => ToolCallResult::error(format!("Todo '{}' not found", args.id)),
            Err(e) => ToolCallResult::error(format!("Failed to update todo: {e}")),
        }
    }

    async fn handle_delete_todo(&self, arguments: Option<Value>) -> ToolCallResult {
        let args: IdInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        match self.store.delete_todo(&args.id) {
            Ok(true) => ToolCallResult::success_text(
                json!({"deleted": true, "id": args.id}).to_string(),
            ),
            Ok(false) => ToolCallResult::error(format!("Todo '{}' not found", args.id)),
            Err(e) => ToolCallResult::error(format!("Failed to delete todo: {e}")),
        }
    }
}

#[async_trait]
impl Skill for TodoSkill {
    fn name(&self) -> &str {
        "Todo"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::create_todo_def(),
            Self::list_todos_def(),
            Self::get_todo_def(),
            Self::update_todo_def(),
            Self::delete_todo_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "create_todo" => Some(self.handle_create_todo(arguments).await),
            "list_todos" => Some(self.handle_list_todos(arguments).await),
            "get_todo" => Some(self.handle_get_todo(arguments).await),
            "update_todo" => Some(self.handle_update_todo(arguments).await),
            "delete_todo" => Some(self.handle_delete_todo(arguments).await),
            _ => None,
        }
    }
}

// ============================================================================
// Input structs
// ============================================================================

#[derive(Debug, Deserialize)]
struct CreateTodoInput {
    title: String,
    description: Option<String>,
    status: Option<String>,
    priority: Option<i64>,
    tags: Option<Vec<String>>,
    due_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListTodosInput {
    status: Option<String>,
}

#[derive(Debug, Deserialize)]
struct IdInput {
    id: String,
}

#[derive(Debug, Deserialize)]
struct UpdateTodoInput {
    id: String,
    title: Option<String>,
    description: Option<Option<String>>,
    status: Option<String>,
    priority: Option<i64>,
    tags: Option<Vec<String>>,
    due_at: Option<Option<String>>,
}

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: Option<Value>) -> Result<T, ToolCallResult> {
    let args = arguments.unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(args)
        .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {e}")))
}
