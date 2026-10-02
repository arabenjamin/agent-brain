//! The `Skill` trait — a collection of MCP tools providing specific capabilities.

use async_trait::async_trait;
use serde_json::Value;

use crate::types::{ToolCallResult, ToolDefinition};

/// A Skill is a collection of Tools that provide specific capabilities.
#[async_trait]
pub trait Skill: Send + Sync {
    /// Get the name of the skill.
    fn name(&self) -> &str;

    /// Get the list of tools provided by this skill.
    fn tools(&self) -> Vec<ToolDefinition>;

    /// Execute a tool by name with arguments.
    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult>;
}

/// One skill instance can back both the tool registry (`tools/list`) and the
/// tool handler (`tools/call`): each holds an `Arc` of the same skill rather
/// than a separately constructed copy that can drift from it.
#[async_trait]
impl<T: Skill + ?Sized> Skill for std::sync::Arc<T> {
    fn name(&self) -> &str {
        (**self).name()
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        (**self).tools()
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        (**self).execute(tool_name, arguments).await
    }
}
