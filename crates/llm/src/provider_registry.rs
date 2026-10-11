//! Provider registry — which LLM endpoints exist, how to reach them, and what
//! kind of provider each one is. Phase 1 of `project-docs/PROVIDER_ROUTING_PLAN.md`.
//!
//! A model in `models.yaml` names a `provider:`; the `providers:` section says
//! how to call it (`kind`, endpoint, and the **name** of the env var holding
//! its key) and what calling it means (`billing`, `data_policy`, `limits`,
//! `spend`). Adding an OpenAI-compatible API — free or paid — is a YAML entry,
//! not code.
//!
//! Phase 1 only *describes* providers: endpoints and keys come from here and
//! the usage ledger records the provider name. Nothing routes on `billing`,
//! `data_policy`, `limits` or `spend` yet — that is the Phase 2 policy router
//! and the Phase 3 budget ledger. The built-in table reproduces the endpoints
//! that used to be hardcoded in `model_router::config_for_catalog_entry`, so a
//! catalog with no `providers:` section behaves exactly as before.

use std::collections::{BTreeMap, HashMap};
use std::sync::{OnceLock, RwLock};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// Which client code talks to the provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderKind {
    /// Native Ollama API (`/api/chat`).
    Ollama,
    /// llama.cpp `llama-server` (OpenAI wire, one model per instance).
    Llamacpp,
    /// Any OpenAI-compatible `/v1/chat/completions` endpoint.
    OpenaiCompat,
    Anthropic,
    Gemini,
}

/// Who pays for a call.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    /// Own hardware.
    Local,
    /// A free tier: $0, rate-limited.
    Free,
    /// Metered money. The default: a provider that does not say is assumed to
    /// cost something, so forgetting the field can never make a paid API look
    /// free.
    #[default]
    Paid,
}

/// What the provider may do with prompts and completions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DataPolicy {
    NoLog,
    MayLog,
    /// The default: unknown terms are treated as the worst case, so private
    /// work can never be routed to a provider nobody checked.
    #[default]
    MayTrain,
}

/// Declared rate limits. All optional; absent = unenforced.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Limits {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpm: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpd: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tpd: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub concurrency: Option<u32>,
}

/// Spend caps in USD for a paid provider.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Spend {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daily_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub monthly_usd: Option<f64>,
}

/// One entry of the `providers:` section.
///
/// Unknown fields are rejected: a misspelt `api_key_env` would otherwise parse
/// as "no key needed" and fail every call with an auth error far from the typo.
/// The `models.yaml` guard tests load the real file, so the build catches it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    pub kind: ProviderKind,
    #[serde(default)]
    pub billing: Billing,
    #[serde(default)]
    pub data_policy: DataPolicy,
    /// Env var overriding the endpoint (e.g. `OLLAMA_URL`).
    #[serde(default)]
    pub base_url_env: Option<String>,
    /// Endpoint used when `base_url_env` is unset or empty.
    #[serde(default)]
    pub base_url_default: Option<String>,
    /// The NAME of the env var holding the API key. Never the key itself.
    #[serde(default)]
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub limits: Limits,
    #[serde(default)]
    pub spend: Option<Spend>,
    /// The owner's override of `data_policy`: purposes whose PRIVATE work may
    /// reach this provider anyway. `data_policy` stays the provider's terms as
    /// written; this records that the owner has read them and accepts them for
    /// these purposes. Empty (the default) leaves privacy-before-budget intact.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub private_ok_for: Vec<crate::routing::Purpose>,
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// Where the `billing` / `data_policy` claim came from, and when it was checked.
    #[serde(default)]
    pub terms: Option<String>,
}

fn default_enabled() -> bool {
    true
}

/// An env var's value, treating unset and blank the same. Compose passes
/// `KEY=${KEY:-}`, so an absent key arrives as an empty string, not as unset.
fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

impl ProviderEntry {
    /// The endpoint: `base_url_env` when set, else `base_url_default`.
    pub fn base_url(&self) -> Option<String> {
        self.base_url_env
            .as_deref()
            .and_then(env_nonempty)
            .or_else(|| self.base_url_default.clone())
    }

    /// The API key read from `api_key_env`, if the provider needs one and it is set.
    pub fn api_key(&self) -> Option<String> {
        self.api_key_env.as_deref().and_then(env_nonempty)
    }

    /// True when the provider needs no key, or its key is present.
    pub fn key_configured(&self) -> bool {
        self.api_key_env.is_none() || self.api_key().is_some()
    }

    /// Declarations that cannot work, or will stop working when the policy
    /// router lands. Reported at startup; never fatal.
    pub fn issues(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.billing == Billing::Paid
            && self
                .spend
                .as_ref()
                .is_none_or(|s| s.daily_usd.is_none() && s.monthly_usd.is_none())
        {
            out.push(
                "billing is paid (or unstated) with no spend cap — the policy router \
                 will refuse to route it"
                    .to_string(),
            );
        }
        if self.kind == ProviderKind::OpenaiCompat
            && self.base_url_env.is_none()
            && self.base_url_default.is_none()
        {
            out.push("kind openai_compat needs base_url_env or base_url_default".to_string());
        }
        out
    }
}

/// Normalise a provider name, folding the historical spellings the catalog
/// and `provider_hint` fields have used onto one key.
pub fn canonical_name(name: &str) -> String {
    let lower = name.trim().to_ascii_lowercase();
    match lower.as_str() {
        "ollamacloud" | "ollama_cloud" => "ollama-cloud".to_string(),
        "llama-cpp" | "llama.cpp" | "llama_cpp" => "llamacpp".to_string(),
        _ => lower,
    }
}

/// The endpoints `config_for_catalog_entry` hardcoded before the registry
/// existed. A `providers:` entry of the same name replaces its row wholesale.
const BUILTIN_PROVIDERS: &str = r#"
ollama:
  kind: ollama
  billing: local
  data_policy: no_log
  base_url_env: OLLAMA_LOCAL_URL
  base_url_default: "http://localhost:11434"
llamacpp:
  kind: llamacpp
  billing: local
  data_policy: no_log
  base_url_env: LLAMACPP_URL
  base_url_default: "http://localhost:8080"
ollama-cloud:
  kind: openai_compat
  billing: free
  data_policy: no_log
  base_url_env: OLLAMA_URL
  base_url_default: "https://ollama.com"
  api_key_env: OLLAMA_API_KEY
anthropic:
  kind: anthropic
  billing: paid
  data_policy: no_log
  api_key_env: ANTHROPIC_API_KEY
gemini:
  kind: gemini
  billing: free
  data_policy: may_train
  api_key_env: GEMINI_API_KEY
"#;

/// The built-in provider table.
pub fn builtin() -> BTreeMap<String, ProviderEntry> {
    serde_yaml::from_str(BUILTIN_PROVIDERS).expect("built-in provider table must parse")
}

/// The built-in table with a catalog's `providers:` section laid over it.
pub fn merged(overrides: &HashMap<String, ProviderEntry>) -> BTreeMap<String, ProviderEntry> {
    let mut table = builtin();
    for (name, entry) in overrides {
        table.insert(canonical_name(name), entry.clone());
    }
    table
}

fn slot() -> &'static RwLock<BTreeMap<String, ProviderEntry>> {
    static SLOT: OnceLock<RwLock<BTreeMap<String, ProviderEntry>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(builtin()))
}

/// Replace the process-wide registry. Called when `models.yaml` is loaded at
/// startup and again by `reload_models`. Returns the number of providers.
pub fn install(overrides: &HashMap<String, ProviderEntry>) -> usize {
    let table = merged(overrides);
    let n = table.len();
    // The concurrency gates are sized from the same table (services/provider_gate.rs).
    crate::provider_gate::configure(&table);
    *slot().write().unwrap_or_else(|p| p.into_inner()) = table;
    n
}

fn model_slot() -> &'static RwLock<BTreeMap<(String, String), Limits>> {
    static SLOT: OnceLock<RwLock<BTreeMap<(String, String), Limits>>> = OnceLock::new();
    SLOT.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// Replace the per-model limits: `(provider, model id) → limits`, from each
/// catalog entry's `limits:`. A model's limits are counted against that model
/// alone, on top of its provider's. Returns how many models declare some.
pub fn install_model_limits(limits: impl IntoIterator<Item = (String, String, Limits)>) -> usize {
    let table: BTreeMap<(String, String), Limits> = limits
        .into_iter()
        .map(|(provider, model, l)| ((canonical_name(&provider), model), l))
        .collect();
    let n = table.len();
    *model_slot().write().unwrap_or_else(|p| p.into_inner()) = table;
    n
}

/// The limits a model declares for itself, if any.
pub fn model_limits(provider: &str, model: &str) -> Option<Limits> {
    model_slot()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(&(canonical_name(provider), model.to_string()))
        .cloned()
}

/// Look up a provider by any of its accepted spellings.
pub fn get(name: &str) -> Option<ProviderEntry> {
    slot()
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .get(&canonical_name(name))
        .cloned()
}

/// The registry as JSON for `/api/models`. Carries the key's env var name and
/// whether it is set — never the key.
pub fn snapshot_json() -> Value {
    let table = slot().read().unwrap_or_else(|p| p.into_inner()).clone();
    Value::Array(
        table
            .into_iter()
            .map(|(name, e)| {
                json!({
                    "name": name,
                    "kind": e.kind,
                    "billing": e.billing,
                    "data_policy": e.data_policy,
                    "enabled": e.enabled,
                    "base_url": e.base_url(),
                    "api_key_env": e.api_key_env,
                    "key_configured": e.key_configured(),
                    "limits": e.limits,
                    "spend": e.spend,
                    "private_ok_for": e.private_ok_for,
                    "terms": e.terms,
                })
            })
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(yaml: &str) -> ProviderEntry {
        serde_yaml::from_str(yaml).expect("entry parses")
    }

    /// The built-in rows are what made the registry a behaviour-neutral change:
    /// each must say exactly what the old hardcoded match arms said.
    #[test]
    fn the_builtin_table_mirrors_the_old_hardcoded_endpoints() {
        let t = builtin();
        let cloud = &t["ollama-cloud"];
        assert_eq!(cloud.kind, ProviderKind::OpenaiCompat);
        assert_eq!(cloud.base_url_env.as_deref(), Some("OLLAMA_URL"));
        assert_eq!(
            cloud.base_url_default.as_deref(),
            Some("https://ollama.com")
        );
        assert_eq!(cloud.api_key_env.as_deref(), Some("OLLAMA_API_KEY"));

        let local = &t["ollama"];
        assert_eq!(local.kind, ProviderKind::Ollama);
        assert_eq!(local.base_url_env.as_deref(), Some("OLLAMA_LOCAL_URL"));
        assert_eq!(
            local.base_url_default.as_deref(),
            Some("http://localhost:11434")
        );
        assert!(local.api_key_env.is_none());

        let sidecar = &t["llamacpp"];
        assert_eq!(sidecar.kind, ProviderKind::Llamacpp);
        assert_eq!(sidecar.base_url_env.as_deref(), Some("LLAMACPP_URL"));

        assert_eq!(
            t["anthropic"].api_key_env.as_deref(),
            Some("ANTHROPIC_API_KEY")
        );
        assert!(t["anthropic"].base_url().is_none());
        assert_eq!(t["gemini"].api_key_env.as_deref(), Some("GEMINI_API_KEY"));
    }

    #[test]
    fn historical_spellings_fold_onto_one_name() {
        for n in ["ollama-cloud", "ollamacloud", "Ollama_Cloud"] {
            assert_eq!(canonical_name(n), "ollama-cloud");
        }
        for n in ["llamacpp", "llama-cpp", "llama.cpp", "llama_cpp"] {
            assert_eq!(canonical_name(n), "llamacpp");
        }
        assert_eq!(canonical_name(" Mistral "), "mistral");
    }

    /// Forgetting a field must fail towards caution: an undeclared provider
    /// costs money and may train on what it is sent.
    #[test]
    fn omitted_billing_and_data_policy_assume_the_worst() {
        let e = parse("kind: openai_compat\nbase_url_default: https://example.test");
        assert_eq!(e.billing, Billing::Paid);
        assert_eq!(e.data_policy, DataPolicy::MayTrain);
        assert!(e.enabled);
    }

    #[test]
    fn a_misspelt_field_is_rejected_not_ignored() {
        let r: Result<ProviderEntry, _> =
            serde_yaml::from_str("kind: openai_compat\napi_key_evn: SOME_KEY");
        assert!(r.is_err(), "api_key_evn must not parse as 'no key needed'");
    }

    #[test]
    fn a_catalog_entry_replaces_the_builtin_row() {
        let mut overrides = HashMap::new();
        overrides.insert(
            "OllamaCloud".to_string(),
            parse("kind: openai_compat\nbilling: free\nbase_url_default: https://proxy.test"),
        );
        let t = merged(&overrides);
        assert_eq!(
            t["ollama-cloud"].base_url_default.as_deref(),
            Some("https://proxy.test")
        );
        assert!(t.contains_key("gemini"), "untouched builtins survive");
    }

    #[test]
    fn a_paid_provider_without_a_spend_cap_is_reported() {
        let e = parse("kind: anthropic\nbilling: paid\ndata_policy: no_log");
        assert_eq!(e.issues().len(), 1);
        let capped =
            parse("kind: anthropic\nbilling: paid\ndata_policy: no_log\nspend: { daily_usd: 1.0 }");
        assert!(capped.issues().is_empty());
    }

    #[test]
    fn an_openai_compat_provider_without_an_endpoint_is_reported() {
        let e = parse("kind: openai_compat\nbilling: free");
        assert!(e.issues().iter().any(|i| i.contains("base_url")));
    }

    #[test]
    fn a_key_var_that_is_unset_means_not_configured() {
        let e = parse(
            "kind: openai_compat\nbilling: free\nbase_url_default: https://x.test\n\
             api_key_env: AGENT_BRAIN_TEST_KEY_THAT_IS_NEVER_SET",
        );
        assert!(e.api_key().is_none());
        assert!(!e.key_configured());
        let keyless = parse("kind: ollama\nbilling: local");
        assert!(keyless.key_configured());
    }

    #[test]
    fn the_snapshot_never_carries_a_key_value() {
        let json = snapshot_json().to_string();
        assert!(json.contains("api_key_env"));
        assert!(!json.contains("\"api_key\":"));
    }
}
