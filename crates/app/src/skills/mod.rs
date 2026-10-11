pub mod agent;
pub mod agent_runner;
pub mod bundle_tools;
pub mod dev_docs;
pub mod exec;
pub mod http;
pub mod knowledge;
pub mod mcp_client;
pub mod model;
pub mod plugin_manager;
pub mod query;
pub mod scheduler;
pub mod search;
pub mod search_engines;
pub mod search_query;
pub mod serde_flex;
pub mod task;
#[cfg(test)]
pub mod test_helpers;
pub mod todo;
pub mod working_memory;

pub use agent::AgentSkill;
pub use agent_runner::AgentRunnerSkill;
pub use exec::ExecSkill;
pub use http::HttpSkill;
pub use knowledge::KnowledgeSkill;
pub use query::QuerySkill;
pub use search::SearchSkill;
pub use task::TaskSkill;
pub use working_memory::WorkingMemorySkill;

// Re-export the Skill trait from the protocol crate so all skill implementations
// continue to work with `use crate::skills::Skill`.
pub use agent_brain_protocol::Skill;
