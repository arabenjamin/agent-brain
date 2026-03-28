//! Procedure Skill - Provides tools for storing and retrieving procedural memory.

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::info;
use uuid::Uuid;

use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use crate::services::traits::ProcedureStore;
use crate::skills::Skill;

/// Procedure Skill implementation.
pub struct ProcedureSkill {
    store: Arc<dyn ProcedureStore>,
}

impl ProcedureSkill {
    /// Create a new procedure skill.
    pub fn new(store: Arc<dyn ProcedureStore>) -> Self {
        Self { store }
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn store_procedure_def() -> ToolDefinition {
        ToolDefinition {
            name: "store_procedure".to_string(),
            description: "Store a named multi-step procedure (workflow) in procedural memory. \
                         Each step should describe a tool call with its purpose."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Short name for the procedure (e.g. 'Research loop')"
                    },
                    "description": {
                        "type": "string",
                        "description": "When and why to use this procedure"
                    },
                    "steps": {
                        "type": "array",
                        "description": "Ordered list of steps. Each step: { tool, args?, purpose }",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": { "type": "string" },
                                "args": { "type": "object" },
                                "purpose": { "type": "string" }
                            },
                            "required": ["tool", "purpose"]
                        }
                    }
                },
                "required": ["name", "description", "steps"]
            }),
        }
    }

    fn search_procedures_def() -> ToolDefinition {
        ToolDefinition {
            name: "search_procedures".to_string(),
            description: "Search stored procedures by name or description using keyword matching."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Keyword to search in procedure names and descriptions"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max results to return (default: 5)"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    async fn handle_store_procedure(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: StoreProcedureInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(name = %input.name, steps = input.steps.len(), "Storing procedure");

        let id = Uuid::new_v4().to_string();
        let timestamp = Utc::now().to_rfc3339();
        let steps_json = match serde_json::to_string(&input.steps) {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Failed to serialize steps: {}", e)),
        };

        match self.store.store_procedure(&id, &input.name, &input.description, &steps_json, &timestamp).await {
            Ok(_) => {
                let response = json!({
                    "success": true,
                    "id": id,
                    "name": input.name,
                    "steps_count": input.steps.len()
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to store procedure: {}", e)),
        }
    }

    async fn handle_search_procedures(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SearchProceduresInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(query = %input.query, "Searching procedures");

        match self.store.search_procedures(&input.query, input.limit).await {
            Ok(procedures) => {
                let response = json!({
                    "count": procedures.len(),
                    "procedures": procedures
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Search failed: {}", e)),
        }
    }
}

#[async_trait]
impl Skill for ProcedureSkill {
    fn name(&self) -> &str {
        "Procedure Memory"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::store_procedure_def(), Self::search_procedures_def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "store_procedure" => Some(self.handle_store_procedure(arguments).await),
            "search_procedures" => Some(self.handle_search_procedures(arguments).await),
            _ => None,
        }
    }
}

// ============================================================================
// Input structs
// ============================================================================

#[derive(Debug, Deserialize)]
struct StoreProcedureInput {
    name: String,
    description: String,
    steps: Vec<Value>,
}

#[derive(Debug, Deserialize)]
struct SearchProceduresInput {
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    5
}

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: Option<Value>) -> Result<T, ToolCallResult> {
    let args = arguments.unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(args)
        .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {}", e)))
}
