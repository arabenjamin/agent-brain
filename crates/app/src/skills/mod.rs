pub mod agent;
pub mod context;
pub mod dynamic;
pub mod knowledge;
pub mod model;
pub mod procedure;
pub mod resource;
pub mod scheduler;
pub mod search;
pub mod sleep;
pub mod task;
pub mod working_memory;
pub mod ws;

pub use agent::AgentSkill;
pub use dynamic::DynamicSkill;
pub use knowledge::KnowledgeSkill;
pub use procedure::ProcedureSkill;
pub use search::SearchSkill;
pub use sleep::SleepSkill;
pub use task::TaskSkill;
pub use working_memory::WorkingMemorySkill;

// Re-export the Skill trait from the protocol crate so all skill implementations
// continue to work with `use crate::skills::Skill`.
pub use agent_brain_protocol::Skill;
