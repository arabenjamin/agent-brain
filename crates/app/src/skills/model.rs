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

    fn list_models_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_models".to_string(),
            description: "List all models in the catalog with their capabilities and cost."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn use_model_def() -> ToolDefinition {
        ToolDefinition {
            name: "use_model".to_string(),
            description: "Switch the active LLM provider for the current session.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "provider": {
                        "type": "string",
                        "enum": ["Ollama", "Anthropic", "Gemini"],
                        "description": "Provider to switch to."
                    },
                    "model": {
                        "type": "string",
                        "description": "Model name (e.g. 'claude-haiku-4-5-20251001')."
                    },
                    "api_key": {
                        "type": "string",
                        "description": "Optional API key override."
                    }
                },
                "required": ["provider"]
            }),
        }
    }

    fn select_model_def() -> ToolDefinition {
        ToolDefinition {
            name: "select_model".to_string(),
            description: "Select the cheapest catalog model satisfying the given capability and cost constraints.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "required_capabilities": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "All capabilities the selected model must have."
                    },
                    "provider_hint": {
                        "type": "string",
                        "description": "Restrict selection to a specific provider ('ollama', 'anthropic', or 'gemini')."
                    },
                    "max_cost_per_1k": {
                        "type": "number",
                        "description": "Maximum combined (input + output) cost per 1,000 tokens in USD."
                    }
                },
                "required": ["required_capabilities"]
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

    async fn handle_list_models(&self) -> ToolCallResult {
        use crate::services::LlmProviderType;

        let config = self.llm_config.read().await;
        let active_provider = config
            .as_ref()
            .map(|c| c.provider.to_string())
            .unwrap_or_else(|| "None".to_string());
        let active_model = config.as_ref().map(|c| c.model.clone()).unwrap_or_default();
        drop(config);

        let registered = if let Some(ref db) = self.telemetry {
            match db.list_models() {
                Ok(models) => Value::Array(models),
                Err(e) => json!({ "error": e.to_string() }),
            }
        } else {
            json!([])
        };

        let response = json!({
            "active_provider": active_provider,
            "active_model":    active_model,
            "available_providers": [
                { "name": "Ollama (local)", "type": LlmProviderType::Ollama.to_string(),      "cost": "free" },
                { "name": "Ollama Cloud",   "type": LlmProviderType::OllamaCloud.to_string(), "cost": "usage-based" },
                { "name": "Anthropic",      "type": LlmProviderType::Anthropic.to_string(),   "cost": "paid" },
                { "name": "Gemini",         "type": LlmProviderType::Gemini.to_string(),      "cost": "paid" },
            ],
            "catalog_models": registered,
        });

        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
    }

    async fn handle_use_model(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            provider: String,
            model: Option<String>,
            api_key: Option<String>,
        }
        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        use crate::services::LlmProviderType;

        let mut config = self.llm_config.write().await;
        let base = config.as_ref().cloned().unwrap_or_default();

        let mut new_config = match input.provider.to_lowercase().as_str() {
            "ollama" => base.with_provider(LlmProviderType::Ollama),
            "ollama-cloud" | "ollamacloud" => base.with_provider(LlmProviderType::OllamaCloud),
            "anthropic" => base.with_provider(LlmProviderType::Anthropic),
            "gemini" => base.with_provider(LlmProviderType::Gemini),
            _ => {
                return ToolCallResult::error(
                    "Invalid provider. Use ollama, ollama-cloud, anthropic, or gemini.".to_string(),
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
        ToolCallResult::success_text(format!("Switched to provider: {}", input.provider))
    }

    async fn handle_select_model(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            required_capabilities: Vec<String>,
            provider_hint: Option<String>,
            max_cost_per_1k: Option<f64>,
        }
        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let Some(ref db) = self.telemetry else {
            return ToolCallResult::error("Telemetry (DuckDB) not available".to_string());
        };

        match db.select_models(
            &input.required_capabilities,
            input.provider_hint.as_deref(),
            input.max_cost_per_1k,
        ) {
            Ok(models) if !models.is_empty() => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "selected": true,
                    "model": models[0],
                    "candidates": models.len(),
                }))
                .unwrap(),
            ),
            Ok(_) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "selected": false,
                    "message": "No catalog model satisfies the given requirements.",
                    "required_capabilities": input.required_capabilities,
                    "provider_hint":         input.provider_hint,
                    "max_cost_per_1k":       input.max_cost_per_1k,
                }))
                .unwrap(),
            ),
            Err(e) => ToolCallResult::error(format!("select_models failed: {}", e)),
        }
    }

    async fn handle_reload_models(&self) -> ToolCallResult {
        let Some(ref db) = self.telemetry else {
            return ToolCallResult::error("Telemetry (DuckDB) not available".to_string());
        };

        let catalog = ModelCatalog::load_or_default(&self.catalog_path);
        match catalog.sync_to_duckdb(db) {
            Ok(count) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "reloaded": true,
                    "models_loaded": count,
                    "path": self.catalog_path.display().to_string(),
                }))
                .unwrap(),
            ),
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
            Self::list_models_def(),
            Self::use_model_def(),
            Self::select_model_def(),
            Self::reload_models_def(),
        ]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "list_models" => Some(self.handle_list_models().await),
            "use_model" => Some(self.handle_use_model(args).await),
            "select_model" => Some(self.handle_select_model(args).await),
            "reload_models" => Some(self.handle_reload_models().await),
            _ => None,
        }
    }
}
