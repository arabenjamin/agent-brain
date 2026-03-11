//! Scheduler Skill — controls the autonomous self-improvement loop.
//!
//! Exposes 5 tools: `start_scheduler`, `stop_scheduler`, `get_scheduler_status`,
//! `configure_scheduler`, `run_scheduler_tick`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::services::scheduler::SchedulerService;
use crate::skills::Skill;

pub struct SchedulerSkill {
    service: Arc<SchedulerService>,
}

impl SchedulerSkill {
    pub fn new(service: Arc<SchedulerService>) -> Self {
        Self { service }
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
            description: "Update one or more scheduler settings at runtime. All fields are optional; \
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
}

// ---------------------------------------------------------------------------
// Custom deserializer: handles absent, null, and string values for session_id
// ---------------------------------------------------------------------------

fn deserialize_optional_session<'de, D>(
    deserializer: D,
) -> Result<Option<Option<String>>, D::Error>
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
        vec![
            Self::start_scheduler_def(),
            Self::stop_scheduler_def(),
            Self::get_scheduler_status_def(),
            Self::configure_scheduler_def(),
            Self::run_scheduler_tick_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        let result = match tool_name {
            "start_scheduler" => self.handle_start_scheduler(arguments).await,
            "stop_scheduler" => self.handle_stop_scheduler().await,
            "get_scheduler_status" => self.handle_get_scheduler_status().await,
            "configure_scheduler" => self.handle_configure_scheduler(arguments).await,
            "run_scheduler_tick" => self.handle_run_scheduler_tick().await,
            _ => return None,
        };
        Some(result)
    }
}
