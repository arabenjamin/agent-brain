//! Trait abstractions for storage and LLM backends.
//!
//! Skills depend on these traits rather than concrete types (`Neo4jClient`,
//! `LlmConfig`) so they can be tested in isolation and swapped at runtime.

use async_trait::async_trait;
use serde_json::Value;

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
    ) -> anyhow::Result<(String, usize)>;

    async fn search_notes(
        &self,
        query: &str,
        limit: usize,
        graph_hops: usize,
    ) -> anyhow::Result<Vec<Value>>;

    async fn search_notes_with_entity_expansion(
        &self,
        query: &str,
        limit: usize,
        graph_hops: usize,
    ) -> anyhow::Result<Vec<Value>>;

    async fn find_related_notes(
        &self,
        note_id: &str,
    ) -> anyhow::Result<Vec<(String, f64)>>;

    async fn prune_old_notes(
        &self,
        days_stale: i64,
        min_accesses: i64,
        score_threshold: Option<f64>,
        lambda: Option<f64>,
        dry_run: bool,
    ) -> anyhow::Result<usize>;

    async fn consolidate_memories(
        &self,
        topic: &str,
        limit: usize,
    ) -> anyhow::Result<(String, usize, String)>;

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

    async fn list_notes(
        &self,
        limit: usize,
        note_type: Option<&str>,
    ) -> anyhow::Result<Vec<Value>>;

    async fn delete_note(&self, id: &str) -> anyhow::Result<bool>;

    async fn update_note(&self, id: &str, content: &str) -> anyhow::Result<bool>;
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
    ) -> anyhow::Result<String>;

    async fn get_task(&self, id: &str) -> anyhow::Result<Option<Task>>;

    async fn link_subtask(
        &self,
        parent_id: &str,
        child_id: &str,
    ) -> anyhow::Result<()>;

    async fn link_task_dependency(
        &self,
        from_id: &str,
        to_id: &str,
    ) -> anyhow::Result<()>;

    async fn update_task_status(
        &self,
        id: &str,
        status: TaskStatus,
    ) -> anyhow::Result<()>;

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

    async fn list_tasks(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Value>>;

    /// If all subtasks of the parent are completed, auto-complete the parent too.
    /// Returns `Some(parent_id)` if the parent was auto-completed, `None` otherwise.
    async fn auto_complete_parent_if_done(
        &self,
        task_id: &str,
    ) -> anyhow::Result<Option<String>>;
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
    async fn get_entries(
        &self,
        session_id: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Value>>;

    /// Return session summaries (session_id, started_at, msg_count, title).
    async fn list_sessions(&self, limit: i64) -> anyhow::Result<Vec<Value>>;

    /// Return all entries for `session_id` ordered by turn (no limit).
    async fn get_all_entries(&self, session_id: &str) -> anyhow::Result<Vec<Value>>;

    /// Delete all WorkingMemory nodes for `session_id`.
    async fn delete_session(&self, session_id: &str) -> anyhow::Result<()>;
}

// ============================================================================
// ProcedureStore
// ============================================================================

/// Storage operations for `ProcedureSkill`.
#[async_trait]
pub trait ProcedureStore: Send + Sync {
    /// Persist a procedure node and return `Ok(())`.
    async fn store_procedure(
        &self,
        id: &str,
        name: &str,
        description: &str,
        steps_json: &str,
        timestamp: &str,
    ) -> anyhow::Result<()>;

    /// Return procedures matching `query` (case-insensitive keyword), up to `limit`.
    async fn search_procedures(
        &self,
        query: &str,
        limit: usize,
    ) -> anyhow::Result<Vec<Value>>;
}
