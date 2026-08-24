//! End-to-end proof that `SharedLlm` falls back to the local model when a
//! cloud provider cannot be reached.
//!
//! The unit tests in `services/shared_llm.rs` cover `classify_unavailable` —
//! which error strings qualify. They cannot prove the surrounding plumbing
//! actually runs the local model and returns its text, and that plumbing is
//! what the Off-Grid Networking Monitor needed on 2026-08-19 and did not get:
//! its one cloud `reason` step failed 3/3 against an unreachable ollama.com,
//! dead-lettered, and failed the owning Task while the local model sat idle.
//!
//! Requires a live local Ollama (the same dependency the brain itself has).
//! Skips rather than fails when one is not reachable, so `cargo test` on a
//! machine without Ollama stays green.

use std::sync::Arc;
use std::time::Duration;

use agent_brain::services::shared_llm::SharedLlm;
use agent_brain::services::traits::LlmProvider;
use agent_brain::services::{LlmConfig, LlmProviderType};
use tokio::sync::RwLock;

/// Local tracing init. Deliberately not `mod common` — this test uses none of
/// that module's Neo4j helpers, and importing it only to reach `init_test_env`
/// makes every unused helper a dead_code warning in this binary.
fn init_test_env() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        let _ = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::DEBUG)
            .try_init();
    });
}

fn local_url() -> String {
    std::env::var("OLLAMA_LOCAL_URL").unwrap_or_else(|_| "http://localhost:11434".to_string())
}

fn local_model() -> String {
    std::env::var("OLLAMA_LOCAL_MODEL").unwrap_or_else(|_| "gemma4:latest".to_string())
}

/// True when a local Ollama answers, so the test can skip instead of failing.
async fn local_ollama_up() -> bool {
    let client = match reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(_) => return false,
    };
    client
        .get(format!("{}/api/tags", local_url()))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn base_config(provider: LlmProviderType, base_url: &str, model: &str) -> LlmConfig {
    LlmConfig {
        provider,
        base_url: Some(base_url.to_string()),
        api_key: Some("test-key".to_string()),
        model: model.to_string(),
        timeout: Duration::from_secs(30),
        ..Default::default()
    }
}

/// A cloud config pointed at a closed port fails with a transport error
/// ("error sending request" / connection refused). Before the 2026-08-23 fix
/// `classify_unavailable` did not recognise that class, so the error
/// propagated and the calling job burned its retries against a host that could
/// not answer. It must now produce local output instead.
#[tokio::test]
async fn cloud_transport_failure_falls_back_to_local() {
    init_test_env();

    if !local_ollama_up().await {
        eprintln!("SKIP: no local Ollama at {} — skipping", local_url());
        return;
    }

    // Port 1 is reserved and nothing listens on it: connection refused, fast.
    let cloud = base_config(
        LlmProviderType::OllamaCloud,
        "http://127.0.0.1:1",
        "unreachable-model",
    );
    let local = base_config(LlmProviderType::Ollama, &local_url(), &local_model());

    let shared = SharedLlm::new_with_local(
        Arc::new(RwLock::new(Some(cloud))),
        Arc::new(RwLock::new(Some(local))),
        None,
    );

    let result = shared
        .generate("Reply with the single word: ok", None)
        .await
        .expect("transport failure on the cloud config must fall back to local, not propagate");

    assert!(
        !result.trim().is_empty(),
        "fallback returned empty text; expected local model output"
    );
}

/// The fallback is guarded by `!is_local_route`. A failing *local* config must
/// propagate its error rather than retry against itself — an infinite-retry
/// guard that matters because both configs point at the same place in a
/// local-only deployment.
#[tokio::test]
async fn local_route_failure_does_not_retry_itself() {
    init_test_env();

    let dead = base_config(LlmProviderType::Ollama, "http://127.0.0.1:1", "no-model");

    let shared = SharedLlm::new_with_local(
        Arc::new(RwLock::new(Some(dead.clone()))),
        Arc::new(RwLock::new(Some(dead))),
        None,
    );

    let result = shared.generate("hello", None).await;
    assert!(
        result.is_err(),
        "a dead local route must surface its error, not loop back into itself"
    );
}
