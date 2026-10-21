//! Fixtures shared by the chat test modules: canned SSE streams, a bare
//! `ChatService`, and a mock-server LLM config.

use super::*;

/// Byte-for-byte the shape ollama.com streamed for the 2026-08-25 dropped
/// turn: an empty `content` delta, `finish_reason: "stop"`, and a usage
/// block counting 249 generated tokens that never appeared anywhere.
pub(super) const EMPTY_COMPLETION_SSE: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},",
    "\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12100,\"completion_tokens\":249}}\n\n",
    "data: [DONE]\n\n",
);

pub(super) const ANSWERED_COMPLETION_SSE: &str = concat!(
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
    "\"content\":\"Substrates and power.\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
    "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}\n\n",
    "data: [DONE]\n\n",
);

pub(super) fn sse(body: &str) -> wiremock::ResponseTemplate {
    wiremock::ResponseTemplate::new(200).set_body_raw(body.as_bytes(), "text/event-stream")
}

/// A `ChatService` with empty registries — these tests exercise the stream,
/// not tool dispatch.
pub(super) fn chat_service(fallback_ladder: Vec<LlmConfig>) -> Arc<ChatService> {
    ChatService::with_context_builder(
        Arc::new(RwLock::new(None)),
        Arc::new(RwLock::new(ToolRegistry::new())),
        Arc::new(RwLock::new(None)),
        Arc::new(RwLock::new(None)),
        None,
        fallback_ladder,
    )
}

pub(super) fn cloud_config(server: &wiremock::MockServer) -> LlmConfig {
    LlmConfig {
        provider: LlmProviderType::OllamaCloud,
        base_url: Some(server.uri()),
        // The routing policy refuses a keyed provider whose key is absent, so
        // a rung meant to reach the mock must carry one.
        api_key: Some("test-key".into()),
        // A fresh model per call. A 5xx benches its model in the process-global
        // budget book for 60s, so a shared model name let one test's 500
        // refuse the next test's primary ("cooling down"): CI failed six ladder
        // tests whenever they ran in that order. (Not the port: freed ports
        // are reused within a run.)
        model: {
            static N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
            format!(
                "test-model-{}",
                N.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            )
        },
        timeout: Duration::from_secs(30),
        ..Default::default()
    }
}

pub(super) fn test_request() -> ChatRequest {
    ChatRequest {
        message: "map the secondary supply chains".into(),
        history: vec![],
        session_id: None,
        tools: None,
        context_profile: None,
        synthesis_provider: None,
        synthesis_model: None,
        model: None,
    }
}
