//! Dynamic Skill — allows defining new MCP tools at runtime backed by procedure pipelines.
//!
//! DynamicTool nodes are persisted in Neo4j so they survive restarts. On startup
//! `load_from_neo4j` populates the in-memory map. Two skill instances share the
//! same `Arc<RwLock<HashMap>>` so both the ToolRegistry and ToolHandler see
//! changes instantly.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio::sync::RwLock;
use tracing::{info, warn};
use uuid::Uuid;

use crate::mcp::tools::{ToolHandler, ToolRegistry};
use crate::repository::Neo4jClient;
use crate::services::procedure_executor;
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

// ============================================================================
// Internal types
// ============================================================================

#[derive(Debug, Clone)]
struct DynamicToolEntry {
    definition: ToolDefinition,
    procedure_id: String,
}

// ============================================================================
// DynamicSkill
// ============================================================================

/// Skill that manages runtime-defined tools backed by procedure pipelines.
pub struct DynamicSkill {
    neo4j: Neo4jClient,
    /// Shared map — both registry and handler instances point to the same Arc.
    tools_map: Arc<RwLock<HashMap<String, DynamicToolEntry>>>,
    /// Reference to the current ToolHandler (for procedure execution).
    tool_handler_ref: Arc<RwLock<Option<ToolHandler>>>,
    /// Reference to the tool registry — used to validate step tool names at define time.
    tool_registry: Arc<RwLock<ToolRegistry>>,
}

impl DynamicSkill {
    /// Create a new DynamicSkill with an empty tools map.
    pub fn new(
        neo4j: Neo4jClient,
        tool_handler_ref: Arc<RwLock<Option<ToolHandler>>>,
        tool_registry: Arc<RwLock<ToolRegistry>>,
    ) -> Self {
        Self {
            neo4j,
            tools_map: Arc::new(RwLock::new(HashMap::new())),
            tool_handler_ref,
            tool_registry,
        }
    }

    /// Create a second instance sharing the same `tools_map` Arc.
    /// Used to populate both the ToolRegistry and the ToolHandler without duplication.
    pub fn clone_shared(&self) -> Self {
        Self {
            neo4j: self.neo4j.clone(),
            tools_map: Arc::clone(&self.tools_map),
            tool_handler_ref: Arc::clone(&self.tool_handler_ref),
            tool_registry: Arc::clone(&self.tool_registry),
        }
    }

    /// Load persisted DynamicTool nodes from Neo4j into the in-memory map.
    pub async fn load_from_neo4j(&self) {
        let cypher = r#"
        MATCH (d:DynamicTool)-[:USES]->(p:Procedure)
        RETURN d.name AS name, d.description AS description,
               d.input_schema AS input_schema, p.id AS procedure_id
        "#;

        let rows = match self.neo4j.execute(neo4rs::query(cypher)).await {
            Ok(r) => r,
            Err(e) => {
                warn!("Failed to load dynamic tools from Neo4j: {}", e);
                return;
            }
        };

        let mut map = self.tools_map.write().await;
        let mut count = 0usize;

        for row in rows {
            let name = row.get::<String>("name").unwrap_or_default();
            let description = row.get::<String>("description").unwrap_or_default();
            let schema_str = row
                .get::<String>("input_schema")
                .unwrap_or_else(|_| "{}".to_string());
            let procedure_id = row.get::<String>("procedure_id").unwrap_or_default();

            if name.is_empty() || procedure_id.is_empty() {
                continue;
            }

            let input_schema: Value = serde_json::from_str(&schema_str).unwrap_or(json!({}));

            map.insert(
                name.clone(),
                DynamicToolEntry {
                    definition: ToolDefinition {
                        name,
                        description,
                        input_schema,
                    },
                    procedure_id,
                },
            );
            count += 1;
        }

        if count > 0 {
            info!(count, "Loaded dynamic tools from Neo4j");
        }
    }

    // ========================================================================
    // Tool definitions (static tools in DynamicSkill itself)
    // ========================================================================

    fn manage_dynamic_tool_def() -> ToolDefinition {
        ToolDefinition {
            name: "manage_dynamic_tool".to_string(),
            description: "Define or remove runtime MCP tools backed by procedure pipelines. \
                         Defined tools are persisted in Neo4j and available immediately. \
                         Actions: define (create a new tool), remove (delete an existing tool)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "required": ["action"],
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["define", "remove"],
                        "description": "Operation: define (create new tool) or remove (delete existing tool)."
                    },
                    "name": {
                        "type": "string",
                        "description": "Tool name. Required for both actions."
                    },
                    "description": {
                        "type": "string",
                        "description": "Human-readable description. Required for define."
                    },
                    "input_schema": {
                        "type": "object",
                        "description": "JSON Schema for the tool's input parameters. Required for define."
                    },
                    "steps": {
                        "type": "array",
                        "description": "Ordered procedure steps. Each step: {tool, args?, purpose, output_var?, condition?}. Required for define.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool": { "type": "string" },
                                "args": { "type": "object" },
                                "purpose": { "type": "string" },
                                "output_var": { "type": "string" },
                                "condition": { "type": "string" },
                                "retry_policy": {
                                    "type": "object",
                                    "properties": {
                                        "max_attempts": { "type": "integer" },
                                        "delay_ms": { "type": "integer" }
                                    }
                                },
                                "loop": { "type": "boolean" },
                                "loop_until": { "type": "string" }
                            },
                            "required": ["tool", "purpose"]
                        }
                    },
                    "test_input": {
                        "type": "object",
                        "description": "Optional input to dry-run the steps before registering (define only)."
                    }
                }
            }),
        }
    }

    fn execute_procedure_def() -> ToolDefinition {
        ToolDefinition {
            name: "execute_procedure".to_string(),
            description: "Execute a stored procedure by its ID with optional input arguments. \
                         Use dry_run to validate steps without calling tools."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "procedure_id": {
                        "type": "string",
                        "description": "ID of the Procedure node to execute"
                    },
                    "input": {
                        "type": "object",
                        "description": "Input arguments passed to the procedure steps"
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "If true, validate steps without executing tools (default: false)"
                    }
                },
                "required": ["procedure_id"]
            }),
        }
    }

    // list_dynamic_tools is served by GET /api/tools/dynamic (REST API)

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

    async fn handle_manage_dynamic_tool(&self, arguments: Option<Value>) -> ToolCallResult {
        let args = arguments.clone().unwrap_or_default();
        let action = match args.get("action").and_then(|v| v.as_str()) {
            Some(a) => a.to_string(),
            None => return ToolCallResult::error("Missing required field: action".to_string()),
        };

        match action.as_str() {
            "define" => self.handle_define_tool(arguments).await,
            "remove" => self.handle_remove_dynamic_tool(arguments).await,
            other => ToolCallResult::error(format!(
                "Unknown action '{other}'. Valid actions: define, remove"
            )),
        }
    }

    // ========================================================================
    // Handlers
    // ========================================================================

    async fn handle_define_tool(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: DefineToolInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        if input.steps.is_empty() {
            return ToolCallResult::error("steps must not be empty".to_string());
        }

        // Validate each step has 'tool' and 'purpose', and that the tool exists.
        {
            let registry = self.tool_registry.read().await;
            for (i, step) in input.steps.iter().enumerate() {
                let tool_name = match step.get("tool").and_then(|v| v.as_str()) {
                    Some(t) => t,
                    None => {
                        return ToolCallResult::error(format!("Step {} missing 'tool' field", i));
                    }
                };
                if step.get("purpose").is_none() {
                    return ToolCallResult::error(format!("Step {} missing 'purpose' field", i));
                }
                if registry.get(tool_name).is_none() {
                    return ToolCallResult::error(format!(
                        "Step {} references unknown tool '{}'. Use list_dynamic_tools or \
                         check available tools — do not invent tool names.",
                        i, tool_name
                    ));
                }
            }
        }

        // Check name uniqueness in-memory
        {
            let map = self.tools_map.read().await;
            if map.contains_key(&input.name) {
                return ToolCallResult::error(format!(
                    "A dynamic tool named '{}' already exists. Remove it first.",
                    input.name
                ));
            }
        }

        // Optional dry-run test
        if let Some(test_input) = &input.test_input {
            let test_map = match test_input.as_object() {
                Some(m) => m.clone(),
                None => Map::new(),
            };

            let handler_guard = self.tool_handler_ref.read().await;
            if let Some(handler) = &*handler_guard {
                let (results, _) =
                    procedure_executor::execute_procedure(&input.steps, &test_map, handler, true)
                        .await;
                info!(steps = results.len(), "Dry-run passed for define_tool");
            }
            drop(handler_guard);
        }

        // Persist Procedure node
        let procedure_id = Uuid::new_v4().to_string();
        let tool_id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        let steps_json = match serde_json::to_string(&input.steps) {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Failed to serialize steps: {}", e)),
        };

        let schema_json = match serde_json::to_string(&input.input_schema) {
            Ok(s) => s,
            Err(e) => {
                return ToolCallResult::error(format!("Failed to serialize input_schema: {}", e));
            }
        };

        let proc_cypher = r#"
        CREATE (p:Procedure {
            id: $proc_id,
            name: $name,
            description: $description,
            steps: $steps,
            created_at: datetime($ts)
        })
        "#;

        if let Err(e) = self
            .neo4j
            .run(
                neo4rs::query(proc_cypher)
                    .param("proc_id", procedure_id.clone())
                    .param("name", input.name.clone())
                    .param("description", input.description.clone())
                    .param("steps", steps_json)
                    .param("ts", now.clone()),
            )
            .await
        {
            return ToolCallResult::error(format!("Failed to store Procedure: {}", e));
        }

        // Persist DynamicTool node with USES->Procedure edge
        let dt_cypher = r#"
        MATCH (p:Procedure {id: $proc_id})
        CREATE (d:DynamicTool {
            id: $tool_id,
            name: $name,
            description: $description,
            input_schema: $schema,
            created_at: datetime($ts)
        })-[:USES]->(p)
        "#;

        if let Err(e) = self
            .neo4j
            .run(
                neo4rs::query(dt_cypher)
                    .param("proc_id", procedure_id.clone())
                    .param("tool_id", tool_id.clone())
                    .param("name", input.name.clone())
                    .param("description", input.description.clone())
                    .param("schema", schema_json)
                    .param("ts", now),
            )
            .await
        {
            return ToolCallResult::error(format!("Failed to store DynamicTool: {}", e));
        }

        // Register in memory (both registry and handler see this via shared Arc)
        {
            let mut map = self.tools_map.write().await;
            map.insert(
                input.name.clone(),
                DynamicToolEntry {
                    definition: ToolDefinition {
                        name: input.name.clone(),
                        description: input.description.clone(),
                        input_schema: input.input_schema.clone(),
                    },
                    procedure_id: procedure_id.clone(),
                },
            );
        }

        info!(tool_name = %input.name, tool_id = %tool_id, "Defined and registered dynamic tool");

        let response = json!({
            "tool_id": tool_id,
            "procedure_id": procedure_id,
            "name": input.name,
            "steps_count": input.steps.len(),
            "registered": true,
        });
        ToolCallResult::success_json(response)
    }

    async fn handle_execute_procedure(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ExecuteProcedureInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        // Fetch procedure from Neo4j
        let cypher = r#"
        MATCH (p:Procedure {id: $id})
        RETURN p.steps AS steps
        "#;

        let rows = match self
            .neo4j
            .execute(neo4rs::query(cypher).param("id", input.procedure_id.clone()))
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Failed to fetch procedure: {}", e)),
        };

        let steps_str = match rows.first().and_then(|r| r.get::<String>("steps").ok()) {
            Some(s) => s,
            None => {
                return ToolCallResult::error(format!(
                    "Procedure '{}' not found",
                    input.procedure_id
                ));
            }
        };

        let steps: Vec<Value> = match serde_json::from_str(&steps_str) {
            Ok(v) => v,
            Err(e) => {
                return ToolCallResult::error(format!("Failed to parse procedure steps: {}", e));
            }
        };

        let input_map = input
            .input
            .as_ref()
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        let dry_run = input.dry_run.unwrap_or(false);

        // Get handler for execution
        let handler_guard = self.tool_handler_ref.read().await;
        let handler = match &*handler_guard {
            Some(h) => h.clone(),
            None => return ToolCallResult::error("ToolHandler not initialized".to_string()),
        };
        drop(handler_guard);

        let (results, total_success) =
            procedure_executor::execute_procedure(&steps, &input_map, &handler, dry_run).await;

        let step_results: Vec<Value> = results
            .iter()
            .map(|r| {
                json!({
                    "step_index": r.step_index,
                    "tool": r.tool,
                    "success": r.success,
                    "output_preview": r.output_preview,
                })
            })
            .collect();

        let response = json!({
            "procedure_id": input.procedure_id,
            "steps_executed": results.len(),
            "results": step_results,
            "total_success": total_success,
            "dry_run": dry_run,
        });
        ToolCallResult::success_json(response)
    }

    async fn handle_store_procedure(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(serde::Deserialize)]
        struct Input {
            name: String,
            description: String,
            steps: Vec<Value>,
        }

        let input: Input = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        if input.steps.is_empty() {
            return ToolCallResult::error("steps must not be empty");
        }

        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let steps_json = match serde_json::to_string(&input.steps) {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Failed to serialize steps: {e}")),
        };

        let cypher = r#"
        CREATE (p:Procedure {
            id: $id,
            name: $name,
            description: $description,
            steps: $steps,
            created_at: datetime($ts)
        })
        "#;

        match self
            .neo4j
            .run(
                neo4rs::query(cypher)
                    .param("id", id.clone())
                    .param("name", input.name.clone())
                    .param("description", input.description.clone())
                    .param("steps", steps_json)
                    .param("ts", now),
            )
            .await
        {
            Ok(_) => {
                info!(name = %input.name, id = %id, "Stored procedure");
                ToolCallResult::success_json(json!({
                    "success": true,
                    "id": id,
                    "name": input.name,
                    "steps_count": input.steps.len()
                }))
            }
            Err(e) => ToolCallResult::error(format!("Failed to store procedure: {e}")),
        }
    }

    async fn handle_remove_dynamic_tool(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: RemoveDynamicToolInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        // Remove from Neo4j (DynamicTool + linked Procedure)
        let cypher = r#"
        MATCH (d:DynamicTool {name: $name})
        OPTIONAL MATCH (d)-[:USES]->(p:Procedure)
        DETACH DELETE d, p
        "#;

        if let Err(e) = self
            .neo4j
            .run(neo4rs::query(cypher).param("name", input.name.clone()))
            .await
        {
            return ToolCallResult::error(format!("Failed to delete DynamicTool: {}", e));
        }

        // Remove from in-memory map (both registry and handler see this)
        {
            let mut map = self.tools_map.write().await;
            map.remove(&input.name);
        }

        info!(tool_name = %input.name, "Removed dynamic tool");

        let response = json!({ "removed": true, "name": input.name });
        ToolCallResult::success_json(response)
    }

    /// Execute a dynamically-defined tool (dispatch to procedure executor).
    ///
    /// Always returns a JSON-serialisable `ToolCallResult` so callers can
    /// safely `JSON.parse` the content regardless of success or failure.
    async fn handle_dynamic_tool(
        &self,
        tool_name: &str,
        arguments: Option<Value>,
    ) -> ToolCallResult {
        // Helper: return a JSON error object (never plain text).
        macro_rules! json_err {
            ($msg:expr) => {
                ToolCallResult::error(
                    serde_json::to_string_pretty(&json!({
                        "error": $msg,
                        "tool":  tool_name,
                        "rows":  [],
                        "count": 0,
                    }))
                    .unwrap(),
                )
            };
        }

        let entry = {
            let map = self.tools_map.read().await;
            map.get(tool_name).cloned()
        };
        let entry = match entry {
            Some(e) => e,
            None => {
                return json_err!(format!(
                    "Dynamic tool '{}' not found in tools_map",
                    tool_name
                ));
            }
        };

        // Fetch procedure steps from Neo4j.
        let cypher = "MATCH (p:Procedure {id: $id}) RETURN p.steps AS steps";
        let rows = match self
            .neo4j
            .execute(neo4rs::query(cypher).param("id", entry.procedure_id.clone()))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return json_err!(format!(
                    "Neo4j fetch failed for procedure '{}': {}",
                    entry.procedure_id, e
                ));
            }
        };

        let steps_str = match rows.first().and_then(|r| r.get::<String>("steps").ok()) {
            Some(s) => s,
            None => {
                return json_err!(format!(
                    "Procedure id='{}' not found — tool '{}' may need reseeding (restart the server)",
                    entry.procedure_id, tool_name
                ));
            }
        };

        let steps: Vec<Value> = match serde_json::from_str(&steps_str) {
            Ok(v) => v,
            Err(e) => {
                return json_err!(format!(
                    "Steps JSON invalid for '{}': {} — steps={}",
                    tool_name,
                    e,
                    &steps_str[..steps_str.len().min(200)]
                ));
            }
        };

        let input_map = arguments
            .as_ref()
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default();

        let handler_guard = self.tool_handler_ref.read().await;
        let handler = match &*handler_guard {
            Some(h) => h.clone(),
            None => return json_err!("ToolHandler not yet initialised — call too early"),
        };
        drop(handler_guard);

        let (results, total_success) =
            procedure_executor::execute_procedure(&steps, &input_map, &handler, false).await;

        let last_output = results
            .last()
            .map(|r| r.output.clone())
            .unwrap_or(Value::Null);

        if total_success {
            // Transparent: return the step's output directly so DynamicTools
            // are drop-in replacements for their Rust counterparts.
            ToolCallResult::success_json(last_output)
        } else {
            let detail = results
                .last()
                .map(|r| r.output_preview.clone())
                .unwrap_or_else(|| "procedure produced no output".to_string());
            json_err!(format!("Step failed for '{}': {}", tool_name, detail))
        }
    }
}

#[async_trait]
impl Skill for DynamicSkill {
    fn name(&self) -> &str {
        "Dynamic Tools"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        // Static management tools
        let mut tools = vec![
            Self::manage_dynamic_tool_def(),
            Self::execute_procedure_def(),
            Self::store_procedure_def(),
        ];

        // Dynamically-registered tools (read from shared Arc)
        if let Ok(map) = self.tools_map.try_read() {
            for entry in map.values() {
                tools.push(entry.definition.clone());
            }
        }

        tools
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "manage_dynamic_tool" => Some(self.handle_manage_dynamic_tool(arguments).await),
            "execute_procedure" => Some(self.handle_execute_procedure(arguments).await),
            "store_procedure" => Some(self.handle_store_procedure(arguments).await),
            name => {
                // Check if this is a dynamically-registered tool
                let is_dynamic = self
                    .tools_map
                    .try_read()
                    .map(|map| map.contains_key(name))
                    .unwrap_or(false);

                if is_dynamic {
                    Some(self.handle_dynamic_tool(name, arguments).await)
                } else {
                    None
                }
            }
        }
    }
}

// ============================================================================
// Input structs
// ============================================================================

#[derive(Debug, Deserialize)]
struct DefineToolInput {
    name: String,
    description: String,
    input_schema: Value,
    steps: Vec<Value>,
    #[serde(default)]
    test_input: Option<Value>,
}

#[derive(Debug, Deserialize)]
struct ExecuteProcedureInput {
    procedure_id: String,
    #[serde(default)]
    input: Option<Value>,
    #[serde(default)]
    dry_run: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct RemoveDynamicToolInput {
    name: String,
}
