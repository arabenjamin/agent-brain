//! Model catalog loader — reads `models.yaml` and syncs entries into DuckDB.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use tracing::{info, warn};

use crate::repository::TelemetryClient;

/// Top-level structure of `models.yaml`.
#[derive(Debug, Deserialize)]
pub struct ModelCatalog {
    pub defaults: ModelDefaults,
    #[serde(default)]
    pub models: HashMap<String, ModelEntry>,
    pub default_system_prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ModelDefaults {
    #[serde(default = "default_temperature")]
    pub temperature: f64,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: i64,
    #[serde(default = "default_timeout_secs")]
    pub timeout_secs: i64,
}

fn default_temperature() -> f64 { 0.7 }
fn default_max_tokens() -> i64 { 4096 }
fn default_timeout_secs() -> i64 { 120 }

/// A single model definition in the catalog.
#[derive(Debug, Deserialize)]
pub struct ModelEntry {
    pub provider: String,
    pub model: String,
    pub context_window: i64,
    pub cost_per_1k_input: f64,
    pub cost_per_1k_output: f64,
    #[serde(default)]
    pub capabilities: Vec<String>,
    pub system_prompt: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<i64>,
    pub timeout_secs: Option<i64>,
}

impl ModelCatalog {
    /// Load catalog from a YAML file.
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        let catalog: Self = serde_yaml::from_str(&content)?;
        Ok(catalog)
    }

    /// Load catalog from YAML, falling back to a built-in minimal default if
    /// the file is missing or unreadable.
    pub fn load_or_default(path: &Path) -> Self {
        match Self::load(path) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "Could not load models.yaml — using empty catalog");
                Self::empty()
            }
        }
    }

    fn empty() -> Self {
        Self {
            defaults: ModelDefaults {
                temperature: 0.7,
                max_tokens: 4096,
                timeout_secs: 120,
            },
            models: HashMap::new(),
            default_system_prompt: Some(
                "You are agent-brain, an autonomous AI agent backed by a persistent \
                 knowledge graph. Think step-by-step and use available tools."
                    .to_string(),
            ),
        }
    }

    /// Return the system prompt for a named model.
    ///
    /// Falls back to `default_system_prompt`, then to a hard-coded fallback.
    pub fn resolve_system_prompt(&self, model_name: &str) -> String {
        if let Some(entry) = self.models.get(model_name) {
            if let Some(ref p) = entry.system_prompt {
                return p.trim().to_string();
            }
        }
        self.default_system_prompt
            .as_deref()
            .unwrap_or(
                "You are agent-brain, an autonomous AI agent backed by a persistent \
                 knowledge graph. Think step-by-step and use available tools.",
            )
            .trim()
            .to_string()
    }

    /// Sync all catalog entries into the DuckDB `model_registry` table.
    ///
    /// Clears the table first so stale entries from removed models are gone.
    /// Returns the number of models written.
    pub fn sync_to_duckdb(&self, db: &TelemetryClient) -> Result<usize> {
        db.clear_model_registry()?;
        let mut count = 0usize;
        for (name, entry) in &self.models {
            let caps_json = serde_json::to_string(&entry.capabilities)?;
            db.upsert_model(
                name,
                &entry.provider,
                &entry.model,
                entry.context_window,
                entry.cost_per_1k_input,
                entry.cost_per_1k_output,
                &caps_json,
                entry.system_prompt.as_deref(),
                entry.temperature.or(Some(self.defaults.temperature)),
                entry.max_tokens.or(Some(self.defaults.max_tokens)),
                entry.timeout_secs.or(Some(self.defaults.timeout_secs)),
            )?;
            count += 1;
        }
        info!(count, "Synced model catalog to DuckDB");
        Ok(count)
    }
}
