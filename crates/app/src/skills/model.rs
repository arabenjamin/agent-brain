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
            let provider_type = match provider_str.to_lowercase().as_str() {
                "anthropic" => LlmProviderType::Anthropic,
                "gemini" => LlmProviderType::Gemini,
                "ollama-cloud" | "ollamacloud" => LlmProviderType::OllamaCloud,
                _ => LlmProviderType::Ollama,
            };
            let model_name = best
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();

            let mut config = self.llm_config.write().await;
            let base = config.as_ref().cloned().unwrap_or_default();
            *config = Some(
                base.with_provider(provider_type)
                    .with_model(model_name.clone()),
            );

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
            "anthropic" => base.with_provider(LlmProviderType::Anthropic),
            "gemini" => base.with_provider(LlmProviderType::Gemini),
            _ => {
                return ToolCallResult::error(
                    "Invalid provider. Use Ollama, OllamaCloud, Anthropic, or Gemini.".to_string(),
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

    async fn handle_reload_models(&self) -> ToolCallResult {
        let Some(ref db) = self.telemetry else {
            return ToolCallResult::error("Telemetry (DuckDB) not available".to_string());
        };

        let catalog = ModelCatalog::load_or_default(&self.catalog_path);
        match catalog.sync_to_duckdb(db) {
            Ok(count) => ToolCallResult::success_json(json!({
                "reloaded": true,
                "models_loaded": count,
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
        vec![Self::use_model_def(), Self::reload_models_def()]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "use_model" => Some(self.handle_use_model(args).await),
            "reload_models" => Some(self.handle_reload_models().await),
            _ => None,
        }
    }
}
