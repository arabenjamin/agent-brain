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
use tracing::debug;

use crate::repository::TelemetryClient;
use crate::services::queue::USE_LOCAL_LLM;
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
        let use_local = USE_LOCAL_LLM.try_with(|&v| v).unwrap_or(false);
        let cfg_arc = if use_local {
            debug!("SharedLlm: routing generate() to local Ollama (USE_LOCAL_LLM=true)");
            &self.local_config
        } else {
            &self.config
        };
        let config = cfg_arc.read().await.clone();
        let llm = config.ok_or_else(|| anyhow::anyhow!("LLM not configured"))?;
        let model_name = llm.model.clone();
        let client =
            LlmClient::with_config(llm).map_err(|e| anyhow::anyhow!("LLM init error: {}", e))?;
        let start = Instant::now();
        let result = client
            .generate_with_system(prompt, system)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e));
        let duration_ms = start.elapsed().as_millis() as i64;
        if let Some(ref tc) = self.telemetry {
            let success = result.is_ok();
            let _ = tc.record_model_usage(&model_name, None, success, Some(duration_ms), None, None);
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
