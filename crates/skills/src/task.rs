//! Task Skill - Provides tools for task management and self-correction.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::{info, warn};

use agent_brain_llm::LlmProvider;
use agent_brain_models::TaskStatus;
use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};
use agent_brain_work::TaskStore;

/// Task Skill implementation.
pub struct TaskSkill {
    llm: Arc<dyn LlmProvider>,
    neo4j: Option<Arc<dyn TaskStore>>,
}

impl TaskSkill {
    /// Create a new task skill.
    pub fn new(llm: Arc<dyn LlmProvider>, neo4j: Option<Arc<dyn TaskStore>>) -> Self {
        Self { llm, neo4j }
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn create_task_def() -> ToolDefinition {
        ToolDefinition {
            name: "create_task".to_string(),
            description: "Create a task the scheduler runs on its next tick. Name a `workflow` \
                (with its `inputs`) to run that one — manage_workflow action=list shows them and \
                their inputs; with no workflow, the goal runs the default workflow. The default \
                workflow reads only stored notes and reasons over them: it never searches the \
                web. Research that needs the web names fill-knowledge-gap (inputs.topic) or \
                deep-dive (inputs.question)."
                .to_string(),
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
                    },
                    "success_criteria": {
                        "type": "string",
                        "description": "Measurable definition of done — used by the evaluator to grade output (e.g. 'Returns valid JSON with all required fields')"
                    },
                    "workflow": {
                        "type": "string",
                        "description": "The workflow to run, by name (e.g. fill-knowledge-gap). Omit for the default workflow."
                    },
                    "inputs": {
                        "type": "object",
                        "description": "The workflow's inputs, e.g. {\"topic\": \"…\"}."
                    }
                },
                "required": ["goal"]
            }),
        }
    }

    fn reflect_def() -> ToolDefinition {
        ToolDefinition {
            name: "reflect_on_work".to_string(),
            description:
                "Analyze the current output or state against the original goal to determine \
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

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    async fn handle_create_task(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: CreateTaskInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let neo4j = match &self.neo4j {
            Some(n) => n,
            None => {
                info!(goal = %input.goal, "Neo4j not available, skipping persistence");
                return ToolCallResult::error(
                    "Persistence layer (Neo4j) not available.".to_string(),
                );
            }
        };

        // Duplicate detection: scan tasks from the last 7 days for overlap.
        //   • Active duplicate  (created / in_progress)  → return it; skip creation.
        //   • Failed duplicate                           → inject prior failure context.
        //   • Completed duplicate                        → allow fresh attempt as-is.
        let mut effective_context = input.context.clone();
        match neo4j.find_similar_tasks(7).await {
            Ok(candidates) => {
                for task in &candidates {
                    // A task naming its workflow is that workflow's run: a
                    // similar goal for another workflow is different work
                    // (2026-10-23: two *-rss drafts' runs merged into one).
                    if !may_be_same_work(input.workflow.as_deref(), task.workflow.as_deref()) {
                        continue;
                    }
                    let sim = jaccard_word_overlap(&input.goal, &task.goal);
                    if sim < 0.6 {
                        continue;
                    }
                    match &task.status {
                        TaskStatus::Created | TaskStatus::InProgress => {
                            let pct = (sim * 100.0) as u32;
                            info!(
                                existing_id = %task.id,
                                similarity_pct = pct,
                                "Suppressing duplicate task creation"
                            );
                            return ToolCallResult::success_json(json!({
                                "id": task.id,
                                "status": task.status,
                                "duplicate": true,
                                "similarity_pct": pct,
                                "message": format!(
                                    "Similar task already active ({pct}% word overlap). \
                                     Returning existing task instead of creating a duplicate."
                                )
                            }));
                        }
                        TaskStatus::Failed => {
                            let prior = format!(
                                "PRIOR ATTEMPT FAILED — goal: \"{}\". Prior context: {}",
                                task.goal,
                                task.context.as_deref().unwrap_or("(none)")
                            );
                            effective_context = Some(match effective_context {
                                Some(ctx) => format!("{prior}\n\n{ctx}"),
                                None => prior,
                            });
                        }
                        _ => {} // completed → allow fresh attempt unchanged
                    }
                    break;
                }
            }
            Err(e) => warn!("find_similar_tasks failed, skipping dedup: {}", e),
        }

        match neo4j
            .start_run(
                input.workflow.as_deref(),
                &input.inputs,
                &input.goal,
                effective_context.as_deref(),
                input.success_criteria.as_deref(),
            )
            .await
        {
            Ok((task_id, workflow)) => {
                info!(task_id = %task_id, goal = %input.goal, workflow = %workflow, "Created new task in DB");
                ToolCallResult::success_json(json!({
                    "id": task_id,
                    "status": "created",
                    "workflow": workflow,
                    "message": "Task created; the scheduler dispatches it on its next tick."
                }))
            }
            Err(e) => ToolCallResult::error(format!("Task not created: {e}")),
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
                "You are a quality reviewer. Respond in English only.\n\
                Your response MUST begin with the line \"Score: N/5\" \
                and follow the exact structure below. Do NOT invent a title, do NOT write a preamble, \
                do NOT use the goal text as a heading. Start your response with \"Score:\".\n\n\
                GOAL TO EVALUATE AGAINST: {}\n\n\
                OUTPUT TO REVIEW:\n{}\n\n\
                PLAN (if any): {}\n\n\
                REQUIRED RESPONSE FORMAT — output these sections in this exact order:\n\n\
                Score: N/5\n\
                (5=FULLY MET; 4=nearly met; 3=PARTIALLY MET; 2=mostly missing; 1=NOT MET)\n\n\
                ## Goal Assessment\n\
                One sentence: FULLY MET / PARTIALLY MET / NOT MET, and why.\n\n\
                ## What Was Done Well\n\
                1-3 specific things the output got right.\n\n\
                ## Critical Issues\n\
                Each issue: problem statement, severity (high/medium/low), exact fix needed.\n\
                Flag as high severity: truncated output, stated intent without results, claimed \
                success without evidence.\n\n\
                ## Required Next Steps\n\
                Numbered concrete steps to fully satisfy the goal.",
                input.goal,
                input.current_state,
                input.plan.as_deref().unwrap_or("none")
            );

            match self.llm.generate(&prompt, None).await {
                Ok(reflection_text) => {
                    let reflection_note_id = if let Some(neo4j) = &self.neo4j {
                        match neo4j
                            .store_reflection_note(&reflection_text, input.task_id.as_deref())
                            .await
                        {
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
                        response_json["id"] = json!(note_id);
                    }

                    ToolCallResult::success_json(response_json)
                }
                Err(e) => ToolCallResult::error(format!("LLM reflection failed: {}", e)),
            }
        }
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
            other => {
                return ToolCallResult::error(format!(
                    "Unknown status '{}'. Use: in_progress, completed, failed, blocked",
                    other
                ));
            }
        };

        if let Err(e) = neo4j.update_task_status(&input.task_id, status).await {
            return ToolCallResult::error(format!("Failed to update task status: {}", e));
        }

        info!(task_id = %input.task_id, status = %input.status, "Updated task status");

        // When a task completes, bubble up to parent if all siblings are now done.
        let auto_completed_parent = if input.status == "completed" {
            match neo4j.auto_complete_parent_if_done(&input.task_id).await {
                Ok(Some(parent_id)) => {
                    info!(parent_id = %parent_id, child_id = %input.task_id, "Auto-completed parent task (all subtasks done)");
                    Some(parent_id)
                }
                Ok(None) => None,
                Err(e) => {
                    warn!(error = %e, "Failed to check parent auto-completion");
                    None
                }
            }
        } else {
            None
        };

        let note_id = if let Some(note_content) = &input.note {
            match neo4j
                .store_outcome_note(note_content, Some(&input.task_id))
                .await
            {
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
            "id": input.task_id,
            "status": input.status,
        });

        if let Some(nid) = note_id {
            response["note_id"] = json!(nid);
        }
        if let Some(pid) = auto_completed_parent {
            response["parent_completed"] = json!(pid);
        }

        ToolCallResult::success_json(response)
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
            Self::update_task_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "create_task" => Some(self.handle_create_task(arguments).await),
            "reflect_on_work" => Some(self.handle_reflect(arguments).await),
            "update_task" => Some(self.handle_update_task(arguments).await),
            _ => None,
        }
    }
}

// ============================================================================
// ============================================================================
// Helpers
// ============================================================================

/// Jaccard word-overlap similarity between two goal strings, range [0, 1].
///
/// Stopwords and tokens ≤ 2 chars are excluded so that minor phrasing
/// differences ("Analyze the repeated failures" vs "Analyze repeated failure")
/// still score above the dedup threshold.
/// Whether an existing task could be the one being created: not when the new
/// task names a workflow the existing one does not run.
fn may_be_same_work(new: Option<&str>, existing: Option<&str>) -> bool {
    new.is_none() || new == existing
}

fn jaccard_word_overlap(a: &str, b: &str) -> f32 {
    const STOPWORDS: &[&str] = &[
        "the", "a", "an", "to", "of", "in", "and", "or", "for", "with", "on", "at", "by", "is",
        "are", "was", "were", "be", "been", "that", "this", "it", "its", "as", "so", "do", "has",
        "have",
    ];
    let tokenize = |s: &str| -> std::collections::HashSet<String> {
        s.split_whitespace()
            .map(|w| {
                w.chars()
                    .filter(|c| c.is_alphabetic())
                    .collect::<String>()
                    .to_lowercase()
            })
            .filter(|w| w.len() > 2 && !STOPWORDS.contains(&w.as_str()))
            .collect()
    };
    let words_a = tokenize(a);
    let words_b = tokenize(b);
    let intersection = words_a.intersection(&words_b).count();
    let union_count = words_a.union(&words_b).count();
    if union_count == 0 {
        return 0.0;
    }
    intersection as f32 / union_count as f32
}

// Input structs
// ============================================================================

#[derive(Debug, Deserialize)]
struct CreateTaskInput {
    goal: String,
    #[serde(default)]
    context: Option<String>,
    #[serde(default)]
    success_criteria: Option<String>,
    #[serde(default)]
    workflow: Option<String>,
    #[serde(default)]
    inputs: serde_json::Map<String, Value>,
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
struct UpdateTaskInput {
    task_id: String,
    status: String,
    #[serde(default)]
    note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use std::sync::Arc;

    fn skill(store: MockTaskStore) -> TaskSkill {
        TaskSkill::new(
            Arc::new(MockLlm::ok("reflection text")),
            Some(Arc::new(store)),
        )
    }

    fn skill_no_db() -> TaskSkill {
        TaskSkill::new(Arc::new(MockLlm::ok("ok")), None)
    }

    // -- tool registry --------------------------------------------------------

    #[test]
    fn a_named_workflow_never_merges_into_another_workflows_task() {
        assert!(!may_be_same_work(Some("a-rss"), Some("b-rss")));
        assert!(!may_be_same_work(Some("a-rss"), None));
        assert!(may_be_same_work(Some("a-rss"), Some("a-rss")));
        assert!(may_be_same_work(None, Some("a-rss")));
    }

    #[test]
    fn tools_list_has_correct_count() {
        assert_eq!(skill(MockTaskStore::default()).tools().len(), 3);
    }

    #[test]
    fn execute_unknown_tool_returns_none() {
        let s = skill_no_db();
        let r = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(s.execute("not_a_tool", None));
        assert!(r.is_none());
    }

    // -- create_task ----------------------------------------------------------

    #[tokio::test]
    async fn create_task_success() {
        let r = result_json(
            skill(MockTaskStore::default())
                .execute(
                    "create_task",
                    Some(serde_json::json!({"goal": "build thing"})),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["id"], "task-id-1");
        assert_eq!(r["status"], "created");
    }

    #[tokio::test]
    async fn create_task_no_db_returns_error() {
        let msg = result_error(
            skill_no_db()
                .execute("create_task", Some(serde_json::json!({"goal": "test"})))
                .await
                .unwrap(),
        );
        assert!(msg.contains("not available"));
    }

    #[tokio::test]
    async fn create_task_missing_goal_returns_error() {
        let r = skill(MockTaskStore::default())
            .execute("create_task", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[tokio::test]
    async fn create_task_store_error_propagates() {
        let store = MockTaskStore {
            create_result: Err("db error".into()),
            ..Default::default()
        };
        let msg = result_error(
            skill(store)
                .execute("create_task", Some(serde_json::json!({"goal": "x"})))
                .await
                .unwrap(),
        );
        assert!(msg.contains("db error"));
    }

    // -- reflect_on_work ------------------------------------------------------

    #[tokio::test]
    async fn reflect_on_work_success_stores_note() {
        let r = result_json(
            skill(MockTaskStore::default())
                .execute(
                    "reflect_on_work",
                    Some(serde_json::json!({
                        "goal": "write tests",
                        "current_state": "wrote some tests",
                        "task_id": "task-id-1"
                    })),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["status"], "reflection_complete");
        assert_eq!(r["id"], "reflection-note-id");
    }

    #[tokio::test]
    async fn reflect_on_work_without_task_id_stores_note_anyway() {
        // neo4j is present: note is stored even without a task_id link
        let r = result_json(
            skill(MockTaskStore::default())
                .execute(
                    "reflect_on_work",
                    Some(serde_json::json!({
                        "goal": "test",
                        "current_state": "state"
                    })),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["status"], "reflection_complete");
        assert_eq!(r["id"], "reflection-note-id");
    }

    #[tokio::test]
    async fn reflect_on_work_no_db_omits_id() {
        let r = result_json(
            skill_no_db()
                .execute(
                    "reflect_on_work",
                    Some(serde_json::json!({
                        "goal": "test",
                        "current_state": "state"
                    })),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["status"], "reflection_complete");
        assert!(r.get("id").is_none());
    }

    #[tokio::test]
    async fn reflect_on_work_llm_error() {
        let s = TaskSkill::new(
            Arc::new(MockLlm::err("llm fail")),
            Some(Arc::new(MockTaskStore::default())),
        );
        let msg = result_error(
            s.execute(
                "reflect_on_work",
                Some(serde_json::json!({"goal": "g", "current_state": "s"})),
            )
            .await
            .unwrap(),
        );
        assert!(msg.contains("llm fail"));
    }

    // -- update_task ----------------------------------------------------------

    #[tokio::test]
    async fn update_task_success() {
        let r = result_json(
            skill(MockTaskStore::default())
                .execute(
                    "update_task",
                    Some(serde_json::json!({"task_id": "task-id-1", "status": "completed"})),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["status"], "completed");
        assert_eq!(r["id"], "task-id-1");
    }

    #[tokio::test]
    async fn update_task_with_note() {
        let r = result_json(
            skill(MockTaskStore::default())
                .execute(
                    "update_task",
                    Some(serde_json::json!({
                        "task_id": "task-id-1",
                        "status": "in_progress",
                        "note": "making progress"
                    })),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["note_id"], "outcome-note-id");
    }

    #[tokio::test]
    async fn update_task_unknown_status() {
        let msg = result_error(
            skill(MockTaskStore::default())
                .execute(
                    "update_task",
                    Some(serde_json::json!({"task_id": "t1", "status": "unknown_val"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("Unknown status"));
    }

    #[tokio::test]
    async fn update_task_auto_completes_parent() {
        let store = MockTaskStore {
            auto_complete_result: Ok(Some("parent-id-1".into())),
            ..Default::default()
        };
        let r = result_json(
            skill(store)
                .execute(
                    "update_task",
                    Some(serde_json::json!({"task_id": "task-id-1", "status": "completed"})),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["parent_completed"], "parent-id-1");
    }

    // -- decompose_goal -------------------------------------------------------

    // -- record_outcome -------------------------------------------------------
}
