use std::sync::Arc;
use std::time::Duration;

use agent_brain_engine::Role;
use serde::{Deserialize, Serialize};
use thiserror::Error;
/// Default timeout for LLM requests (2 minutes for slow models).
const DEFAULT_TIMEOUT_SECS: u64 = 120;

/// Default timeout for the **local** background model, which needs a much
/// larger budget than a cloud endpoint.
///
/// A cloud model that has not answered in 120s is not going to; a local one on
/// a shared consumer GPU routinely is. Measured 2026-09-01 on an RTX 3060 Ti
/// (8 GB) serving `gemma4:latest` — a 9.86 GB model that gets 3.2 GB of VRAM
/// and runs **32% on GPU** at ~7 tok/s: an unloaded call takes 54–79s, and
/// under the twice-daily media-watch burst the average climbs to 80–120s.
/// At a 120s ceiling that burst was pure data loss — one hour recorded 31
/// calls, **31 failures, average exactly 120.0s** — and each dead job took its
/// parked chain steps and its owning Task down with it (26 dead jobs, 151
/// cancelled steps, 26 failed Tasks in 24 hours).
///
/// Raising the ceiling costs nothing the GPU was not already spending: the
/// work is in flight either way, and killing it at 120s throws away the
/// compute *and* the chain. Daily volume fits comfortably (~445 calls at ~80s
/// over two concurrent slots is under 20% duty cycle) — only the bursts
/// overflowed. It is still bounded, because a job holds a queue semaphore
/// permit for the whole call.
const DEFAULT_LOCAL_TIMEOUT_SECS: u64 = 600;

/// Resolve the local model's request timeout from `LOCAL_TIMEOUT_SECS`,
/// falling back to [`DEFAULT_LOCAL_TIMEOUT_SECS`]. An unparseable or zero
/// value warns and uses the default rather than disabling the timeout.
pub fn local_timeout() -> Duration {
    match std::env::var("LOCAL_TIMEOUT_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => Duration::from_secs(secs),
            _ => {
                tracing::warn!(
                    value = %raw,
                    default_secs = DEFAULT_LOCAL_TIMEOUT_SECS,
                    "LOCAL_TIMEOUT_SECS is not a positive integer — using the default"
                );
                Duration::from_secs(DEFAULT_LOCAL_TIMEOUT_SECS)
            }
        },
        Err(_) => Duration::from_secs(DEFAULT_LOCAL_TIMEOUT_SECS),
    }
}

/// The config for **background and scheduled work**, the local lane a step takes
/// with `provider_hint: local`: the engine's endpoint and its generation model,
/// with the long local timeout.
pub fn build_local_llm_config() -> LlmConfig {
    LlmConfig::default().with_timeout(local_timeout())
}

/// The model a config names before anything chooses one, if the engine has none.
const DEFAULT_MODEL: &str = "local";

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP request failed: {}", crate::llm_providers::describe_reqwest(.0))]
    Request(#[from] reqwest::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Model not available: {0}")]
    ModelNotAvailable(String),

    #[error("Generation failed: {0}")]
    GenerationFailed(String),

    #[error("Failed to parse LLM response: {0}")]
    ParseError(String),

    #[error("Server not reachable: {0}")]
    ServerNotReachable(String),

    #[error("Provider error: {0}")]
    Provider(#[from] crate::llm_providers::LlmProviderError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum LlmProviderType {
    /// The local engine (`agent_brain_engine`), over its OpenAI wire. Context window
    /// and GPU offload belong to the engine's own configuration, not to a request.
    #[default]
    #[serde(
        alias = "ollama",
        alias = "llamacpp",
        alias = "llama-cpp",
        alias = "llama.cpp"
    )]
    Local,
    /// Ollama Cloud (https://ollama.com), a cloud provider on the OpenAI wire.
    OllamaCloud,
    Anthropic,
    Gemini,
    /// Any other OpenAI-compatible `/v1/chat/completions` API (Mistral,
    /// OpenRouter, a paid OpenAI-wire provider). Endpoint and key come from the
    /// provider registry; `LlmConfig::provider_name` says which provider it is.
    #[serde(rename = "openai_compat", alias = "openai-compat")]
    OpenAiCompat,
}

impl std::fmt::Display for LlmProviderType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LlmProviderType::Local => write!(f, "local"),
            LlmProviderType::OllamaCloud => write!(f, "ollama-cloud"),
            LlmProviderType::OpenAiCompat => write!(f, "openai-compat"),
            LlmProviderType::Anthropic => write!(f, "anthropic"),
            LlmProviderType::Gemini => write!(f, "gemini"),
        }
    }
}

/// Configuration for the LLM client.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// LLM provider type.
    pub provider: LlmProviderType,

    /// API base URL.
    pub base_url: Option<String>,

    /// API key (required for cloud providers).
    pub api_key: Option<String>,

    /// Model name to use for text generation.
    pub model: String,

    /// Request timeout.
    pub timeout: Duration,

    /// Temperature for generation (0.0 - 1.0).
    pub temperature: f32,

    /// Maximum tokens to generate.
    pub max_tokens: Option<u32>,

    /// The provider-registry key this config was built from (`mistral`,
    /// `ollama-cloud`, …). `provider` says which client code runs; this says
    /// whose endpoint it runs against, which matters once several providers
    /// share the OpenAI-compatible client. `None` for configs not built from
    /// the registry. Recorded in the usage ledger via [`Self::provider_label`].
    pub provider_name: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        let engine = agent_brain_engine::current();
        Self {
            provider: LlmProviderType::Local,
            base_url: Some(engine.url.clone()),
            api_key: None,
            model: engine
                .model(Role::Generation)
                .unwrap_or(DEFAULT_MODEL)
                .to_string(),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            temperature: 0.7,
            max_tokens: None,
            provider_name: None,
        }
    }
}

impl LlmConfig {
    /// The provider name for the usage ledger: the registry key when known,
    /// else the provider type.
    pub fn provider_label(&self) -> String {
        self.provider_name
            .clone()
            .unwrap_or_else(|| self.provider.to_string())
    }

    /// Create config from environment-style parameters.
    pub fn new(base_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            base_url: Some(base_url.into()),
            model: model.into(),
            ..Default::default()
        }
    }

    /// Set the provider type.
    pub fn with_provider(mut self, provider: LlmProviderType) -> Self {
        self.provider = provider;
        // A registry name describes the old provider; keeping it would record
        // calls against the wrong provider in the ledger.
        self.provider_name = None;
        self
    }

    /// Set the API key.
    pub fn with_api_key(mut self, api_key: impl Into<String>) -> Self {
        self.api_key = Some(api_key.into());
        self
    }

    /// Set the base URL.
    pub fn with_base_url(mut self, base_url: impl Into<String>) -> Self {
        self.base_url = Some(base_url.into());
        self
    }

    /// Set the model name.
    pub fn with_model(mut self, model: impl Into<String>) -> Self {
        self.model = model.into();
        self
    }

    /// Set temperature.
    pub fn with_temperature(mut self, temperature: f32) -> Self {
        self.temperature = temperature.clamp(0.0, 1.0);
        self
    }

    /// Set max tokens.
    pub fn with_max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = Some(max_tokens);
        self
    }

    /// Set timeout.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }
}

/// LLM chat message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: "system".to_string(),
            content: content.into(),
        }
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: "user".to_string(),
            content: content.into(),
        }
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: "assistant".to_string(),
            content: content.into(),
        }
    }
}

/// Result of LLM generation with metadata.
#[derive(Debug, Clone)]
pub struct LlmResponse {
    /// The generated text.
    pub text: String,

    /// Total duration in nanoseconds (if available).
    pub duration_ns: Option<u64>,

    /// Number of tokens evaluated (if available).
    pub tokens_evaluated: Option<u32>,

    /// Prompt/input tokens (if the provider reports them separately).
    pub tokens_in: Option<u32>,

    /// Completion/output tokens (if the provider reports them separately).
    pub tokens_out: Option<u32>,
}

/// LLM client for interacting with various providers.
pub struct LlmClient {
    provider: Arc<dyn crate::llm_providers::LlmProvider>,
    embed_provider: Arc<dyn crate::llm_providers::LlmProvider>,
    config: LlmConfig,
}

impl LlmClient {
    /// Create a new LLM client with default configuration.
    pub fn new() -> Result<Self, LlmError> {
        Self::with_config(LlmConfig::default())
    }

    /// Create a new LLM client with custom configuration.
    pub fn with_config(config: LlmConfig) -> Result<Self, LlmError> {
        use crate::llm_providers::{
            ProviderConfig, anthropic::AnthropicProvider, openai_compat::OpenAiCompatProvider,
        };

        let provider_config = ProviderConfig {
            provider_name: config.provider_label(),
            model: config.model.clone(),
            api_key: config.api_key.clone(),
            base_url: config.base_url.clone(),
            timeout: config.timeout,
            temperature: config.temperature,
            max_tokens: config.max_tokens,
        };

        let provider: Arc<dyn crate::llm_providers::LlmProvider> = match config.provider {
            // The local engine, Ollama Cloud, and every other OpenAI-wire provider
            // (Mistral, OpenRouter, ...) share one client; the endpoint and key come
            // from the registry. Gemini too, through Google's OpenAI-compatible API
            // (`openai_compat::api_root`; CORE_AND_PLUGINS_PLAN 6b).
            LlmProviderType::Local
            | LlmProviderType::OllamaCloud
            | LlmProviderType::OpenAiCompat
            | LlmProviderType::Gemini => {
                Arc::new(OpenAiCompatProvider::new(provider_config, config.provider))
            }
            LlmProviderType::Anthropic => Arc::new(AnthropicProvider::new(provider_config)),
        };

        // Embeddings always run on the local engine, whatever generates: a remote
        // API's embedding would be a different model and dimension.
        let engine = agent_brain_engine::current();
        let embed_provider: Arc<dyn crate::llm_providers::LlmProvider> =
            Arc::new(OpenAiCompatProvider::new(
                ProviderConfig {
                    provider_name: LlmProviderType::Local.to_string(),
                    model: engine
                        .model(Role::Embeddings)
                        .unwrap_or(DEFAULT_MODEL)
                        .to_string(),
                    api_key: None,
                    base_url: Some(engine.url.clone()),
                    timeout: config.timeout,
                    temperature: 0.0,
                    max_tokens: None,
                },
                LlmProviderType::Local,
            ));

        Ok(Self {
            provider,
            embed_provider,
            config,
        })
    }

    /// Get the current configuration.
    pub fn config(&self) -> &LlmConfig {
        &self.config
    }

    /// Local generation stops while a GPU lease is held (`services/gpu_lease.rs`).
    /// The routing policy already refuses local models; this backstop covers
    /// callers that build a client directly (the distiller, the context builder).
    fn refuse_local_if_gpu_leased(&self) -> Result<(), LlmError> {
        let local = crate::provider_registry::get(&self.config.provider_label())
            .map(|e| e.billing == crate::provider_registry::Billing::Local)
            .unwrap_or(matches!(self.config.provider, LlmProviderType::Local));
        match crate::gpu_lease::refusal() {
            Some(reason) if local => Err(LlmError::ServerNotReachable(reason)),
            _ => Ok(()),
        }
    }

    /// Embeddings run on the local engine (bge-m3 lives on the GPU), so a GPU lease
    /// pauses them: a single search would otherwise reload bge-m3 mid-training.
    /// Callers already degrade (search to BM25; a stored note is marked
    /// `embedding_pending` and backfilled on release).
    fn refuse_embeddings_if_gpu_leased(&self) -> Result<(), LlmError> {
        match crate::gpu_lease::refusal() {
            Some(reason) => Err(LlmError::ServerNotReachable(reason)),
            None => Ok(()),
        }
    }

    /// Check if the provider is reachable.
    pub async fn health_check(&self) -> Result<bool, LlmError> {
        Ok(self.provider.health_check().await)
    }

    /// Generate text from a prompt.
    pub async fn generate(&self, prompt: &str) -> Result<LlmResponse, LlmError> {
        self.generate_with_system(prompt, None).await
    }

    /// Generate text with a system prompt.
    pub async fn generate_with_system(
        &self,
        prompt: &str,
        system: Option<&str>,
    ) -> Result<LlmResponse, LlmError> {
        self.refuse_local_if_gpu_leased()?;
        // One provider concurrency slot for exactly this call (services/provider_gate.rs).
        let _permit = crate::provider_gate::acquire(&self.config.provider_label()).await;
        self.provider
            .generate(prompt, system)
            .await
            .map_err(LlmError::from)
    }

    /// Generate embeddings for a text.
    pub async fn embeddings(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        if crate::gpu_lease::held().is_some()
            && let Some((provider, model)) = std::env::var("GPU_LEASE_EMBEDDINGS")
                .ok()
                .as_deref()
                .and_then(parse_lease_embedder)
        {
            return self.lease_embeddings(&provider, &model, text).await;
        }
        self.refuse_embeddings_if_gpu_leased()?;
        self.embed_provider
            .embed(text)
            .await
            .map_err(LlmError::from)
    }

    /// Embed through the provider named in `GPU_LEASE_EMBEDDINGS` while the GPU is
    /// leased. It must serve the local model: the index holds one vector space
    /// (bge-m3 through OpenRouter measured cosine 0.9999+ against local, 2026-11-04).
    /// A provider that is unknown, over its cap or out of requests refuses, and
    /// callers degrade as they do without embeddings.
    async fn lease_embeddings(
        &self,
        provider: &str,
        model: &str,
        text: &str,
    ) -> Result<Vec<f32>, LlmError> {
        use crate::llm_providers::{
            LlmProvider, ProviderConfig, openai_compat::OpenAiCompatProvider,
        };
        let entry = crate::provider_registry::get(provider).ok_or_else(|| {
            LlmError::ServerNotReachable(format!("GPU_LEASE_EMBEDDINGS: no provider '{provider}'"))
        })?;
        crate::budget::verdict(provider, model, &entry).map_err(LlmError::ServerNotReachable)?;
        let client = OpenAiCompatProvider::new(
            ProviderConfig {
                provider_name: provider.to_string(),
                model: model.to_string(),
                api_key: entry.api_key(),
                base_url: entry.base_url(),
                timeout: self.config.timeout,
                temperature: 0.0,
                max_tokens: None,
            },
            LlmProviderType::OpenAiCompat,
        );
        let _permit = crate::provider_gate::acquire(provider).await;
        client.embed(text).await.map_err(LlmError::from)
    }

    /// Chat with the model using message history.
    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmResponse, LlmError> {
        self.refuse_local_if_gpu_leased()?;
        let _permit = crate::provider_gate::acquire(&self.config.provider_label()).await;
        self.provider.chat(messages).await.map_err(LlmError::from)
    }
}

/// `<provider>:<model>` from `GPU_LEASE_EMBEDDINGS`; empty or malformed means none.
fn parse_lease_embedder(v: &str) -> Option<(String, String)> {
    let (provider, model) = v.trim().split_once(':')?;
    (!provider.is_empty() && !model.is_empty()).then(|| (provider.to_string(), model.to_string()))
}

#[cfg(test)]
mod lease_embedder_tests {
    use super::parse_lease_embedder;

    #[test]
    fn names_a_provider_and_a_model_or_nothing() {
        assert_eq!(
            parse_lease_embedder(" openrouter-paid:baai/bge-m3 "),
            Some(("openrouter-paid".into(), "baai/bge-m3".into()))
        );
        for off in ["", "off", ":x", "y:"] {
            assert_eq!(parse_lease_embedder(off), None);
        }
    }
}

impl Default for LlmClient {
    fn default() -> Self {
        Self::with_config(LlmConfig::default()).expect("Failed to create default LLM client")
    }
}

// ============================================================================
// Response Parsing
// ============================================================================

// `first_json_object` and `escape_stray_backslashes` live in plugin-api so
// plugins parse model output the same way.
pub use agent_brain_plugin_api::json::{escape_stray_backslashes, first_json_object};

/// Extract JSON from a response that might be wrapped in markdown code blocks.
pub fn extract_json(text: &str) -> &str {
    let trimmed = text.trim();

    // Check for ```json ... ``` blocks
    if let Some(start) = trimmed.find("```json") {
        let content_start = start + 7;
        if let Some(end) = trimmed[content_start..].find("```") {
            return trimmed[content_start..content_start + end].trim();
        }
    }

    // Check for ``` ... ``` blocks
    if let Some(start) = trimmed.find("```") {
        let content_start = start + 3;
        // Skip the optional language identifier on the first line
        let content = &trimmed[content_start..];
        let actual_start = content.find('\n').map_or(0, |i| i + 1);
        if let Some(end) = content[actual_start..].find("```") {
            return content[actual_start..actual_start + end].trim();
        }
    }

    // Look for a bare JSON object or array. Pick whichever structure OPENS
    // first, so a top-level array of objects (`[{...}]`) isn't mis-extracted to
    // its first inner object (previously '{' was always preferred).
    let obj = (trimmed.find('{'), trimmed.rfind('}'));
    let arr = (trimmed.find('['), trimmed.rfind(']'));
    match (obj, arr) {
        ((Some(os), Some(oe)), (Some(ars), Some(are))) => {
            if ars < os {
                return &trimmed[ars..=are];
            }
            return &trimmed[os..=oe];
        }
        ((Some(os), Some(oe)), _) => return &trimmed[os..=oe],
        (_, (Some(ars), Some(are))) => return &trimmed[ars..=are],
        _ => {}
    }

    trimmed
}

#[cfg(test)]
mod tests {

    #[test]
    fn an_invalid_escape_in_a_string_is_repaired_not_fatal() {
        // The 2026-09-30 brain-exercise output, trimmed.
        let text =
            r#"{"answer":"Research newsnation.","inferences":["c-span (154/14 $\approx$ 11.0)"]}"#;
        let v = first_json_object(text, &["answer"]).expect("repaired");
        assert_eq!(v["answer"], "Research newsnation.");
        assert!(v["inferences"][0].as_str().unwrap().contains(r"$\approx$"));
    }

    #[test]
    fn valid_escapes_survive_the_repair_unchanged() {
        let valid = r#"{"a":"line\nnext \"q\" \\ \u00e9 \/"}"#;
        assert_eq!(escape_stray_backslashes(valid), valid);
        assert_eq!(
            escape_stray_backslashes(r#"{"p":"C:\data"}"#),
            r#"{"p":"C:\\data"}"#
        );
        // Outside a string nothing is touched.
        assert_eq!(escape_stray_backslashes(r"\x {}"), r"\x {}");
    }
    use super::*;

    /// The 2026-09-26 reply: the same fenced answer emitted twice.
    #[test]
    fn first_json_object_survives_a_duplicated_fenced_answer() {
        let one = "```json\n{\"answer\": \"## REPORT\\nline\", \"confidence\": 5}\n```";
        let twice = format!("{one}\n{one}");
        let v = first_json_object(&twice, &["answer"]).expect("first object parses");
        assert_eq!(v["answer"], "## REPORT\nline");
    }

    #[test]
    fn first_json_object_skips_prose_braces_and_requires_keys() {
        let t = r#"Use {braces} freely. {"steps": [1]} trailing {"x":1}"#;
        assert_eq!(first_json_object(t, &["steps"]).unwrap()["steps"][0], 1);
        // A broken outer object must not yield its inner one.
        let broken = r#"{"answer": "a", "meta": {"k": 1}, oops}"#;
        assert!(first_json_object(broken, &["answer"]).is_none());
    }

    #[test]
    fn test_llm_config_default() {
        let config = LlmConfig::default();
        let engine = agent_brain_engine::current();
        assert_eq!(config.provider, LlmProviderType::Local);
        assert_eq!(config.base_url.as_deref(), Some(engine.url.as_str()));
        assert_eq!(Some(config.model.as_str()), engine.model(Role::Generation));
        assert_eq!(config.temperature, 0.7);
    }

    #[test]
    fn test_llm_config_builder() {
        let config = LlmConfig::new("http://custom:1234", "mistral")
            .with_temperature(0.5)
            .with_max_tokens(1000)
            .with_timeout(Duration::from_secs(60));

        assert_eq!(config.base_url.as_deref(), Some("http://custom:1234"));
        assert_eq!(config.model, "mistral");
        assert_eq!(config.temperature, 0.5);
        assert_eq!(config.max_tokens, Some(1000));
        assert_eq!(config.timeout, Duration::from_secs(60));
    }

    #[test]
    fn test_llm_config_temperature_clamping() {
        let config = LlmConfig::default().with_temperature(2.0);
        assert_eq!(config.temperature, 1.0);

        let config = LlmConfig::default().with_temperature(-0.5);
        assert_eq!(config.temperature, 0.0);
    }

    #[test]
    fn test_chat_message_constructors() {
        let system = ChatMessage::system("You are helpful.");
        assert_eq!(system.role, "system");
        assert_eq!(system.content, "You are helpful.");

        let user = ChatMessage::user("Hello");
        assert_eq!(user.role, "user");
        assert_eq!(user.content, "Hello");

        let assistant = ChatMessage::assistant("Hi there!");
        assert_eq!(assistant.role, "assistant");
        assert_eq!(assistant.content, "Hi there!");
    }

    #[test]
    fn test_extract_json_plain() {
        let text = r#"{"key": "value"}"#;
        assert_eq!(extract_json(text), r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_json_with_markdown() {
        let text = r#"Here is the response:
```json
{"key": "value"}
```
That's all."#;
        assert_eq!(extract_json(text), r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_json_with_plain_code_block() {
        let text = r#"```
{"key": "value"}
```"#;
        assert_eq!(extract_json(text), r#"{"key": "value"}"#);
    }

    #[test]
    fn test_extract_json_surrounded_by_text() {
        let text = r#"The corrected body is: {"name": "test", "id": 123} and that should work."#;
        assert_eq!(extract_json(text), r#"{"name": "test", "id": 123}"#);
    }

    #[test]
    fn test_llm_client_creation() {
        let client = LlmClient::new();
        assert!(client.is_ok());
    }

    #[test]
    fn test_llm_client_with_config() {
        let config = LlmConfig::new("http://test:1234", "test-model");
        let client = LlmClient::with_config(config);
        assert!(client.is_ok());

        let client = client.unwrap();
        assert_eq!(
            client.config().base_url.as_deref(),
            Some("http://test:1234")
        );
        assert_eq!(client.config().model, "test-model");
    }
}
