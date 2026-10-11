//! Agent Skill — manages the background job queue.
//!
//! 3 tools: `enqueue_jobs`, `manage_job` (the HBI cancel/retry buttons), `dead_letter`
//! Queue stats → GET /api/queue/status  |  Drain → POST /api/queue/drain
//! List jobs   → GET /api/jobs or neo4j_query: MATCH (j:AgentJob) RETURN j ORDER BY j.created_at DESC LIMIT 50

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use agent_brain_work::queue::{ChainStep, QueueService};

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

    fn enqueue_jobs_def() -> ToolDefinition {
        ToolDefinition {
            name: "enqueue_jobs".to_string(),
            description: "Submit one or more background jobs. \
                 Pass a single step to queue one job, or multiple steps for a sequential chain. \
                 In a chain, step 1 is queued immediately; each subsequent step is held as 'parked' \
                 until its predecessor completes successfully. \
                 If any step exhausts all retries the remaining steps are automatically cancelled. \
                 Returns the list of job IDs in order. Read results with neo4j_query on (:AgentJob {id})."
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
            Self::dead_letter_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "enqueue_jobs" => Some(self.handle_enqueue_jobs(arguments).await),
            "manage_job" => Some(self.handle_manage_job(arguments).await),
            "dead_letter" => Some(self.handle_dead_letter(arguments).await),
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
            AgentSkill::dead_letter_def(),
        ]
    }

    #[test]
    fn tools_list_has_three_tools() {
        assert_eq!(tool_defs().len(), 3);
    }

    #[test]
    fn tool_names_are_correct() {
        let defs = tool_defs();
        let names: Vec<&str> = defs.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"enqueue_jobs"));
        assert!(names.contains(&"manage_job"));
        assert!(names.contains(&"dead_letter"));
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
