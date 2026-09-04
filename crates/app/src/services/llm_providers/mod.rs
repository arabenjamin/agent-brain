use async_trait::async_trait;
use std::time::Duration;
use thiserror::Error;

use crate::services::llm::{ChatMessage, LlmResponse};

/// Render a `reqwest::Error` with the cause that actually explains it.
///
/// `reqwest::Error`'s own `Display` is just `error sending request for url (X)`
/// — the part that says *why* (timed out, connection refused, dns failure)
/// lives in its `source()` chain and is dropped by `{0}`. That made a
/// client-side **timeout** indistinguishable from an unreachable host in every
/// log line and every dead job's `error` field: the 26 jobs that dead-lettered
/// on 2026-09-01 all read `error sending request for url
/// (http://host.docker.internal:11434/api/generate)` while Ollama was up and
/// serving, and the only way to tell them apart was to notice that each had
/// run for exactly 120 seconds.
///
/// The kind is prefixed rather than appended so it survives the truncation
/// that job/error fields apply, and it deliberately uses the phrases
/// `classify_unavailable` in `shared_llm.rs` already matches on.
pub fn describe_reqwest(e: &reqwest::Error) -> String {
    let mut out = String::new();
    if e.is_timeout() {
        out.push_str("operation timed out: ");
    } else if e.is_connect() {
        out.push_str("connection refused: ");
    }
    out.push_str(&e.to_string());
    let mut source = std::error::Error::source(e);
    while let Some(inner) = source {
        out.push_str(": ");
        out.push_str(&inner.to_string());
        source = inner.source();
    }
    out
}

#[derive(Debug, Error)]
pub enum LlmProviderError {
    #[error("HTTP request failed: {}", describe_reqwest(.0))]
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
    /// Ollama-only model residency window (Go duration string). Other providers
    /// ignore it — only `OllamaProvider` serializes it onto the request.
    pub keep_alive: Option<String>,
    /// Ollama-only context window override. `None` leaves Ollama's 4096 default,
    /// which silently truncates longer prompts.
    pub num_ctx: Option<u32>,
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    /// Get the name of the provider.
    fn name(&self) -> &'static str;

    /// Generate text from a prompt.
    async fn generate(
        &self,
        prompt: &str,
        system: Option<&str>,
    ) -> Result<LlmResponse, LlmProviderError>;

    /// Generate embeddings for a text.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, LlmProviderError>;

    /// Chat with the model using message history.
    async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmResponse, LlmProviderError>;

    /// Check if the provider is healthy.
    async fn health_check(&self) -> bool;
}

pub mod anthropic;
pub mod gemini;
pub mod ollama;
pub mod openai_compat;

#[cfg(test)]
mod describe_reqwest_tests {
    use super::*;

    /// The defect this fixes: a client-side timeout rendered identically to an
    /// unreachable host, because `reqwest::Error`'s `Display` stops at
    /// "error sending request for url (…)". Every one of the 26 jobs that
    /// dead-lettered on 2026-09-01 carried that string while Ollama was up.
    #[tokio::test]
    async fn a_timeout_says_it_timed_out() {
        // Bind a listener that accepts but never responds, so the client
        // deadline — not the connect — is what fires.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let mut held = Vec::new();
            while let Ok((sock, _)) = listener.accept().await {
                held.push(sock); // hold the connection open, never reply
            }
        });

        let client = reqwest::Client::builder()
            .timeout(Duration::from_millis(150))
            .build()
            .unwrap();
        let err = client
            .get(format!("http://{addr}/api/generate"))
            .send()
            .await
            .expect_err("a server that never replies must time out");

        let described = describe_reqwest(&err);
        assert!(
            described.starts_with("operation timed out"),
            "timeout must be named first so it survives truncation: {described}"
        );
        // The phrase `classify_unavailable` matches on must be present.
        assert!(described.contains("operation timed out"));
    }

    /// A refused connection must stay distinguishable from the timeout above —
    /// the two need different responses (wait longer vs. the host is down).
    #[tokio::test]
    async fn a_refused_connection_says_so() {
        // Bind then drop, so the port is known-closed.
        let addr = {
            let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            l.local_addr().unwrap()
        };
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(5))
            .build()
            .unwrap();
        let err = client
            .get(format!("http://{addr}/api/generate"))
            .send()
            .await
            .expect_err("a closed port must refuse");

        let described = describe_reqwest(&err);
        assert!(
            !described.starts_with("operation timed out"),
            "a refused connection must not be reported as a timeout: {described}"
        );
        // The cause chain is the point — bare Display would end at the URL.
        assert!(
            described.len() > err.to_string().len(),
            "the source chain must be appended: {described}"
        );
    }
}
