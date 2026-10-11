//! Trait abstractions for knowledge storage and LLM backends.
//!
//! Skills depend on these traits rather than concrete types (`Neo4jClient`,
//! `LlmConfig`) so they can be tested in isolation and swapped at runtime.
//! The task and working-memory stores live with their `Neo4jClient` impls
//! (`store_impls`), since a run starts through the workflows above this crate.

use async_trait::async_trait;
use serde_json::Value;

use agent_brain_models::ProvenanceFlag;

pub use agent_brain_llm::traits::LlmProvider;

// ============================================================================
// KnowledgeStore
// ============================================================================

/// Methods on [`crate::KnowledgeService`] that `KnowledgeSkill` calls.
/// What a `recall_window` call is asking for.
///
/// A struct rather than seven positional arguments because the last three
/// (`offset`, `sample_limit`, `full`) are a single decision — how much of the
/// matching set to return and *to whom* — and passing them separately made it
/// easy to change one without the others. `full` in particular is not a bigger
/// page: it routes the rows to the human and leaves the model a receipt.
#[derive(Debug, Clone, Copy)]
pub struct RecallSpec<'a> {
    /// Window start, UTC ISO-8601, inclusive.
    pub since_iso: &'a str,
    /// Window end, UTC ISO-8601, exclusive.
    pub until_iso: &'a str,
    /// Max sample rows for this page.
    pub sample_limit: usize,
    pub source_context: Option<&'a str>,
    pub note_type: Option<&'a str>,
    /// Rows to skip. Paging exists because raising `sample_limit` past the
    /// caller's channel does not enumerate more — it moves where the clipping
    /// falls.
    pub offset: usize,
    /// Deliver the COMPLETE matching set with whole note content, for a person
    /// to read. The caller is expected to route these rows to the user rather
    /// than into a model's context.
    pub full: bool,
}

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

    /// Flag a stored note and its chunks as having failed verification.
    /// Default no-op so test mocks and minimal stores compile unchanged.
    async fn mark_verification_failed(&self, _note_id: &str, _reason: &str) -> anyhow::Result<()> {
        Ok(())
    }

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

    /// Link a note to the task it reflects on (`REFLECTS_ON`). `Ok(false)` when
    /// either node is missing. Default no-op so test mocks compile unchanged.
    async fn link_note_to_task(&self, _note_id: &str, _task_id: &str) -> anyhow::Result<bool> {
        Ok(false)
    }

    async fn search_notes(
        &self,
        query: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> anyhow::Result<Vec<Value>>;

    /// Hybrid search that does NOT bump `access_count` / `last_accessed_at`.
    ///
    /// For callers that read the graph to decide something rather than to
    /// answer a user — the retrieval eval harness, and the "do we already know
    /// this?" probe that fronts every web search. Those must not make a note
    /// look popular, or measurement and housekeeping start shaping the spaced
    /// repetition schedule they are supposed to observe.
    ///
    /// Default forwards to `search_notes`; the real store overrides it.
    async fn search_notes_readonly(
        &self,
        query: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> anyhow::Result<Vec<Value>> {
        self.search_notes(query, limit, graph_hops, note_type, source_context)
            .await
    }

    async fn search_notes_with_entity_expansion(
        &self,
        query: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> anyhow::Result<Vec<Value>>;

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

    async fn search_by_entity(
        &self,
        entity_name: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> anyhow::Result<Vec<Value>>;

    /// Enumerate notes by LABEL — `note_type` and/or `source_context` — rather
    /// than ranking them by similarity. When the caller already knows the label,
    /// a top-K sample is the wrong shape: it truncates and misses, and an agent
    /// reads the gap as absence. Same doctrine as `recall_recent` and
    /// `list_endpoints`. Chunk fragments are excluded so one note is not
    /// returned several times over.
    async fn list_notes_filtered(
        &self,
        limit: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> anyhow::Result<Vec<Value>>;

    /// Whole-graph label inventory: the `note_type`s and `source_context`s that
    /// actually exist, with counts, plus the total note count (chunks excluded).
    ///
    /// The escape hatch behind a zero-result enumeration. A caller that filters
    /// on a label it guessed cannot correct itself from a bare `count: 0` — it
    /// reads as "the graph holds none of these" and gets reported as absence.
    /// Unbounded sibling of the window buckets `recall_window` already returns.
    async fn label_buckets(&self) -> anyhow::Result<(Vec<Value>, Vec<Value>, i64)>;

    /// Deterministic time-window recall: notes created in `[since_iso, until_iso)`
    /// (UTC ISO-8601), counted by `note_type` and by `source_context`, plus a
    /// bounded, most-recent sample. Chunk notes (`-[:PART_OF]->`) are excluded so
    /// counts reflect distinct items, not re-embedded fragments. This is the
    /// aggregate a "what did we learn / what happened" question needs — the thing
    /// a top-K semantic search over a date string cannot answer.
    ///
    /// `source_context` / `note_type`, when given, narrow every aggregation and
    /// the sample alike, so any bucket this returns can be enumerated by calling
    /// again with that bucket's name. A count a caller cannot drill into is how a
    /// false negative gets manufactured — see the impl for the incident.
    ///
    /// `offset` skips rows, so a matching set larger than one tool result stays
    /// reachable by paging. Raising `sample_limit` past what the caller's
    /// channel carries does NOT enumerate more — it only moves where the
    /// truncation falls, which is how a complete result came back reading as
    /// "not the complete list".
    async fn recall_window(&self, spec: RecallSpec<'_>) -> anyhow::Result<Value>;

    async fn reason_structured(
        &self,
        question: &str,
        limit: usize,
        store_inference: bool,
        run_critic: bool,
    ) -> anyhow::Result<crate::knowledge::ReasonOutput>;

    async fn create_gap_tasks(
        &self,
        gaps: &[String],
        triggering_note_id: &str,
    ) -> anyhow::Result<Vec<String>>;
}
