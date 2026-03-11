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

#[derive(Debug, Deserialize)]
struct GeminiResponse {
    candidates: Vec<GeminiCandidate>,
    usage_metadata: Option<GeminiUsage>,
}

#[derive(Debug, Deserialize)]
struct GeminiCandidate {
    content: GeminiContent,
}

#[derive(Debug, Deserialize)]
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

        let tokens = gemini_res
            .usage_metadata
            .map(|u| u.prompt_token_count + u.candidates_token_count);

        Ok(LlmResponse {
            text,
            duration_ns: None,
            tokens_evaluated: tokens,
        })
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmProviderError> {
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

        let tokens = gemini_res
            .usage_metadata
            .map(|u| u.prompt_token_count + u.candidates_token_count);

        Ok(LlmResponse {
            text,
            duration_ns: None,
            tokens_evaluated: tokens,
        })
    }

    async fn health_check(&self) -> bool {
        self.config.api_key.is_some()
    }
}
