//! Model catalog loader — reads `models.yaml` and syncs entries into DuckDB.

use std::collections::HashMap;
use std::path::Path;

use anyhow::Result;
use serde::Deserialize;
use tracing::{info, warn};

use crate::repository::TelemetryClient;
use crate::services::provider_registry::{self, ProviderEntry};

/// Context window forced onto a LOCAL chat-fallback rung. Ollama serves 4096 by
/// default (silently truncating), so the local safety net needs a window that
/// can hold a real chat turn. Sized to cover the current ~20k-token tool-using
/// turn with generation headroom; granite4 (the local rung) carries a 128k
/// catalog window, and this stays well under it. Lower it if the turn size is
/// trimmed and GPU memory is tight.
const CHAT_LOCAL_FALLBACK_NUM_CTX: u32 = 32768;

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
    /// Ordered catalog keys a background `generate()` walks after its primary
    /// and the active model, before the local model. See the block comment on
    /// `background_fallback_ladder` in `models.yaml`.
    #[serde(default)]
    pub background_fallback_ladder: Vec<String>,
    /// The `providers:` section — how to reach each provider and what kind of
    /// provider it is. Laid over the built-in table in
    /// [`provider_registry::builtin`], so an entry left out keeps today's
    /// endpoint. Installed process-wide by [`Self::install_providers`].
    #[serde(default)]
    pub providers: HashMap<String, ProviderEntry>,
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
    /// This model's own rate limits, counted apart from other models of the
    /// same provider (Google's free-tier quotas are per model, per project).
    /// The provider's `limits` still apply on top.
    #[serde(default)]
    pub limits: Option<provider_registry::Limits>,
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
            background_fallback_ladder: Vec::new(),
            providers: HashMap::new(),
        }
    }

    /// Install this catalog's providers as the process-wide registry and warn
    /// about every declaration that cannot work. Returns the provider count.
    pub fn install_providers(&self) -> usize {
        let n = provider_registry::install(&self.providers);
        let limited =
            provider_registry::install_model_limits(self.models.values().filter_map(|m| {
                m.limits
                    .clone()
                    .map(|l| (m.provider.clone(), m.model.clone(), l))
            }));
        for issue in self.provider_issues() {
            warn!("{issue}");
        }
        info!(
            providers = n,
            models_with_limits = limited,
            "Installed provider registry"
        );
        n
    }

    /// Problems with the provider declarations this catalog produces: a model
    /// naming a provider the registry does not know (it would be called as
    /// local Ollama), a paid provider with no spend cap, an OpenAI-compatible
    /// provider with no endpoint. Sorted, so the startup log is stable.
    pub fn provider_issues(&self) -> Vec<String> {
        let registry = provider_registry::merged(&self.providers);
        let mut out = Vec::new();
        for (name, entry) in &registry {
            for issue in entry.issues() {
                out.push(format!("provider {name}: {issue}"));
            }
        }
        let mut models: Vec<_> = self.models.iter().collect();
        models.sort_by(|a, b| a.0.cmp(b.0));
        for (key, m) in models {
            if !registry.contains_key(&provider_registry::canonical_name(&m.provider)) {
                out.push(format!(
                    "model {key}: provider '{}' is not in the provider registry — \
                     it would be called as local Ollama",
                    m.provider
                ));
            }
        }
        out
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
        self.resolve_ladder(
            "chat_fallback_ladder",
            &self.chat_fallback_ladder,
            base,
            active_model,
        )
    }

    /// Resolve `background_fallback_ladder` the same way as the chat ladder.
    pub fn resolve_background_fallback_ladder(
        &self,
        base: &crate::services::LlmConfig,
        active_model: &str,
    ) -> Vec<crate::services::LlmConfig> {
        self.resolve_ladder(
            "background_fallback_ladder",
            &self.background_fallback_ladder,
            base,
            active_model,
        )
    }

    fn resolve_ladder(
        &self,
        ladder: &str,
        names: &[String],
        base: &crate::services::LlmConfig,
        active_model: &str,
    ) -> Vec<crate::services::LlmConfig> {
        let mut out = Vec::new();
        let mut seen: Vec<String> = vec![active_model.to_string()];

        for name in names {
            let Some(entry) = self.models.get(name) else {
                warn!(
                    model = %name,
                    ladder,
                    "fallback ladder names a model that is not in the catalog — skipping"
                );
                continue;
            };
            if seen.iter().any(|s| s == &entry.model) {
                continue;
            }
            seen.push(entry.model.clone());
            let mut cfg = crate::services::model_router::config_for_catalog_entry(
                base,
                &entry.provider,
                &entry.model,
            );
            // A LOCAL fallback rung must be given a real context window. Ollama
            // silently serves 4096 tokens by default, which truncates a real
            // (~20k-token) chat turn to a fragment — the local safety net then
            // answers from garbage (the 2026-09-09 "apply to #N → generic
            // refusal" incident). Cloud rungs carry the model's catalog window;
            // only the local rung needs this override.
            if cfg.provider == crate::services::LlmProviderType::Ollama {
                cfg = cfg.with_num_ctx(CHAT_LOCAL_FALLBACK_NUM_CTX);
            }
            out.push(cfg);
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
            limits: None,
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
        assert_eq!(
            resolved[0].num_ctx,
            Some(CHAT_LOCAL_FALLBACK_NUM_CTX),
            "a local rung must get a real context window, not Ollama's 4096 default"
        );
    }

    #[test]
    fn a_cloud_rung_keeps_the_default_context_window() {
        let c = catalog(
            &["cloud"],
            &[("cloud", "ollama-cloud", "gpt-oss:120b-cloud")],
        );
        let resolved = c.resolve_chat_fallback_ladder(&LlmConfig::default(), "none");
        assert_eq!(resolved.len(), 1);
        assert_eq!(
            resolved[0].provider,
            crate::services::LlmProviderType::OllamaCloud
        );
        assert!(
            resolved[0].num_ctx.is_none(),
            "the num_ctx override is local-only; cloud rungs carry the catalog window"
        );
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

    /// A profile's `fallback_models` must name catalog models: an unknown name
    /// is skipped at runtime with a WARN, so a typo would silently shorten the
    /// ladder exactly when it is needed.
    #[test]
    fn every_profile_fallback_model_is_in_the_catalog() {
        use crate::services::context_builder::ContextProfile;
        let c = real_catalog();
        let known = |name: &str| c.models.iter().any(|(k, m)| k == name || m.model == name);
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../contexts");
        let mut missing = Vec::new();
        for entry in std::fs::read_dir(&dir).expect("contexts dir") {
            let path = entry.unwrap().path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let Ok(p) = serde_yaml::from_str::<ContextProfile>(&text) else {
                continue; // boot/init protocols are not profiles
            };
            for m in &p.fallback_models {
                if !known(m) {
                    missing.push(format!("{}: {m}", p.name));
                }
            }
        }
        assert!(
            missing.is_empty(),
            "fallback_models not in models.yaml: {missing:?}"
        );
    }

    /// Google's quotas are per model, so the `gemini` provider declares no rpm
    /// and each model carries its own. A gemini entry without one would be
    /// unmetered in the budget and admitted to sub-agent loops (the 5-RPM trap).
    #[test]
    fn every_gemini_model_declares_its_own_rate_limit() {
        let c = real_catalog();
        let unlimited: Vec<&String> = c
            .models
            .iter()
            .filter(|(_, m)| m.provider == "gemini")
            .filter(|(_, m)| m.limits.as_ref().and_then(|l| l.rpm).is_none())
            .map(|(k, _)| k)
            .collect();
        assert!(
            unlimited.is_empty(),
            "gemini models without limits.rpm: {unlimited:?}"
        );
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

    /// Tier 2 adds the keyed paid providers. The Gemini free tier bills nothing,
    /// so `gemini-2.5-flash` is catalogued at cost 0.0 and rank 5 — below the
    /// ollama-cloud band — which is what makes it take every reasoning step when
    /// `CLOUD_TIER=2`. This is the deployed routing; lock it by name like the
    /// tier-1 guard above so a future catalog edit that steals `reasoning` from
    /// Gemini (or hands it to a paid model on cost) is caught at build time.
    const TIER2: &[&str] = &["ollama", "ollama-cloud", "gemini", "anthropic"];

    #[test]
    fn reasoning_at_tier2_routes_to_gpt_oss() {
        // 2026-10-02: gemini-2.5-flash moved behind the ollama-cloud band after
        // a week of 503s and 31s reasoning calls; gpt-oss had none.
        assert_eq!(
            routes_to(&real_catalog(), "reasoning", TIER2),
            "gpt-oss:120b-cloud"
        );
    }

    /// `deep_reasoning` is how a step asks to spend money: `cost ASC` sorts
    /// every paid model behind every $0 one, so a paid model only wins a
    /// capability nothing free holds. A second holder — any $0 model — would
    /// silently take every paid synthesis step back, and a spent budget would
    /// stop being the only reason it runs on gpt-oss.
    #[test]
    fn deep_reasoning_is_held_by_sonnet_5_alone() {
        let c = real_catalog();
        let holders: Vec<_> = c
            .models
            .iter()
            .filter(|(_, e)| e.capabilities.iter().any(|x| x == "deep_reasoning"))
            .map(|(k, _)| k.as_str())
            .collect();
        assert_eq!(holders, vec!["claude-sonnet-5"]);
        assert_eq!(routes_to(&c, "deep_reasoning", TIER2), "claude-sonnet-5");
        assert_eq!(routes_to(&c, "deep_reasoning", TIER1), "");
    }

    /// Adding a paid model that also claims `reasoning` must not move the
    /// free reasoning route.
    #[test]
    fn a_paid_reasoner_does_not_capture_plain_reasoning() {
        assert_eq!(
            routes_to(&real_catalog(), "reasoning", TIER2),
            "gpt-oss:120b-cloud"
        );
    }

    /// `gemini-2.5-flash` deliberately does not claim `vision`, and the one
    /// Gemini entry that does (`gemini-3-flash`) is ranked far below
    /// gemma4:31b-cloud, so vision stays on the free ollama-cloud vision model
    /// even at tier 2 — no accidental capture from adding the flash models.
    #[test]
    fn vision_at_tier2_stays_on_the_ollama_cloud_vision_model() {
        assert_eq!(
            routes_to(&real_catalog(), "vision", TIER2),
            "gemma4:31b-cloud"
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

    /// Which model a capability step gets under the routing policy, through the
    /// real catalog ordering and the checked-in provider declarations. Keys are
    /// assumed present so the test does not depend on the environment.
    fn routes_under_policy(
        c: &ModelCatalog,
        capability: &str,
        class: crate::services::routing::DataClass,
        policy: &crate::services::routing::RoutingPolicy,
    ) -> Vec<String> {
        use crate::services::routing::{Purpose, RouteContext, route};
        let db = TelemetryClient::new(":memory:").unwrap();
        c.sync_to_duckdb(&db).unwrap();
        let rows = db
            .select_models(&[capability.to_string()], None, None)
            .unwrap();
        let registry = provider_registry::merged(&c.providers);
        route(
            &rows,
            RouteContext::new(Purpose::Background, class),
            policy,
            |n| registry.get(n).cloned(),
            |_| true,
            |_, _, _| Ok(()),
            &[],
        )
        .candidates
        .into_iter()
        .map(|c| c.name)
        .collect()
    }

    fn deployed_policy() -> crate::services::routing::RoutingPolicy {
        // Compose: CLOUD_TIER=2, ROUTING_PAID_FOR unset.
        crate::services::routing::RoutingPolicy::from_values(None, Some("2"), None)
    }

    #[test]
    fn private_reasoning_stays_on_a_no_log_model() {
        use crate::services::routing::DataClass;
        let got = routes_under_policy(
            &real_catalog(),
            "reasoning",
            DataClass::Private,
            &deployed_policy(),
        );
        assert_eq!(got.first().map(String::as_str), Some("gpt-oss:120b-cloud"));
    }

    #[test]
    fn public_reasoning_routes_to_gpt_oss_like_private() {
        use crate::services::routing::DataClass;
        let got = routes_under_policy(
            &real_catalog(),
            "reasoning",
            DataClass::Public,
            &deployed_policy(),
        );
        assert_eq!(got.first().map(String::as_str), Some("gpt-oss:120b-cloud"));
        // Gemini stays reachable as a later rung, ahead of local.
        assert!(got.iter().any(|m| m == "gemini-2.5-flash"), "{got:?}");
    }

    #[test]
    fn private_work_never_reaches_a_training_provider_for_any_capability() {
        // Unless the owner opened that provider to background work by name
        // (`private_ok_for`, models.yaml) — Mistral and OpenRouter since
        // 2026-10-10. The override is the only way through.
        use crate::services::provider_registry::DataPolicy;
        use crate::services::routing::{DataClass, Purpose};
        let c = real_catalog();
        let registry = provider_registry::merged(&c.providers);
        let mut caps: Vec<String> = c
            .models
            .values()
            .flat_map(|m| m.capabilities.clone())
            .collect();
        caps.sort();
        caps.dedup();
        for cap in caps {
            for name in routes_under_policy(&c, &cap, DataClass::Private, &deployed_policy()) {
                let provider = provider_registry::canonical_name(&c.models[&name].provider);
                let p = &registry[&provider];
                assert!(
                    p.data_policy == DataPolicy::NoLog
                        || p.private_ok_for.contains(&Purpose::Background),
                    "private {cap} work admitted {name} on {provider}"
                );
            }
        }
    }

    #[test]
    fn paid_models_stay_unroutable_until_a_purpose_may_spend() {
        use crate::services::routing::{DataClass, RoutingPolicy};
        let c = real_catalog();
        let paid = |policy: &RoutingPolicy| {
            routes_under_policy(&c, "reasoning", DataClass::Public, policy)
                .into_iter()
                .filter(|n| c.models[n].provider == "anthropic")
                .count()
        };
        assert_eq!(paid(&deployed_policy()), 0);
        let background = RoutingPolicy::from_values(None, Some("2"), Some("background"));
        assert!(
            paid(&background) > 0,
            "opting background in admits the capped paid provider"
        );
    }

    #[test]
    fn the_checked_in_provider_declarations_have_no_issues() {
        // Every model names a known provider, every paid provider carries a
        // spend cap, every OpenAI-compatible provider has an endpoint.
        let issues = real_catalog().provider_issues();
        assert!(issues.is_empty(), "provider issues: {issues:#?}");
    }

    #[test]
    fn the_training_free_tiers_stay_declared_may_train() {
        // The privacy rule keys off this field. Flipping one of these to
        // no_log without re-reading the provider's terms would send private
        // work to a provider that trains on it.
        use crate::services::provider_registry::DataPolicy;
        let registry = provider_registry::merged(&real_catalog().providers);
        for name in ["mistral", "openrouter", "gemini"] {
            assert_eq!(
                registry[name].data_policy,
                DataPolicy::MayTrain,
                "{name} data_policy"
            );
        }
    }

    /// Private reasoning keeps preferring Ollama Cloud; Mistral is the next
    /// cloud rung, ahead of every local model, and Gemini (no override for
    /// background) is never admitted.
    #[test]
    fn private_reasoning_overflows_to_mistral_before_local() {
        use crate::services::routing::DataClass;
        let got = routes_under_policy(
            &real_catalog(),
            "reasoning",
            DataClass::Private,
            &deployed_policy(),
        );
        let pos = |m: &str| got.iter().position(|x| x == m);
        assert_eq!(got.first().map(String::as_str), Some("gpt-oss:120b-cloud"));
        let mistral = pos("ministral-14b-latest").expect("mistral admitted");
        let local = pos("gemma4:latest").expect("local admitted");
        assert!(mistral < local, "{got:?}");
        assert!(pos("gemini-2.5-flash").is_none(), "{got:?}");
    }

    #[test]
    fn every_background_fallback_rung_resolves() {
        let c = real_catalog();
        for name in &c.background_fallback_ladder {
            assert!(
                c.models.contains_key(name),
                "background ladder names unknown model {name}"
            );
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
