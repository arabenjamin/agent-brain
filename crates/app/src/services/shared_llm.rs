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
use crate::services::queue::{SELECTED_LLM, USE_LOCAL_LLM};
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

        // If a cloud call was rate-limited OR rejected as subscription-only
        // (Ollama Cloud's free tier is undocumented — 403 "requires a
        // subscription" is how we learn a model isn't free), fall back to
        // local Ollama before giving up. Applies to both the active config
        // and capability-selected cloud models.
        let unavailable_kind = match &result {
            Err(e) if is_rate_limited(e) => Some("rate_limited"),
            Err(e) if is_subscription_required(e) => Some("subscription_required"),
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
                    None,
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
                        None,
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
                None,
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
