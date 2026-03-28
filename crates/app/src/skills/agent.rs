//! Agent Skill — manages the background job queue.
//!
//! Exposes 7 tools: `enqueue_agent`, `queue_status`, `get_job_result`,
//! `cancel_job`, `retry_job`, `set_worker_config`, `drain_queue`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use crate::services::queue::{ChainStep, QueueService};
use crate::skills::Skill;

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

    fn enqueue_agent_def() -> ToolDefinition {
        ToolDefinition {
            name: "enqueue_agent".to_string(),
            description:
                "Submit an MCP tool call as a background job. Jobs are executed asynchronously \
                 in priority order (3=critical … 0=low). Returns a job_id you can poll with \
                 get_job_result."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "The MCP tool to invoke in the background"
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
                        "description": "Maximum execution attempts before marking Dead (default 3)"
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Optional session to associate this job with"
                    },
                    "parent_job_id": {
                        "type": "string",
                        "description": "Optional parent job ID for sub-task chaining"
                    },
                    "provider_hint": {
                        "type": "string",
                        "description": "Optional hint for choosing a specific LLM provider (e.g. 'anthropic', 'gemini', 'ollama')"
                    }
                },
                "required": ["tool_name"]
            }),
        }
    }

    fn queue_status_def() -> ToolDefinition {
        ToolDefinition {
            name: "queue_status".to_string(),
            description: "Show current queue statistics: pending, running, and per-status counts."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn get_job_result_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_job_result".to_string(),
            description: "Get the full details and result of a background job by its ID."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id": {
                        "type": "string",
                        "description": "Job ID returned by enqueue_agent"
                    }
                },
                "required": ["job_id"]
            }),
        }
    }

    fn cancel_job_def() -> ToolDefinition {
        ToolDefinition {
            name: "cancel_job".to_string(),
            description: "Cancel a queued or running job.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "The job ID to cancel" }
                },
                "required": ["job_id"]
            }),
        }
    }

    fn retry_job_def() -> ToolDefinition {
        ToolDefinition {
            name: "retry_job".to_string(),
            description: "Requeue a failed or dead job for another execution attempt.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "job_id": { "type": "string", "description": "The job ID to retry" }
                },
                "required": ["job_id"]
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

    fn drain_queue_def() -> ToolDefinition {
        ToolDefinition {
            name: "drain_queue".to_string(),
            description: "Cancel all currently queued (pending) jobs.".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn enqueue_chain_def() -> ToolDefinition {
        ToolDefinition {
            name: "enqueue_chain".to_string(),
            description: "Submit a sequential chain of background jobs. \
                 Step 1 is queued immediately; each subsequent step is held as 'parked' until \
                 its predecessor completes successfully. \
                 If any step exhausts all retries the remaining steps are automatically cancelled. \
                 Returns the list of job IDs in chain order."
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

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_enqueue_agent(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            tool_name: String,
            #[serde(default)]
            arguments: Option<Value>,
            #[serde(default = "default_priority")]
            priority: u8,
            #[serde(default = "default_max_attempts")]
            max_attempts: u32,
            #[serde(default)]
            session_id: Option<String>,
            #[serde(default)]
            parent_job_id: Option<String>,
            #[serde(default)]
            provider_hint: Option<String>,
        }
        fn default_priority() -> u8 {
            1
        }
        fn default_max_attempts() -> u32 {
            3
        }

        let input: Input = match args.and_then(|v| serde_json::from_value(v).ok()) {
            Some(i) => i,
            None => return ToolCallResult::error("Missing required field: tool_name"),
        };

        match self
            .queue
            .enqueue(
                &input.tool_name,
                input.arguments.as_ref(),
                input.priority,
                input.max_attempts,
                input.session_id.as_deref(),
                input.parent_job_id.as_deref(),
                input.provider_hint.as_deref(),
            )
            .await
        {
            Ok(job_id) => ToolCallResult::success_text(
                json!({
                    "id": job_id,
                    "status": "queued",
                    "tool_name": input.tool_name,
                    "priority": input.priority,
                    "max_attempts": input.max_attempts,
                    "provider_hint": input.provider_hint,
                })
                .to_string(),
            ),
            Err(e) => ToolCallResult::error(format!("Failed to enqueue job: {e}")),
        }
    }

    async fn handle_queue_status(&self) -> ToolCallResult {
        ToolCallResult::success_text(
            serde_json::to_string_pretty(&self.queue.stats().await).unwrap(),
        )
    }

    async fn handle_get_job_result(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            job_id: String,
        }
        let input: Input = match args.and_then(|v| serde_json::from_value(v).ok()) {
            Some(i) => i,
            None => return ToolCallResult::error("Missing required field: job_id"),
        };

        match self.queue.get_job(&input.job_id).await {
            Some(job) => {
                ToolCallResult::success_text(serde_json::to_string_pretty(&job).unwrap_or_default())
            }
            None => ToolCallResult::error(format!("Job not found: {}", input.job_id)),
        }
    }

    async fn handle_cancel_job(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            job_id: String,
        }
        let input: Input = match args.and_then(|v| serde_json::from_value(v).ok()) {
            Some(i) => i,
            None => return ToolCallResult::error("Missing required field: job_id"),
        };

        match self.queue.cancel(&input.job_id).await {
            Ok(true) => ToolCallResult::success_text(
                json!({ "cancelled": true, "id": input.job_id }).to_string(),
            ),
            Ok(false) => ToolCallResult::error(format!(
                "Job {} not found or already in a terminal state",
                input.job_id
            )),
            Err(e) => ToolCallResult::error(format!("Error: {e}")),
        }
    }

    async fn handle_retry_job(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            job_id: String,
        }
        let input: Input = match args.and_then(|v| serde_json::from_value(v).ok()) {
            Some(i) => i,
            None => return ToolCallResult::error("Missing required field: job_id"),
        };

        match self.queue.retry(&input.job_id).await {
            Ok(true) => ToolCallResult::success_text(
                json!({ "requeued": true, "id": input.job_id, "status": "queued" }).to_string(),
            ),
            Ok(false) => ToolCallResult::error(format!(
                "Job {} not found or not in a retryable state (must be failed/dead/cancelled)",
                input.job_id
            )),
            Err(e) => ToolCallResult::error(format!("Error: {e}")),
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

    async fn handle_drain_queue(&self) -> ToolCallResult {
        match self.queue.drain().await {
            Ok(count) => ToolCallResult::success_text(
                json!({
                    "count": count,
                    "message": format!("Drained {count} queued jobs"),
                })
                .to_string(),
            ),
            Err(e) => ToolCallResult::error(format!("Drain failed: {e}")),
        }
    }

    async fn handle_enqueue_chain(&self, args: Option<Value>) -> ToolCallResult {
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
            return ToolCallResult::error("Chain must contain at least one step");
        }

        match self
            .queue
            .enqueue_chain(&input.steps, input.session_id.as_deref())
            .await
        {
            Ok(ids) => ToolCallResult::success_text(
                json!({
                    "chain_length": ids.len(),
                    "ids": ids,
                    "message": format!(
                        "Chain of {} jobs enqueued. First job is queued; {} are parked.",
                        ids.len(),
                        ids.len().saturating_sub(1)
                    ),
                })
                .to_string(),
            ),
            Err(e) => ToolCallResult::error(format!("Failed to enqueue chain: {e}")),
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
            Self::enqueue_agent_def(),
            Self::queue_status_def(),
            Self::get_job_result_def(),
            Self::cancel_job_def(),
            Self::retry_job_def(),
            Self::set_worker_config_def(),
            Self::drain_queue_def(),
            Self::enqueue_chain_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "enqueue_agent" => Some(self.handle_enqueue_agent(arguments).await),
            "queue_status" => Some(self.handle_queue_status().await),
            "get_job_result" => Some(self.handle_get_job_result(arguments).await),
            "cancel_job" => Some(self.handle_cancel_job(arguments).await),
            "retry_job" => Some(self.handle_retry_job(arguments).await),
            "set_worker_config" => Some(self.handle_set_worker_config(arguments).await),
            "drain_queue" => Some(self.handle_drain_queue().await),
            "enqueue_chain" => Some(self.handle_enqueue_chain(arguments).await),
            _ => None,
        }
    }
}
