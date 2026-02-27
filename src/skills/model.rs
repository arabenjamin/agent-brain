//! ModelSkill — runtime LLM provider switching and model registry.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::RwLock;

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::models::ModelSpec;
use crate::repository::Neo4jClient;
use crate::services::model_selector::ModelSelector;
use crate::services::LlmConfig;
use crate::skills::Skill;

pub struct ModelSkill {
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
    neo4j: Option<Neo4jClient>,
}

impl ModelSkill {
    pub fn new(llm_config: Arc<RwLock<Option<LlmConfig>>>, neo4j: Option<Neo4jClient>) -> Self {
        Self { llm_config, neo4j }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    fn list_models_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_models".to_string(),
            description: "List available LLM providers and all registered model specs.".to_string(),
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
                        "description": "Model name to use (e.g. 'claude-haiku-4-5-20251001')."
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

    fn register_model_def() -> ToolDefinition {
        ToolDefinition {
            name: "register_model".to_string(),
            description: "Register a model specification in the knowledge graph for intelligent selection.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "name": {
                        "type": "string",
                        "description": "Model ID as used by the provider (e.g. 'claude-haiku-4-5-20251001')."
                    },
                    "provider": {
                        "type": "string",
                        "enum": ["ollama", "anthropic", "gemini"],
                        "description": "Provider name."
                    },
                    "cost_per_1k_tokens_input": {
                        "type": "number",
                        "description": "Input token cost in USD per 1,000 tokens."
                    },
                    "cost_per_1k_tokens_output": {
                        "type": "number",
                        "description": "Output token cost in USD per 1,000 tokens."
                    },
                    "context_window": {
                        "type": "integer",
                        "description": "Maximum context window in tokens."
                    },
                    "capabilities": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Capability tags, e.g. ['reasoning', 'code', 'fast', 'vision']."
                    }
                },
                "required": ["name", "provider", "cost_per_1k_tokens_input", "cost_per_1k_tokens_output", "context_window", "capabilities"]
            }),
        }
    }

    fn select_model_def() -> ToolDefinition {
        ToolDefinition {
            name: "select_model".to_string(),
            description: "Select the cheapest registered model that satisfies given capability and cost requirements.".to_string(),
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

    fn get_model_stats_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_model_stats".to_string(),
            description: "Get AgentJob usage statistics for a specific model or provider hint.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "model": {
                        "type": "string",
                        "description": "Model name or provider_hint string to look up."
                    }
                },
                "required": ["model"]
            }),
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
        let active_model = config
            .as_ref()
            .map(|c| c.model.clone())
            .unwrap_or_default();
        drop(config);

        let providers = vec![
            json!({ "name": "Ollama", "type": LlmProviderType::Ollama.to_string(), "cost": "free (local)" }),
            json!({ "name": "Anthropic", "type": LlmProviderType::Anthropic.to_string(), "cost": "paid" }),
            json!({ "name": "Gemini", "type": LlmProviderType::Gemini.to_string(), "cost": "paid" }),
        ];

        // Include registered model specs if Neo4j is available.
        let registered = if let Some(ref neo4j) = self.neo4j {
            match neo4j.list_model_specs().await {
                Ok(specs) => serde_json::to_value(specs).unwrap_or(json!([])),
                Err(e) => json!({ "error": e.to_string() }),
            }
        } else {
            json!([])
        };

        let response = json!({
            "active_provider": active_provider,
            "active_model": active_model,
            "available_providers": providers,
            "registered_models": registered,
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

        let mut new_config = match input.provider.as_str() {
            "Ollama" => base.with_provider(LlmProviderType::Ollama),
            "Anthropic" => base.with_provider(LlmProviderType::Anthropic),
            "Gemini" => base.with_provider(LlmProviderType::Gemini),
            _ => return ToolCallResult::error("Invalid provider. Use Ollama, Anthropic, or Gemini.".to_string()),
        };

        if let Some(model) = input.model {
            new_config = new_config.with_model(model);
        }
        if let Some(api_key) = input.api_key {
            new_config = new_config.with_api_key(api_key);
        }

        *config = Some(new_config);
        ToolCallResult::success_text(format!(
            "Switched to provider: {}",
            input.provider
        ))
    }

    async fn handle_register_model(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            name: String,
            provider: String,
            cost_per_1k_tokens_input: f64,
            cost_per_1k_tokens_output: f64,
            context_window: u32,
            capabilities: Vec<String>,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let valid_providers = ["ollama", "anthropic", "gemini"];
        if !valid_providers.contains(&input.provider.as_str()) {
            return ToolCallResult::error(format!(
                "Invalid provider '{}'. Must be one of: ollama, anthropic, gemini",
                input.provider
            ));
        }

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        let spec = ModelSpec {
            id: String::new(), // assigned by repo
            name: input.name.clone(),
            provider: input.provider.clone(),
            cost_per_1k_tokens_input: input.cost_per_1k_tokens_input,
            cost_per_1k_tokens_output: input.cost_per_1k_tokens_output,
            context_window: input.context_window,
            capabilities: input.capabilities.clone(),
            created_at: String::new(), // assigned by repo
        };

        match neo4j.register_model_spec(&spec).await {
            Ok(id) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "registered": true,
                    "model_id": id,
                    "name": input.name,
                    "provider": input.provider,
                    "capabilities": input.capabilities,
                }))
                .unwrap(),
            ),
            Err(e) => ToolCallResult::error(format!("Failed to register model: {}", e)),
        }
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

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        let selector = ModelSelector::new(neo4j.clone());
        let result = selector
            .select(
                &input.required_capabilities,
                input.provider_hint.as_deref(),
                input.max_cost_per_1k,
            )
            .await;

        match result {
            Some(spec) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "selected": true,
                    "model": spec.name,
                    "provider": spec.provider,
                    "cost_per_1k_input": spec.cost_per_1k_tokens_input,
                    "cost_per_1k_output": spec.cost_per_1k_tokens_output,
                    "context_window": spec.context_window,
                    "capabilities": spec.capabilities,
                }))
                .unwrap(),
            ),
            None => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "selected": false,
                    "message": "No registered model satisfies the given requirements.",
                    "required_capabilities": input.required_capabilities,
                    "provider_hint": input.provider_hint,
                    "max_cost_per_1k": input.max_cost_per_1k,
                }))
                .unwrap(),
            ),
        }
    }

    async fn handle_get_model_stats(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            model: String,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        match neo4j.get_model_usage_stats(&input.model).await {
            Ok(stats) => {
                ToolCallResult::success_text(serde_json::to_string_pretty(&stats).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to get stats: {}", e)),
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
            Self::register_model_def(),
            Self::select_model_def(),
            Self::get_model_stats_def(),
        ]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "list_models" => Some(self.handle_list_models().await),
            "use_model" => Some(self.handle_use_model(args).await),
            "register_model" => Some(self.handle_register_model(args).await),
            "select_model" => Some(self.handle_select_model(args).await),
            "get_model_stats" => Some(self.handle_get_model_stats(args).await),
            _ => None,
        }
    }
}
