use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};

use super::{LlmProvider, LlmProviderError, ProviderConfig};
use crate::services::llm::{ChatMessage, LlmResponse};

pub struct OllamaProvider {
    client: Client,
    config: ProviderConfig,
}

impl OllamaProvider {
    pub fn new(config: ProviderConfig) -> Self {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .unwrap_or_else(|_| Client::new());

        Self { client, config }
    }
}

#[derive(Debug, Serialize)]
struct GenerateRequest<'a> {
    model: &'a str,
    prompt: &'a str,
    stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<&'a str>,
    options: GenerateOptions,
}

#[derive(Debug, Serialize)]
struct GenerateOptions {
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    num_predict: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct GenerateResponse {
    response: String,
    #[serde(default)]
    total_duration: Option<u64>,
    #[serde(default)]
    eval_count: Option<u32>,
}

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: &'a [ChatMessage],
    stream: bool,
    options: GenerateOptions,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    message: ChatMessage,
    #[serde(default)]
    total_duration: Option<u64>,
    #[serde(default)]
    eval_count: Option<u32>,
}

#[derive(Debug, Serialize)]
struct EmbeddingsRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Debug, Deserialize)]
struct EmbeddingsResponse {
    embedding: Vec<f32>,
}

#[async_trait]
impl LlmProvider for OllamaProvider {
    fn name(&self) -> &'static str {
        "ollama"
    }

    async fn generate(
        &self,
        prompt: &str,
        system: Option<&str>,
    ) -> Result<LlmResponse, LlmProviderError> {
        let base_url = self
            .config
            .base_url
            .as_deref()
            .unwrap_or("http://localhost:11434");
        let url = format!("{}/api/generate", base_url);

        let request = GenerateRequest {
            model: &self.config.model,
            prompt,
            stream: false,
            system,
            options: GenerateOptions {
                temperature: self.config.temperature,
                num_predict: self.config.max_tokens,
            },
        };

        let response = self.client.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "Status {}: {}",
                status, body
            )));
        }

        let gen_response: GenerateResponse = response.json().await?;

        Ok(LlmResponse {
            text: gen_response.response,
            duration_ns: gen_response.total_duration,
            tokens_evaluated: gen_response.eval_count,
        })
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmProviderError> {
        let base_url = self
            .config
            .base_url
            .as_deref()
            .unwrap_or("http://localhost:11434");
        let url = format!("{}/api/embeddings", base_url);

        let request = EmbeddingsRequest {
            model: &self.config.model,
            prompt: text,
        };

        let response = self.client.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "Status {}: {}",
                status, body
            )));
        }

        let emb_response: EmbeddingsResponse = response.json().await?;
        Ok(emb_response.embedding)
    }

    async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmResponse, LlmProviderError> {
        let base_url = self
            .config
            .base_url
            .as_deref()
            .unwrap_or("http://localhost:11434");
        let url = format!("{}/api/chat", base_url);

        let request = ChatRequest {
            model: &self.config.model,
            messages,
            stream: false,
            options: GenerateOptions {
                temperature: self.config.temperature,
                num_predict: self.config.max_tokens,
            },
        };

        let response = self.client.post(&url).json(&request).send().await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "Status {}: {}",
                status, body
            )));
        }

        let chat_response: ChatResponse = response.json().await?;

        Ok(LlmResponse {
            text: chat_response.message.content,
            duration_ns: chat_response.total_duration,
            tokens_evaluated: chat_response.eval_count,
        })
    }

    async fn health_check(&self) -> bool {
        let base_url = self
            .config
            .base_url
            .as_deref()
            .unwrap_or("http://localhost:11434");
        let url = format!("{}/api/tags", base_url);

        match self.client.get(&url).send().await {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }
}
