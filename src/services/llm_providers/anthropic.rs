use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::services::llm::{ChatMessage, LlmResponse};
use super::{LlmProvider, LlmProviderError, ProviderConfig};

pub struct AnthropicProvider {
    client: Client,
    config: ProviderConfig,
}

impl AnthropicProvider {
    pub fn new(config: ProviderConfig) -> Self {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .unwrap_or_else(|_| Client::new());
        
        Self { client, config }
    }
}

#[derive(Debug, Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    messages: Vec<AnthropicMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    max_tokens: u32,
    temperature: f32,
}

#[derive(Debug, Serialize, Deserialize)]
struct AnthropicMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContent>,
    usage: AnthropicUsage,
}

#[derive(Debug, Deserialize)]
struct AnthropicContent {
    text: String,
}

#[derive(Debug, Deserialize)]
struct AnthropicUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &'static str {
        "anthropic"
    }

    async fn generate(&self, prompt: &str, system: Option<&str>) -> Result<LlmResponse, LlmProviderError> {
        let url = self.config.base_url.as_deref().unwrap_or("https://api.anthropic.com/v1/messages");
        let api_key = self.config.api_key.as_ref()
            .ok_or_else(|| LlmProviderError::InvalidConfig("Anthropic API key is missing".to_string()))?;

        let messages = vec![AnthropicMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
        }];

        let request = AnthropicRequest {
            model: &self.config.model,
            messages,
            system,
            max_tokens: self.config.max_tokens.unwrap_or(4096),
            temperature: self.config.temperature,
        };

        let response = self.client.post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "Anthropic API Error (Status {}): {}",
                status, body
            )));
        }

        let anthropic_res: AnthropicResponse = response.json().await?;
        let text = anthropic_res.content.first()
            .map(|c| c.text.clone())
            .unwrap_or_default();

        Ok(LlmResponse {
            text,
            duration_ns: None,
            tokens_evaluated: Some(anthropic_res.usage.input_tokens + anthropic_res.usage.output_tokens),
        })
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmProviderError> {
        Err(LlmProviderError::UnsupportedCapability("Anthropic does not currently offer a native embeddings API via Messages".to_string()))
    }

    async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmResponse, LlmProviderError> {
        let url = self.config.base_url.as_deref().unwrap_or("https://api.anthropic.com/v1/messages");
        let api_key = self.config.api_key.as_ref()
            .ok_or_else(|| LlmProviderError::InvalidConfig("Anthropic API key is missing".to_string()))?;

        let mut anthropic_messages = Vec::new();
        let mut system_prompt = None;

        for msg in messages {
            match msg.role.as_str() {
                "system" => system_prompt = Some(msg.content.as_str()),
                "user" | "assistant" => anthropic_messages.push(AnthropicMessage {
                    role: msg.role.clone(),
                    content: msg.content.clone(),
                }),
                _ => warn!("Unsupported role for Anthropic: {}", msg.role),
            }
        }

        let request = AnthropicRequest {
            model: &self.config.model,
            messages: anthropic_messages,
            system: system_prompt,
            max_tokens: self.config.max_tokens.unwrap_or(4096),
            temperature: self.config.temperature,
        };

        let response = self.client.post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "Anthropic API Error (Status {}): {}",
                status, body
            )));
        }

        let anthropic_res: AnthropicResponse = response.json().await?;
        let text = anthropic_res.content.first()
            .map(|c| c.text.clone())
            .unwrap_or_default();

        Ok(LlmResponse {
            text,
            duration_ns: None,
            tokens_evaluated: Some(anthropic_res.usage.input_tokens + anthropic_res.usage.output_tokens),
        })
    }

    async fn health_check(&self) -> bool {
        // Anthropic doesn't have a simple health endpoint without auth, 
        // so we assume true if the config has an API key.
        self.config.api_key.is_some()
    }
}
