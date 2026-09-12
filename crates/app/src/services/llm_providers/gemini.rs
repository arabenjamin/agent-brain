use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tracing::warn;

use super::{LlmProvider, LlmProviderError, ProviderConfig};
use crate::services::llm::{ChatMessage, LlmResponse};

pub struct GeminiProvider {
    client: Client,
    config: ProviderConfig,
}

impl GeminiProvider {
    pub fn new(config: ProviderConfig) -> Self {
        let client = Client::builder()
            .timeout(config.timeout)
            .build()
            .unwrap_or_else(|_| Client::new());

        Self { client, config }
    }
}

#[derive(Debug, Serialize)]
struct GeminiRequest {
    contents: Vec<GeminiContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    system_instruction: Option<GeminiSystemInstruction>,
    generation_config: GeminiGenerationConfig,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiContent {
    role: String,
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize, Deserialize)]
struct GeminiPart {
    text: String,
}

#[derive(Debug, Serialize)]
struct GeminiSystemInstruction {
    parts: Vec<GeminiPart>,
}

#[derive(Debug, Serialize)]
struct GeminiGenerationConfig {
    temperature: f32,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u32>,
}

/// Gemini answers in camelCase (`usageMetadata`), so this needs `rename_all`.
///
/// Without it serde simply does not match the field and `usage_metadata` is
/// `None` — no error, no warning. The token counts were extracted correctly by
/// both `generate()` and `chat()`; they were just always extracting from a
/// `None`. Every Gemini row in `model_usage` had NULL `tokens_in`/`tokens_out`
/// while gpt-oss and llama.cpp rows were populated, so orchestrator spend was
/// invisible in the ledger — which matters most for the one provider here that
/// is actually metered.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    usage_metadata: Option<GeminiUsage>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: GeminiContent,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GeminiUsage {
    prompt_token_count: u32,
    candidates_token_count: u32,
}

#[derive(Debug, Serialize)]
struct GeminiEmbedRequest<'a> {
    model: &'a str,
    content: GeminiContent,
}

#[derive(Debug, Deserialize)]
struct GeminiEmbedResponse {
    embedding: GeminiEmbedding,
}

#[derive(Debug, Deserialize)]
struct GeminiEmbedding {
    values: Vec<f32>,
}

#[async_trait]
impl LlmProvider for GeminiProvider {
    fn name(&self) -> &'static str {
        "gemini"
    }

    async fn generate(
        &self,
        prompt: &str,
        system: Option<&str>,
    ) -> Result<LlmResponse, LlmProviderError> {
        let api_key = self.config.api_key.as_ref().ok_or_else(|| {
            LlmProviderError::InvalidConfig("Gemini API key is missing".to_string())
        })?;

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.config.model, api_key
        );

        let contents = vec![GeminiContent {
            role: "user".to_string(),
            parts: vec![GeminiPart {
                text: prompt.to_string(),
            }],
        }];

        let system_instruction = system.map(|s| GeminiSystemInstruction {
            parts: vec![GeminiPart {
                text: s.to_string(),
            }],
        });

        let request = GeminiRequest {
            contents,
            system_instruction,
            generation_config: GeminiGenerationConfig {
                temperature: self.config.temperature,
                max_output_tokens: self.config.max_tokens,
            },
        };

        let response = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "Gemini API Error (Status {}): {}",
                status, body
            )));
        }

        let gemini_res: GeminiResponse = response.json().await?;
        let text = gemini_res
            .candidates
            .first()
            .map(|c| {
                c.content
                    .parts
                    .first()
                    .map(|p| p.text.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default();

        let tokens_in = gemini_res
            .usage_metadata
            .as_ref()
            .map(|u| u.prompt_token_count);
        let tokens_out = gemini_res
            .usage_metadata
            .as_ref()
            .map(|u| u.candidates_token_count);
        let tokens = gemini_res
            .usage_metadata
            .map(|u| u.prompt_token_count + u.candidates_token_count);

        Ok(LlmResponse {
            text,
            duration_ns: None,
            tokens_evaluated: tokens,
            tokens_in,
            tokens_out,
        })
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmProviderError> {
        let text = super::prepare_embed_input(text)?;
        let api_key = self.config.api_key.as_ref().ok_or_else(|| {
            LlmProviderError::InvalidConfig("Gemini API key is missing".to_string())
        })?;

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:embedContent?key={}",
            "text-embedding-004", // Default embedding model for Gemini
            api_key
        );

        let request = GeminiEmbedRequest {
            model: "models/text-embedding-004",
            content: GeminiContent {
                role: "user".to_string(),
                parts: vec![GeminiPart {
                    text: text.to_string(),
                }],
            },
        };

        let response = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "Gemini Embedding Error (Status {}): {}",
                status, body
            )));
        }

        let embed_res: GeminiEmbedResponse = response.json().await?;
        Ok(embed_res.embedding.values)
    }

    async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmResponse, LlmProviderError> {
        let api_key = self.config.api_key.as_ref().ok_or_else(|| {
            LlmProviderError::InvalidConfig("Gemini API key is missing".to_string())
        })?;

        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{}:generateContent?key={}",
            self.config.model, api_key
        );

        let mut gemini_contents = Vec::new();
        let mut system_prompt = None;

        for msg in messages {
            match msg.role.as_str() {
                "system" => system_prompt = Some(msg.content.clone()),
                "user" | "assistant" => {
                    let role = if msg.role == "assistant" {
                        "model"
                    } else {
                        "user"
                    };
                    gemini_contents.push(GeminiContent {
                        role: role.to_string(),
                        parts: vec![GeminiPart {
                            text: msg.content.clone(),
                        }],
                    });
                }
                _ => warn!("Unsupported role for Gemini: {}", msg.role),
            }
        }

        let system_instruction = system_prompt.map(|s| GeminiSystemInstruction {
            parts: vec![GeminiPart { text: s }],
        });

        let request = GeminiRequest {
            contents: gemini_contents,
            system_instruction,
            generation_config: GeminiGenerationConfig {
                temperature: self.config.temperature,
                max_output_tokens: self.config.max_tokens,
            },
        };

        let response = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .json(&request)
            .send()
            .await?;

        if !response.status().is_success() {
            let status = response.status();
            let body = response.text().await.unwrap_or_default();
            return Err(LlmProviderError::GenerationFailed(format!(
                "Gemini API Error (Status {}): {}",
                status, body
            )));
        }

        let gemini_res: GeminiResponse = response.json().await?;
        let text = gemini_res
            .candidates
            .first()
            .map(|c| {
                c.content
                    .parts
                    .first()
                    .map(|p| p.text.clone())
                    .unwrap_or_default()
            })
            .unwrap_or_default();

        let tokens_in = gemini_res
            .usage_metadata
            .as_ref()
            .map(|u| u.prompt_token_count);
        let tokens_out = gemini_res
            .usage_metadata
            .as_ref()
            .map(|u| u.candidates_token_count);
        let tokens = gemini_res
            .usage_metadata
            .map(|u| u.prompt_token_count + u.candidates_token_count);

        Ok(LlmResponse {
            text,
            duration_ns: None,
            tokens_evaluated: tokens,
            tokens_in,
            tokens_out,
        })
    }

    async fn health_check(&self) -> bool {
        self.config.api_key.is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Gemini answers in camelCase. Without `rename_all` serde matches nothing,
    /// `usage_metadata` is silently `None`, and every Gemini row in the usage
    /// ledger records NULL tokens — invisible spend on the one metered provider
    /// in the fleet. This is a real captured response body, trimmed.
    #[test]
    fn usage_metadata_is_parsed_from_gemini_camel_case() {
        let body = r#"{
          "candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]}}],
          "usageMetadata": {
            "promptTokenCount": 1234,
            "candidatesTokenCount": 56,
            "totalTokenCount": 1290
          }
        }"#;
        let res: GeminiResponse = serde_json::from_str(body).expect("parses");
        let usage = res.usage_metadata.expect("usageMetadata must deserialize");
        assert_eq!(usage.prompt_token_count, 1234);
        assert_eq!(usage.candidates_token_count, 56);
    }

    /// A response without usage must still parse — the field is optional and a
    /// missing one must not fail the whole call.
    #[test]
    fn a_response_without_usage_still_parses() {
        let body = r#"{"candidates":[{"content":{"role":"model","parts":[{"text":"hi"}]}}]}"#;
        let res: GeminiResponse = serde_json::from_str(body).expect("parses");
        assert!(res.usage_metadata.is_none());
    }
}
