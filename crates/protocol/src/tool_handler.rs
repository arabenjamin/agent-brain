//! `ToolHandlerTrait` — the abstract interface used by services to execute tools.
//!
//! The concrete `ToolHandler` struct lives in `mcp/tools.rs` and implements this
//! trait.  Services hold `Arc<dyn ToolHandlerTrait>` so they do not depend on
//! the concrete MCP-level type.

use async_trait::async_trait;
use serde_json::Value;

use crate::types::ToolCallResult;

/// Abstract interface for executing MCP tools.
#[async_trait]
pub trait ToolHandlerTrait: Send + Sync {
    /// Execute a tool by name with the given arguments.
    async fn execute(&self, name: &str, arguments: Option<Value>) -> ToolCallResult;
}
