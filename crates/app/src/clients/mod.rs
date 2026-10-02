//! Client adapters.
//!
//! Each adapter in this module translates a specific client protocol or
//! interaction model into calls against [`crate::brain_core::BrainCore`].
//!
//! Current adapters:
//! - [`chat`] — conversational LLM loop for human-facing `/chat` SSE sessions
//! - [`rest`] — REST API adapter for todos, scheduled tasks, and scheduler config

pub mod chat;
#[cfg(feature = "http-transport")]
pub mod rest;

pub use chat::{ChatEvent, ChatHistoryMessage, ChatRequest, ChatService};
#[cfg(feature = "http-transport")]
pub use rest::RestAdapter;
