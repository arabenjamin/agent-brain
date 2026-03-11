//! Procedure Skill - Provides tools for storing and retrieving procedural memory.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::info;
use uuid::Uuid;
use chrono::Utc;

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::repository::Neo4jClient;
use crate::skills::Skill;

/// Procedure Skill implementation.
pub struct ProcedureSkill {
    neo4j: Neo4jClient,
}

impl ProcedureSkill {
    /// Create a new procedure skill.
    pub fn new(neo4j: Neo4jClient) -> Self {
        Self { neo4j }
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

        let cypher = r#"
        CREATE (p:Procedure {
            id: $id,
            name: $name,
            description: $description,
            steps: $steps,
            created_at: datetime($timestamp)
        })
        "#;

        let query = neo4rs::query(cypher)
            .param("id", id.clone())
            .param("name", input.name.clone())
            .param("description", input.description.clone())
            .param("steps", steps_json)
            .param("timestamp", timestamp);

        match self.neo4j.run(query).await {
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

        let cypher = r#"
        MATCH (p:Procedure)
        WHERE toLower(p.name) CONTAINS toLower($query)
           OR toLower(p.description) CONTAINS toLower($query)
        RETURN p.id AS id, p.name AS name, p.description AS description, p.steps AS steps
        LIMIT $limit
        "#;

        let query = neo4rs::query(cypher)
            .param("query", input.query.clone())
            .param("limit", input.limit as i64);

        match self.neo4j.execute(query).await {
            Ok(rows) => {
                let mut procedures = Vec::new();
                for row in rows {
                    let id = row.get::<String>("id").unwrap_or_default();
                    let name = row.get::<String>("name").unwrap_or_default();
                    let description = row.get::<String>("description").unwrap_or_default();
                    let steps_str = row.get::<String>("steps").unwrap_or_else(|_| "[]".to_string());
                    let steps: Value = serde_json::from_str(&steps_str)
                        .unwrap_or(json!([]));

                    procedures.push(json!({
                        "id": id,
                        "name": name,
                        "description": description,
                        "steps": steps
                    }));
                }

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
        vec![
            Self::store_procedure_def(),
            Self::search_procedures_def(),
        ]
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

fn parse_args<T: for<'de> Deserialize<'de>>(
    arguments: Option<Value>,
) -> Result<T, ToolCallResult> {
    let args = arguments.unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(args)
        .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {}", e)))
}
