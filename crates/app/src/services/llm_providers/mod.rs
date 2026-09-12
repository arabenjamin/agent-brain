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
/// Largest input we will send to an embedding endpoint, in characters.
///
/// THE REAL LIMIT IS TOKENS, AND THIS CAP IS CHARACTERS, SO IT MUST BE
/// PESSIMISTIC. Bisected against the live `bge-m3` on 2026-09-12 with two kinds
/// of input: token-sparse prose (lorem ipsum) embedded up to ~10,327 chars,
/// while dense real content (a stored note full of JSON, URLs and punctuation)
/// failed from ~5,819 — both around 3,200 tokens. A cap set from the prose
/// measurement 500s on the dense case, which is exactly how the first attempt
/// at this failed: 3 of the 4 notes it was meant to repair still errored.
///
/// 4,000 is safe for ordinary dense content; `embed_with_backoff` covers what
/// is denser still, so this does not have to be right for pathological input.
/// Note it sits far below the 32K the model's 8192-token context implies —
/// that is Ollama's serving config, not the model, and cannot be derived.
pub const MAX_EMBED_CHARS: usize = 4_000;

/// Compile-time guard on the measured ceiling: ~10,327 chars embed, ~11,882
/// return 500. Raising `MAX_EMBED_CHARS` past the observed threshold must break
/// the build, not a test someone can skip — the number came from bisecting a
/// live endpoint and cannot be re-derived from the model's advertised context.
const _: () = assert!(MAX_EMBED_CHARS < 5_819);

/// Make an embedding input safe to send, or say why it is not embeddable.
///
/// Two real failure modes, both found by probing the endpoint rather than from
/// the error text:
///
/// * **Over-length input 500s.** Nothing truncated before `embed()`, so a long
///   note simply failed and was stored with no vector — permanently invisible to
///   semantic search. Four notes were in that state, the longest 201,259 chars,
///   20× the ceiling. Truncating turns a hard failure into a slightly lossy
///   embedding, which is strictly better: the note's chunks are embedded
///   separately anyway, so the parent vector only has to be directionally right.
/// * **Empty input returns HTTP 200 with a zero-length vector.** That is worse
///   than an error, because it flows into `db.index.vector.queryNodes` as a
///   silent no-op and looks like "nothing is similar". Rejected here instead.
///
/// Note this does NOT reproduce the "certain strings return NaN" behaviour the
/// codebase long assumed. Emoji, control characters, zero-width joiners, CJK,
/// bare punctuation and whitespace-only input all embed cleanly at 1024 dims.
pub fn prepare_embed_input(text: &str) -> Result<&str, LlmProviderError> {
    if text.trim().is_empty() {
        return Err(LlmProviderError::InvalidConfig(
            "cannot embed empty input (the endpoint answers 200 with a zero-length \
             vector, which reads downstream as 'nothing is similar')"
                .to_string(),
        ));
    }
    if text.len() <= MAX_EMBED_CHARS {
        return Ok(text);
    }
    // Slice on a char boundary — `len()` is bytes and a mid-codepoint cut panics.
    let mut end = MAX_EMBED_CHARS;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    Ok(&text[..end])
}

/// Shrinking retry for an embedding call whose only failure mode is "too long".
///
/// A character cap cannot be correct for a token limit — content density varies
/// by nearly 2× between prose and JSON — so rather than pick a cap pessimistic
/// enough for the worst case and throw away context for the common one, halve
/// and retry. Two extra attempts cover a 4× density range.
///
/// Only length-shaped failures are retried. A missing API key or a dead
/// endpoint is returned immediately: shrinking the input cannot fix it, and
/// retrying would just triple the latency of a call that was always going to
/// fail.
pub async fn embed_with_backoff<F, Fut>(text: &str, call: F) -> Result<Vec<f32>, LlmProviderError>
where
    F: Fn(String) -> Fut,
    Fut: std::future::Future<Output = Result<Vec<f32>, LlmProviderError>>,
{
    let prepared = prepare_embed_input(text)?;
    let mut attempt = prepared.to_string();
    let mut last_err = None;
    for _ in 0..3 {
        match call(attempt.clone()).await {
            Ok(v) if !v.is_empty() => return Ok(v),
            Ok(_) => {
                last_err = Some(LlmProviderError::GenerationFailed(
                    "embedding endpoint returned an empty vector".to_string(),
                ));
                break;
            }
            Err(e) => {
                let msg = e.to_string();
                let length_shaped = msg.contains("500") || msg.to_lowercase().contains("too long");
                last_err = Some(e);
                if !length_shaped || attempt.chars().count() <= 256 {
                    break;
                }
                let keep = attempt.chars().count() / 2;
                attempt = attempt.chars().take(keep).collect();
                tracing::warn!(
                    retry_chars = keep,
                    "Embedding failed on length — retrying with half the input"
                );
            }
        }
    }
    Err(last_err
        .unwrap_or_else(|| LlmProviderError::GenerationFailed("embedding failed".to_string())))
}

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

#[cfg(test)]
mod embed_input_tests {
    use super::*;

    /// Empty input must be an ERROR, not a request. The endpoint answers it with
    /// HTTP 200 and a zero-length vector, which flows into the vector index as a
    /// silent no-op and reads downstream as "nothing is similar".
    #[test]
    fn empty_and_whitespace_are_rejected() {
        for bad in ["", "   ", "\n\t "] {
            assert!(
                prepare_embed_input(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn normal_input_passes_through_untouched() {
        let s = "Home Assistant REST API documentation";
        assert_eq!(prepare_embed_input(s).unwrap(), s);
    }

    /// Over-length input 500s at the endpoint. Truncating is strictly better
    /// than failing: the note's chunks are embedded separately, so the parent
    /// vector only has to be directionally right. Before this, four notes (the
    /// longest 201,259 chars) were stored with no embedding at all.
    /// The backoff is what makes a character cap survivable against a token
    /// limit. First attempt too long, second succeeds — and the caller gets a
    /// vector rather than an error.
    #[tokio::test]
    async fn backoff_halves_the_input_and_succeeds() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let out = embed_with_backoff(&"x".repeat(MAX_EMBED_CHARS), |p| {
            let n = calls.fetch_add(1, Ordering::SeqCst);
            async move {
                if n == 0 {
                    Err(LlmProviderError::GenerationFailed("Status 500: ...".into()))
                } else {
                    assert!(p.chars().count() < MAX_EMBED_CHARS, "input was not shrunk");
                    Ok(vec![0.1_f32; 1024])
                }
            }
        })
        .await
        .expect("second attempt should succeed");
        assert_eq!(out.len(), 1024);
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }

    /// A non-length failure must NOT be retried — shrinking cannot fix a missing
    /// key, and retrying would triple the latency of a doomed call.
    #[tokio::test]
    async fn backoff_does_not_retry_a_non_length_failure() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let calls = AtomicUsize::new(0);
        let r = embed_with_backoff("hello", |_| {
            calls.fetch_add(1, Ordering::SeqCst);
            async { Err(LlmProviderError::InvalidConfig("no api key".into())) }
        })
        .await;
        assert!(r.is_err());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "must not retry");
    }

    /// An empty vector is a failure dressed as success — it must not be returned.
    #[tokio::test]
    async fn backoff_rejects_an_empty_vector() {
        let r = embed_with_backoff("hello", |_| async { Ok(vec![]) }).await;
        assert!(r.is_err());
    }

    #[test]
    fn over_length_input_is_truncated_not_rejected() {
        let long = "a".repeat(MAX_EMBED_CHARS * 3);
        let out = prepare_embed_input(&long).expect("must not error");
        assert_eq!(out.len(), MAX_EMBED_CHARS);
    }

    /// `len()` is bytes; slicing mid-codepoint panics. A note full of CJK or
    /// emoji must truncate on a boundary, not abort the process.
    #[test]
    fn truncation_respects_char_boundaries() {
        for filler in ["é", "ホ", "🙂"] {
            let long = filler.repeat(MAX_EMBED_CHARS);
            let out = prepare_embed_input(&long).expect("must not error");
            assert!(out.len() <= MAX_EMBED_CHARS);
            // The real assertion: it is still valid UTF-8 we can read back.
            assert!(out.chars().all(|c| filler.starts_with(c)));
        }
    }
}
