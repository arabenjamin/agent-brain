//! Trait abstractions for storage and LLM backends.
//!
//! Skills depend on these traits rather than concrete types (`Neo4jClient`,
//! `LlmConfig`) so they can be tested in isolation and swapped at runtime.

use async_trait::async_trait;
use serde_json::Value;

use agent_brain_models::ProvenanceFlag;

use crate::models::{Task, TaskStatus};

// ============================================================================
// LlmProvider
// ============================================================================

/// Minimal LLM interface used by skills.
///
/// The concrete implementation is [`crate::services::shared_llm::SharedLlm`]
/// which wraps the live `Arc<RwLock<Option<LlmConfig>>>`.
#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Generate text from a prompt with an optional system message.
    async fn generate(&self, prompt: &str, system: Option<&str>) -> anyhow::Result<String>;

    /// Generate a dense embedding vector for `text`.
    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>>;

    /// Human-readable model identifier (e.g. `"granite3.3:8b"`).
    fn model_name(&self) -> &str;

    /// Return `true` if the backing LLM is currently configured.
    fn is_available(&self) -> bool;

    /// Generate strict JSON with self-correcting retries.
    ///
    /// Local models routinely wrap JSON in prose or markdown fences, emit
    /// trailing commentary, or drop required fields. This helper calls
    /// [`generate`](Self::generate), extracts the JSON payload, and parses it.
    /// On a parse failure — or when a required top-level key is missing from a
    /// JSON object — it re-prompts the model with the *specific* error appended
    /// (the "targeted self-correction" pattern) up to `max_retries` extra
    /// attempts, then returns the parsed [`Value`] or the last error.
    ///
    /// `required_keys` is only enforced for JSON objects; a bare array or
    /// scalar passes as long as it parses (pass `&[]` to accept any valid JSON).
    async fn generate_json(
        &self,
        prompt: &str,
        system: Option<&str>,
        required_keys: &[&str],
        max_retries: u32,
    ) -> anyhow::Result<Value> {
        let mut current = prompt.to_string();
        let mut attempt = 0u32;
        loop {
            let raw = self.generate(&current, system).await?;
            let candidate = crate::services::llm::extract_json(&raw);
            let correction = match serde_json::from_str::<Value>(candidate) {
                Ok(value) => {
                    let missing: Vec<&str> = value
                        .as_object()
                        .map(|obj| {
                            required_keys
                                .iter()
                                .copied()
                                .filter(|k| !obj.contains_key(*k))
                                .collect()
                        })
                        .unwrap_or_default();
                    if missing.is_empty() {
                        return Ok(value);
                    }
                    format!(
                        "Your previous response was missing required field(s): {missing:?}. \
                         Return ONLY a JSON object containing every required field. \
                         No prose, no markdown fences."
                    )
                }
                Err(e) => format!(
                    "Your previous response was not valid JSON ({e}). \
                     Return ONLY a valid JSON value — no prose, no markdown fences, no comments."
                ),
            };

            if attempt >= max_retries {
                let preview: String = raw.chars().take(200).collect();
                anyhow::bail!(
                    "LLM failed to produce valid JSON after {} attempt(s): {correction} \
                     Last output: {preview}",
                    attempt + 1
                );
            }
            current = format!("{prompt}\n\n{correction}");
            attempt += 1;
        }
    }
}

// ============================================================================
// KnowledgeStore
// ============================================================================

/// Methods on [`crate::services::KnowledgeService`] that `KnowledgeSkill` calls.
#[async_trait]
pub trait KnowledgeStore: Send + Sync {
    async fn store_note(
        &self,
        content: &str,
        note_type: Option<&str>,
        source_context: Option<&str>,
        event_at: Option<&str>,
        provenance: Option<ProvenanceFlag>,
    ) -> anyhow::Result<(String, usize)>;

    /// Store an inference note for a context-grounded `reason()` call.
    /// Default no-op so test mocks and minimal stores compile unchanged.
    async fn store_context_inference(
        &self,
        _question: &str,
        _answer: &str,
        _inferences: &[String],
    ) -> Option<String> {
        None
    }

    async fn search_notes(
        &self,
        query: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
    ) -> anyhow::Result<Vec<Value>>;

    async fn search_notes_with_entity_expansion(
        &self,
        query: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
    ) -> anyhow::Result<Vec<Value>>;

    async fn find_related_notes(&self, note_id: &str) -> anyhow::Result<Vec<(String, f64)>>;

    #[allow(clippy::too_many_arguments)]
    async fn prune_old_notes(
        &self,
        days_stale: i64,
        min_accesses: i64,
        score_threshold: Option<f64>,
        lambda: Option<f64>,
        dry_run: bool,
        min_retain: i64,
        max_pct: f64,
    ) -> anyhow::Result<usize>;

    async fn consolidate_memories(
        &self,
        topic: &str,
        limit: usize,
    ) -> anyhow::Result<(String, usize, String)>;

    async fn synthesize_knowledge(
        &self,
        topic: &str,
        limit: usize,
    ) -> anyhow::Result<(String, String)>;

    async fn review_due_notes(&self, limit: usize) -> anyhow::Result<Vec<Value>>;

    async fn reason(
        &self,
        question: &str,
        limit: usize,
        store_inference: bool,
    ) -> anyhow::Result<(String, Vec<String>, f64, Vec<String>, Option<String>)>;

    async fn audit_action(
        &self,
        action: &str,
        context: Option<&str>,
    ) -> anyhow::Result<(bool, f64, Vec<String>, Vec<String>, String)>;

    async fn explain_reasoning(
        &self,
        decision: &str,
        task_id: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<(String, Vec<Value>)>;

    async fn export_graph_visualization(
        &self,
        max_nodes: usize,
    ) -> anyhow::Result<(Vec<Value>, Vec<Value>)>;

    async fn get_note(&self, id: &str) -> anyhow::Result<Option<Value>>;

    async fn search_by_entity(
        &self,
        entity_name: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Value>>;

    async fn list_notes(&self, limit: usize, note_type: Option<&str>)
    -> anyhow::Result<Vec<Value>>;

    async fn delete_note(&self, id: &str) -> anyhow::Result<bool>;

    async fn update_note(&self, id: &str, content: &str) -> anyhow::Result<bool>;

    async fn reason_structured(
        &self,
        question: &str,
        limit: usize,
        store_inference: bool,
        run_critic: bool,
    ) -> anyhow::Result<crate::services::knowledge::ReasonOutput>;

    async fn create_gap_tasks(
        &self,
        gaps: &[String],
        triggering_note_id: &str,
    ) -> anyhow::Result<Vec<String>>;
}

// ============================================================================
// TaskStore
// ============================================================================

/// Methods on `Neo4jClient` (task repository) used by `TaskSkill`.
#[async_trait]
pub trait TaskStore: Send + Sync {
    async fn create_task(
        &self,
        goal: &str,
        context: Option<&str>,
        success_criteria: Option<&str>,
    ) -> anyhow::Result<String>;

    async fn get_task(&self, id: &str) -> anyhow::Result<Option<Task>>;

    async fn link_subtask(&self, parent_id: &str, child_id: &str) -> anyhow::Result<()>;

    async fn link_task_dependency(&self, from_id: &str, to_id: &str) -> anyhow::Result<()>;

    async fn update_task_status(&self, id: &str, status: TaskStatus) -> anyhow::Result<()>;

    async fn store_reflection_note(
        &self,
        content: &str,
        task_id: Option<&str>,
    ) -> anyhow::Result<String>;

    async fn store_outcome_note(
        &self,
        content: &str,
        task_id: Option<&str>,
    ) -> anyhow::Result<String>;

    async fn list_tasks(&self, status: Option<&str>, limit: usize) -> anyhow::Result<Vec<Value>>;

    /// If all subtasks of the parent are completed, auto-complete the parent too.
    /// Returns `Some(parent_id)` if the parent was auto-completed, `None` otherwise.
    async fn auto_complete_parent_if_done(&self, task_id: &str) -> anyhow::Result<Option<String>>;

    /// Return recent tasks for duplicate detection.
    ///
    /// Returns tasks created within `days_lookback` days with active or terminal
    /// statuses.  Similarity scoring is the caller's responsibility.
    async fn find_similar_tasks(&self, days_lookback: u32) -> anyhow::Result<Vec<Task>>;
}

// ============================================================================
// WorkingMemoryStore
// ============================================================================

/// Low-level storage operations for `WorkingMemorySkill`.
///
/// These map directly to the Cypher queries in the skill.
#[async_trait]
pub trait WorkingMemoryStore: Send + Sync {
    /// Insert a new working-memory entry and return the `turn_index` assigned.
    async fn push_entry(
        &self,
        id: &str,
        session_id: &str,
        content: &str,
        role: &str,
        ts: &str,
    ) -> anyhow::Result<i64>;

    /// Return entries for `session_id` ordered by turn, capped at `limit`.
    async fn get_entries(&self, session_id: &str, limit: usize) -> anyhow::Result<Vec<Value>>;

    /// Return session summaries (session_id, started_at, msg_count, title).
    async fn list_sessions(&self, limit: i64) -> anyhow::Result<Vec<Value>>;

    /// Return all entries for `session_id` ordered by turn (no limit).
    async fn get_all_entries(&self, session_id: &str) -> anyhow::Result<Vec<Value>>;

    /// Delete all WorkingMemory nodes for `session_id`.
    async fn delete_session(&self, session_id: &str) -> anyhow::Result<()>;

    /// Mark a session archived (hidden from default list but preserved for training data).
    async fn archive_session(&self, session_id: &str) -> anyhow::Result<()>;
}

#[cfg(test)]
mod generate_json_tests {
    use super::*;
    use std::sync::Mutex;

    /// LLM stub that returns a scripted response per call, in order. Records the
    /// prompts it received so tests can assert the self-correction re-prompt.
    struct ScriptedLlm {
        responses: Mutex<Vec<String>>,
        prompts: Mutex<Vec<String>>,
    }

    impl ScriptedLlm {
        fn new(responses: &[&str]) -> Self {
            Self {
                responses: Mutex::new(responses.iter().rev().map(|s| s.to_string()).collect()),
                prompts: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl LlmProvider for ScriptedLlm {
        async fn generate(&self, prompt: &str, _system: Option<&str>) -> anyhow::Result<String> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            self.responses
                .lock()
                .unwrap()
                .pop()
                .ok_or_else(|| anyhow::anyhow!("no more scripted responses"))
        }
        async fn embed(&self, _text: &str) -> anyhow::Result<Vec<f32>> {
            Ok(vec![])
        }
        fn model_name(&self) -> &str {
            "scripted"
        }
        fn is_available(&self) -> bool {
            true
        }
    }

    #[tokio::test]
    async fn clean_json_first_try_no_retry() {
        let llm = ScriptedLlm::new(&[r#"{"answer": "42"}"#]);
        let v = llm.generate_json("q", None, &["answer"], 2).await.unwrap();
        assert_eq!(v["answer"], "42");
        assert_eq!(
            llm.prompts.lock().unwrap().len(),
            1,
            "should not retry on success"
        );
    }

    #[tokio::test]
    async fn strips_markdown_fences_and_prose() {
        let llm = ScriptedLlm::new(&["Sure! Here you go:\n```json\n{\"answer\": \"ok\"}\n```"]);
        let v = llm.generate_json("q", None, &[], 2).await.unwrap();
        assert_eq!(v["answer"], "ok");
    }

    #[tokio::test]
    async fn self_corrects_invalid_then_valid() {
        let llm = ScriptedLlm::new(&["not json at all", r#"{"answer": "recovered"}"#]);
        let v = llm
            .generate_json("original prompt", None, &["answer"], 2)
            .await
            .unwrap();
        assert_eq!(v["answer"], "recovered");
        let prompts = llm.prompts.lock().unwrap();
        assert_eq!(prompts.len(), 2, "should re-prompt once");
        assert!(
            prompts[1].contains("not valid JSON"),
            "retry prompt names the error"
        );
        assert!(
            prompts[1].contains("original prompt"),
            "retry prompt keeps original task"
        );
    }

    #[tokio::test]
    async fn self_corrects_missing_required_key() {
        let llm = ScriptedLlm::new(&[r#"{"other": 1}"#, r#"{"needs_clarification": true}"#]);
        let v = llm
            .generate_json("q", None, &["needs_clarification"], 2)
            .await
            .unwrap();
        assert_eq!(v["needs_clarification"], true);
        assert!(llm.prompts.lock().unwrap()[1].contains("missing required field"));
    }

    #[tokio::test]
    async fn bare_array_passes_when_no_required_keys() {
        let llm = ScriptedLlm::new(&[r#"[{"name": "x"}]"#]);
        let v = llm.generate_json("q", None, &[], 1).await.unwrap();
        assert!(v.is_array());
    }

    #[tokio::test]
    async fn errors_after_exhausting_retries() {
        let llm = ScriptedLlm::new(&["nope", "still nope", "nope again"]);
        let err = llm
            .generate_json("q", None, &["answer"], 2)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("after 3 attempt"));
        // 1 initial + 2 retries = 3 calls, all consumed.
        assert_eq!(llm.prompts.lock().unwrap().len(), 3);
    }
}
