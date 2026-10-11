// The skills live in `agent-brain-retrieve` and `agent-brain-skills`
// (CORE_AND_PLUGINS_PLAN 6f) and keep their `skills::` paths here. The plugin
// tools are in `agent-brain-host` with the plugin machinery.
pub use agent_brain_retrieve::{
    http, mcp_client, query, search, search_engines, search_query, serde_flex,
};
pub use agent_brain_skills::{
    agent, agent_runner, knowledge, model, scheduler, task, todo, working_memory,
};

pub use agent_brain_host::{bundle_tools, plugin_manager};

pub use agent::AgentSkill;
pub use agent_runner::AgentRunnerSkill;
pub use http::HttpSkill;
pub use knowledge::KnowledgeSkill;
pub use query::QuerySkill;
pub use search::SearchSkill;
pub use task::TaskSkill;
pub use working_memory::WorkingMemorySkill;

// Re-export the Skill trait from the protocol crate so all skill implementations
// continue to work with `use crate::skills::Skill`.
pub use agent_brain_protocol::Skill;
