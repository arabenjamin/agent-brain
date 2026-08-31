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
    /// Ordered catalog keys `/chat` walks when a turn produces nothing the user
    /// can see. See the block comment on `chat_fallback_ladder` in
    /// `models.yaml` for why the order is what it is, and
    /// [`Self::resolve_chat_fallback_ladder`] for how names become configs.
    #[serde(default)]
    pub chat_fallback_ladder: Vec<String>,
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

fn default_temperature() -> f64 {
    0.7
}
fn default_max_tokens() -> i64 {
    4096
}
fn default_timeout_secs() -> i64 {
    120
}

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
    /// Preference among models that a capability query ties on cost. Lower
    /// wins; unset sorts behind every ranked model. This is the field that
    /// decides which model a `required_capabilities` step actually gets, since
    /// every local and Ollama-Cloud entry costs $0 — see the ordering note on
    /// [`TelemetryClient::select_models`] and the `selection_rank` block
    /// comment in `models.yaml`.
    pub selection_rank: Option<i64>,
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
            chat_fallback_ladder: Vec::new(),
        }
    }

    /// Return the system prompt for a named model.
    ///
    /// Falls back to `default_system_prompt`, then to a hard-coded fallback.
    pub fn resolve_system_prompt(&self, model_name: &str) -> String {
        if let Some(entry) = self.models.get(model_name)
            && let Some(ref p) = entry.system_prompt
        {
            return p.trim().to_string();
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

    /// Resolve `chat_fallback_ladder` into ready-to-call configs, in order.
    ///
    /// `base` supplies everything the catalog does not carry — timeouts,
    /// temperature, and the embedding settings that must stay local whichever
    /// provider is named. `active_model` is the model `/chat` already tries
    /// first and is skipped here, so a ladder listing the active model does not
    /// spend a rung re-running the thing that just failed.
    ///
    /// A name with no catalog entry is **dropped with a warning** rather than
    /// guessed at. The ladder exists to be walked when something is already
    /// broken; a rung that resolves to a model the provider has never heard of
    /// turns one failure into two, and does it at the worst moment.
    pub fn resolve_chat_fallback_ladder(
        &self,
        base: &crate::services::LlmConfig,
        active_model: &str,
    ) -> Vec<crate::services::LlmConfig> {
        let mut out = Vec::new();
        let mut seen: Vec<String> = vec![active_model.to_string()];

        for name in &self.chat_fallback_ladder {
            let Some(entry) = self.models.get(name) else {
                warn!(
                    model = %name,
                    "chat_fallback_ladder names a model that is not in the catalog — skipping"
                );
                continue;
            };
            if seen.iter().any(|s| s == &entry.model) {
                continue;
            }
            seen.push(entry.model.clone());
            out.push(crate::services::model_router::config_for_catalog_entry(
                base,
                &entry.provider,
                &entry.model,
            ));
        }
        out
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
                entry.selection_rank,
            )?;
            count += 1;
        }
        info!(count, "Synced model catalog to DuckDB");
        Ok(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::services::LlmConfig;

    fn entry(provider: &str, model: &str) -> ModelEntry {
        ModelEntry {
            provider: provider.into(),
            model: model.into(),
            context_window: 131072,
            cost_per_1k_input: 0.0,
            cost_per_1k_output: 0.0,
            capabilities: vec![],
            system_prompt: None,
            temperature: None,
            max_tokens: None,
            timeout_secs: None,
            selection_rank: None,
        }
    }

    fn catalog(ladder: &[&str], models: &[(&str, &str, &str)]) -> ModelCatalog {
        let mut c = ModelCatalog::empty();
        c.chat_fallback_ladder = ladder.iter().map(|s| s.to_string()).collect();
        for (key, provider, model) in models {
            c.models.insert((*key).to_string(), entry(provider, model));
        }
        c
    }

    fn ladder_models(c: &ModelCatalog, active: &str) -> Vec<String> {
        c.resolve_chat_fallback_ladder(&LlmConfig::default(), active)
            .into_iter()
            .map(|c| c.model)
            .collect()
    }

    #[test]
    fn the_ladder_keeps_the_order_it_was_written_in() {
        let c = catalog(
            &["b", "a", "c"],
            &[
                ("a", "ollama-cloud", "a-model"),
                ("b", "ollama-cloud", "b-model"),
                ("c", "ollama", "c-model"),
            ],
        );
        // Order is a measured preference, not a set — resolving must not sort it.
        assert_eq!(ladder_models(&c, "none"), ["b-model", "a-model", "c-model"]);
    }

    /// The active model is already tried first, so a rung naming it would spend
    /// an attempt re-running exactly what just failed.
    #[test]
    fn the_active_model_is_not_a_rung() {
        let c = catalog(
            &["a", "b"],
            &[
                ("a", "ollama-cloud", "a-model"),
                ("b", "ollama-cloud", "b-model"),
            ],
        );
        assert_eq!(ladder_models(&c, "a-model"), ["b-model"]);
    }

    /// A rung the provider has never heard of turns one failure into two, at
    /// the exact moment something is already broken.
    #[test]
    fn a_name_with_no_catalog_entry_is_dropped() {
        let c = catalog(&["ghost", "a"], &[("a", "ollama-cloud", "a-model")]);
        assert_eq!(ladder_models(&c, "none"), ["a-model"]);
    }

    #[test]
    fn a_repeated_name_is_only_tried_once() {
        let c = catalog(
            &["a", "a-again", "b"],
            &[
                ("a", "ollama-cloud", "a-model"),
                ("a-again", "ollama-cloud", "a-model"),
                ("b", "ollama-cloud", "b-model"),
            ],
        );
        assert_eq!(ladder_models(&c, "none"), ["a-model", "b-model"]);
    }

    #[test]
    fn no_ladder_configured_means_no_fallback() {
        let c = catalog(&[], &[("a", "ollama-cloud", "a-model")]);
        assert!(ladder_models(&c, "none").is_empty());
    }

    /// The catalog names a provider; the endpoint and key come from the
    /// environment, so a checked-in file never carries a credential.
    #[test]
    fn a_local_rung_resolves_to_the_local_provider() {
        let c = catalog(&["local"], &[("local", "ollama", "gemma4:latest")]);
        let resolved = c.resolve_chat_fallback_ladder(&LlmConfig::default(), "none");
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].provider,
            crate::services::LlmProviderType::Ollama
        );
        assert_eq!(resolved[0].model, "gemma4:latest");
        assert!(resolved[0].api_key.is_none(), "a local rung needs no key");
    }

    // ── Guards against the real checked-in catalog ───────────────────────────
    //
    // The `selection_rank` incident was not a code bug — every line behaved as
    // written. It was a *catalog* edit whose routing consequence was invisible
    // until the quota ran out. So these assert on `models.yaml` itself: add a
    // model that captures a capability and the build says so, by name, before
    // it ever dispatches a job.

    fn real_catalog() -> ModelCatalog {
        ModelCatalog::load(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../models.yaml"),
        )
        .expect("models.yaml must parse")
    }

    /// Which model a capability step resolves to, through the **real** path:
    /// `models.yaml` → `sync_to_duckdb` → the `select_models` ordering SQL,
    /// then the provider filter `resolve_model_config` applies for the tier.
    ///
    /// Deliberately not a Rust reimplementation of the ordering. The bug being
    /// guarded lived in the interaction between a YAML edit and an ORDER BY
    /// clause; a test that recomputes the ordering in Rust would happily agree
    /// with itself while production disagreed with both.
    fn routes_to(c: &ModelCatalog, capability: &str, providers: &[&str]) -> String {
        let db = TelemetryClient::new(":memory:").unwrap();
        c.sync_to_duckdb(&db).unwrap();
        db.select_models(&[capability.to_string()], None, None)
            .unwrap()
            .into_iter()
            .find(|m| providers.contains(&m["provider"].as_str().unwrap_or("")))
            .map(|m| m["name"].as_str().unwrap_or("").to_string())
            .unwrap_or_default()
    }

    /// Tier 1 — local plus $0 Ollama Cloud. This is the deployed configuration,
    /// so these are the models the seven cloud-routed chain/schedule steps get.
    const TIER1: &[&str] = &["ollama", "ollama-cloud"];

    #[test]
    fn reasoning_routes_to_the_fastest_clean_cloud_model() {
        // 1.8s and 0/4 empty completions on the 2026-08-25 trials. The failure
        // being locked down: minimax-m3:cloud taking this on window size alone,
        // at 20.2s a call.
        assert_eq!(
            routes_to(&real_catalog(), "reasoning", TIER1),
            "gpt-oss:120b-cloud"
        );
    }

    #[test]
    fn vision_routes_to_a_model_that_actually_has_vision() {
        // Rank is global, so the vision winner is necessarily ranked below the
        // reasoning winner — the gpt-oss entries have no vision to offer.
        let c = real_catalog();
        let winner = routes_to(&c, "vision", TIER1);
        assert_eq!(winner, "gemma4:31b-cloud");
        assert!(c.models[&winner].capabilities.iter().any(|x| x == "vision"));
    }

    #[test]
    fn computation_still_routes_to_the_code_model() {
        // `execute_code` steps must reach a model that emits Python. Its rank
        // of 150 loses to every cloud entry, so this only holds while the
        // capability has exactly one holder — which is the point of the test.
        assert_eq!(
            routes_to(&real_catalog(), "computation", TIER1),
            "qwen2.5-coder:7b"
        );
    }

    #[test]
    fn tier_zero_reasoning_is_the_background_workhorse() {
        // With cloud filtered out, local must resolve deterministically.
        // Three local models tie at a 128000 window, so before ranking this
        // was decided by whatever order the rows came back in.
        assert_eq!(
            routes_to(&real_catalog(), "reasoning", &["ollama"]),
            "gemma4:latest"
        );
    }

    #[test]
    fn every_catalog_entry_declares_a_selection_rank() {
        // An unranked entry sorts last, which is safe but silent. Requiring the
        // field makes adding a model a deliberate statement about where it sits.
        let c = real_catalog();
        let unranked: Vec<_> = c
            .models
            .iter()
            .filter(|(_, e)| e.selection_rank.is_none())
            .map(|(k, _)| k.as_str())
            .collect();
        assert!(
            unranked.is_empty(),
            "models.yaml entries missing selection_rank: {unranked:?}"
        );
    }

    #[test]
    fn selection_ranks_are_unique() {
        // Two models sharing a rank fall through to context_window DESC, which
        // is the tiebreak this field was added to stop relying on.
        let c = real_catalog();
        let mut seen: HashMap<i64, &str> = HashMap::new();
        for (name, e) in &c.models {
            if let Some(r) = e.selection_rank
                && let Some(prev) = seen.insert(r, name)
            {
                panic!("selection_rank {r} used by both {prev} and {name}");
            }
        }
    }

    #[test]
    fn every_chat_fallback_rung_resolves() {
        // Already enforced at startup with a warning; asserted here because a
        // ladder is walked when something is already broken, and a dud rung
        // turns one failure into two at the worst possible moment.
        let c = real_catalog();
        for name in &c.chat_fallback_ladder {
            assert!(
                c.models.contains_key(name),
                "ladder names unknown model {name}"
            );
        }
    }
}
