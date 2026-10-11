//! The core's own tools beyond retrieval: knowledge (store, recall, reason),
//! tasks and todos, working memory, agents and the sub-agent runner, the
//! scheduler and workflows, models, dev notes, and the sandboxes. Split out of
//! the app crate in CORE_AND_PLUGINS_PLAN 6f; the app re-exports each module
//! at its old `skills::` path.

pub mod agent;
pub mod agent_runner;
pub mod dev_docs;
pub mod exec;
pub mod knowledge;
pub mod model;
pub mod scheduler;
pub mod task;
#[cfg(test)]
pub mod test_helpers;
pub mod todo;
pub mod working_memory;

pub use agent::AgentSkill;
pub use agent_runner::AgentRunnerSkill;
pub use exec::ExecSkill;
pub use knowledge::KnowledgeSkill;
pub use task::TaskSkill;
pub use working_memory::WorkingMemorySkill;
