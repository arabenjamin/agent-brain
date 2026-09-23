//! ModelSkill — runtime LLM provider switching and model registry (DuckDB-backed).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::repository::TelemetryClient;
use crate::services::LlmConfig;
use crate::services::model_config::ModelCatalog;
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

pub struct ModelSkill {
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
    telemetry: Option<TelemetryClient>,
    /// Path to models.yaml, used for reload_models.
    catalog_path: PathBuf,
}

impl ModelSkill {
    pub fn new(
        llm_config: Arc<RwLock<Option<LlmConfig>>>,
        telemetry: Option<TelemetryClient>,
        catalog_path: PathBuf,
    ) -> Self {
        Self {
            llm_config,
            telemetry,
            catalog_path,
        }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    // list_models is served by GET /api/models (REST API)

    fn use_model_def() -> ToolDefinition {
        ToolDefinition {
            name: "use_model".to_string(),
            description: "Switch the active LLM provider, either explicitly or by auto-selecting \
                the cheapest catalog model that satisfies capability constraints. \
                Provide `provider` for an explicit switch; provide `required_capabilities` \
                to auto-select and switch in one call."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "provider": {
                        "type": "string",
                        "enum": ["Ollama", "OllamaCloud", "Anthropic", "Gemini"],
                        "description": "Provider to switch to (explicit mode)."
                    },
                    "model": {
                        "type": "string",
                        "description": "Model name for explicit switch (e.g. 'claude-haiku-4-5-20251001')."
                    },
                    "api_key": {
                        "type": "string",
                        "description": "Optional API key override."
                    },
                    "required_capabilities": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Auto-select mode: capabilities the chosen model must have."
                    },
                    "provider_hint": {
                        "type": "string",
                        "description": "Auto-select mode: restrict to a specific provider."
                    },
                    "max_cost_per_1k": {
                        "type": "number",
                        "description": "Auto-select mode: max combined cost per 1k tokens (USD)."
                    }
                }
            }),
        }
    }

    fn reload_models_def() -> ToolDefinition {
        ToolDefinition {
            name: "reload_models".to_string(),
            description: "Re-read models.yaml and sync into DuckDB without restarting the server."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn get_llm_budget_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_llm_budget".to_string(),
            description: "The live LLM budget per provider: billing and data policy, declared \
                limits and spend caps, today's requests/tokens/spend (UTC day) and this month's \
                spend, headroom left, and per-model cooldowns with their reasons and the last \
                rate-limit headers seen. Use it to answer why a model was skipped or how much \
                free-tier quota is left; get_usage is the historical ledger."
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

    fn get_usage_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_usage".to_string(),
            description: "Get model usage statistics: calls, success rate, token totals, cost, \
                and rate-limit hits per model. Pass `hours` to restrict to a recent window \
                (quota budgets are time-windowed — use hours=24 to see today's consumption \
                before choosing a cloud model). Omit `model` for all models."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "model": {
                        "type": "string",
                        "description": "Restrict stats to one model name (optional)."
                    },
                    "hours": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Restrict to the last N hours (optional; omit for all-time)."
                    }
                }
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_use_model(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            // Explicit-switch fields
            provider: Option<String>,
            model: Option<String>,
            api_key: Option<String>,
            // Auto-select fields
            required_capabilities: Option<Vec<String>>,
            provider_hint: Option<String>,
            max_cost_per_1k: Option<f64>,
        }
        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        use crate::services::LlmProviderType;

        // Auto-select mode: pick cheapest model satisfying capabilities, then switch.
        if let Some(ref caps) = input.required_capabilities {
            let Some(ref db) = self.telemetry else {
                return ToolCallResult::error(
                    "Telemetry (DuckDB) required for auto-select mode".to_string(),
                );
            };
            let candidates =
                match db.select_models(caps, input.provider_hint.as_deref(), input.max_cost_per_1k)
                {
                    Ok(c) => c,
                    Err(e) => return ToolCallResult::error(format!("select_models failed: {}", e)),
                };
            let Some(best) = candidates.into_iter().next() else {
                return ToolCallResult::success_json(json!({
                    "selected": false,
                    "message": "No catalog model satisfies the given requirements.",
                    "required_capabilities": caps,
                    "provider_hint": input.provider_hint,
                    "max_cost_per_1k": input.max_cost_per_1k,
                }));
            };

            // Parse provider from the selected ModelSpec.
            let provider_str = best
                .get("provider")
                .and_then(|v| v.as_str())
                .unwrap_or("ollama");
            let model_name = best
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            // The provider's model id, which can differ from the catalog key
            // (`gemma4-e4b` is served under its sidecar alias).
            let model_id = best
                .get("model")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .unwrap_or(&model_name)
                .to_string();

            // Endpoint and key come from the provider registry, so a selected
            // Mistral or OpenRouter model is called at its own API rather than
            // at whatever endpoint the previous config pointed to.
            let mut config = self.llm_config.write().await;
            let base = config.as_ref().cloned().unwrap_or_default();
            *config = Some(crate::services::model_router::config_for_catalog_entry(
                &base,
                provider_str,
                &model_id,
            ));

            return ToolCallResult::success_json(json!({
                "selected": true,
                "switched_to": { "provider": provider_str, "model": model_name },
            }));
        }

        // Explicit-switch mode.
        let Some(provider) = input.provider else {
            return ToolCallResult::error(
                "Provide either `provider` (explicit switch) or `required_capabilities` (auto-select).".to_string(),
            );
        };

        let mut config = self.llm_config.write().await;
        let base = config.as_ref().cloned().unwrap_or_default();

        let mut new_config = match provider.to_lowercase().as_str() {
            "ollama" => base.with_provider(LlmProviderType::Ollama),
            "ollama-cloud" | "ollamacloud" => base.with_provider(LlmProviderType::OllamaCloud),
            "llamacpp" | "llama-cpp" | "llama.cpp" => base.with_provider(LlmProviderType::LlamaCpp),
            "anthropic" => base.with_provider(LlmProviderType::Anthropic),
            "gemini" => base.with_provider(LlmProviderType::Gemini),
            _ => {
                return ToolCallResult::error(
                    "Invalid provider. Use Ollama, OllamaCloud, LlamaCpp, Anthropic, or Gemini."
                        .to_string(),
                );
            }
        };

        if let Some(model) = input.model {
            new_config = new_config.with_model(model);
        }
        if let Some(api_key) = input.api_key {
            new_config = new_config.with_api_key(api_key);
        }

        *config = Some(new_config);
        ToolCallResult::success_text(format!("Switched to provider: {}", provider))
    }

    async fn handle_get_usage(&self, args: Option<Value>) -> ToolCallResult {
        let Some(ref db) = self.telemetry else {
            return ToolCallResult::error("Telemetry (DuckDB) not available".to_string());
        };
        let args = args.unwrap_or_default();
        let model = args["model"].as_str();
        let hours = args["hours"].as_i64();
        match db.get_model_stats(model, hours) {
            Ok(stats) => ToolCallResult::success_json(stats),
            Err(e) => ToolCallResult::error(format!("get_usage failed: {}", e)),
        }
    }

    async fn handle_reload_models(&self) -> ToolCallResult {
        let Some(ref db) = self.telemetry else {
            return ToolCallResult::error("Telemetry (DuckDB) not available".to_string());
        };

        let catalog = ModelCatalog::load_or_default(&self.catalog_path);
        let providers = catalog.install_providers();
        match catalog.sync_to_duckdb(db) {
            Ok(count) => ToolCallResult::success_json(json!({
                "reloaded": true,
                "models_loaded": count,
                "providers_loaded": providers,
                "provider_issues": catalog.provider_issues(),
                "path": self.catalog_path.display().to_string(),
            })),
            Err(e) => ToolCallResult::error(format!("reload_models failed: {}", e)),
        }
    }
}

#[async_trait]
impl Skill for ModelSkill {
    fn name(&self) -> &str {
        "Model Management"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::use_model_def(),
            Self::reload_models_def(),
            Self::get_usage_def(),
            Self::get_llm_budget_def(),
            Self::gpu_lease_def(),
        ]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "use_model" => Some(self.handle_use_model(args).await),
            "reload_models" => Some(self.handle_reload_models().await),
            "get_usage" => Some(self.handle_get_usage(args).await),
            "get_llm_budget" => Some(ToolCallResult::success_json(
                crate::services::budget::snapshot_json(),
            )),
            "gpu_lease" => Some(
                match crate::services::gpu_lease::handle(args.unwrap_or_else(|| json!({}))).await {
                    Ok(v) => ToolCallResult::success_json(v),
                    Err(e) => ToolCallResult::error(e),
                },
            ),
            _ => None,
        }
    }
}
