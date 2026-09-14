//! Per-step model routing — Phase 1 of the Agent Constructor plan.
//!
//! A `ChainStep` may declare `required_capabilities`. At job execution time
//! the queue resolves the cheapest catalog model that satisfies them *within
//! the active cloud tier* and scopes the job's LLM calls to it via the
//! `SELECTED_LLM` task-local. Steps without capabilities keep today's
//! behavior (background jobs pinned to local Ollama).
//!
//! Cloud tiers (env `CLOUD_TIER`, default `1`):
//! - `0` — local Ollama only.
//! - `1` — local + Ollama Cloud models with $0 catalog cost (free tier).
//!   Requires `OLLAMA_API_KEY`; candidates without a usable key are skipped.
//! - `2` — any provider with a configured API key ("income mode").
//!
//! Selection inherits `select_models` ordering: cost ascending, then
//! `selection_rank` ascending, then context window descending. Since every
//! local and Ollama-Cloud entry costs $0, `selection_rank` — set per model in
//! `models.yaml` — is what actually decides these steps; see the block comment
//! there for the bands and for the incident that motivated the field.
//! If no candidate survives the tier filter the caller falls back to normal
//! routing — a missing model degrades the step, never blocks it.

use tracing::{debug, info};

use crate::repository::TelemetryClient;
use crate::services::llm::{LlmConfig, LlmProviderType};
use crate::services::provider_registry::{self, ProviderEntry, ProviderKind};

/// Read the active cloud tier from `CLOUD_TIER` (default 1).
pub fn cloud_tier() -> u8 {
    std::env::var("CLOUD_TIER")
        .ok()
        .and_then(|v| v.trim().parse::<u8>().ok())
        .map(|t| t.min(2))
        .unwrap_or(1)
}

/// Whether a catalog candidate is usable at the given tier.
/// `has_key` closures keep this testable without touching the environment.
fn candidate_allowed(
    provider: &str,
    cost_in: f64,
    cost_out: f64,
    tier: u8,
    has_ollama_key: bool,
    has_anthropic_key: bool,
    has_gemini_key: bool,
) -> bool {
    match provider {
        "ollama" => true,
        "ollama-cloud" | "ollamacloud" => {
            has_ollama_key && (tier == 2 || (tier == 1 && cost_in == 0.0 && cost_out == 0.0))
        }
        "anthropic" => tier == 2 && has_anthropic_key,
        "gemini" => tier == 2 && has_gemini_key,
        _ => false,
    }
}

/// Turn a catalog entry's `provider` + `model` into a full [`LlmConfig`].
///
/// The endpoint and the NAME of the key's env var come from the provider
/// registry (`providers:` in `models.yaml`, over the built-in table in
/// [`provider_registry`]); the key itself only ever comes from the environment,
/// because `models.yaml` is checked in. Everything else — embedding settings
/// above all — is inherited from `base`, so a re-pointed config keeps
/// embeddings local no matter which provider it names.
///
/// Shared by capability routing ([`resolve_model_config`]), the `/chat`
/// fallback ladder and profile pins, which all answer the same question: *this
/// catalog row is the model now; what does it take to call it?* A provider the
/// registry does not know is called as local Ollama, as it always was; startup
/// names every such model (`ModelCatalog::provider_issues`).
pub fn config_for_catalog_entry(base: &LlmConfig, provider: &str, model: &str) -> LlmConfig {
    let name = provider_registry::canonical_name(provider);
    match provider_registry::get(&name) {
        Some(entry) => config_for_provider(base, &name, &entry, model),
        None => {
            let local = provider_registry::get("ollama")
                .unwrap_or_else(|| provider_registry::builtin()["ollama"].clone());
            config_for_provider(base, "ollama", &local, model)
        }
    }
}

/// The local Ollama endpoint embeddings are pinned to, whatever generates.
fn local_embed_url() -> String {
    std::env::var("OLLAMA_LOCAL_URL")
        .ok()
        .filter(|v| !v.trim().is_empty())
        .unwrap_or_else(|| "http://localhost:11434".to_string())
}

/// [`config_for_catalog_entry`] for a resolved registry entry. Reads only the
/// env vars the entry names, so it is testable with a hand-built entry.
pub fn config_for_provider(
    base: &LlmConfig,
    name: &str,
    entry: &ProviderEntry,
    model: &str,
) -> LlmConfig {
    let mut cfg = base.clone();
    cfg.model = model.to_string();
    match entry.kind {
        ProviderKind::OpenaiCompat => {
            // Ollama Cloud keeps its own provider type: CLOUD_TIER and the
            // sub-agent clamp still recognise it by type, and the registry must
            // not change who may use it. Every other OpenAI-wire API is the
            // generic type, which neither of those admits.
            cfg.provider = if name == "ollama-cloud" {
                LlmProviderType::OllamaCloud
            } else {
                LlmProviderType::OpenAiCompat
            };
            cfg.base_url = entry.base_url();
            cfg.api_key = entry.api_key();
            // A remote OpenAI-wire API has no bge-m3; embeddings stay local.
            cfg.embed_base_url.get_or_insert_with(local_embed_url);
        }
        ProviderKind::Anthropic => {
            cfg.provider = LlmProviderType::Anthropic;
            cfg.base_url = entry.base_url();
            cfg.api_key = entry.api_key();
        }
        ProviderKind::Gemini => {
            cfg.provider = LlmProviderType::Gemini;
            cfg.base_url = entry.base_url();
            cfg.api_key = entry.api_key();
        }
        ProviderKind::Llamacpp => {
            // The llama.cpp `llama-server` sidecar (OpenAI-compatible). base_url
            // points at LLAMACPP_URL regardless of LOCAL_LLM_BACKEND, because the
            // caller asked for this provider explicitly (a profile/catalog pin,
            // not the background backend switch). A single-model llama-server
            // cannot serve embeddings, so embeddings stay pinned to host Ollama —
            // mirroring build_local_llm_config.
            cfg.provider = LlmProviderType::LlamaCpp;
            cfg.base_url = entry.base_url();
            cfg.embed_base_url = Some(local_embed_url());
            cfg.api_key = None;
        }
        ProviderKind::Ollama => {
            cfg.provider = LlmProviderType::Ollama;
            cfg.base_url = entry.base_url();
            cfg.api_key = None;
            // The router is called with `LlmConfig::default()` as base, so
            // keep_alive has to be sourced here like base_url and api_key are.
            cfg.keep_alive = std::env::var("OLLAMA_KEEP_ALIVE")
                .ok()
                .and_then(|s| crate::config::validate_go_duration(s.trim()));
            // Same reason as base_url/keep_alive: the router builds on
            // `LlmConfig::default()`, whose `num_ctx` is None — which means
            // Ollama's silent 4096. A capability-routed local model (granite4 at
            // selection_rank 120) landed here and answered `reason` off a clipped
            // prompt 58 times in 30 days. See `llm::ollama_num_ctx`.
            cfg.num_ctx = Some(crate::services::llm::ollama_num_ctx());
        }
    }
    cfg.provider_name = Some(name.to_string());
    cfg
}

/// Resolve the model config for a step's `required_capabilities`, or `None`
/// when nothing in the catalog satisfies them within the active tier.
pub fn resolve_model_config(
    telemetry: &TelemetryClient,
    required_capabilities: &[String],
    base: &LlmConfig,
) -> Option<LlmConfig> {
    let candidates = match telemetry.select_models(required_capabilities, None, None) {
        Ok(c) => c,
        Err(e) => {
            debug!(error = %e, "Model routing: catalog query failed");
            return None;
        }
    };

    let tier = cloud_tier();
    let has_ollama_key = std::env::var("OLLAMA_API_KEY").is_ok();
    let has_anthropic_key = std::env::var("ANTHROPIC_API_KEY").is_ok();
    let has_gemini_key = std::env::var("GEMINI_API_KEY").is_ok();

    // Observed availability: the catalog says what *should* be free, the
    // ledger records what the provider actually served. A model that recently
    // returned "subscription_required" (Ollama Cloud's undocumented paid
    // tier) is skipped for a week rather than retried into dead-letter.
    let unavailable = telemetry
        .models_with_recent_errors("subscription_required", 24 * 7)
        .unwrap_or_default();

    let chosen = candidates.iter().find(|c| {
        let model = c["model"].as_str().unwrap_or("");
        !unavailable.iter().any(|u| u == model)
            && candidate_allowed(
                c["provider"].as_str().unwrap_or(""),
                c["cost_per_1k_input"].as_f64().unwrap_or(f64::MAX),
                c["cost_per_1k_output"].as_f64().unwrap_or(f64::MAX),
                tier,
                has_ollama_key,
                has_anthropic_key,
                has_gemini_key,
            )
    })?;

    let provider = chosen["provider"].as_str().unwrap_or("ollama");
    let model = chosen["model"].as_str().unwrap_or("").to_string();
    if model.is_empty() {
        return None;
    }

    // Build a full config for the chosen provider. Embedding settings are
    // inherited from the base config — embeddings always stay local.
    let cfg = config_for_catalog_entry(base, provider, &model);

    // `rank` and `runner_up` are logged because the incident that produced
    // `selection_rank` was invisible at runtime: a new catalog entry captured
    // every reasoning step and the only evidence was slower jobs. Naming the
    // winner alongside what it beat makes a silent reorder greppable.
    info!(
        capabilities = ?required_capabilities,
        provider = provider,
        model = %model,
        rank = chosen["selection_rank"].as_i64().unwrap_or(-1),
        runner_up = candidates
            .iter()
            .find(|c| c["model"].as_str() != Some(model.as_str()))
            .and_then(|c| c["model"].as_str())
            .unwrap_or("-"),
        tier = tier,
        "Model routing: resolved step model"
    );
    Some(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn llamacpp_provider_resolves_to_the_sidecar() {
        // A profile/catalog pin with provider `llamacpp` must produce a LlamaCpp
        // config (routed to the sidecar), NOT collapse into the host-Ollama
        // default branch. Embeddings stay pinned (a single-model llama-server
        // can't embed) and no api key is sent (llama-server needs none).
        let base = LlmConfig::default();
        for name in ["llamacpp", "llama-cpp", "llama.cpp", "llama_cpp"] {
            let cfg = config_for_catalog_entry(&base, name, "gemma4-e4b");
            assert_eq!(
                cfg.provider,
                LlmProviderType::LlamaCpp,
                "provider for {name}"
            );
            assert_eq!(cfg.model, "gemma4-e4b");
            assert!(cfg.base_url.is_some(), "base_url set for {name}");
            assert!(
                cfg.embed_base_url.is_some(),
                "embed endpoint pinned for {name}"
            );
            assert!(cfg.api_key.is_none(), "no api key for {name}");
        }
    }

    #[test]
    fn tier0_allows_only_local() {
        assert!(candidate_allowed("ollama", 0.0, 0.0, 0, true, true, true));
        assert!(!candidate_allowed(
            "ollama-cloud",
            0.0,
            0.0,
            0,
            true,
            true,
            true
        ));
        assert!(!candidate_allowed(
            "anthropic",
            0.001,
            0.005,
            0,
            true,
            true,
            true
        ));
    }

    #[test]
    fn tier1_allows_free_cloud_with_key() {
        assert!(candidate_allowed(
            "ollama-cloud",
            0.0,
            0.0,
            1,
            true,
            false,
            false
        ));
        // No key — not usable even if free.
        assert!(!candidate_allowed(
            "ollama-cloud",
            0.0,
            0.0,
            1,
            false,
            false,
            false
        ));
        // Paid cloud model — locked at tier 1.
        assert!(!candidate_allowed(
            "ollama-cloud",
            0.001,
            0.002,
            1,
            true,
            false,
            false
        ));
        assert!(!candidate_allowed(
            "anthropic",
            0.001,
            0.005,
            1,
            true,
            true,
            true
        ));
    }

    #[test]
    fn tier2_allows_paid_providers_with_keys() {
        assert!(candidate_allowed(
            "anthropic",
            0.001,
            0.005,
            2,
            true,
            true,
            true
        ));
        assert!(candidate_allowed(
            "gemini", 0.0001, 0.0004, 2, true, true, true
        ));
        assert!(!candidate_allowed(
            "anthropic",
            0.001,
            0.005,
            2,
            true,
            false,
            true
        ));
        assert!(candidate_allowed(
            "ollama-cloud",
            0.002,
            0.004,
            2,
            true,
            false,
            false
        ));
    }

    #[test]
    fn unknown_provider_rejected() {
        assert!(!candidate_allowed("vllm", 0.0, 0.0, 2, true, true, true));
    }

    /// The training free tiers must not become reachable through CLOUD_TIER
    /// before the policy router can keep private work off them.
    #[test]
    fn may_train_providers_are_not_routable_through_cloud_tier() {
        for p in ["mistral", "openrouter"] {
            assert!(!candidate_allowed(p, 0.0, 0.0, 2, true, true, true), "{p}");
        }
    }

    #[test]
    fn a_generic_openai_compat_provider_gets_its_endpoint_and_name() {
        let entry: ProviderEntry = serde_yaml::from_str(
            "kind: openai_compat\nbilling: free\ndata_policy: may_train\n\
             base_url_default: https://api.mistral.ai\n\
             api_key_env: AGENT_BRAIN_TEST_KEY_THAT_IS_NEVER_SET",
        )
        .unwrap();
        let cfg = config_for_provider(
            &LlmConfig::default(),
            "mistral",
            &entry,
            "mistral-medium-latest",
        );
        assert_eq!(cfg.provider, LlmProviderType::OpenAiCompat);
        assert_eq!(cfg.base_url.as_deref(), Some("https://api.mistral.ai"));
        assert_eq!(cfg.model, "mistral-medium-latest");
        assert!(cfg.api_key.is_none(), "an unset key var yields no key");
        assert_eq!(cfg.provider_label(), "mistral");
        assert!(
            cfg.embed_base_url.is_some(),
            "embeddings stay local for a remote OpenAI-wire API"
        );
    }

    #[test]
    fn ollama_cloud_keeps_its_provider_type_under_any_spelling() {
        for name in ["ollama-cloud", "ollamacloud"] {
            let cfg = config_for_catalog_entry(&LlmConfig::default(), name, "gpt-oss:120b-cloud");
            assert_eq!(cfg.provider, LlmProviderType::OllamaCloud, "{name}");
            assert_eq!(cfg.provider_label(), "ollama-cloud");
            assert!(cfg.base_url.is_some());
        }
    }

    #[test]
    fn an_unknown_provider_is_still_called_as_local_ollama() {
        let cfg = config_for_catalog_entry(&LlmConfig::default(), "vllm", "some-model");
        assert_eq!(cfg.provider, LlmProviderType::Ollama);
        assert_eq!(cfg.provider_label(), "ollama");
        assert!(
            cfg.num_ctx.is_some(),
            "local Ollama never runs at its 4096 default"
        );
    }

    #[test]
    fn switching_provider_type_drops_a_stale_registry_name() {
        let cfg = config_for_catalog_entry(&LlmConfig::default(), "ollama-cloud", "m")
            .with_provider(LlmProviderType::Anthropic);
        assert_eq!(cfg.provider_label(), "anthropic");
    }
}
