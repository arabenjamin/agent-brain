//! agent-brain-protocol — shared protocol types for the Agent Brain MCP server.
//!
//! This crate contains pure data types and traits that are used by both the
//! `services/` layer and the `mcp/` transport layer, breaking the circular
//! dependency between those two modules.

pub mod types;
pub mod skill;
pub mod tool_handler;
pub mod sse_notifier;

pub use types::*;
pub use skill::Skill;
pub use tool_handler::ToolHandlerTrait;
pub use sse_notifier::SseNotifier;
