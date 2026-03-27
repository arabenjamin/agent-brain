use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use thiserror::Error;

use crate::services::llm::{ChatMessage, LlmResponse};

#[derive(Debug, Error)]
pub enum LlmProviderError {
    #[error("HTTP request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Generation failed: {0}")]
    GenerationFailed(String),

    #[error("Model not available: {0}")]
    ModelNotAvailable(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),

    #[error("Unsupported capability: {0}")]
    UnsupportedCapability(String),
}

#[derive(Debug, Clone)]
pub struct ProviderConfig {
    pub model: String,
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub timeout: Duration,
    pub temperature: f32,
    pub max_tokens: Option<u32>,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Get the name of the provider.
    fn name(&self) -> &'static str;

    /// Generate text from a prompt.
    async fn generate(&self, prompt: &str, system: Option<&str>) -> Result<LlmResponse, LlmProviderError>;

    /// Generate embeddings for a text.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmProviderError>;

    /// Chat with the model using message history.
    async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmResponse, LlmProviderError>;

    /// Check if the provider is healthy.
    async fn health_check(&self) -> bool;
}

pub mod ollama;
pub mod anthropic;
pub mod gemini;
