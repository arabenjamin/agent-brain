//! OpenAI-compatible LLM provider.
//!
//! Works with any server that implements the OpenAI REST API, including:
//! - **vLLM** (`vllm serve <model>`, default http://localhost:8000)
//! - LM Studio, Jan.ai, LocalAI, llama.cpp server
//! - Groq, Together AI, Fireworks AI (with api_key)
//!
//! Endpoints used:
//! - `POST /v1/chat/completions` — chat and single-turn generation
//! - `POST /v1/embeddings`       — vector embeddings (model must support it)
//! - `GET  /health`              — liveness probe

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::services::llm::{ChatMessage, LlmResponse};
use super::{LlmProvider, LlmProviderError, ProviderConfig};

pub struct OpenAiCompatProvider {
    client: Client,
    config: ProviderConfig,
}

impl OpenAiCompatProvider {
    pub fn new(config: ProviderConfig) -> Self {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .unwrap_or_else(|_| Client::new());
        Self { client, config }
    }

    fn base_url(&self) -> &str {
        self.config.base_url.as_deref().unwrap_or("http://localhost:8000")
    }

    /// Add Authorization header if an API key is configured.
    fn maybe_auth(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        if let Some(key) = &self.config.api_key {
            builder.header("Authorization", format!("Bearer {}", key))
        } else {
            builder
        }
    }
}

// ─── Request / response shapes (OpenAI wire format) ──────────────────────────

#[derive(Debug, Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<OaiMessage>,
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    stream: bool,
}

#[derive(Debug, Serialize, Deserialize)]
struct OaiMessage {
    role: String,
    content: String,
}

#[derive(Debug, Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
    #[serde(default)]
    usage: Option<OaiUsage>,
}

#[derive(Debug, Deserialize)]
struct ChatChoice {
    message: OaiMessage,
}

#[derive(Debug, Deserialize)]
struct OaiUsage {
    #[serde(default)]
    completion_tokens: u32,
    #[serde(default)]
    prompt_tokens: u32,
}

#[derive(Debug, Serialize)]
struct EmbedRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Debug, Deserialize)]
struct EmbedResponse {
    data: Vec<EmbedData>,
}

#[derive(Debug, Deserialize)]
struct EmbedData {
    embedding: Vec<f32>,
}

// ─── LlmProvider impl ────────────────────────────────────────────────────────

#[async_trait]
impl LlmProvider for OpenAiCompatProvider {
    fn name(&self) -> &'static str {
        "vllm"
    }

    async fn generate(&self, prompt: &str, system: Option<&str>) -> Result<LlmResponse, LlmProviderError> {
        let mut messages = Vec::new();
        if let Some(sys) = system {
            messages.push(OaiMessage { role: "system".to_string(), content: sys.to_string() });
        }
        messages.push(OaiMessage { role: "user".to_string(), content: prompt.to_string() });
        self.chat_inner(messages).await
    }

    async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmResponse, LlmProviderError> {
        let oai_messages = messages.iter().map(|m| OaiMessage {
            role: m.role.clone(),
            content: m.content.clone(),
        }).collect();
        self.chat_inner(oai_messages).await
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmProviderError> {
        let url = format!("{}/v1/embeddings", self.base_url());
        let req = EmbedRequest { model: &self.config.model, input: text };

        let response = self.maybe_auth(self.client.post(&url))
            .json(&req)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "vLLM embeddings error ({}): {}",
                status, body
            )));
        }

        let resp: EmbedResponse = response.json().await?;
        resp.data.into_iter().next()
            .map(|d| d.embedding)
            .ok_or_else(|| LlmProviderError::GenerationFailed("Empty embedding response".to_string()))
    }

    async fn health_check(&self) -> bool {
        let url = format!("{}/health", self.base_url());
        match self.client.get(&url).send().await {
            Ok(r) => r.status().is_success(),
            Err(_) => {
                // Fall back to /v1/models — some servers don't expose /health
                let models_url = format!("{}/v1/models", self.base_url());
                match self.maybe_auth(self.client.get(&models_url)).send().await {
                    Ok(r) => r.status().is_success(),
                    Err(e) => {
                        warn!(error = %e, "vLLM health check failed");
                        false
                    }
                }
            }
        }
    }
}

impl OpenAiCompatProvider {
    async fn chat_inner(&self, messages: Vec<OaiMessage>) -> Result<LlmResponse, LlmProviderError> {
        let url = format!("{}/v1/chat/completions", self.base_url());

        let req = ChatRequest {
            model: &self.config.model,
            messages,
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            stream: false,
        };

        debug!(url = %url, model = %self.config.model, "OpenAI-compat chat request");

        let response = self.maybe_auth(self.client.post(&url))
            .json(&req)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "vLLM chat error ({}): {}",
                status, body
            )));
        }

        let chat_res: ChatResponse = response.json().await?;
        let text = chat_res.choices.into_iter().next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        let tokens = chat_res.usage.map(|u| u.prompt_tokens + u.completion_tokens);

        Ok(LlmResponse {
            text,
            duration_ns: None,
            tokens_evaluated: tokens,
        })
    }
}
