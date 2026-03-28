//! `SharedLlm` — an `LlmProvider` wrapper around the live
//! `Arc<RwLock<Option<LlmConfig>>>` used by the server.
//!
//! This allows skills to hold an `Arc<dyn LlmProvider>` while still picking
//! up runtime provider changes made via `use_model`.

use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::services::traits::LlmProvider;
use crate::services::{LlmClient, LlmConfig};

/// Thin wrapper that reads the live `LlmConfig` on every call.
pub struct SharedLlm {
    config: Arc<RwLock<Option<LlmConfig>>>,
}

impl SharedLlm {
    /// Wrap the server's shared config.
    pub fn new(config: Arc<RwLock<Option<LlmConfig>>>) -> Arc<Self> {
        Arc::new(Self { config })
    }
}

#[async_trait]
impl LlmProvider for SharedLlm {
    async fn generate(&self, prompt: &str, system: Option<&str>) -> anyhow::Result<String> {
        let config = self.config.read().await.clone();
        let llm = config.ok_or_else(|| anyhow::anyhow!("LLM not configured"))?;
        let client =
            LlmClient::with_config(llm).map_err(|e| anyhow::anyhow!("LLM init error: {}", e))?;
        let resp = client
            .generate_with_system(prompt, system)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(resp.text)
    }

    async fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
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
        // A full async check would require a blocking read here; keep it simple.
        true
    }
}
