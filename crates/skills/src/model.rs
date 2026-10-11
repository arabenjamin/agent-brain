//! ModelSkill — the LLM budget view and the GPU lease (both process-wide services).

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

    fn gpu_lease_def() -> ToolDefinition {
        ToolDefinition {
            name: "gpu_lease".to_string(),
            description: "Lease the GPU to a job that needs all of it (fine-tuning). While held, \
                local models are refused, local-pinned background jobs wait, embeddings pause \
                (search falls back to BM25) and the brain runs on cloud providers; on release, \
                notes stored meanwhile are embedded and linked. Leases expire (default 12 h, \
                max 72 h); acquire again as the same holder to extend. Actions: status, \
                acquire (holder, reason, ttl_hours), release (holder, or force)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["status", "acquire", "release"] },
                    "holder": { "type": "string", "description": "Who holds the GPU, e.g. finetune." },
                    "reason": { "type": "string" },
                    "ttl_hours": { "type": "integer", "minimum": 1, "maximum": 72 },
                    "force": { "type": "boolean", "description": "Release a lease held by someone else." }
                },
                "required": ["action"]
            }),
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
        vec![Self::get_llm_budget_def(), Self::gpu_lease_def()]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "get_llm_budget" => Some(ToolCallResult::success_json(
                agent_brain_llm::budget::snapshot_json(),
            )),
            "gpu_lease" => Some(
                match agent_brain_llm::gpu_lease::handle(args.unwrap_or_else(|| json!({}))).await {
                    Ok(v) => ToolCallResult::success_json(v),
                    Err(e) => ToolCallResult::error(e),
                },
            ),
            _ => None,
        }
    }
}
