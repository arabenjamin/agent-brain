pub mod agent;
pub mod dynamic;
pub mod knowledge;
pub mod model;
pub mod procedure;
pub mod scheduler;
pub mod search;
pub mod sleep;
pub mod task;
pub mod working_memory;

pub use agent::AgentSkill;
pub use dynamic::DynamicSkill;
pub use knowledge::KnowledgeSkill;
pub use procedure::ProcedureSkill;
pub use search::SearchSkill;
pub use sleep::SleepSkill;
pub use task::TaskSkill;
pub use working_memory::WorkingMemorySkill;

use async_trait::async_trait;
use serde_json::Value;

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};

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
