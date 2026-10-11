//! Client adapters.
//!
//! Each adapter translates a client protocol or interaction model into calls
//! against [`crate::brain_core::BrainCore`]. They live in their own crates
//! (CORE_AND_PLUGINS_PLAN 6f) and keep their `clients::` paths here:
//! - [`chat`] — conversational LLM loop for human-facing `/chat` SSE sessions
//!   (`agent-brain-chat`)
//! - the HBI's REST API (`agent-brain-api`): [`rest`] (shared plumbing, todos,
//!   workflows, scheduler config), [`entities`], [`feed`], [`items`],
//!   [`related`], [`search`], [`system`], [`topics`], [`work`]

#[cfg(feature = "http-transport")]
pub use agent_brain_api::{entities, feed, items, related, rest, search, system, topics, work};
pub use agent_brain_chat::chat;

pub use chat::{ChatEvent, ChatHistoryMessage, ChatRequest, ChatService};
#[cfg(feature = "http-transport")]
pub use rest::RestAdapter;
