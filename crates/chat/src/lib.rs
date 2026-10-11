//! Chat: one round loop for every provider (the Anthropic, host-Ollama and
//! OpenAI-wire adapters), synthesis, delegation, and the sub-agent runner that
//! shares the loop. Split out of the app crate in CORE_AND_PLUGINS_PLAN 6f;
//! the app re-exports `chat` at `clients::chat` and `agent_runner` at
//! `services::agent_runner`.

pub mod agent_runner;
pub mod chat;

pub use agent_runner::{
    AgentBudget, AgentFindings, AgentRunner, AgentSelection, SubagentLoopRunner,
};
pub use chat::{ChatEvent, ChatHistoryMessage, ChatRequest, ChatService};
