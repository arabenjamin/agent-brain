//! Work: the durable job queue and its tool dispatch, workflows (validated at
//! the write boundary and seeded from bundles), agent profiles and the context
//! they build, the task and working-memory stores, and the MCP client. Split
//! out of the app crate in CORE_AND_PLUGINS_PLAN 6f; the app re-exports each
//! module at its old path.

pub mod context_builder;
pub mod cypher_guard;
pub mod events;
pub mod graph_schema;
pub mod mcp_client;
pub mod profiles;
pub mod programs;
pub mod queue;
pub mod store_impls;
pub mod tools;
pub mod validation;
pub mod workflow_seeder;
pub mod workflows;

pub use context_builder::ContextBuilderService;
pub use queue::{ChainStep, QueueService, WorkerConfig};
pub use store_impls::{TaskStore, WorkingMemoryStore};
