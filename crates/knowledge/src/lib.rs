//! Knowledge: storing notes, hybrid retrieval, reasoning over what is stored,
//! consolidation and maintenance, plus the store traits skills depend on and
//! the retrieval eval. Split out of the app crate in
//! CORE_AND_PLUGINS_PLAN 6f; the app re-exports each module at its old
//! `services::` path.

pub mod knowledge;
pub mod retrieval_eval;
pub mod traits;
pub mod web_result;

pub use knowledge::{KnowledgeService, ReasonOutput, SourceRef};
pub use traits::{KnowledgeStore, LlmProvider};
