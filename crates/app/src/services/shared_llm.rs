//! `SharedLlm` — an `LlmProvider` wrapper around the live
//! `Arc<RwLock<Option<LlmConfig>>>` used by the server.
//!
//! This allows skills to hold an `Arc<dyn LlmProvider>` while still picking
//! up runtime provider changes made via `use_model`.
//!
//! When a background job sets the `USE_LOCAL_LLM` task-local (see `queue.rs`),
//! `generate()` routes to `local_config` (always local Ollama) instead of the
//! active config, preventing maintenance tasks from consuming cloud quota.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tracing::{debug, warn};

use crate::repository::TelemetryClient;
use crate::services::queue::{CURRENT_TOOL, SELECTED_LLM, USE_LOCAL_LLM};
use crate::services::traits::LlmProvider;
use crate::services::{LlmClient, LlmConfig};

/// Thin wrapper that reads the live `LlmConfig` on every call.
pub struct SharedLlm {
    /// Active (possibly cloud) config — used for interactive calls.
    config: Arc<RwLock<Option<LlmConfig>>>,
    /// Local-Ollama-only config — used when `USE_LOCAL_LLM` task-local is set.
    local_config: Arc<RwLock<Option<LlmConfig>>>,
    /// Optional telemetry sink for per-call usage logging.
    telemetry: Option<TelemetryClient>,
}

impl SharedLlm {
    /// Wrap the server's shared config. `local_config` falls back to `config`
    /// when not provided (legacy callers that don't need local routing).
    pub fn new(config: Arc<RwLock<Option<LlmConfig>>>) -> Arc<Self> {
        Arc::new(Self {
            local_config: Arc::clone(&config),
            config,
            telemetry: None,
        })
    }

    /// Full constructor: active config, local-only config, and optional telemetry.
    pub fn new_with_local(
        config: Arc<RwLock<Option<LlmConfig>>>,
        local_config: Arc<RwLock<Option<LlmConfig>>>,
        telemetry: Option<TelemetryClient>,
    ) -> Arc<Self> {
        Arc::new(Self {
            config,
            local_config,
            telemetry,
        })
    }
}

#[async_trait]
impl LlmProvider for SharedLlm {
    async fn generate(&self, prompt: &str, system: Option<&str>) -> anyhow::Result<String> {
        // Routing precedence: capability-selected model (per-step router) >
        // local pin (background jobs) > active config.
        let selected = SELECTED_LLM.try_with(|v| v.clone()).unwrap_or(None);
        let use_local = USE_LOCAL_LLM.try_with(|&v| v).unwrap_or(false);
        // Set by the queue coordinator to the job's tool name. `None` outside a
        // job (a direct MCP call, a startup protocol step) is honest — those
        // rows genuinely have no owning tool.
        let tool = CURRENT_TOOL.try_with(|v| v.clone()).unwrap_or(None);
        let tool = tool.as_deref();
        let (llm, is_local_route) = if let Some(sel) = selected {
            debug!(model = %sel.model, "SharedLlm: routing generate() to capability-selected model");
            let local = matches!(sel.provider, crate::services::LlmProviderType::Ollama);
            (sel, local)
        } else if use_local {
            debug!("SharedLlm: routing generate() to local Ollama (USE_LOCAL_LLM=true)");
            let config = self.local_config.read().await.clone();
            (
                config.ok_or_else(|| anyhow::anyhow!("LLM not configured"))?,
                true,
            )
        } else {
            let config = self.config.read().await.clone();
            (
                config.ok_or_else(|| anyhow::anyhow!("LLM not configured"))?,
                false,
            )
        };
        let model_name = llm.model.clone();
        let client =
            LlmClient::with_config(llm).map_err(|e| anyhow::anyhow!("LLM init error: {}", e))?;
        let start = Instant::now();
        let result = client
            .generate_with_system(prompt, system)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e));
        let duration_ms = start.elapsed().as_millis() as i64;

        // If the cloud call failed in a way that says "this provider cannot
        // answer right now" — as opposed to "this prompt is bad" — fall back to
        // local Ollama before giving up. Applies to both the active config and
        // capability-selected cloud models.
        let unavailable_kind = match &result {
            Err(e) => classify_unavailable(e),
            _ => None,
        };
        if !is_local_route && let Some(kind) = unavailable_kind {
            warn!(
                error_kind = kind,
                "SharedLlm: cloud LLM unavailable, falling back to local Ollama"
            );
            // Record the failed cloud call FIRST — these events are how the
            // model router learns observed availability; they must not vanish.
            if let Some(ref tc) = self.telemetry {
                let _ = tc.record_model_usage(
                    &model_name,
                    tool,
                    false,
                    Some(duration_ms),
                    None,
                    None,
                    Some(kind),
                );
            }
            if let Some(local_llm) = self.local_config.read().await.clone() {
                let local_model = local_llm.model.clone();
                let local_client = LlmClient::with_config(local_llm)
                    .map_err(|e| anyhow::anyhow!("Local LLM init error: {}", e))?;
                let local_start = Instant::now();
                let local_result = local_client
                    .generate_with_system(prompt, system)
                    .await
                    .map_err(|e| anyhow::anyhow!("{}", e));
                let local_duration_ms = local_start.elapsed().as_millis() as i64;
                if let Some(ref tc) = self.telemetry {
                    let success = local_result.is_ok();
                    let (tin, tout) = response_tokens(&local_result);
                    let _ = tc.record_model_usage(
                        &local_model,
                        tool,
                        success,
                        Some(local_duration_ms),
                        tin,
                        tout,
                        None,
                    );
                }
                return local_result.map(|r| r.text);
            }
        }

        if let Some(ref tc) = self.telemetry {
            let success = result.is_ok();
            let (tin, tout) = response_tokens(&result);
            let _ = tc.record_model_usage(
                &model_name,
                tool,
                success,
                Some(duration_ms),
                tin,
                tout,
                None,
            );
        }
        result.map(|r| r.text)
    }

    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        // Embeddings always use the active config (embed_base_url is already pinned
        // to local Ollama even when provider=ollama-cloud).
        let config = self.config.read().await.clone();
        let llm = config.ok_or_else(|| anyhow::anyhow!("LLM not configured"))?;
        let client =
            LlmClient::with_config(llm).map_err(|e| anyhow::anyhow!("LLM init error: {}", e))?;
        client
            .embeddings(text)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    fn model_name(&self) -> &str {
        // Can't read async here; return a static placeholder.
        // Callers that need the live model name should read llm_config directly
        // (only ModelSkill does that).
        "dynamic"
    }

    fn is_available(&self) -> bool {
        // Non-async probe: treat as available if the config is set.
        true
    }
}

/// Classify a failed LLM call as a provider-unavailable condition worth
/// retrying locally, returning the telemetry `error_kind` for it.
///
/// The distinction that matters is **"the provider could not answer"** vs
/// **"the provider answered and rejected this request"**. Only the first is
/// worth re-running against a different model: falling back on a 400 would
/// re-send a malformed prompt to a weaker model and get a worse rejection.
///
/// Returns `None` for anything unrecognised, which preserves the previous
/// behaviour of propagating the error.
fn classify_unavailable(e: &anyhow::Error) -> Option<&'static str> {
    if is_rate_limited(e) {
        Some("rate_limited")
    } else if is_subscription_required(e) {
        Some("subscription_required")
    } else if is_transport_failure(e) {
        Some("transport")
    } else if is_server_error(e) {
        Some("server_error")
    } else {
        None
    }
}

/// Returns true if the error looks like an HTTP 429 / rate-limit / quota error.
fn is_rate_limited(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("429") || msg.contains("Too Many Requests") || msg.contains("usage limit")
}

/// Returns true if a cloud provider rejected the model as paid-only
/// (observed from Ollama Cloud: HTTP 403 "this model requires a subscription").
fn is_subscription_required(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("requires a subscription") || msg.contains("403 Forbidden")
}

/// Returns true if the call never reached the provider — DNS failure, refused
/// connection, TLS error, or a client-side timeout.
///
/// This is the case that took down the Off-Grid Networking Monitor on
/// 2026-08-19: its cloud `reason` step failed 3/3 with
/// `Provider error: HTTP request failed: error sending request for url
/// (https://ollama.com/v1/chat/completions)`, dead-lettered, and — via
/// chain-death attribution — failed the owning Task. Retrying an unreachable
/// host three times cannot succeed; a local model can. The weekly report was
/// simply missing, and nothing surfaced why until a human went looking.
fn is_transport_failure(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("error sending request")
        || msg.contains("Server not reachable")
        || msg.contains("operation timed out")
        || msg.contains("connection refused")
        || msg.contains("dns error")
}

/// Returns true if the provider answered with a 5xx — it is up, but not
/// serving. Every provider formats status codes through `StatusCode`'s
/// `Display`, so matching the canonical reason phrases covers all four
/// (`Status 503: …`, `Gemini API Error (Status 503 Service Unavailable): …`).
///
/// Matched by phrase rather than by bare number on purpose: `contains("500")`
/// would fire on a token count or a model name in an unrelated error body.
fn is_server_error(e: &anyhow::Error) -> bool {
    let msg = e.to_string();
    msg.contains("500 Internal Server Error")
        || msg.contains("502 Bad Gateway")
        || msg.contains("503 Service Unavailable")
        || msg.contains("504 Gateway Timeout")
}

/// Extract (tokens_in, tokens_out) from a generate result for telemetry.
fn response_tokens(
    result: &anyhow::Result<crate::services::llm::LlmResponse>,
) -> (Option<i64>, Option<i64>) {
    match result {
        Ok(r) => (
            r.tokens_in.map(|t| t as i64),
            r.tokens_out.map(|t| t as i64),
        ),
        Err(_) => (None, None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn err(msg: &str) -> anyhow::Error {
        anyhow::anyhow!("{}", msg)
    }

    /// The verbatim error that failed the Off-Grid Networking Monitor run of
    /// 2026-08-19, read back out of that Task's `context`.
    #[test]
    fn classifies_the_off_grid_monitor_failure_as_transport() {
        let e = err("Reasoning failed: Provider error: HTTP request failed: \
             error sending request for url (https://ollama.com/v1/chat/completions)");
        assert_eq!(classify_unavailable(&e), Some("transport"));
    }

    #[test]
    fn classifies_rate_limit_and_subscription_first() {
        assert_eq!(
            classify_unavailable(&err("HTTP 429 Too Many Requests")),
            Some("rate_limited")
        );
        assert_eq!(
            classify_unavailable(&err("403 Forbidden: this model requires a subscription")),
            Some("subscription_required")
        );
    }

    #[test]
    fn classifies_transport_variants() {
        for msg in [
            "error sending request for url (https://ollama.com/v1/chat/completions)",
            "Server not reachable: http://localhost:11434",
            "operation timed out",
            "tcp connect error: connection refused",
            "dns error: failed to lookup address information",
        ] {
            assert_eq!(classify_unavailable(&err(msg)), Some("transport"), "{msg}");
        }
    }

    #[test]
    fn classifies_5xx_across_provider_error_formats() {
        for msg in [
            "Status 500 Internal Server Error: upstream failure",
            "OpenAI-compat error (502 Bad Gateway): ",
            "Gemini API Error (Status 503 Service Unavailable): overloaded",
            "Anthropic API Error (Status 504 Gateway Timeout): ",
        ] {
            assert_eq!(
                classify_unavailable(&err(msg)),
                Some("server_error"),
                "{msg}"
            );
        }
    }

    /// A provider that answered and rejected the request must NOT fall back —
    /// re-sending a bad prompt to a weaker model only produces a worse error.
    #[test]
    fn does_not_fall_back_on_request_rejections() {
        for msg in [
            "Status 400 Bad Request: invalid role in messages[2]",
            "Model not available: qwen3.5:4b",
            "Failed to parse LLM response: expected value at line 1",
            "Status 401 Unauthorized: invalid api key",
        ] {
            assert_eq!(classify_unavailable(&err(msg)), None, "{msg}");
        }
    }

    /// `contains("500")` would fire here; the canonical-phrase match must not.
    #[test]
    fn status_like_numbers_in_a_body_are_not_server_errors() {
        let e = err("Generation failed: prompt exceeds 500 tokens for context window 4096");
        assert_eq!(classify_unavailable(&e), None);
    }
}
