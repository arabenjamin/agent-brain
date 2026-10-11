//! ModelSkill — the LLM budget view. The GPU lease is driven over REST
//! (`/api/gpu-lease`, `workspace/finetune/scripts/gpu_lease.sh`).

use async_trait::async_trait;
use serde_json::{Value, json};

use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

pub struct ModelSkill;

impl ModelSkill {
    // =========================================================================
    // Tool definitions
    // =========================================================================

    // list_models is served by GET /api/models (REST API)

    fn get_llm_budget_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_llm_budget".to_string(),
            description: "The live LLM budget per provider: billing and data policy, declared \
                limits and spend caps, today's requests/tokens/spend (UTC day) and this month's \
                spend, headroom left, and per-model cooldowns with their reasons and the last \
                rate-limit headers seen. Use it to answer why a model was skipped or how much \
                free-tier quota is left. The historical ledger is `model_usage` (duckdb_query)."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================
}

#[async_trait]
impl Skill for ModelSkill {
    fn name(&self) -> &str {
        "Model Management"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::get_llm_budget_def()]
    }

    async fn execute(&self, name: &str, _args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "get_llm_budget" => Some(ToolCallResult::success_json(
                agent_brain_llm::budget::snapshot_json(),
            )),
            _ => None,
        }
    }
}
