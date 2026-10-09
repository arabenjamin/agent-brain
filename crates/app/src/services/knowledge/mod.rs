//! Knowledge service: notes, their embeddings, and everything that reads them.
//!
//! One `impl KnowledgeService` block per concern:
//! `store` (writes, chunking, embedding backfill) · `retrieval` (vector + BM25
//! hybrid search) · `recall` (deterministic listings and local-day windows) ·
//! `labels` (claim / source-record labelling on every retrieval path) ·
//! `entities` · `maintenance` (review + pruning) · `reasoning` (`reason`, critic,
//! gap tasks) · `analysis` (synthesize / audit / explain) · `consolidation` ·
//! `export`. `store_trait` is the forwarding `KnowledgeStore` impl.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use chrono::Utc;
use serde_json::{Value, json};
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::repository::Neo4jClient;
use crate::services::LlmProvider;
use crate::services::snapshot::SnapshotService;
use agent_brain_models::ProvenanceFlag;

mod analysis;
mod consolidation;
mod entities;
mod export;
mod labels;
mod maintenance;
mod reasoning;
mod recall;
mod retrieval;
mod store;
mod store_trait;

pub(crate) use consolidation::CONSOLIDATION_SOURCE_PREDICATE;
pub use retrieval::{MatchTrace, TRACE_NO_VECTOR, sanitize_lucene_query};

/// Notes a caller must not get back from retrieval, by `note_type` or
/// `source_context`. Set per agent in its profile (`retrieval_exclude:`) and
/// scoped around the agent's loop with [`RETRIEVAL_EXCLUDE`], so it binds every
/// `search_notes` the agent makes without depending on the agent asking.
///
/// Why: on 2026-10-03 a `deep-diver` re-run of the Reticulum question found the
/// first run's wrong answer (re-typed `unsourced_synthesis`) through
/// `search_notes`, cited it as its evidence, and never read the spec. An
/// instruction not to trust its own notes was ignored, so the notes are not
/// shown at all.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct RetrievalExclude {
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub note_types: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub source_contexts: Vec<String>,
}

impl RetrievalExclude {
    pub fn is_empty(&self) -> bool {
        self.note_types.is_empty() && self.source_contexts.is_empty()
    }
}

tokio::task_local! {
    /// The running agent's [`RetrievalExclude`]. Unset outside an agent run.
    pub static RETRIEVAL_EXCLUDE: RetrievalExclude;
}

tokio::task_local! {
    /// Set while a tool runs for an MCP client's `tools/call` (`mcp/server.rs`):
    /// someone outside the brain asked for this write.
    pub static CLIENT_CALL: ();
}

/// The provenance a note is stored with, decided by who is writing it rather
/// than assumed. The brain's own work (a queue job, a chat turn, a sub-agent:
/// anything under a `ROUTE_CONTEXT`) is `synthesis_inference`, and may not
/// claim `user_input` even when a model passes it, because a model chose the
/// words. Only an outside MCP client's call, with no route context of its own,
/// is `user_input`; it may pass any value. Anything else unscoped is brain code
/// and defaults to `synthesis_inference`. Until 2026-10-08 `store_note`
/// assumed `user_input`, and 11,513 machine-written notes said a person wrote
/// them.
pub fn effective_provenance(requested: Option<ProvenanceFlag>) -> ProvenanceFlag {
    let brain_work = crate::services::routing::ROUTE_CONTEXT
        .try_with(|_| ())
        .is_ok();
    let client = CLIENT_CALL.try_with(|_| ()).is_ok();
    match requested {
        Some(ProvenanceFlag::UserInput) if brain_work || !client => {
            ProvenanceFlag::SynthesisInference
        }
        Some(p) => p,
        None if client && !brain_work => ProvenanceFlag::UserInput,
        None => ProvenanceFlag::SynthesisInference,
    }
}

/// The exclusion in scope for this task, if any.
pub(crate) fn current_retrieval_exclude() -> Option<RetrievalExclude> {
    RETRIEVAL_EXCLUDE
        .try_with(|e| e.clone())
        .ok()
        .filter(|e| !e.is_empty())
}

/// A source note referenced by a structured reasoning result.
pub struct SourceRef {
    pub note_id: String,
    /// First 120 characters of the note content.
    pub preview: String,
}

/// Structured output from `reason_structured()`.
pub struct ReasonOutput {
    pub answer: String,
    pub sources: Vec<SourceRef>,
    pub confidence: f64,
    pub caveats: Vec<String>,
    pub follow_up_questions: Vec<String>,
    pub inferences: Vec<String>,
    pub gaps: Vec<String>,
    pub inference_note_id: Option<String>,
    /// Counter-arguments from the adversarial critic pass (empty when `run_critic=false`).
    pub critic_counter_arguments: Vec<String>,
    /// True when the model never produced parseable JSON and `answer` is raw prose.
    ///
    /// Everything else on this struct is then a **placeholder, not a measurement**:
    /// `confidence` was not reported, and empty `caveats`/`gaps` mean "the model
    /// never answered the question" — not "the model declared no limits". Consumers
    /// must not read those fields as the model's own assessment when this is set.
    pub structured_output_failed: bool,
}

/// Service for managing general knowledge (RAG).
pub struct KnowledgeService {
    pub(crate) neo4j: Neo4jClient,
    pub(crate) llm: Option<Arc<dyn LlmProvider>>,
    pub(crate) snapshot_svc: Option<Arc<SnapshotService>>,
    pub(crate) auto_snapshot: bool,
    pub(crate) auto_snapshot_prune: bool,
}

/// Reserved envelope key carrying rows destined for the PERSON, not the model.
/// Lifted into `ToolCallResult::bulk` and stripped by the skill layer.
pub const BULK_ROWS_KEY: &str = "__bulk_rows";

impl KnowledgeService {
    /// Create a new knowledge service.
    pub fn new(neo4j: Neo4jClient, llm: Option<Arc<dyn LlmProvider>>) -> Self {
        Self {
            neo4j,
            llm,
            snapshot_svc: None,
            auto_snapshot: false,
            auto_snapshot_prune: false,
        }
    }

    /// Attach a snapshot service for auto-snapshotting before major operations.
    pub fn with_snapshot(
        mut self,
        svc: Arc<SnapshotService>,
        auto_consolidate: bool,
        auto_prune: bool,
    ) -> Self {
        self.snapshot_svc = Some(svc);
        self.auto_snapshot = auto_consolidate;
        self.auto_snapshot_prune = auto_prune;
        self
    }

    // =========================================================================
    // Private helpers
    // =========================================================================
}

#[cfg(test)]
mod provenance_tests {
    use super::*;
    use crate::services::routing::{DataClass, Purpose, ROUTE_CONTEXT, RouteContext};
    use ProvenanceFlag::*;

    fn brain() -> RouteContext {
        RouteContext::new(Purpose::Background, DataClass::Private)
    }

    #[tokio::test]
    async fn a_client_call_is_user_input_and_may_choose() {
        let got = CLIENT_CALL
            .scope((), async {
                (
                    effective_provenance(None),
                    effective_provenance(Some(UserInput)),
                    effective_provenance(Some(SynthesisInference)),
                )
            })
            .await;
        assert_eq!(got, (UserInput, UserInput, SynthesisInference));
    }

    #[tokio::test]
    async fn the_brains_own_work_never_claims_user_input() {
        let got = ROUTE_CONTEXT
            .scope(brain(), async {
                (
                    effective_provenance(None),
                    effective_provenance(Some(UserInput)),
                )
            })
            .await;
        assert_eq!(got, (SynthesisInference, SynthesisInference));
        // A client's call that runs an agent: the agent's writes are the brain's.
        let nested = CLIENT_CALL
            .scope(
                (),
                ROUTE_CONTEXT.scope(brain(), async { effective_provenance(None) }),
            )
            .await;
        assert_eq!(nested, SynthesisInference);
    }

    #[test]
    fn unscoped_brain_code_defaults_to_synthesis_and_keeps_explicit_values() {
        assert_eq!(effective_provenance(None), SynthesisInference);
        assert_eq!(effective_provenance(Some(UserInput)), SynthesisInference);
        assert_eq!(effective_provenance(Some(SystemRecord)), SystemRecord);
        assert_eq!(effective_provenance(Some(CoreTraining)), CoreTraining);
    }
}
