//! Per-step model routing.
//!
//! A `ChainStep` may declare `required_capabilities`. At job execution time
//! the queue resolves the best catalog model that satisfies them **and that the
//! routing policy admits for the step's purpose and data class**
//! (`services/routing.rs`), and scopes the job's LLM calls to it via the
//! `SELECTED_LLM` task-local. Steps without capabilities keep today's
//! behaviour (background jobs pinned to local).
//!
//! Which providers are eligible is policy, not code: `ROUTING_BILLING` (or the
//! legacy `CLOUD_TIER` mapping `0 → local`, `1 → local,free`, `2 → +paid`),
//! `ROUTING_PAID_FOR`, and each provider's registry declaration. Private work
//! only reaches `no_log` providers.
//!
//! Selection inherits `select_models` ordering: cost ascending, then
//! `selection_rank` ascending, then context window descending. Since every
//! local and Ollama-Cloud entry costs $0, `selection_rank` — set per model in
//! `models.yaml` — is what actually decides these steps; see the block comment
//! there for the bands and for the incident that motivated the field.
//! If no candidate survives the policy the caller falls back to normal
//! routing — a missing model degrades the step, never blocks it — and normal
//! routing is itself judged by the policy in `SharedLlm`.
//!
//! A step (or a `run_agent` call) may instead **pin** a model by name
//! ([`pinned_config`]). A pin is matched exactly against the catalog, never by
//! substring, and is not a preference: an unknown or refused pin fails the
//! work with the reason rather than quietly running on something else.

use tracing::{debug, info, warn};

use crate::llm::{LlmConfig, LlmProviderType};
use crate::provider_registry::{self, ProviderEntry, ProviderKind};
use crate::routing::{RouteContext, RoutingPolicy, route};
use agent_brain_repository::TelemetryClient;

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
            // Ollama Cloud keeps its own provider type: config parsing, the chat
            // loop dispatch and the REST provider list still name it. Who may use
            // it is the routing policy's call (`services/routing.rs`), by registry
            // name, for every OpenAI-wire provider alike.
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
                .and_then(|s| crate::llm::validate_go_duration(s.trim()));
            // Same reason as base_url/keep_alive: the router builds on
            // `LlmConfig::default()`, whose `num_ctx` is None — which means
            // Ollama's silent 4096. A capability-routed local model (granite4 at
            // selection_rank 120) landed here and answered `reason` off a clipped
            // prompt 58 times in 30 days. See `llm::ollama_num_ctx`.
            cfg.num_ctx = Some(crate::llm::ollama_num_ctx());
        }
    }
    cfg.provider_name = Some(name.to_string());
    cfg
}

/// One row of the model catalog, as much of it as a pin needs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogModel {
    pub name: String,
    pub provider: String,
    pub model: String,
}

/// The catalog's models (empty when the catalog cannot be read).
pub fn catalog_models(telemetry: &TelemetryClient) -> Vec<CatalogModel> {
    telemetry
        .list_models()
        .unwrap_or_default()
        .iter()
        .filter_map(|m| {
            Some(CatalogModel {
                name: m["name"].as_str()?.to_string(),
                provider: m["provider"].as_str()?.to_string(),
                model: m["model"].as_str()?.to_string(),
            })
        })
        .collect()
}

/// The catalog row a pinned name means: its `model` id or its `name`, ignoring
/// case. Exact only — "gemma4" must not quietly become `gemma4:31b-cloud`.
pub fn find_model<'a>(rows: &'a [CatalogModel], wanted: &str) -> Option<&'a CatalogModel> {
    let wanted = wanted.trim();
    if wanted.is_empty() {
        return None;
    }
    rows.iter()
        .find(|m| m.model.eq_ignore_ascii_case(wanted))
        .or_else(|| rows.iter().find(|m| m.name.eq_ignore_ascii_case(wanted)))
}

/// The step `provider_hint` a provider implies: `ollama` (the queue's local
/// lane) for any provider billed as local, else the provider's own name. The
/// queue gives `anthropic` and `gemini` their own semaphores; every other name
/// shares the default one, as a cloud step with no hint always has.
pub fn provider_hint_for(provider: &str) -> String {
    let name = provider_registry::canonical_name(provider);
    match provider_registry::get(&name) {
        Some(e) if e.billing != provider_registry::Billing::Local => name,
        // A provider the registry does not know is called as local Ollama
        // (`config_for_catalog_entry`), so it rides the local lane too.
        _ => "ollama".to_string(),
    }
}

/// Whether a step's written `provider_hint` agrees with its pinned model's
/// provider: the derived hint itself, or the provider's own name.
pub fn hint_agrees(hint: &str, provider: &str) -> bool {
    let hint = hint.trim();
    hint.is_empty()
        || hint.eq_ignore_ascii_case(&provider_hint_for(provider))
        || provider_registry::canonical_name(hint) == provider_registry::canonical_name(provider)
}

/// A pinned model's config, admitted by the routing policy for `ctx`. `Err`
/// says why it cannot run: not in the catalog, or the policy's refusal.
pub fn pinned_config(
    rows: &[CatalogModel],
    wanted: &str,
    base: &LlmConfig,
    ctx: RouteContext,
) -> Result<LlmConfig, String> {
    let Some(row) = find_model(rows, wanted) else {
        return Err(format!(
            "model '{}' is not in the model catalog (models.yaml)",
            wanted.trim()
        ));
    };
    let cfg = config_for_catalog_entry(base, &row.provider, &row.model);
    RoutingPolicy::from_env()
        .admit(&cfg, ctx)
        .map_err(|reason| {
            format!(
                "the routing policy refused model '{}' ({}) for {} {} work: {reason}",
                row.model, row.provider, ctx.data_class, ctx.purpose
            )
        })?;
    Ok(cfg)
}

/// Resolve the model config for a step's `required_capabilities`, or `None`
/// when nothing in the catalog satisfies them under the routing policy for this
/// work (`ctx`: purpose + data class).
pub fn resolve_model_config(
    telemetry: &TelemetryClient,
    required_capabilities: &[String],
    base: &LlmConfig,
    ctx: RouteContext,
) -> Option<LlmConfig> {
    let rows = match telemetry.select_models(required_capabilities, None, None) {
        Ok(c) => c,
        Err(e) => {
            debug!(error = %e, "Model routing: catalog query failed");
            return None;
        }
    };

    let policy = RoutingPolicy::from_env();

    // Observed availability: the catalog says what *should* be free, the
    // ledger records what the provider actually served. A model that recently
    // returned "subscription_required" (Ollama Cloud's undocumented paid
    // tier) is skipped for a week rather than retried into dead-letter.
    let unavailable = telemetry
        .models_with_recent_errors("subscription_required", 24 * 7)
        .unwrap_or_default();

    let plan = route(
        &rows,
        ctx,
        &policy,
        provider_registry::get,
        |e| e.key_configured(),
        crate::budget::verdict,
        &unavailable,
    );

    let Some(chosen) = plan.candidates.first() else {
        if !plan.rejected.is_empty() {
            warn!(
                capabilities = ?required_capabilities,
                purpose = %ctx.purpose,
                data_class = %ctx.data_class,
                policy = %policy.source,
                rejected = %plan.rejected_before(usize::MAX),
                "Model routing: the policy admits no capable model — step uses normal routing"
            );
        }
        return None;
    };

    // Build a full config for the chosen provider. Embedding settings are
    // inherited from the base config — embeddings always stay local.
    let cfg = config_for_catalog_entry(base, &chosen.provider, &chosen.model);

    // `rank` and `runner_up` are logged because the incident that produced
    // `selection_rank` was invisible at runtime: a new catalog entry captured
    // every reasoning step and the only evidence was slower jobs. `skipped`
    // names every better-ranked model the policy refused, with the reason, so
    // "why didn't it use X?" is answered in the same line.
    info!(
        capabilities = ?required_capabilities,
        provider = %chosen.provider,
        model = %chosen.model,
        rank = chosen.selection_rank,
        runner_up = plan
            .candidates
            .get(1)
            .map(|c| c.model.as_str())
            .unwrap_or("-"),
        purpose = %ctx.purpose,
        data_class = %ctx.data_class,
        policy = %policy.source,
        skipped = %plan.rejected_before(chosen.position),
        "Model routing: resolved step model"
    );
    Some(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(name: &str, provider: &str, model: &str) -> CatalogModel {
        CatalogModel {
            name: name.into(),
            provider: provider.into(),
            model: model.into(),
        }
    }

    #[test]
    fn a_pin_matches_exactly_never_by_substring() {
        let rows = vec![
            row("claude-haiku-5-5", "anthropic", "claude-haiku-5-5"),
            row("gemma4:31b-cloud", "ollama-cloud", "gemma4:31b-cloud"),
            row("gemma4:latest", "ollama", "gemma4:latest"),
        ];
        assert_eq!(
            find_model(&rows, "GEMMA4:latest").unwrap().provider,
            "ollama"
        );
        assert_eq!(
            find_model(&rows, "claude-haiku-5-5").unwrap().model,
            "claude-haiku-5-5"
        );
        assert!(find_model(&rows, "gemma4").is_none());
        assert!(find_model(&rows, "  ").is_none());
        let ctx = RouteContext::new(crate::routing::Purpose::Background, Default::default());
        let err = pinned_config(&rows, "gemma4", &LlmConfig::default(), ctx).unwrap_err();
        assert!(err.contains("not in the model catalog"), "{err}");
    }

    #[test]
    fn a_hint_must_agree_with_the_pinned_provider() {
        // Built-in registry: ollama and llamacpp are local, ollama-cloud is not.
        assert_eq!(provider_hint_for("ollama"), "ollama");
        assert_eq!(provider_hint_for("llamacpp"), "ollama");
        assert_eq!(provider_hint_for("ollama-cloud"), "ollama-cloud");
        assert_eq!(provider_hint_for("anthropic"), "anthropic");
        // The Zero Trust schedule: a host-local model on a step marked cloud.
        assert!(!hint_agrees("ollama-cloud", "ollama"));
        assert!(hint_agrees("ollama", "ollama"));
        assert!(hint_agrees("ollama", "llamacpp"));
        assert!(hint_agrees("", "anthropic"));
        assert!(hint_agrees("anthropic", "anthropic"));
        // A cloud model on a step marked local.
        assert!(!hint_agrees("ollama", "ollama-cloud"));
    }

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
