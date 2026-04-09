//! Scheduler Skill — controls the autonomous self-improvement loop.
//!
//! Exposes 8 tools: `start_scheduler`, `stop_scheduler`, `get_scheduler_status`,
//! `configure_scheduler`, `run_scheduler_tick`, `define_scheduler_chain`,
//! `list_scheduler_chains`, `remove_scheduler_chain`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::repository::Neo4jClient;
use crate::services::scheduler::SchedulerService;
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

pub struct SchedulerSkill {
    service: Arc<SchedulerService>,
    neo4j: Option<Neo4jClient>,
}

impl SchedulerSkill {
    pub fn new(service: Arc<SchedulerService>, neo4j: Option<Neo4jClient>) -> Self {
        Self { service, neo4j }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    fn start_scheduler_def() -> ToolDefinition {
        ToolDefinition {
            name: "start_scheduler".to_string(),
            description: "Enable the autonomous scheduler loop that periodically dispatches \
                          pending tasks as background job chains. Optionally update the poll \
                          interval and session ID at the same time."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "interval_secs": {
                        "type": "integer",
                        "minimum": 10,
                        "description": "How often to poll for pending tasks (seconds). Omit to keep current value."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Optional session ID to attach to all enqueued jobs."
                    }
                }
            }),
        }
    }

    fn stop_scheduler_def() -> ToolDefinition {
        ToolDefinition {
            name: "stop_scheduler".to_string(),
            description: "Pause the autonomous scheduler loop. Queued and running jobs are \
                          not affected — only future ticks are suppressed."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn get_scheduler_status_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_scheduler_status".to_string(),
            description: "Return a snapshot of the scheduler's current configuration and \
                          runtime state (interval, enabled flag, tasks dispatched, last run, etc.)."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn configure_scheduler_def() -> ToolDefinition {
        ToolDefinition {
            name: "configure_scheduler".to_string(),
            description:
                "Update one or more scheduler settings at runtime. All fields are optional; \
                          only supplied fields are changed."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "interval_secs": {
                        "type": "integer",
                        "minimum": 10,
                        "description": "Polling interval in seconds (default 300)"
                    },
                    "enabled": {
                        "type": "boolean",
                        "description": "Enable or disable the scheduler loop"
                    },
                    "max_tasks_per_run": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "description": "Maximum tasks to dispatch per tick (default 3)"
                    },
                    "error_budget": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Consecutive error limit before auto-pause (default 5)"
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Session ID attached to enqueued jobs (null to clear)"
                    },
                    "idle_sleep_after_ticks": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Consecutive idle ticks before entering sleep mode (default 3)"
                    },
                    "sleep_interval_secs": {
                        "type": "integer",
                        "minimum": 60,
                        "description": "Tick interval while in sleep mode in seconds (default 1800)"
                    }
                }
            }),
        }
    }

    fn run_scheduler_tick_def() -> ToolDefinition {
        ToolDefinition {
            name: "run_scheduler_tick".to_string(),
            description: "Execute a scheduler tick immediately, bypassing the timer. \
                          Lists pending tasks, builds job chains, and enqueues them. \
                          Returns the number of tasks found and dispatched."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_start_scheduler(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            interval_secs: Option<u64>,
            session_id: Option<String>,
        }

        let input: Input = args
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();

        let cfg = self
            .service
            .update_config(
                input.interval_secs,
                Some(true),
                None,
                None,
                input.session_id.map(Some),
                None,
                None,
            )
            .await;

        ToolCallResult::success_text(
            json!({
                "started": true,
                "interval_secs": cfg.interval_secs,
                "session_id": cfg.session_id,
            })
            .to_string(),
        )
    }

    async fn handle_stop_scheduler(&self) -> ToolCallResult {
        self.service
            .update_config(None, Some(false), None, None, None, None, None)
            .await;

        ToolCallResult::success_text(
            json!({ "stopped": true, "message": "Scheduler paused; existing jobs continue running." })
                .to_string(),
        )
    }

    async fn handle_get_scheduler_status(&self) -> ToolCallResult {
        ToolCallResult::success_text(self.service.status().await.to_string())
    }

    async fn handle_configure_scheduler(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            interval_secs: Option<u64>,
            enabled: Option<bool>,
            max_tasks_per_run: Option<usize>,
            error_budget: Option<u32>,
            // Use Value so we can distinguish null (clear) from absent
            #[serde(default, deserialize_with = "deserialize_optional_session")]
            session_id: Option<Option<String>>,
            idle_sleep_after_ticks: Option<u32>,
            sleep_interval_secs: Option<u64>,
        }

        let input: Input = args
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();

        let cfg = self
            .service
            .update_config(
                input.interval_secs,
                input.enabled,
                input.max_tasks_per_run,
                input.error_budget,
                input.session_id,
                input.idle_sleep_after_ticks,
                input.sleep_interval_secs,
            )
            .await;

        ToolCallResult::success_text(
            json!({
                "updated": true,
                "config": {
                    "interval_secs": cfg.interval_secs,
                    "enabled": cfg.enabled,
                    "max_tasks_per_run": cfg.max_tasks_per_run,
                    "error_budget": cfg.error_budget,
                    "session_id": cfg.session_id,
                    "idle_sleep_after_ticks": cfg.idle_sleep_after_ticks,
                    "sleep_interval_secs": cfg.sleep_interval_secs,
                }
            })
            .to_string(),
        )
    }

    async fn handle_run_scheduler_tick(&self) -> ToolCallResult {
        match self.service.run_tick().await {
            Ok(result) => ToolCallResult::success_text(
                json!({
                    "success": true,
                    "tasks_found": result.tasks_found,
                    "tasks_dispatched": result.tasks_dispatched,
                    "skipped": result.skipped,
                    "new_tasks_created": result.new_tasks_created,
                })
                .to_string(),
            ),
            Err(e) => ToolCallResult::error(format!("Scheduler tick failed: {e}")),
        }
    }

    // =========================================================================
    // SchedulerChain tool definitions
    // =========================================================================

    fn define_scheduler_chain_def() -> ToolDefinition {
        ToolDefinition {
            name: "define_scheduler_chain".to_string(),
            description: "Store or update a routing chain in Neo4j. When a task goal contains \
                          'pattern' (case-insensitive), the scheduler dispatches these steps \
                          instead of the built-in heuristics. Lower priority = checked first."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {
                        "type": "string",
                        "description": "Substring to match in the task goal (case-insensitive)"
                    },
                    "steps": {
                        "type": "array",
                        "description": "Array of ChainStep objects (tool_name, arguments, context_profile). \
                                        Use {{task_id}}, {{goal}}, {{date}} as template vars."
                    },
                    "priority": {
                        "type": "integer",
                        "description": "Check order — lower = checked first (default: 100)"
                    },
                    "description": {
                        "type": "string",
                        "description": "Human-readable description of what this chain does"
                    }
                },
                "required": ["pattern", "steps"]
            }),
        }
    }

    fn list_scheduler_chains_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_scheduler_chains".to_string(),
            description: "List all SchedulerChain nodes stored in Neo4j (pattern, priority, \
                          description, step count). Does not return the full step definitions."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn remove_scheduler_chain_def() -> ToolDefinition {
        ToolDefinition {
            name: "remove_scheduler_chain".to_string(),
            description: "Delete a SchedulerChain by its id. The built-in heuristics continue \
                          to apply for goals that no longer match any stored chain."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "SchedulerChain node id to delete" }
                },
                "required": ["id"]
            }),
        }
    }

    // =========================================================================
    // SchedulerChain handlers
    // =========================================================================

    async fn handle_define_scheduler_chain(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            pattern: String,
            steps: Value,
            #[serde(default = "default_priority")]
            priority: i64,
            #[serde(default)]
            description: Option<String>,
        }
        fn default_priority() -> i64 { 100 }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        let steps_json = match serde_json::to_string(&input.steps) {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Failed to serialize steps: {}", e)),
        };

        let id = uuid::Uuid::new_v4().to_string();
        let cypher = "MERGE (c:SchedulerChain {pattern: $pattern}) \
                      SET c.id          = COALESCE(c.id, $id), \
                          c.steps       = $steps, \
                          c.priority    = $priority, \
                          c.description = $description, \
                          c.updated_at  = datetime()";

        if let Err(e) = neo4j
            .run(
                neo4rs::query(cypher)
                    .param("pattern",     input.pattern.clone())
                    .param("id",          id)
                    .param("steps",       steps_json)
                    .param("priority",    input.priority)
                    .param("description", input.description.unwrap_or_default()),
            )
            .await
        {
            return ToolCallResult::error(format!("Failed to store SchedulerChain: {}", e));
        }

        ToolCallResult::success_text(
            serde_json::to_string_pretty(&json!({
                "stored": true,
                "pattern": input.pattern,
                "priority": input.priority,
                "step_count": input.steps.as_array().map(|a| a.len()).unwrap_or(0),
            }))
            .unwrap(),
        )
    }

    async fn handle_list_scheduler_chains(&self) -> ToolCallResult {
        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        let cypher = "MATCH (c:SchedulerChain) \
                      RETURN c.id AS id, c.pattern AS pattern, c.priority AS priority, \
                             c.description AS description, c.steps AS steps \
                      ORDER BY c.priority ASC, c.pattern ASC";

        match neo4j.execute(neo4rs::query(cypher)).await {
            Ok(rows) => {
                let chains: Vec<Value> = rows.iter().map(|row| {
                    let step_count = row.get::<String>("steps").ok()
                        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                        .and_then(|v| v.as_array().map(|a| a.len()))
                        .unwrap_or(0);
                    json!({
                        "id":          row.get::<String>("id").unwrap_or_default(),
                        "pattern":     row.get::<String>("pattern").unwrap_or_default(),
                        "priority":    row.get::<i64>("priority").unwrap_or(100),
                        "description": row.get::<String>("description").unwrap_or_default(),
                        "step_count":  step_count,
                    })
                }).collect();
                let count = chains.len();
                ToolCallResult::success_text(
                    serde_json::to_string_pretty(&json!({ "count": count, "chains": chains })).unwrap()
                )
            }
            Err(e) => ToolCallResult::error(format!("Failed to list chains: {}", e)),
        }
    }

    async fn handle_remove_scheduler_chain(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input { id: String }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        let cypher = "MATCH (c:SchedulerChain {id: $id}) DELETE c RETURN count(c) AS deleted";
        match neo4j.execute(neo4rs::query(cypher).param("id", input.id.clone())).await {
            Ok(rows) => {
                let deleted = rows.first()
                    .and_then(|r| r.get::<i64>("deleted").ok())
                    .unwrap_or(0);
                if deleted > 0 {
                    ToolCallResult::success_text(
                        json!({ "deleted": true, "id": input.id }).to_string()
                    )
                } else {
                    ToolCallResult::error(format!("No SchedulerChain found with id '{}'", input.id))
                }
            }
            Err(e) => ToolCallResult::error(format!("Failed to remove chain: {}", e)),
        }
    }
}

// ---------------------------------------------------------------------------
// Custom deserializer: handles absent, null, and string values for session_id
// ---------------------------------------------------------------------------

fn deserialize_optional_session<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<Value> = Option::deserialize(deserializer)?;
    match v {
        None => Ok(None),                            // field absent
        Some(Value::Null) => Ok(Some(None)),         // explicit null → clear
        Some(Value::String(s)) => Ok(Some(Some(s))), // string → set
        _ => Ok(None),
    }
}

// =========================================================================
// Skill implementation
// =========================================================================

#[async_trait]
impl Skill for SchedulerSkill {
    fn name(&self) -> &str {
        "scheduler"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        let mut tools = vec![
            Self::start_scheduler_def(),
            Self::stop_scheduler_def(),
            Self::get_scheduler_status_def(),
            Self::configure_scheduler_def(),
            Self::run_scheduler_tick_def(),
        ];
        if self.neo4j.is_some() {
            tools.push(Self::define_scheduler_chain_def());
            tools.push(Self::list_scheduler_chains_def());
            tools.push(Self::remove_scheduler_chain_def());
        }
        tools
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        let result = match tool_name {
            "start_scheduler"        => self.handle_start_scheduler(arguments).await,
            "stop_scheduler"         => self.handle_stop_scheduler().await,
            "get_scheduler_status"   => self.handle_get_scheduler_status().await,
            "configure_scheduler"    => self.handle_configure_scheduler(arguments).await,
            "run_scheduler_tick"     => self.handle_run_scheduler_tick().await,
            "define_scheduler_chain" => self.handle_define_scheduler_chain(arguments).await,
            "list_scheduler_chains"  => self.handle_list_scheduler_chains().await,
            "remove_scheduler_chain" => self.handle_remove_scheduler_chain(arguments).await,
            _ => return None,
        };
        Some(result)
    }
}
