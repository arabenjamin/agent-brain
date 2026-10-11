//! The LLM stack: clients and wire providers, the provider registry, the
//! routing policy (may this run?), the budget (is there headroom?), the gate
//! (how many at once?), model selection, and the GPU lease. Split out of the
//! app crate in CORE_AND_PLUGINS_PLAN 6f; the app re-exports each module at
//! its old `services::` path.

pub mod budget;
pub mod ctx;
pub mod gpu_lease;
pub mod llm;
pub mod llm_providers;
pub mod model_config;
pub mod model_router;
pub mod provider_gate;
pub mod provider_registry;
pub mod routing;
pub mod shared_llm;
pub mod traits;

pub use llm::{LlmClient, LlmConfig, LlmProviderType};
pub use model_config::ModelCatalog;
pub use shared_llm::SharedLlm;
pub use traits::LlmProvider;
