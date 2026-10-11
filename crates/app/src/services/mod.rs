// Modules split into their own crates (CORE_AND_PLUGINS_PLAN 6f) keep their
// `services::` paths here.
pub use agent_brain_chat::agent_runner;
pub use agent_brain_graph::{architecture, clock, entity_resolution, snapshot, topics};
pub use agent_brain_host::{plugin_host, plugins, self_model};
pub use agent_brain_knowledge::{knowledge, retrieval_eval, traits, web_result};
pub use agent_brain_llm::{
    budget, gpu_lease, llm, llm_providers, model_config, model_router, provider_gate,
    provider_registry, routing, shared_llm,
};
pub use agent_brain_scheduler::{item_dispatch, items, repair, scheduler};
pub use agent_brain_work::{
    context_builder, cypher_guard, graph_schema, mcp_client, profiles, queue, store_impls,
    validation, workflow_seeder, workflows,
};

pub use agent_runner::{
    AgentBudget, AgentFindings, AgentRunner, AgentSelection, SubagentLoopRunner,
};
pub use context_builder::ContextBuilderService;
pub use knowledge::{KnowledgeService, ReasonOutput, SourceRef};
pub use llm::{LlmClient, LlmConfig, LlmProviderType};
pub use model_config::ModelCatalog;
pub use queue::{ChainStep, QueueService, WorkerConfig};
pub use scheduler::{SchedulerConfig, SchedulerService};
pub use shared_llm::SharedLlm;
pub use snapshot::SnapshotService;
pub use store_impls::{TaskStore, WorkingMemoryStore};
pub use traits::{KnowledgeStore, LlmProvider};
