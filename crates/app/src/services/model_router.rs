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
//! Selection inherits `select_models` ordering: cost ascending, then context
//! window descending — so among $0-tied candidates the largest-context model
//! wins, which at tier 1 prefers the big cloud models over local 4B ones.
//! If no candidate survives the tier filter the caller falls back to normal
//! routing — a missing model degrades the step, never blocks it.

use tracing::{debug, info};

use crate::repository::TelemetryClient;
use crate::services::llm::{LlmConfig, LlmProviderType};

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

    let chosen = candidates.iter().find(|c| {
        candidate_allowed(
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
    let mut cfg = base.clone();
    cfg.model = model.clone();
    match provider {
        "ollama-cloud" | "ollamacloud" => {
            cfg.provider = LlmProviderType::OllamaCloud;
            cfg.base_url = Some(
                std::env::var("OLLAMA_URL").unwrap_or_else(|_| "https://ollama.com".to_string()),
            );
            cfg.api_key = std::env::var("OLLAMA_API_KEY").ok();
        }
        "anthropic" => {
            cfg.provider = LlmProviderType::Anthropic;
            cfg.base_url = None;
            cfg.api_key = std::env::var("ANTHROPIC_API_KEY").ok();
        }
        "gemini" => {
            cfg.provider = LlmProviderType::Gemini;
            cfg.base_url = None;
            cfg.api_key = std::env::var("GEMINI_API_KEY").ok();
        }
        _ => {
            cfg.provider = LlmProviderType::Ollama;
            cfg.base_url = Some(
                std::env::var("OLLAMA_LOCAL_URL")
                    .unwrap_or_else(|_| "http://localhost:11434".to_string()),
            );
            cfg.api_key = None;
        }
    }

    info!(
        capabilities = ?required_capabilities,
        provider = provider,
        model = %model,
        tier = tier,
        "Model routing: resolved step model"
    );
    Some(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
