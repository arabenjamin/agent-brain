//! Task Skill - Provides tools for task management and self-correction.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{info, warn};
use std::sync::Arc;

use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use crate::models::TaskStatus;
use crate::services::traits::{LlmProvider, TaskStore};
use crate::services::queue::{ChainStep, QueueService};
use crate::skills::Skill;

/// Task Skill implementation.
pub struct TaskSkill {
    llm: Arc<dyn LlmProvider>,
    neo4j: Option<Arc<dyn TaskStore>>,
    queue: Option<Arc<QueueService>>,
}

impl TaskSkill {
    /// Create a new task skill.
    pub fn new(
        llm: Arc<dyn LlmProvider>,
        neo4j: Option<Arc<dyn TaskStore>>,
        queue: Option<Arc<QueueService>>,
    ) -> Self {
        Self { llm, neo4j, queue }
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn create_task_def() -> ToolDefinition {
        ToolDefinition {
            name: "create_task".to_string(),
            description: "Create a new high-level task or goal to track execution against.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "The main objective of the task"
                    },
                    "context": {
                        "type": "string",
                        "description": "Additional context or constraints"
                    }
                },
                "required": ["goal"]
            }),
        }
    }

    fn reflect_def() -> ToolDefinition {
        ToolDefinition {
            name: "reflect_on_work".to_string(),
            description: "Analyze the current output or state against the original goal to determine \
                         next steps or corrections. Persists a reflection Note linked to the task \
                         when task_id is provided."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "The original goal"
                    },
                    "current_state": {
                        "type": "string",
                        "description": "What has been achieved so far or current output"
                    },
                    "plan": {
                        "type": "string",
                        "description": "The original plan (optional)"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Optional task ID — when provided, a reflection Note is stored \
                                        in the graph with a REFLECTS_ON edge to the task"
                    }
                },
                "required": ["goal", "current_state"]
            }),
        }
    }

    fn decompose_goal_def() -> ToolDefinition {
        ToolDefinition {
            name: "decompose_goal".to_string(),
            description: "Break a high-level task into an ordered list of concrete sub-tasks using LLM. \
                         Creates SUBTASK_OF edges in the graph linking each sub-task to the parent."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal_task_id": {
                        "type": "string",
                        "description": "ID of the parent task to decompose"
                    },
                    "context": {
                        "type": "string",
                        "description": "Additional context to guide decomposition"
                    },
                    "max_steps": {
                        "type": "integer",
                        "description": "Maximum number of sub-tasks to generate (default: 5)"
                    }
                },
                "required": ["goal_task_id"]
            }),
        }
    }

    fn update_task_def() -> ToolDefinition {
        ToolDefinition {
            name: "update_task".to_string(),
            description: "Update a task's status and optionally attach a progress note. \
                         The note is stored as an outcome Note with a REFLECTS_ON edge to the task."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "task_id": {
                        "type": "string",
                        "description": "ID of the task to update"
                    },
                    "status": {
                        "type": "string",
                        "enum": ["in_progress", "completed", "failed", "blocked"],
                        "description": "New status for the task"
                    },
                    "note": {
                        "type": "string",
                        "description": "Optional progress note to store alongside the status change"
                    }
                },
                "required": ["task_id", "status"]
            }),
        }
    }

    fn list_tasks_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_tasks".to_string(),
            description: "List tasks from the graph, optionally filtered by status. \
                         Returns parent_id for sub-tasks created via decompose_goal."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "status": {
                        "type": "string",
                        "description": "Filter by status: created, in_progress, completed, failed, blocked"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum number of tasks to return (default: 20)"
                    }
                }
            }),
        }
    }

    fn record_outcome_def() -> ToolDefinition {
        ToolDefinition {
            name: "record_outcome".to_string(),
            description: "Record an episodic outcome note for a tool call or task attempt. \
                         Stored as an outcome Note retrievable via search_notes. \
                         Optionally linked to a task via REFLECTS_ON."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "tool_name": {
                        "type": "string",
                        "description": "Name of the tool that was used"
                    },
                    "summary": {
                        "type": "string",
                        "description": "Description of what happened (success details or error)"
                    },
                    "success": {
                        "type": "boolean",
                        "description": "Whether the action succeeded"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Optional task ID to link the outcome to"
                    }
                },
                "required": ["tool_name", "summary", "success"]
            }),
        }
    }

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    async fn handle_create_task(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: CreateTaskInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        if let Some(neo4j) = &self.neo4j {
             match neo4j.create_task(&input.goal, input.context.as_deref()).await {
                Ok(id) => {
                    info!(task_id = %id, goal = %input.goal, "Created new task in DB");
                    let response = json!({
                        "task_id": id,
                        "status": "created",
                        "message": "Task created successfully in database."
                    });
                    ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
                },
                Err(e) => ToolCallResult::error(format!("Failed to create task in DB: {}", e))
             }
        } else {
             info!(goal = %input.goal, "Neo4j not available, skipping persistence");
             ToolCallResult::error("Persistence layer (Neo4j) not available.".to_string())
        }
    }

    async fn handle_reflect(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ReflectInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        info!(goal = %input.goal, "Reflecting on work");

        {
            let prompt = format!(
                "You are a critical reviewer. Analyze the following work against the goal.\n\n\
                GOAL: {}\n\n\
                CURRENT STATE/OUTPUT:\n{}\n\n\
                PLAN (Optional): {}\n\n\
                Provide a critique and specific next steps. Focus on missing requirements or errors.",
                input.goal,
                input.current_state,
                input.plan.as_deref().unwrap_or("")
            );

            match self.llm.generate(&prompt, None).await {
                Ok(reflection_text) => {
                    let reflection_note_id = if let Some(neo4j) = &self.neo4j {
                        match neo4j.store_reflection_note(
                            &reflection_text,
                            input.task_id.as_deref(),
                        ).await {
                            Ok(id) => {
                                info!(note_id = %id, "Stored reflection note");
                                Some(id)
                            }
                            Err(e) => {
                                warn!("Failed to store reflection note: {}", e);
                                None
                            }
                        }
                    } else {
                        None
                    };

                    let mut response_json = json!({
                        "critique": reflection_text,
                        "status": "reflection_complete"
                    });

                    if let Some(note_id) = reflection_note_id {
                        response_json["reflection_note_id"] = json!(note_id);
                    }

                    ToolCallResult::success_text(serde_json::to_string_pretty(&response_json).unwrap())
                },
                Err(e) => ToolCallResult::error(format!("LLM reflection failed: {}", e))
            }
        }
    }

    async fn handle_decompose_goal(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: DecomposeGoalInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let neo4j = match &self.neo4j {
            Some(n) => n,
            None => return ToolCallResult::error("Neo4j not available".to_string()),
        };

        // Fetch the parent task to get its goal
        let parent_task = match neo4j.get_task(&input.goal_task_id).await {
            Ok(Some(t)) => t,
            Ok(None) => return ToolCallResult::error(format!("Task {} not found", input.goal_task_id)),
            Err(e) => return ToolCallResult::error(format!("Failed to fetch task: {}", e)),
        };

        let max_steps = input.max_steps.unwrap_or(5);
        let context = input.context.as_deref().unwrap_or("");

        let prompt = format!(
            "You are a task planner. Decompose the following goal into at most {} concrete, \
             ordered sub-tasks. Each sub-task should be independently actionable using available tools. \
             Use 'depends_on_step' (0-indexed) when a step cannot start before the referenced step finishes. \
             Output ONLY a JSON array with no additional text: \
             [{{\"title\": \"...\", \"purpose\": \"...\", \"tool_hint\": \"...\", \"depends_on_step\": null}}]\n\n\
             GOAL: {}\n\
             CONTEXT: {}",
            max_steps, parent_task.goal, context
        );

        let llm_text = match self.llm.generate(&prompt, None).await {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("LLM decomposition failed: {}", e)),
        };

        // Parse JSON from LLM response
        let text = llm_text.trim();
        let json_start = text.find('[').unwrap_or(0);
        let json_end = text.rfind(']').map(|i| i + 1).unwrap_or(text.len());
        let json_str = &text[json_start..json_end];

        let subtask_specs: Vec<Value> = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => return ToolCallResult::error(format!("Failed to parse LLM subtask JSON: {} — raw: {}", e, text)),
        };

        // First pass: create all subtask nodes and collect their IDs.
        let mut created_subtasks: Vec<(String, &Value)> = Vec::new();

        for spec in &subtask_specs {
            let title = spec.get("title").and_then(|v| v.as_str()).unwrap_or("Unnamed subtask");
            let purpose = spec.get("purpose").and_then(|v| v.as_str()).unwrap_or("");

            match neo4j.create_task(title, Some(purpose)).await {
                Ok(child_id) => {
                    if let Err(e) = neo4j.link_subtask(&input.goal_task_id, &child_id).await {
                        warn!("Failed to link subtask {}: {}", child_id, e);
                    }
                    created_subtasks.push((child_id, spec));
                }
                Err(e) => {
                    warn!("Failed to create subtask '{}': {}", title, e);
                }
            }
        }

        // Second pass: wire DEPENDS_ON edges from LLM-specified step indices.
        for (idx, (child_id, spec)) in created_subtasks.iter().enumerate() {
            if let Some(dep_step) = spec.get("depends_on_step").and_then(|v| v.as_u64()) {
                let dep_idx = dep_step as usize;
                if dep_idx < created_subtasks.len() && dep_idx != idx {
                    let dep_id = &created_subtasks[dep_idx].0;
                    if let Err(e) = neo4j.link_task_dependency(child_id, dep_id).await {
                        warn!("Failed to link dependency {} -> {}: {}", child_id, dep_id, e);
                    }
                }
            }
        }

        // Build response list.
        let created_subtasks: Vec<Value> = created_subtasks.into_iter().map(|(child_id, spec)| {
            let title = spec.get("title").and_then(|v| v.as_str()).unwrap_or("");
            let purpose = spec.get("purpose").and_then(|v| v.as_str()).unwrap_or("");
            let tool_hint = spec.get("tool_hint").and_then(|v| v.as_str()).unwrap_or("");
            let depends_on_step = spec.get("depends_on_step").cloned().unwrap_or(Value::Null);
            json!({
                "id": child_id,
                "title": title,
                "purpose": purpose,
                "tool_hint": tool_hint,
                "depends_on_step": depends_on_step,
            })
        }).collect();

        info!(
            parent_id = %input.goal_task_id,
            subtasks = created_subtasks.len(),
            "Decomposed goal into subtasks"
        );

        let response = json!({
            "parent_task_id": input.goal_task_id,
            "subtasks": created_subtasks
        });
        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
    }

    async fn handle_update_task(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: UpdateTaskInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let neo4j = match &self.neo4j {
            Some(n) => n,
            None => return ToolCallResult::error("Neo4j not available".to_string()),
        };

        let status = match input.status.as_str() {
            "in_progress" => TaskStatus::InProgress,
            "completed" => TaskStatus::Completed,
            "failed" => TaskStatus::Failed,
            "blocked" => TaskStatus::Blocked,
            other => return ToolCallResult::error(format!("Unknown status '{}'. Use: in_progress, completed, failed, blocked", other)),
        };

        if let Err(e) = neo4j.update_task_status(&input.task_id, status).await {
            return ToolCallResult::error(format!("Failed to update task status: {}", e));
        }

        info!(task_id = %input.task_id, status = %input.status, "Updated task status");

        let note_id = if let Some(note_content) = &input.note {
            match neo4j.store_outcome_note(note_content, Some(&input.task_id)).await {
                Ok(id) => {
                    info!(note_id = %id, "Stored progress note");
                    Some(id)
                }
                Err(e) => {
                    warn!("Failed to store progress note: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let mut response = json!({
            "task_id": input.task_id,
            "status": input.status,
        });

        if let Some(nid) = note_id {
            response["note_id"] = json!(nid);
        }

        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
    }

    async fn handle_list_tasks(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ListTasksInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let neo4j = match &self.neo4j {
            Some(n) => n,
            None => return ToolCallResult::error("Neo4j not available".to_string()),
        };

        let limit = input.limit.unwrap_or(20);

        match neo4j.list_tasks(input.status.as_deref(), limit).await {
            Ok(tasks) => {
                let response = json!({
                    "count": tasks.len(),
                    "tasks": tasks
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to list tasks: {}", e)),
        }
    }

    async fn handle_record_outcome(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: RecordOutcomeInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let neo4j = match &self.neo4j {
            Some(n) => n,
            None => return ToolCallResult::error("Neo4j not available".to_string()),
        };

        let content = format!(
            "Tool: {} | Success: {}\n{}",
            input.tool_name, input.success, input.summary
        );

        match neo4j.store_outcome_note(&content, input.task_id.as_deref()).await {
            Ok(outcome_id) => {
                info!(
                    outcome_id = %outcome_id,
                    tool = %input.tool_name,
                    success = input.success,
                    "Recorded outcome"
                );

                // Meta-learning: when a failure is recorded with a task_id, enqueue a
                // reflect_on_work job so the agent automatically learns from the failure.
                let mut reflection_job_id: Option<String> = None;
                if !input.success {
                    if let (Some(queue), Some(task_id)) = (&self.queue, &input.task_id) {
                        let steps = vec![
                            ChainStep {
                                tool_name: "reflect_on_work".to_string(),
                                arguments: Some(json!({
                                    "goal": format!("Understand why '{}' failed", input.tool_name),
                                    "current_state": input.summary,
                                    "task_id": task_id
                                })),
                                priority: Some(1),
                                max_attempts: Some(2),
                                provider_hint: None,
                            },
                            ChainStep {
                                tool_name: "store_note".to_string(),
                                arguments: Some(json!({
                                    "content": format!(
                                        "Failure pattern for '{}': {}",
                                        input.tool_name, input.summary
                                    ),
                                    "note_type": "reflection"
                                })),
                                priority: Some(1),
                                max_attempts: Some(2),
                                provider_hint: None,
                            },
                        ];
                        match queue.enqueue_chain(&steps, None).await {
                            Ok(ids) => {
                                info!(
                                    tool = %input.tool_name,
                                    job_id = ?ids.first(),
                                    "Enqueued meta-learning reflection for failure"
                                );
                                reflection_job_id = ids.into_iter().next();
                            }
                            Err(e) => warn!(error = %e, "Failed to enqueue meta-learning reflection"),
                        }
                    }
                }

                let mut response = json!({
                    "outcome_id": outcome_id,
                    "tool_name": input.tool_name,
                    "success": input.success,
                });
                if let Some(jid) = reflection_job_id {
                    response["reflection_job_id"] = json!(jid);
                }
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to store outcome: {}", e)),
        }
    }
}

#[async_trait]
impl Skill for TaskSkill {
    fn name(&self) -> &str {
        "Task Manager"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::create_task_def(),
            Self::reflect_def(),
            Self::decompose_goal_def(),
            Self::update_task_def(),
            Self::list_tasks_def(),
            Self::record_outcome_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "create_task" => Some(self.handle_create_task(arguments).await),
            "reflect_on_work" => Some(self.handle_reflect(arguments).await),
            "decompose_goal" => Some(self.handle_decompose_goal(arguments).await),
            "update_task" => Some(self.handle_update_task(arguments).await),
            "list_tasks" => Some(self.handle_list_tasks(arguments).await),
            "record_outcome" => Some(self.handle_record_outcome(arguments).await),
            _ => None,
        }
    }
}

// ============================================================================
// Input structs
// ============================================================================

#[derive(Debug, Deserialize)]
struct CreateTaskInput {
    goal: String,
    #[serde(default)]
    context: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ReflectInput {
    goal: String,
    current_state: String,
    #[serde(default)]
    plan: Option<String>,
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(Debug, Deserialize)]
struct DecomposeGoalInput {
    goal_task_id: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    max_steps: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct UpdateTaskInput {
    task_id: String,
    status: String,
    #[serde(default)]
    note: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ListTasksInput {
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct RecordOutcomeInput {
    tool_name: String,
    summary: String,
    success: bool,
    #[serde(default)]
    task_id: Option<String>,
}

fn parse_args<T: for<'de> Deserialize<'de>>(
    arguments: Option<Value>,
) -> Result<T, ToolCallResult> {
    let args = arguments.unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(args)
        .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {}", e)))
}
