//! Model list, budget, GPU lease and usage.

use super::{RestState, internal};
use agent_brain_llm::LlmConfig;
use axum::{Extension, Json, extract::Query, http::StatusCode, response::IntoResponse};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// GET /api/models — active provider + catalog models.
pub async fn handle_list_models(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    use agent_brain_llm::LlmProviderType;

    // Read a (provider, model) pair off an optional config Arc.
    async fn read_cfg(arc: &Option<Arc<RwLock<Option<LlmConfig>>>>) -> (String, String) {
        match arc {
            Some(a) => {
                let cfg = a.read().await;
                (
                    cfg.as_ref()
                        .map_or_else(|| "None".to_string(), |c| c.provider.to_string()),
                    cfg.as_ref().map(|c| c.model.clone()).unwrap_or_default(),
                )
            }
            None => ("None".to_string(), String::new()),
        }
    }

    // `active_*` is the brain's model (background/scheduled cognition); `chat_*`
    // is what the human-facing /chat runs on. They diverge whenever CHAT_LLM_*
    // is set — the ChatPanel reads `chat_*` so its indicator names the model the
    // user is actually talking to, not the brain's.
    let (active_provider, active_model) = read_cfg(&state.llm_config).await;
    let (chat_provider, chat_model) = read_cfg(&state.chat_llm_config).await;

    let catalog_models = if let Some(ref db) = state.telemetry {
        let mut rows = db.list_models().unwrap_or_default();
        for row in rows.iter_mut() {
            let admission = model_admission(
                row["provider"].as_str().unwrap_or(""),
                row["model"].as_str().unwrap_or(""),
            );
            row["admission"] = admission;
        }
        Value::Array(rows)
    } else {
        json!([])
    };

    Json(json!({
        "active_provider": active_provider,
        "active_model":    active_model,
        "chat_provider":   chat_provider,
        "chat_model":      chat_model,
        // `cost` describes the tier this deployment uses, not the vendor's whole
        // price list: Ollama Cloud and Gemini are consumed on their free tiers
        // (429 at the ceiling, no billing). Anthropic is the only metered path.
        "available_providers": [
            { "name": "Local engine",   "type": LlmProviderType::Local.to_string(),       "cost": "free" },
            { "name": "Ollama Cloud",   "type": LlmProviderType::OllamaCloud.to_string(), "cost": "free tier" },
            { "name": "Anthropic",      "type": LlmProviderType::Anthropic.to_string(),   "cost": "paid" },
            { "name": "Gemini",         "type": LlmProviderType::Gemini.to_string(),      "cost": "free tier" },
        ],
        // The provider registry: kind, billing, data_policy, limits, spend, and
        // whether each provider's key env var is set — never the key.
        "providers": agent_brain_llm::provider_registry::snapshot_json(),
        // Which billing classes are routable and which purposes may spend.
        "routing_policy": agent_brain_llm::routing::RoutingPolicy::from_env().describe(),
        "catalog_models": catalog_models,
    })).into_response()
}

/// Whether the routing policy would let `model` run, per purpose and data
/// class, right now: `null` when it may, else the refusal (spend cap, cooldown,
/// a private-work rule, a missing key). The model picker shows these, so a
/// choice that would be refused says so before it is saved.
fn model_admission(provider: &str, model: &str) -> Value {
    use agent_brain_llm::routing::{DataClass, Purpose, RouteContext, RoutingPolicy};
    let policy = RoutingPolicy::from_env();
    let cfg = agent_brain_llm::model_router::config_for_catalog_entry(
        &LlmConfig::default(),
        provider,
        model,
    );
    // A chat turn is the owner's only when it authenticated, as this request
    // did if `OWNER_REQUEST` is set; the picker shows what this caller's chat
    // may run.
    let chat_by = RouteContext::current().initiator;
    let mut out = serde_json::Map::new();
    for purpose in [Purpose::Background, Purpose::Subagent, Purpose::Chat] {
        let mut by_class = serde_json::Map::new();
        for class in [DataClass::Private, DataClass::Public] {
            let mut ctx = RouteContext::new(purpose, class);
            if purpose == Purpose::Chat {
                ctx = ctx.with_initiator(chat_by);
            }
            let verdict = policy.admit(&cfg, ctx);
            by_class.insert(
                class.as_str().to_string(),
                verdict.err().map_or(Value::Null, Value::String),
            );
        }
        out.insert(purpose.as_str().to_string(), Value::Object(by_class));
    }
    Value::Object(out)
}

/// GET /api/llm/budget — the live budget ledger: per-provider usage against
/// declared limits and spend caps, plus per-model cooldowns and headers.
pub async fn handle_llm_budget() -> impl IntoResponse {
    Json(agent_brain_llm::budget::snapshot_json()).into_response()
}

/// GET /api/gpu-lease — whether the GPU is leased, to whom and until when.
pub async fn handle_gpu_lease_status() -> impl IntoResponse {
    Json(agent_brain_llm::gpu_lease::status_json()).into_response()
}

/// POST /api/gpu-lease — `{action: status|acquire|release, holder, reason,
/// ttl_hours, force}`. A refused action (the GPU held by someone else, a
/// missing holder) is 409 with the reason.
pub async fn handle_gpu_lease(Json(body): Json<Value>) -> impl IntoResponse {
    match agent_brain_llm::gpu_lease::handle(body).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response(),
    }
}

/// GET /api/models/usage?hours=<n> — aggregated model usage stats from DuckDB.
/// `hours` restricts the window (quota budgets are time-windowed); omit for all-time.
pub async fn handle_model_usage(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref db) = state.telemetry else {
        return Json(json!({ "models": [], "available": false })).into_response();
    };
    let hours: Option<i64> = params.get("hours").and_then(|h| h.parse().ok());
    match db.get_model_stats(None, hours) {
        Ok(stats) => {
            let models = stats.get("models").cloned().unwrap_or(json!([]));
            Json(json!({ "models": models, "available": true, "window_hours": hours }))
                .into_response()
        }
        Err(e) => internal(format!("Failed to get model usage: {e}")),
    }
}
