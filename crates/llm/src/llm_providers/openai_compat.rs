//! OpenAI-compatible LLM provider.
//!
//! Serves `LlmProviderType::OllamaCloud` (`https://ollama.com/v1/chat/completions`),
//! `LlmProviderType::LlamaCpp` (the `llama-server` sidecar) and
//! `LlmProviderType::OpenAiCompat` — any provider-registry entry of
//! `kind: openai_compat` (Mistral, OpenRouter, …), whose base URL and key the
//! registry supplies — and `LlmProviderType::Gemini`, through Google's
//! OpenAI-compatible API (CORE_AND_PLUGINS_PLAN 6b: measured against the native
//! `generateContent` API on 2026-10-11, same latency, same prompt/completion
//! token counts, thinking on in both).
//!
//! Endpoints used, under [`api_root`]:
//! - `POST {root}/chat/completions` — chat and single-turn generation
//! - `POST {root}/embeddings`       — vector embeddings (model must support it)
//! - `GET  {base}/health`, else `GET {root}/models` — liveness probe

use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use super::{LlmProvider, LlmProviderError, ProviderConfig};
use crate::llm::{ChatMessage, LlmProviderType, LlmResponse};

/// Google's OpenAI-compatible API root, used when a Gemini config names none.
pub const GEMINI_OPENAI_ROOT: &str = "https://generativelanguage.googleapis.com/v1beta/openai";

/// The versioned root an OpenAI-wire request path hangs off: `{base}/v1`, except
/// Gemini's, which is `/v1beta/openai` (no `/v1`). `default_base` is used when
/// the config names no base URL. Shared with the chat adapter
/// (`clients/chat/openai_loop.rs`), so both build the same URLs.
pub fn api_root(provider: LlmProviderType, base_url: Option<&str>, default_base: &str) -> String {
    match provider {
        LlmProviderType::Gemini => base_url.unwrap_or(GEMINI_OPENAI_ROOT).to_string(),
        _ => format!("{}/v1", base_url.unwrap_or(default_base)),
    }
}

pub struct OpenAiCompatProvider {
    client: Client,
    config: ProviderConfig,
    root: String,
}

impl OpenAiCompatProvider {
    pub fn new(config: ProviderConfig, provider: LlmProviderType) -> Self {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .unwrap_or_else(|_| Client::new());
        let root = api_root(
            provider,
            config.base_url.as_deref(),
            "http://localhost:8000",
        );
        Self {
            client,
            config,
            root,
        }
    }

    fn base_url(&self) -> &str {
        self.config
            .base_url
            .as_deref()
            .unwrap_or("http://localhost:8000")
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
        "openai-compat"
    }

    async fn generate(
        &self,
        prompt: &str,
        system: Option<&str>,
    ) -> Result<LlmResponse, LlmProviderError> {
        let mut messages = Vec::new();
        if let Some(sys) = system {
            messages.push(OaiMessage {
                role: "system".to_string(),
                content: sys.to_string(),
            });
        }
        messages.push(OaiMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
        });
        self.chat_inner(messages).await
    }

    async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmResponse, LlmProviderError> {
        let oai_messages = messages
            .iter()
            .map(|m| OaiMessage {
                role: m.role.clone(),
                content: m.content.clone(),
            })
            .collect();
        self.chat_inner(oai_messages).await
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmProviderError> {
        let text = super::prepare_embed_input(text)?;
        let url = format!("{}/embeddings", self.root);
        let req = EmbedRequest {
            model: &self.config.model,
            input: text,
        };

        let response = self
            .maybe_auth(self.client.post(&url))
            .json(&req)
            .send()
            .await?;

        crate::budget::observe_headers(
            &self.config.provider_name,
            &self.config.model,
            response.status().as_u16(),
            response.headers(),
        );

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "OpenAI-compat embeddings error ({}): {}",
                status, body
            )));
        }

        let resp: EmbedResponse = response.json().await?;
        resp.data
            .into_iter()
            .next()
            .map(|d| d.embedding)
            .ok_or_else(|| {
                LlmProviderError::GenerationFailed("Empty embedding response".to_string())
            })
    }

    async fn health_check(&self) -> bool {
        let url = format!("{}/health", self.base_url());
        match self.client.get(&url).send().await {
            Ok(r) => r.status().is_success(),
            Err(_) => {
                // Fall back to /v1/models — some servers don't expose /health
                let models_url = format!("{}/models", self.root);
                match self.maybe_auth(self.client.get(&models_url)).send().await {
                    Ok(r) => r.status().is_success(),
                    Err(e) => {
                        warn!(error = %e, "OpenAI-compat health check failed");
                        false
                    }
                }
            }
        }
    }
}

impl OpenAiCompatProvider {
    async fn chat_inner(&self, messages: Vec<OaiMessage>) -> Result<LlmResponse, LlmProviderError> {
        let url = format!("{}/chat/completions", self.root);

        let req = ChatRequest {
            model: &self.config.model,
            messages,
            temperature: self.config.temperature,
            max_tokens: self.config.max_tokens,
            stream: false,
        };

        debug!(url = %url, model = %self.config.model, "OpenAI-compat chat request");

        let response = self
            .maybe_auth(self.client.post(&url))
            .json(&req)
            .send()
            .await?;

        crate::budget::observe_headers(
            &self.config.provider_name,
            &self.config.model,
            response.status().as_u16(),
            response.headers(),
        );

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "OpenAI-compat chat error ({}): {}",
                status, body
            )));
        }

        let chat_res: ChatResponse = response.json().await?;
        let text = chat_res
            .choices
            .into_iter()
            .next()
            .map(|c| c.message.content)
            .unwrap_or_default();

        let tokens_in = chat_res.usage.as_ref().map(|u| u.prompt_tokens);
        let tokens_out = chat_res.usage.as_ref().map(|u| u.completion_tokens);
        let tokens = chat_res
            .usage
            .map(|u| u.prompt_tokens + u.completion_tokens);

        Ok(LlmResponse {
            text,
            duration_ns: None,
            tokens_evaluated: tokens,
            tokens_in,
            tokens_out,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gemini_root_has_no_v1_and_others_do() {
        assert_eq!(
            api_root(LlmProviderType::Gemini, None, "http://x"),
            GEMINI_OPENAI_ROOT
        );
        assert_eq!(
            api_root(
                LlmProviderType::OpenAiCompat,
                Some("https://api.mistral.ai"),
                "http://x"
            ),
            "https://api.mistral.ai/v1"
        );
        assert_eq!(
            api_root(LlmProviderType::LlamaCpp, None, "http://localhost:8000"),
            "http://localhost:8000/v1"
        );
    }
}
