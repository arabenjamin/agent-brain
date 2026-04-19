//! Agent Skill — manages the background job queue.
//!
//! 5 tools: `enqueue_jobs`, `manage_job`, `set_worker_config`, `dead_letter`, `update_job_progress`
//! Queue stats → GET /api/queue/status  |  Drain → POST /api/queue/drain
//! List jobs   → GET /api/jobs or neo4j_query: MATCH (j:AgentJob) RETURN j ORDER BY j.created_at DESC LIMIT 50

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use crate::services::queue::{ChainStep, QueueService};
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

pub struct AgentSkill {
    queue: Arc<QueueService>,
}

impl AgentSkill {
    pub fn new(queue: Arc<QueueService>) -> Self {
        Self { queue }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    // queue_status → GET /api/queue/status (REST)
    // drain_queue  → POST /api/queue/drain (REST)
    // cleanup_jobs → use neo4j_query to delete old AgentJob nodes

    fn manage_job_def() -> ToolDefinition {
        ToolDefinition {
            name: "manage_job".to_string(),
            description: "Cancel or retry a background job. \
                action=cancel: halt a queued or running job. \
                action=retry: requeue a failed, dead, or cancelled job for another attempt."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["cancel", "retry"],
                        "description": "cancel: halt the job. retry: requeue it."
                    },
                    "job_id": {
                        "type": "string",
                        "description": "The AgentJob ID to act on."
                    }
                },
                "required": ["action", "job_id"]
            }),
        }
    }

    fn set_worker_config_def() -> ToolDefinition {
        ToolDefinition {
            name: "set_worker_config".to_string(),
            description:
                "Update queue worker settings at runtime. Use enabled=false to pause processing."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "max_concurrent": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Global max simultaneous job executions (informational)"
                    },
                    "max_concurrent_ollama": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Concurrency limit for Ollama (local) jobs — takes effect immediately"
                    },
                    "max_concurrent_anthropic": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Concurrency limit for Anthropic API jobs — takes effect immediately"
                    },
                    "max_concurrent_gemini": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Concurrency limit for Gemini API jobs — takes effect immediately"
                    },
                    "enabled": {
                        "type": "boolean",
                        "description": "Enable (true) or pause (false) job processing"
                    },
                    "poll_interval_secs": {
                        "type": "integer",
                        "minimum": 5,
                        "description": "How often the coordinator polls Neo4j for missed jobs"
                    }
                }
            }),
        }
    }

    fn enqueue_jobs_def() -> ToolDefinition {
        ToolDefinition {
            name: "enqueue_jobs".to_string(),
            description: "Submit one or more background jobs. \
                 Pass a single step to queue one job, or multiple steps for a sequential chain. \
                 In a chain, step 1 is queued immediately; each subsequent step is held as 'parked' \
                 until its predecessor completes successfully. \
                 If any step exhausts all retries the remaining steps are automatically cancelled. \
                 Returns the list of job IDs in order. Poll results with get_job_result."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "steps": {
                        "type": "array",
                        "description": "Ordered list of tool calls to execute sequentially.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "tool_name": {
                                    "type": "string",
                                    "description": "The MCP tool to invoke"
                                },
                                "arguments": {
                                    "type": "object",
                                    "description": "Arguments to pass to the tool"
                                },
                                "priority": {
                                    "type": "integer",
                                    "minimum": 0,
                                    "maximum": 3,
                                    "description": "0=low, 1=normal (default), 2=high, 3=critical"
                                },
                                "max_attempts": {
                                    "type": "integer",
                                    "minimum": 1,
                                    "description": "Maximum execution attempts (default 3)"
                                },
                                "provider_hint": {
                                    "type": "string",
                                    "description": "Optional LLM provider hint (ollama/anthropic/gemini)"
                                }
                            },
                            "required": ["tool_name"]
                        },
                        "minItems": 1
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Optional session ID applied to all jobs in the chain"
                    }
                },
                "required": ["steps"]
            }),
        }
    }

    fn dead_letter_def() -> ToolDefinition {
        ToolDefinition {
            name: "dead_letter".to_string(),
            description: "Manage the dead letter queue (permanently failed jobs). Use action to select the operation:\n\
                - list:   show dead letter entries (optional limit, default 20)\n\
                - retry:  re-queue a job for execution (job_id required)\n\
                - delete: permanently remove an entry (job_id required)"
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "retry", "delete"],
                        "description": "The operation to perform"
                    },
                    "job_id": {
                        "type": "string",
                        "description": "Dead letter job ID — required for retry and delete"
                    },
                    "limit": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 100,
                        "description": "Max entries to return for list (default 20)"
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn update_job_progress_def() -> ToolDefinition {
        ToolDefinition {
            name: "update_job_progress".to_string(),
            description: "Update progress for a running job (0-100 percent). Use from within a job to report progress.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "The job ID to update" },
                    "percent": {
                        "type": "integer",
                        "minimum": 0,
                        "maximum": 100,
                        "description": "Progress percentage (0-100)"
                    },
                    "message": {
                        "type": "string",
                        "description": "Optional status message describing current phase"
                    }
                },
                "required": ["job_id", "percent"]
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_manage_job(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            action: String,
            job_id: String,
        }
        let input: Input = match args.and_then(|v| serde_json::from_value(v).ok()) {
            Some(i) => i,
            None => return ToolCallResult::error("Missing required fields: action, job_id"),
        };

        match input.action.as_str() {
            "cancel" => match self.queue.cancel(&input.job_id).await {
                Ok(true) => {
                    ToolCallResult::success_json(json!({ "cancelled": true, "id": input.job_id }))
                }
                Ok(false) => ToolCallResult::error(format!(
                    "Job {} not found or already in a terminal state",
                    input.job_id
                )),
                Err(e) => ToolCallResult::error(format!("Error: {e}")),
            },
            "retry" => match self.queue.retry(&input.job_id).await {
                Ok(true) => ToolCallResult::success_json(
                    json!({ "requeued": true, "id": input.job_id, "status": "queued" }),
                ),
                Ok(false) => ToolCallResult::error(format!(
                    "Job {} not found or not retryable (must be failed/dead/cancelled)",
                    input.job_id
                )),
                Err(e) => ToolCallResult::error(format!("Error: {e}")),
            },
            other => {
                ToolCallResult::error(format!("Unknown action `{other}`. Use cancel or retry."))
            }
        }
    }

    async fn handle_set_worker_config(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            max_concurrent: Option<usize>,
            max_concurrent_ollama: Option<usize>,
            max_concurrent_anthropic: Option<usize>,
            max_concurrent_gemini: Option<usize>,
            enabled: Option<bool>,
            poll_interval_secs: Option<u64>,
        }
        let input: Input = args
            .and_then(|v| serde_json::from_value(v).ok())
            .unwrap_or_default();

        let cfg = self
            .queue
            .update_config(
                input.max_concurrent,
                input.max_concurrent_ollama,
                input.max_concurrent_anthropic,
                input.max_concurrent_gemini,
                input.enabled,
                input.poll_interval_secs,
            )
            .await;

        ToolCallResult::success_text(
            json!({
                "max_concurrent": cfg.max_concurrent,
                "enabled": cfg.enabled,
                "poll_interval_secs": cfg.poll_interval_secs,
            })
            .to_string(),
        )
    }

    async fn handle_enqueue_jobs(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            steps: Vec<ChainStep>,
            #[serde(default)]
            session_id: Option<String>,
        }

        let input: Input = match args.and_then(|v| serde_json::from_value(v).ok()) {
            Some(i) => i,
            None => return ToolCallResult::error("Missing required field: steps"),
        };

        if input.steps.is_empty() {
            return ToolCallResult::error("Must contain at least one step");
        }

        match self
            .queue
            .enqueue_chain(&input.steps, input.session_id.as_deref())
            .await
        {
            Ok(ids) => {
                let message = if ids.len() == 1 {
                    "Job enqueued.".to_string()
                } else {
                    format!(
                        "Chain of {} jobs enqueued. First job is queued; {} are parked.",
                        ids.len(),
                        ids.len().saturating_sub(1)
                    )
                };
                ToolCallResult::success_text(
                    json!({
                        "count": ids.len(),
                        "ids": ids,
                        "message": message,
                    })
                    .to_string(),
                )
            }
            Err(e) => ToolCallResult::error(format!("Failed to enqueue jobs: {e}")),
        }
    }

    async fn handle_dead_letter(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            action: String,
            job_id: Option<String>,
            limit: Option<usize>,
        }
        let input: Input = match args.and_then(|v| serde_json::from_value(v).ok()) {
            Some(i) => i,
            None => return ToolCallResult::error("Missing required field: action"),
        };

        match input.action.as_str() {
            "list" => {
                let limit = input.limit.unwrap_or(20).min(100);
                match self.queue.list_dead_letter(limit).await {
                    Ok(jobs) => {
                        let rows: Vec<Value> = jobs
                            .into_iter()
                            .map(|j| {
                                json!({
                                    "id": j.id,
                                    "tool_name": j.tool_name,
                                    "reason": j.dead_letter_reason,
                                    "attempt_count": j.attempt_count,
                                    "error": j.error,
                                    "dead_lettered_at": j.dead_lettered_at,
                                })
                            })
                            .collect();
                        ToolCallResult::success_json(
                            json!({ "entries": rows, "count": rows.len() }),
                        )
                    }
                    Err(e) => ToolCallResult::error(format!("Failed to list dead letter: {e}")),
                }
            }

            "retry" => {
                let id = match &input.job_id {
                    Some(id) => id.clone(),
                    None => return ToolCallResult::error("retry requires 'job_id'"),
                };
                match self.queue.retry_dead_letter(&id).await {
                    Ok(true) => ToolCallResult::success_text(
                        json!({ "requeued": true, "id": id }).to_string(),
                    ),
                    Ok(false) => {
                        ToolCallResult::error(format!("Job {id} not found in dead letter"))
                    }
                    Err(e) => ToolCallResult::error(format!("Error: {e}")),
                }
            }

            "delete" => {
                let id = match &input.job_id {
                    Some(id) => id.clone(),
                    None => return ToolCallResult::error("delete requires 'job_id'"),
                };
                match self.queue.delete_dead_letter(&id).await {
                    Ok(true) => ToolCallResult::success_text(
                        json!({ "deleted": true, "id": id }).to_string(),
                    ),
                    Ok(false) => {
                        ToolCallResult::error(format!("Job {id} not found in dead letter"))
                    }
                    Err(e) => ToolCallResult::error(format!("Error: {e}")),
                }
            }

            other => ToolCallResult::error(format!(
                "Unknown action '{other}'. Use: list, retry, delete"
            )),
        }
    }

    async fn handle_update_job_progress(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            job_id: String,
            percent: u8,
            message: Option<String>,
        }
        let input: Input = match args.and_then(|v| serde_json::from_value(v).ok()) {
            Some(i) => i,
            None => return ToolCallResult::error("Missing required fields"),
        };

        match self
            .queue
            .update_progress(&input.job_id, input.percent, input.message.as_deref(), None)
            .await
        {
            Ok(_) => ToolCallResult::success_text(
                json!({ "updated": true, "job_id": input.job_id, "percent": input.percent })
                    .to_string(),
            ),
            Err(e) => ToolCallResult::error(format!("Error: {e}")),
        }
    }
}

#[async_trait]
impl Skill for AgentSkill {
    fn name(&self) -> &str {
        "agent"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::enqueue_jobs_def(),
            Self::manage_job_def(),
            Self::set_worker_config_def(),
            Self::dead_letter_def(),
            Self::update_job_progress_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "enqueue_jobs" => Some(self.handle_enqueue_jobs(arguments).await),
            "manage_job" => Some(self.handle_manage_job(arguments).await),
            "set_worker_config" => Some(self.handle_set_worker_config(arguments).await),
            "dead_letter" => Some(self.handle_dead_letter(arguments).await),
            "update_job_progress" => Some(self.handle_update_job_progress(arguments).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // AgentSkill wraps a concrete QueueService that requires a live Neo4j
    // connection, so we test only the pure (non-network) surface: tool
    // definitions and the execute() routing table.

    fn tool_defs() -> Vec<ToolDefinition> {
        vec![
            AgentSkill::enqueue_jobs_def(),
            AgentSkill::manage_job_def(),
            AgentSkill::set_worker_config_def(),
            AgentSkill::dead_letter_def(),
            AgentSkill::update_job_progress_def(),
        ]
    }

    #[test]
    fn tools_list_has_five_tools() {
        assert_eq!(tool_defs().len(), 5);
    }

    #[test]
    fn tool_names_are_correct() {
        let defs = tool_defs();
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"enqueue_jobs"));
        assert!(names.contains(&"manage_job"));
        assert!(names.contains(&"set_worker_config"));
        assert!(names.contains(&"dead_letter"));
        assert!(names.contains(&"update_job_progress"));
    }

    #[test]
    fn enqueue_jobs_schema_requires_steps() {
        let def = AgentSkill::enqueue_jobs_def();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("steps")));
    }

    #[test]
    fn manage_job_schema_requires_action_and_job_id() {
        let def = AgentSkill::manage_job_def();
        let required = def.input_schema["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v.as_str() == Some("action")));
        assert!(required.iter().any(|v| v.as_str() == Some("job_id")));
    }
}
