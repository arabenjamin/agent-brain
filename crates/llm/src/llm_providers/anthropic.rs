use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{LlmProvider, LlmProviderError, ProviderConfig};
use crate::llm::{ChatMessage, LlmResponse};

/// Output ceiling when the caller sets none. On the current models thinking is
/// on by default and its tokens count against `max_tokens`, so the old 4096
/// truncated a ~3K-token report once the model had thought first. It is a
/// ceiling, not a charge — only generated tokens are billed.
const DEFAULT_MAX_TOKENS: u32 = 16_000;

/// Floor on the request timeout. A thinking model writing a report over a
/// ~55K-token prompt runs past the shared 120s cloud default, and a call killed
/// at 120s is billed anyway.
const MIN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// Whether `model` accepts `temperature`. Sonnet 5, Opus 5, Opus 4.7/4.8 and the
/// Fable/Mythos models reject sampling parameters with a 400; older models
/// (Sonnet 4.6, Haiku 4.5) still take them.
fn accepts_sampling(model: &str) -> bool {
    const NO_SAMPLING: &[&str] = &[
        "claude-sonnet-5",
        "claude-opus-5",
        "claude-opus-4-7",
        "claude-opus-4-8",
        "claude-fable",
        "claude-mythos",
    ];
    !NO_SAMPLING.iter().any(|prefix| model.starts_with(prefix))
}

/// The reply text: every `text` block, in order. With thinking on, the first
/// block is a `thinking` block, so reading `content[0]` returned nothing.
fn reply_text(content: &[AnthropicContent]) -> String {
    content
        .iter()
        .filter(|c| c.kind == "text")
        .map(|c| c.text.as_str())
        .collect::<Vec<_>>()
        .join("")
}

pub struct AnthropicProvider {
    client: Client,
    config: ProviderConfig,
}

impl AnthropicProvider {
    pub fn new(config: ProviderConfig) -> Self {
        let client = Client::builder()
            .timeout(config.timeout.max(MIN_TIMEOUT))
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
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
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
    #[serde(rename = "type", default)]
    kind: String,
    #[serde(default)]
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

    async fn generate(
        &self,
        prompt: &str,
        system: Option<&str>,
    ) -> Result<LlmResponse, LlmProviderError> {
        let url = self
            .config
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com/v1/messages");
        let api_key = self.config.api_key.as_ref().ok_or_else(|| {
            LlmProviderError::InvalidConfig("Anthropic API key is missing".to_string())
        })?;

        let messages = vec![AnthropicMessage {
            role: "user".to_string(),
            content: prompt.to_string(),
        }];

        let request = AnthropicRequest {
            model: &self.config.model,
            messages,
            system,
            max_tokens: self.config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            temperature: accepts_sampling(&self.config.model).then_some(self.config.temperature),
        };

        let response = self
            .client
            .post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
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
                "Anthropic API Error (Status {}): {}",
                status, body
            )));
        }

        let anthropic_res: AnthropicResponse = response.json().await?;
        let text = reply_text(&anthropic_res.content);

        Ok(LlmResponse {
            text,
            duration_ns: None,
            tokens_evaluated: Some(
                anthropic_res.usage.input_tokens + anthropic_res.usage.output_tokens,
            ),
            tokens_in: Some(anthropic_res.usage.input_tokens),
            tokens_out: Some(anthropic_res.usage.output_tokens),
        })
    }

    async fn embed(&self, _text: &str) -> Result<Vec<f32>, LlmProviderError> {
        Err(LlmProviderError::UnsupportedCapability(
            "Anthropic does not currently offer a native embeddings API via Messages".to_string(),
        ))
    }

    async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmResponse, LlmProviderError> {
        let url = self
            .config
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com/v1/messages");
        let api_key = self.config.api_key.as_ref().ok_or_else(|| {
            LlmProviderError::InvalidConfig("Anthropic API key is missing".to_string())
        })?;

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
            max_tokens: self.config.max_tokens.unwrap_or(DEFAULT_MAX_TOKENS),
            temperature: accepts_sampling(&self.config.model).then_some(self.config.temperature),
        };

        let response = self
            .client
            .post(url)
            .header("x-api-key", api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&request)
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
                "Anthropic API Error (Status {}): {}",
                status, body
            )));
        }

        let anthropic_res: AnthropicResponse = response.json().await?;
        let text = reply_text(&anthropic_res.content);

        Ok(LlmResponse {
            text,
            duration_ns: None,
            tokens_evaluated: Some(
                anthropic_res.usage.input_tokens + anthropic_res.usage.output_tokens,
            ),
            tokens_in: Some(anthropic_res.usage.input_tokens),
            tokens_out: Some(anthropic_res.usage.output_tokens),
        })
    }

    async fn health_check(&self) -> bool {
        // Anthropic doesn't have a simple health endpoint without auth,
        // so we assume true if the config has an API key.
        self.config.api_key.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_models_get_no_temperature() {
        for m in [
            "claude-sonnet-5",
            "claude-opus-5",
            "claude-opus-4-8",
            "claude-fable-5-1",
        ] {
            assert!(!accepts_sampling(m), "{m} rejects sampling params");
        }
        for m in ["claude-sonnet-4-6", "claude-haiku-4-5-20251001"] {
            assert!(accepts_sampling(m), "{m} still accepts temperature");
        }
    }

    #[test]
    fn a_leading_thinking_block_does_not_hide_the_answer() {
        let res: AnthropicResponse = serde_json::from_str(
            r#"{"content":[{"type":"thinking","thinking":"","signature":"x"},
                           {"type":"text","text":"the answer"}],
                "usage":{"input_tokens":10,"output_tokens":5}}"#,
        )
        .unwrap();
        assert_eq!(reply_text(&res.content), "the answer");
    }

    #[test]
    fn temperature_is_omitted_from_the_wire_when_unsupported() {
        let req = AnthropicRequest {
            model: "claude-sonnet-5",
            messages: vec![],
            system: None,
            max_tokens: DEFAULT_MAX_TOKENS,
            temperature: None,
        };
        let v = serde_json::to_value(&req).unwrap();
        assert!(v.get("temperature").is_none());
    }
}
