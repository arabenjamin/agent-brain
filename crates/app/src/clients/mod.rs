//! Client adapters.
//!
//! Each adapter in this module translates a specific client protocol or
//! interaction model into calls against [`crate::brain_core::BrainCore`].
//!
//! Current adapters:
//! - [`architecture`] — the live architecture: layout groups filled from the registry, routes and graph, plus snapshot diffs
//! - [`chat`] — conversational LLM loop for human-facing `/chat` SSE sessions
//! - [`entities`] — entity merge proposals, answers to them, and undo
//! - [`rest`] — REST API adapter for todos, scheduled tasks, and scheduler config
//! - [`search`] — semantic / keyword / id search, entity and id lookup for the HBI's Knowledge workspace
//! - [`system`] — one-call system health (every dependency probed) and the registry
//! - [`topics`] — the domains notes are classified into, the topic map, coverage
//! - [`work`] — lineage of tasks, jobs, schedules and todos for the HBI's Work workspace

#[cfg(feature = "http-transport")]
pub mod architecture;
pub mod chat;
#[cfg(feature = "http-transport")]
pub mod entities;
pub mod feed;
pub mod items;
#[cfg(feature = "http-transport")]
pub mod rest;
#[cfg(feature = "http-transport")]
pub mod search;
#[cfg(feature = "http-transport")]
pub mod system;
#[cfg(feature = "http-transport")]
pub mod topics;
#[cfg(feature = "http-transport")]
pub mod work;

pub use chat::{ChatEvent, ChatHistoryMessage, ChatRequest, ChatService};
#[cfg(feature = "http-transport")]
pub use rest::RestAdapter;
