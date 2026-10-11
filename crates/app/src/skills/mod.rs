pub mod agent;
pub mod agent_runner;
pub mod claims;
pub mod codebase;
pub mod dev_docs;
pub mod dynamic;
pub mod exec;
pub mod http;
pub mod knowledge;
pub mod mcp_client;
pub mod media;
pub mod model;
pub mod query;
pub mod relations;
pub mod scheduler;
pub mod search;
pub mod search_query;
pub mod serde_flex;
pub mod task;
#[cfg(test)]
pub mod test_helpers;
pub mod todo;
pub mod wiki;
pub mod working_memory;

pub use agent::AgentSkill;
pub use agent_runner::AgentRunnerSkill;
pub use codebase::CodebaseSkill;
pub use dynamic::DynamicSkill;
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
