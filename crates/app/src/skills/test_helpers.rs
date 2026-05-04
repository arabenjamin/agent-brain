//! Shared mock implementations of skill traits for unit tests.
//!
//! Usage: `use crate::skills::test_helpers::*;`

use async_trait::async_trait;
use serde_json::Value;

use crate::models::{Task, TaskStatus};
use crate::services::traits::{KnowledgeStore, LlmProvider, TaskStore, WorkingMemoryStore};

// ============================================================================
// MockLlm
// ============================================================================

/// Configurable mock LLM. Returns `response` on every `generate` call.
pub struct MockLlm {
    pub response: Result<String, String>,
    pub embedding: Vec<f32>,
}

impl MockLlm {
    pub fn ok(response: impl Into<String>) -> Self {
        Self {
            response: Ok(response.into()),
            embedding: vec![0.1, 0.2, 0.3],
        }
    }

    pub fn err(msg: impl Into<String>) -> Self {
        Self {
            response: Err(msg.into()),
            embedding: vec![],
        }
    }
}

#[async_trait]
impl LlmProvider for MockLlm {
    async fn generate(&self, _prompt: &str, _system: Option<&str>) -> anyhow::Result<String> {
        self.response.clone().map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
        if self.embedding.is_empty() {
            anyhow::bail!("embed error");
        }
        Ok(self.embedding.clone())
    }

    fn model_name(&self) -> &str {
        "mock-model"
    }

    fn is_available(&self) -> bool {
        self.response.is_ok()
    }
}

// ============================================================================
// MockKnowledgeStore
// ============================================================================

type ReasonResult = Result<(String, Vec<String>, f64, Vec<String>, Option<String>), String>;
type AuditResult = Result<(bool, f64, Vec<String>, Vec<String>, String), String>;

pub struct MockKnowledgeStore {
    pub store_result: Result<(String, usize), String>,
    pub search_result: Result<Vec<Value>, String>,
    pub prune_result: Result<usize, String>,
    pub consolidate_result: Result<(String, usize, String), String>,
    pub synthesize_result: Result<(String, String), String>,
    pub reason_result: ReasonResult,
    pub audit_result: AuditResult,
    pub explain_result: Result<(String, Vec<Value>), String>,
}

impl Default for MockKnowledgeStore {
    fn default() -> Self {
        Self {
            store_result: Ok(("note-id-1".into(), 2)),
            search_result: Ok(vec![]),
            prune_result: Ok(3),
            consolidate_result: Ok(("consolidated-id".into(), 5, "preview text".into())),
            synthesize_result: Ok(("synth-id".into(), "synth preview".into())),
            reason_result: Ok((
                "answer text".into(),
                vec!["inference 1".into()],
                0.85,
                vec![],
                Some("inf-note-id".into()),
            )),
            audit_result: Ok((true, 0.9, vec![], vec![], "aligned".into())),
            explain_result: Ok(("explanation text".into(), vec![])),
        }
    }
}

#[async_trait]
impl KnowledgeStore for MockKnowledgeStore {
    async fn store_note(
        &self,
        _content: &str,
        _note_type: Option<&str>,
        _source_context: Option<&str>,
        _event_at: Option<&str>,
        _provenance: Option<agent_brain_models::ProvenanceFlag>,
    ) -> anyhow::Result<(String, usize)> {
        self.store_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn search_notes(
        &self,
        _query: &str,
        _limit: usize,
        _graph_hops: usize,
    ) -> anyhow::Result<Vec<Value>> {
        self.search_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn search_notes_with_entity_expansion(
        &self,
        _query: &str,
        _limit: usize,
        _graph_hops: usize,
    ) -> anyhow::Result<Vec<Value>> {
        self.search_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn find_related_notes(&self, _note_id: &str) -> anyhow::Result<Vec<(String, f64)>> {
        Ok(vec![])
    }

    async fn prune_old_notes(
        &self,
        _days_stale: i64,
        _min_accesses: i64,
        _score_threshold: Option<f64>,
        _lambda: Option<f64>,
        _dry_run: bool,
        _min_retain: i64,
        _max_pct: f64,
    ) -> anyhow::Result<usize> {
        self.prune_result
            .as_ref()
            .map(|v| *v)
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn consolidate_memories(
        &self,
        _topic: &str,
        _limit: usize,
    ) -> anyhow::Result<(String, usize, String)> {
        self.consolidate_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn synthesize_knowledge(
        &self,
        _topic: &str,
        _limit: usize,
    ) -> anyhow::Result<(String, String)> {
        self.synthesize_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn review_due_notes(&self, _limit: usize) -> anyhow::Result<Vec<Value>> {
        Ok(vec![])
    }

    async fn reason(
        &self,
        _question: &str,
        _limit: usize,
        _store_inference: bool,
    ) -> anyhow::Result<(String, Vec<String>, f64, Vec<String>, Option<String>)> {
        self.reason_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn audit_action(
        &self,
        _action: &str,
        _context: Option<&str>,
    ) -> anyhow::Result<(bool, f64, Vec<String>, Vec<String>, String)> {
        self.audit_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn explain_reasoning(
        &self,
        _decision: &str,
        _task_id: Option<&str>,
        _limit: usize,
    ) -> anyhow::Result<(String, Vec<Value>)> {
        self.explain_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn export_graph_visualization(
        &self,
        _max_nodes: usize,
    ) -> anyhow::Result<(Vec<Value>, Vec<Value>)> {
        Ok((vec![], vec![]))
    }

    async fn get_note(&self, _id: &str) -> anyhow::Result<Option<Value>> {
        Ok(None)
    }

    async fn search_by_entity(
        &self,
        _entity_name: &str,
        _entity_type: Option<&str>,
        _limit: usize,
    ) -> anyhow::Result<Vec<Value>> {
        self.search_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn list_notes(
        &self,
        _limit: usize,
        _note_type: Option<&str>,
    ) -> anyhow::Result<Vec<Value>> {
        Ok(vec![])
    }

    async fn delete_note(&self, _id: &str) -> anyhow::Result<bool> {
        Ok(true)
    }

    async fn update_note(&self, _id: &str, _content: &str) -> anyhow::Result<bool> {
        Ok(true)
    }

    async fn reason_structured(
        &self,
        _question: &str,
        _limit: usize,
        _store_inference: bool,
        _run_critic: bool,
    ) -> anyhow::Result<crate::services::knowledge::ReasonOutput> {
        Ok(crate::services::knowledge::ReasonOutput {
            answer: String::new(),
            sources: vec![],
            confidence: 0.5,
            caveats: vec![],
            follow_up_questions: vec![],
            inferences: vec![],
            gaps: vec![],
            inference_note_id: None,
            critic_counter_arguments: vec![],
        })
    }

    async fn create_gap_tasks(
        &self,
        _gaps: &[String],
        _triggering_note_id: &str,
    ) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }
}

// ============================================================================
// MockTaskStore
// ============================================================================

pub struct MockTaskStore {
    pub create_result: Result<String, String>,
    pub get_result: Result<Option<Task>, String>,
    pub store_reflection_result: Result<String, String>,
    pub store_outcome_result: Result<String, String>,
    pub update_status_result: Result<(), String>,
    pub auto_complete_result: Result<Option<String>, String>,
}

impl Default for MockTaskStore {
    fn default() -> Self {
        Self {
            create_result: Ok("task-id-1".into()),
            get_result: Ok(Some(Task {
                id: "task-id-1".into(),
                goal: "Test goal".into(),
                context: None,
                success_criteria: None,
                status: TaskStatus::Created,
                created_at: "2026-01-01T00:00:00Z".into(),
                updated_at: "2026-01-01T00:00:00Z".into(),
            })),
            store_reflection_result: Ok("reflection-note-id".into()),
            store_outcome_result: Ok("outcome-note-id".into()),
            update_status_result: Ok(()),
            auto_complete_result: Ok(None),
        }
    }
}

#[async_trait]
impl TaskStore for MockTaskStore {
    async fn create_task(
        &self,
        _goal: &str,
        _context: Option<&str>,
        _success_criteria: Option<&str>,
    ) -> anyhow::Result<String> {
        self.create_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn get_task(&self, _id: &str) -> anyhow::Result<Option<Task>> {
        self.get_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn link_subtask(&self, _parent_id: &str, _child_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn link_task_dependency(&self, _from_id: &str, _to_id: &str) -> anyhow::Result<()> {
        Ok(())
    }

    async fn update_task_status(&self, _id: &str, _status: TaskStatus) -> anyhow::Result<()> {
        self.update_status_result
            .as_ref()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn store_reflection_note(
        &self,
        _content: &str,
        _task_id: Option<&str>,
    ) -> anyhow::Result<String> {
        self.store_reflection_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn store_outcome_note(
        &self,
        _content: &str,
        _task_id: Option<&str>,
    ) -> anyhow::Result<String> {
        self.store_outcome_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn list_tasks(&self, _status: Option<&str>, _limit: usize) -> anyhow::Result<Vec<Value>> {
        Ok(vec![])
    }

    async fn auto_complete_parent_if_done(&self, _task_id: &str) -> anyhow::Result<Option<String>> {
        self.auto_complete_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}

// ============================================================================
// MockWorkingMemoryStore
// ============================================================================

pub struct MockWorkingMemoryStore {
    pub push_result: Result<i64, String>,
    pub get_all_result: Result<Vec<Value>, String>,
    pub delete_result: Result<(), String>,
}

impl Default for MockWorkingMemoryStore {
    fn default() -> Self {
        Self {
            push_result: Ok(0),
            get_all_result: Ok(vec![
                serde_json::json!({"role": "observation", "content": "some context"}),
                serde_json::json!({"role": "result", "content": "done"}),
            ]),
            delete_result: Ok(()),
        }
    }
}

#[async_trait]
impl WorkingMemoryStore for MockWorkingMemoryStore {
    async fn push_entry(
        &self,
        _id: &str,
        _session_id: &str,
        _content: &str,
        _role: &str,
        _ts: &str,
    ) -> anyhow::Result<i64> {
        self.push_result
            .as_ref()
            .map(|v| *v)
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn get_entries(&self, _session_id: &str, _limit: usize) -> anyhow::Result<Vec<Value>> {
        self.get_all_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn list_sessions(&self, _limit: i64) -> anyhow::Result<Vec<Value>> {
        Ok(vec![])
    }

    async fn get_all_entries(&self, _session_id: &str) -> anyhow::Result<Vec<Value>> {
        self.get_all_result
            .clone()
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn delete_session(&self, _session_id: &str) -> anyhow::Result<()> {
        self.delete_result
            .as_ref()
            .map(|_| ())
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn archive_session(&self, _session_id: &str) -> anyhow::Result<()> {
        Ok(())
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn first_text(result: &agent_brain_protocol::ToolCallResult) -> String {
    result
        .content
        .first()
        .and_then(|c| {
            if let agent_brain_protocol::Content::Text { text } = c {
                Some(text.clone())
            } else {
                None
            }
        })
        .unwrap_or_default()
}

/// Parse a `ToolCallResult` content as JSON. Panics if the result is an error
/// or the content is not valid JSON — useful for compact test assertions.
pub fn result_json(result: agent_brain_protocol::ToolCallResult) -> serde_json::Value {
    assert!(
        result.is_error != Some(true),
        "Expected success but got error: {}",
        first_text(&result)
    );
    let text = first_text(&result);
    serde_json::from_str(&text).expect("Expected JSON content")
}

/// Extract the error message from a `ToolCallResult`. Panics if it's a success.
pub fn result_error(result: agent_brain_protocol::ToolCallResult) -> String {
    assert!(
        result.is_error == Some(true),
        "Expected error but got success: {}",
        first_text(&result)
    );
    first_text(&result)
}
