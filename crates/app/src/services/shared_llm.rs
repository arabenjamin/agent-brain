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
use crate::services::routing::{RouteContext, RoutingPolicy};
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

    /// Run one generation against `llm`, timing it and recording the outcome in
    /// the usage ledger. Shared by the primary attempt and every fallback rung
    /// so each call — including a failed cloud call that then falls back — lands
    /// in `model_usage` exactly once, with its `error_kind` classified.
    async fn run_and_record(
        &self,
        llm: LlmConfig,
        prompt: &str,
        system: Option<&str>,
        tool: Option<&str>,
    ) -> anyhow::Result<crate::services::llm::LlmResponse> {
        let model_name = llm.model.clone();
        let provider_label = llm.provider_label();
        let client =
            LlmClient::with_config(llm).map_err(|e| anyhow::anyhow!("LLM init error: {}", e))?;
        let start = Instant::now();
        let result = client
            .generate_with_system(prompt, system)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e));
        let duration_ms = start.elapsed().as_millis() as i64;
        // One write path for the usage ledger and the budget ledger, so every
        // call that spends quota is counted where the router can see it. The
        // error text carries retry hints (Gemini's retryDelay) for cooldowns.
        let (tin, tout) = response_tokens(&result);
        let error_text = result.as_ref().err().map(|e| e.to_string());
        crate::services::budget::record_call(
            self.telemetry.as_ref(),
            &provider_label,
            &model_name,
            tool,
            result.is_ok(),
            Some(duration_ms),
            tin,
            tout,
            failure_kind(&result),
            error_text.as_deref(),
        );
        result
    }
}

/// Build the ordered fallback chain for a cloud route that came back
/// unavailable (quota, transport, or 5xx).
///
/// The ordering encodes "fall back to what we're doing now, then to what always
/// answers":
/// - A **capability-selected** cloud route (e.g. a `reasoning` step routed to
///   gemini-2.5-flash) falls back first to the **active config** — the free
///   Ollama Cloud model the deployment was using before, which does not share
///   the selected model's quota — and only then to **local** Ollama.
/// - A route that WAS the active config skips the active tier (retrying the same
///   model that just failed is pointless) and goes straight to local.
///
/// A candidate whose model equals the one that just failed is dropped, so a
/// deployment whose active and local configs coincide with the primary never
/// re-runs it.
fn build_fallback_chain(
    is_selected: bool,
    primary_model: &str,
    active: Option<LlmConfig>,
    local: Option<LlmConfig>,
) -> Vec<LlmConfig> {
    let mut chain: Vec<LlmConfig> = Vec::new();
    if is_selected
        && let Some(active) = active
        && active.model != primary_model
        && !matches!(active.provider, crate::services::LlmProviderType::Ollama)
    {
        chain.push(active);
    }
    if let Some(local) = local
        && local.model != primary_model
        && !chain.iter().any(|c| c.model == local.model)
    {
        chain.push(local);
    }
    chain
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
        // `is_selected` distinguishes a capability-routed model (e.g. a
        // `reasoning` step routed to gemini-2.5-flash) from the plain active
        // config. It decides the fallback chain below: a selected cloud route
        // that hits its quota falls back to the *active* config first — the free
        // Ollama Cloud model that is "what we're doing now" — before dropping to
        // local. A route that WAS the active config skips straight to local.
        let (primary, is_local_route, is_selected) = if let Some(sel) = selected {
            debug!(model = %sel.model, "SharedLlm: routing generate() to capability-selected model");
            let local = matches!(sel.provider, crate::services::LlmProviderType::Ollama);
            (sel, local, true)
        } else if use_local {
            debug!("SharedLlm: routing generate() to local Ollama (USE_LOCAL_LLM=true)");
            let config = self.local_config.read().await.clone();
            (
                config.ok_or_else(|| anyhow::anyhow!("LLM not configured"))?,
                true,
                false,
            )
        } else {
            let config = self.config.read().await.clone();
            (
                config.ok_or_else(|| anyhow::anyhow!("LLM not configured"))?,
                false,
                false,
            )
        };

        // Every rung — primary included — must be admitted by the routing policy
        // for the work this call belongs to (`ROUTE_CONTEXT`: the queue step, the
        // chat turn or the sub-agent run). A refused primary is not a failure of
        // the model: the chain is walked for a permitted rung instead, and when
        // none exists the call fails with every reason rather than quietly
        // sending private work somewhere it may not go.
        let ctx = RouteContext::current();
        let policy = RoutingPolicy::from_env();
        let primary_model = primary.model.clone();
        let mut refusals: Vec<String> = Vec::new();
        // The primary attempt is recorded (success or failure) inside the
        // helper, so a failed cloud call still lands in the ledger before any
        // fallback — those rows are how the model router learns observed
        // availability and must not vanish.
        let result = match policy.admit(&primary, ctx) {
            Ok(()) => self.run_and_record(primary, prompt, system, tool).await,
            Err(reason) => {
                warn!(
                    model = %primary_model,
                    provider = %primary.provider_label(),
                    purpose = %ctx.purpose,
                    data_class = %ctx.data_class,
                    reason = %reason,
                    "SharedLlm: routing policy refused the primary model"
                );
                refusals.push(format!("{primary_model} ({reason})"));
                Err(anyhow::anyhow!(
                    "routing policy refused {primary_model}: {reason}"
                ))
            }
        };
        let refused_primary = !refusals.is_empty();

        // If the call failed in a way that says "this provider cannot answer
        // right now" — as opposed to "this prompt is bad" — walk a fallback
        // chain before giving up. Only for cloud routes: a local model has
        // nowhere better to fall back to and must not retry against itself.
        // A policy refusal walks the chain too, whatever the route.
        let unavailable_kind = if refused_primary {
            None
        } else {
            result.as_ref().err().and_then(classify_unavailable)
        };
        if refused_primary || (!is_local_route && unavailable_kind.is_some()) {
            let active = self.config.read().await.clone();
            let local = self.local_config.read().await.clone();
            let chain = build_fallback_chain(is_selected, &primary_model, active, local);
            warn!(
                error_kind = unavailable_kind.unwrap_or("policy_refused"),
                primary = %primary_model,
                fallbacks = %chain
                    .iter()
                    .map(|c| c.model.as_str())
                    .collect::<Vec<_>>()
                    .join(", "),
                "SharedLlm: primary model unusable, walking fallback chain"
            );
            let mut last_err: Option<anyhow::Error> = None;
            for cfg in chain {
                if let Err(reason) = policy.admit(&cfg, ctx) {
                    warn!(
                        model = %cfg.model,
                        provider = %cfg.provider_label(),
                        reason = %reason,
                        "SharedLlm: routing policy refused a fallback rung"
                    );
                    refusals.push(format!("{} ({reason})", cfg.model));
                    continue;
                }
                let fb_model = cfg.model.clone();
                match self.run_and_record(cfg, prompt, system, tool).await {
                    Ok(r) => return Ok(r.text),
                    Err(e) => {
                        warn!(model = %fb_model, error = %e, "SharedLlm: fallback model also failed");
                        last_err = Some(e);
                    }
                }
            }
            // Every fallback also failed. Return the last fallback error if we
            // attempted any, otherwise the primary error falls through below.
            if let Some(e) = last_err {
                return Err(e);
            }
            if refused_primary {
                return Err(anyhow::anyhow!(
                    "no provider the routing policy permits for {} {} work: {}",
                    ctx.data_class,
                    ctx.purpose,
                    refusals.join("; ")
                ));
            }
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

/// Telemetry `error_kind` for a failure known only by its message: the classified
/// reason, or `"other"`. For call sites that hold a formatted error rather than an
/// `anyhow::Error` — the chat loops, which record their own usage.
pub(crate) fn failure_kind_of(message: &str) -> &'static str {
    classify_unavailable(&anyhow::anyhow!(message.to_string())).unwrap_or("other")
}

/// Telemetry `error_kind` for a finished call: `None` on success, otherwise the
/// classified reason, falling back to `"other"`.
///
/// The two non-fallback `record_model_usage` sites used to hardcode `None` for
/// this column, so **every** failed call landed with `error_kind IS NULL` — 124
/// of them in the 30 hours before 2026-09-01, the exact rows you query first
/// when asking why a model started failing. `classify_unavailable` was already
/// being computed one branch away; only the cloud-fallback path recorded it.
///
/// `"other"` rather than `None` on an unclassified failure, so the column
/// separates "we did not classify this" from "this did not fail".
fn failure_kind(
    result: &anyhow::Result<crate::services::llm::LlmResponse>,
) -> Option<&'static str> {
    match result {
        Ok(_) => None,
        Err(e) => Some(classify_unavailable(e).unwrap_or("other")),
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

    fn ok_response() -> anyhow::Result<crate::services::llm::LlmResponse> {
        Ok(crate::services::llm::LlmResponse {
            text: "hi".to_string(),
            duration_ns: None,
            tokens_evaluated: None,
            tokens_in: None,
            tokens_out: None,
        })
    }

    /// A success must leave `error_kind` NULL — the column has to separate
    /// "did not fail" from "failed, unclassified".
    #[test]
    fn successful_calls_record_no_error_kind() {
        assert_eq!(failure_kind(&ok_response()), None);
    }

    /// The regression: every failed call used to land with `error_kind IS NULL`,
    /// so the 26 dead jobs of 2026-09-01 were unattributable by the one query
    /// you run when a model starts failing.
    #[test]
    fn failed_calls_always_record_an_error_kind() {
        let timeout = Err(anyhow::anyhow!(
            "Provider error: HTTP request failed: operation timed out: \
             error sending request for url (http://host.docker.internal:11434/api/generate)"
        ));
        assert_eq!(failure_kind(&timeout), Some("transport"));

        // Unclassified failures are labelled, never left NULL.
        let odd = Err(anyhow::anyhow!("Generation failed: something new"));
        assert_eq!(failure_kind(&odd), Some("other"));
    }

    /// A local timeout must classify as `transport`, which is what the enriched
    /// `describe_reqwest` output guarantees by prefixing "operation timed out".
    /// The fallback itself is still skipped for local routes (`!is_local_route`),
    /// so this only affects how the failure is *labelled*.
    #[test]
    fn a_local_timeout_is_classified_as_transport() {
        let e = err(
            "Summarization failed: Provider error: HTTP request failed: operation timed out: \
             error sending request for url (http://host.docker.internal:11434/api/generate)",
        );
        assert_eq!(classify_unavailable(&e), Some("transport"));
    }

    fn cfg(provider: crate::services::LlmProviderType, model: &str) -> LlmConfig {
        LlmConfig::default()
            .with_provider(provider)
            .with_model(model)
    }

    fn models(chain: &[LlmConfig]) -> Vec<&str> {
        chain.iter().map(|c| c.model.as_str()).collect()
    }

    /// A capability-selected cloud route (gemini) that 429s falls back to the
    /// active cloud config first — "what we're doing now" — then to local. This
    /// is the whole point of the change: a reasoning step must not silently drop
    /// straight to the weak local model the instant Gemini's daily quota is hit.
    #[test]
    fn selected_cloud_route_falls_back_to_active_then_local() {
        use crate::services::LlmProviderType::{Gemini, Ollama, OllamaCloud};
        let chain = build_fallback_chain(
            true,
            "gemini-2.5-flash",
            Some(cfg(OllamaCloud, "gpt-oss:120b-cloud")),
            Some(cfg(Ollama, "granite4:latest")),
        );
        assert_eq!(
            models(&chain),
            vec!["gpt-oss:120b-cloud", "granite4:latest"]
        );
        // Sanity: the selected primary was gemini, not represented in the chain.
        assert!(!chain.iter().any(|c| c.provider == Gemini));
    }

    /// A route that WAS the active config (not capability-selected) skips the
    /// active tier — retrying the model that just failed is pointless — and goes
    /// straight to local.
    #[test]
    fn active_config_route_skips_active_and_falls_back_to_local_only() {
        use crate::services::LlmProviderType::{Ollama, OllamaCloud};
        let chain = build_fallback_chain(
            false,
            "gpt-oss:120b-cloud",
            Some(cfg(OllamaCloud, "gpt-oss:120b-cloud")),
            Some(cfg(Ollama, "granite4:latest")),
        );
        assert_eq!(models(&chain), vec!["granite4:latest"]);
    }

    /// The active config is never used as a fallback for itself, even when the
    /// route was capability-selected onto the same model the active config names.
    #[test]
    fn active_fallback_is_dropped_when_it_equals_the_primary() {
        use crate::services::LlmProviderType::{Ollama, OllamaCloud};
        let chain = build_fallback_chain(
            true,
            "gpt-oss:120b-cloud",
            Some(cfg(OllamaCloud, "gpt-oss:120b-cloud")),
            Some(cfg(Ollama, "granite4:latest")),
        );
        assert_eq!(models(&chain), vec!["granite4:latest"]);
    }

    /// A local active config is not a meaningful cloud fallback — the chain must
    /// not push it as the "active" rung (it would still appear once as the local
    /// rung, and never twice).
    #[test]
    fn a_local_active_config_is_not_used_as_the_cloud_rung() {
        use crate::services::LlmProviderType::Ollama;
        let chain = build_fallback_chain(
            true,
            "gemini-2.5-flash",
            Some(cfg(Ollama, "granite4:latest")),
            Some(cfg(Ollama, "granite4:latest")),
        );
        assert_eq!(models(&chain), vec!["granite4:latest"]);
    }
}
