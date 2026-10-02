use std::sync::Arc;
use std::time::Duration;

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

/// Resolve the local model's request timeout from `OLLAMA_LOCAL_TIMEOUT_SECS`,
/// falling back to [`DEFAULT_LOCAL_TIMEOUT_SECS`]. An unparseable or zero
/// value warns and uses the default rather than disabling the timeout.
pub fn local_timeout() -> Duration {
    match std::env::var("OLLAMA_LOCAL_TIMEOUT_SECS") {
        Ok(raw) => match raw.trim().parse::<u64>() {
            Ok(secs) if secs > 0 => Duration::from_secs(secs),
            _ => {
                tracing::warn!(
                    value = %raw,
                    default_secs = DEFAULT_LOCAL_TIMEOUT_SECS,
                    "OLLAMA_LOCAL_TIMEOUT_SECS is not a positive integer — using the default"
                );
                Duration::from_secs(DEFAULT_LOCAL_TIMEOUT_SECS)
            }
        },
        Err(_) => Duration::from_secs(DEFAULT_LOCAL_TIMEOUT_SECS),
    }
}

/// Default Ollama API URL.
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434";

/// Default `llama-server` OpenAI-compatible endpoint.
const DEFAULT_LLAMACPP_URL: &str = "http://localhost:8080";

/// Build the LLM config used for **background / scheduled jobs** (the "local"
/// route taken whenever a step sets `provider_hint: "ollama"` / `USE_LOCAL_LLM`).
///
/// The generation backend is selected by `LOCAL_LLM_BACKEND`:
/// - `ollama` (default) — native Ollama at `OLLAMA_LOCAL_URL` / `OLLAMA_LOCAL_MODEL`.
/// - `llamacpp` — `llama-server`'s OpenAI-compatible API at `LLAMACPP_URL` /
///   `LLAMACPP_MODEL` (falls back to `OLLAMA_LOCAL_MODEL` for the model name).
///
/// **Embeddings always stay on local Ollama** regardless of the generation
/// backend: `embed_base_url` is pinned to `OLLAMA_LOCAL_URL` and the embed model
/// to `OLLAMA_EMBED_MODEL` (`bge-m3`). llama.cpp single-model servers don't
/// serve `/api/embeddings`, so this keeps `search_notes` / `store_note` working
/// unchanged while generation moves to llama.cpp.
/// Context window for **host-Ollama generation**, from `OLLAMA_NUM_CTX`.
///
/// Ollama serves a 4096-token window by default whatever the model claims, and
/// input past it is dropped **silently** — no error, no marker, just a shorter
/// prompt. Measured 2026-09-11 over 30 days: 103 of 874 `reason` calls came back
/// with `tokens_in` sitting exactly on 4096 (granite4 58, gemma4 45; the
/// llama.cpp and cloud paths showed zero), i.e. they were clipped and answered
/// from a truncated prompt. Every host-Ollama config must therefore set this
/// explicitly — leaving `num_ctx: None` is the bug, not the default.
///
/// Raising it costs KV-cache VRAM on a shared GPU, so it is env-tunable; 16384
/// covers 99.4% of observed `reason` inputs (only 5/874 exceeded it).
pub fn ollama_num_ctx() -> u32 {
    const DEFAULT_OLLAMA_NUM_CTX: u32 = 16384;
    std::env::var("OLLAMA_NUM_CTX")
        .ok()
        .and_then(|v| v.trim().parse::<u32>().ok())
        .filter(|n| *n > 0)
        .unwrap_or(DEFAULT_OLLAMA_NUM_CTX)
}

pub fn build_local_llm_config() -> LlmConfig {
    let backend = std::env::var("LOCAL_LLM_BACKEND")
        .unwrap_or_default()
        .trim()
        .to_lowercase();

    let ollama_url =
        std::env::var("OLLAMA_LOCAL_URL").unwrap_or_else(|_| DEFAULT_OLLAMA_URL.to_string());

    let (provider, base_url, model) = match backend.as_str() {
        "llamacpp" | "llama-cpp" | "llama.cpp" | "llama_cpp" => {
            let url =
                std::env::var("LLAMACPP_URL").unwrap_or_else(|_| DEFAULT_LLAMACPP_URL.to_string());
            // A single-model llama-server serves whatever GGUF it was launched
            // with regardless of the requested name, so this is mostly a ledger
            // label; keep it meaningful by reusing OLLAMA_LOCAL_MODEL if set.
            let model = std::env::var("LLAMACPP_MODEL")
                .or_else(|_| std::env::var("OLLAMA_LOCAL_MODEL"))
                .unwrap_or_else(|_| "local".to_string());
            (LlmProviderType::LlamaCpp, url, model)
        }
        _ => {
            let model =
                std::env::var("OLLAMA_LOCAL_MODEL").unwrap_or_else(|_| "gemma4:latest".to_string());
            (LlmProviderType::Ollama, ollama_url.clone(), model)
        }
    };

    let mut cfg = LlmConfig::default()
        .with_provider(provider)
        .with_base_url(base_url)
        .with_model(model)
        // Embeddings are ALWAYS local Ollama — pin the embed endpoint to it even
        // when generation runs on llama.cpp (whose base_url can't serve embeds).
        .with_embed_base_url(ollama_url)
        .with_timeout(local_timeout());

    // Only the Ollama path needs this: llama.cpp fixes its context at launch
    // (`-c` / `-np`) and ignores a per-request num_ctx entirely.
    if matches!(cfg.provider, LlmProviderType::Ollama) {
        cfg = cfg.with_num_ctx(ollama_num_ctx());
    }

    if let Ok(em) = std::env::var("OLLAMA_EMBED_MODEL") {
        cfg = cfg.with_embed_model(em);
    }
    // keep_alive is Ollama-only (ignored by the llama.cpp/OpenAI-compat provider,
    // which never serializes it). Keeping it set is harmless when backend=llamacpp.
    if let Some(ka) = std::env::var("OLLAMA_KEEP_ALIVE")
        .ok()
        .and_then(|s| crate::config::validate_go_duration(s.trim()))
    {
        cfg = cfg.with_keep_alive(ka);
    }
    cfg
}

/// Default model to use.
const DEFAULT_MODEL: &str = "qwen3.5:4b";

#[derive(Debug, Error)]
pub enum LlmError {
    #[error("HTTP request failed: {}", crate::services::llm_providers::describe_reqwest(.0))]
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
    Provider(#[from] crate::services::llm_providers::LlmProviderError),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum LlmProviderType {
    #[default]
    Ollama,
    /// Ollama Cloud (https://ollama.com) — same wire format as local, requires API key.
    OllamaCloud,
    /// Local llama.cpp `llama-server`, spoken via its OpenAI-compatible API
    /// (`/v1/chat/completions`, `/v1/embeddings`). One model per server instance;
    /// context window / GPU offload are launch-time flags, not per-request.
    #[serde(alias = "llamacpp", alias = "llama-cpp", alias = "llama.cpp")]
    LlamaCpp,
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
            LlmProviderType::Ollama => write!(f, "ollama"),
            LlmProviderType::OllamaCloud => write!(f, "ollama-cloud"),
            LlmProviderType::LlamaCpp => write!(f, "llamacpp"),
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

    /// Model name to use for embeddings.
    pub embed_model: Option<String>,

    /// Base URL for the embedding endpoint when it differs from the generation endpoint.
    /// Used by OllamaCloud to pin embed() calls to local Ollama instead of the cloud endpoint.
    pub embed_base_url: Option<String>,

    /// Request timeout.
    pub timeout: Duration,

    /// Temperature for generation (0.0 - 1.0).
    pub temperature: f32,

    /// Maximum tokens to generate.
    pub max_tokens: Option<u32>,

    /// Ollama-only: how long the model stays resident in VRAM after a request
    /// (Go duration string, e.g. `30m`; a negative duration pins it indefinitely).
    /// Ignored by every non-Ollama provider. `None` = Ollama's own default.
    pub keep_alive: Option<String>,

    /// Ollama-only context window override (`options.num_ctx`).
    ///
    /// Ollama defaults this to **4096 tokens regardless of what the model
    /// supports** — `gemma4:latest` advertises a 131 072-token context but is
    /// served at 4096 unless asked otherwise, and input past that is silently
    /// truncated. `None` keeps Ollama's default; raise it only for calls that
    /// genuinely need to read a large payload, since the window costs VRAM.
    pub num_ctx: Option<u32>,

    /// The provider-registry key this config was built from (`mistral`,
    /// `ollama-cloud`, …). `provider` says which client code runs; this says
    /// whose endpoint it runs against, which matters once several providers
    /// share the OpenAI-compatible client. `None` for configs not built from
    /// the registry. Recorded in the usage ledger via [`Self::provider_label`].
    pub provider_name: Option<String>,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            provider: LlmProviderType::Ollama,
            base_url: Some(DEFAULT_OLLAMA_URL.to_string()),
            api_key: None,
            model: DEFAULT_MODEL.to_string(),
            embed_model: None,
            embed_base_url: None,
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECS),
            temperature: 0.7,
            max_tokens: None,
            keep_alive: None,
            num_ctx: None,
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

    /// Set the embedding model (separate from the generation model).
    pub fn with_embed_model(mut self, model: impl Into<String>) -> Self {
        self.embed_model = Some(model.into());
        self
    }

    /// Set a separate base URL for the embedding endpoint.
    /// When set, embed() calls are routed to this URL instead of `base_url`.
    pub fn with_embed_base_url(mut self, url: impl Into<String>) -> Self {
        self.embed_base_url = Some(url.into());
        self
    }

    /// Set the Ollama `keep_alive` residency window (Go duration string).
    /// No-op for non-Ollama providers, which never serialize the field.
    pub fn with_keep_alive(mut self, keep_alive: impl Into<String>) -> Self {
        self.keep_alive = Some(keep_alive.into());
        self
    }

    /// Override Ollama's context window (`options.num_ctx`) for this config.
    /// See [`LlmConfig::num_ctx`] — Ollama's 4096 default truncates silently.
    pub fn with_num_ctx(mut self, num_ctx: u32) -> Self {
        self.num_ctx = Some(num_ctx);
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
    provider: Arc<dyn crate::services::llm_providers::LlmProvider>,
    embed_provider: Arc<dyn crate::services::llm_providers::LlmProvider>,
    config: LlmConfig,
}

impl LlmClient {
    /// Create a new LLM client with default configuration.
    pub fn new() -> Result<Self, LlmError> {
        Self::with_config(LlmConfig::default())
    }

    /// Create a new LLM client with custom configuration.
    pub fn with_config(config: LlmConfig) -> Result<Self, LlmError> {
        use crate::services::llm_providers::{
            ProviderConfig, anthropic::AnthropicProvider, gemini::GeminiProvider,
            ollama::OllamaProvider,
        };

        let provider_config = ProviderConfig {
            provider_name: config.provider_label(),
            model: config.model.clone(),
            api_key: config.api_key.clone(),
            base_url: config.base_url.clone(),
            timeout: config.timeout,
            temperature: config.temperature,
            max_tokens: config.max_tokens,
            keep_alive: config.keep_alive.clone(),
            num_ctx: config.num_ctx,
        };

        let provider: Arc<dyn crate::services::llm_providers::LlmProvider> = match config.provider {
            LlmProviderType::Ollama => Arc::new(OllamaProvider::new(provider_config)),
            // Ollama Cloud uses the OpenAI-compatible API at /v1/chat/completions,
            // not the native Ollama /api/chat endpoint.
            LlmProviderType::OllamaCloud => {
                use crate::services::llm_providers::openai_compat::OpenAiCompatProvider;
                Arc::new(OpenAiCompatProvider::new(provider_config))
            }
            // llama.cpp's `llama-server` speaks the same OpenAI-compatible wire
            // format as Ollama Cloud, so it reuses the same provider. `keep_alive`
            // / `num_ctx` in provider_config are ignored (llama.cpp sets those at
            // server launch, not per-request).
            // Generic OpenAI-compatible providers (Mistral, OpenRouter, …) use the
            // same client; their endpoint and key come from the provider registry.
            LlmProviderType::LlamaCpp | LlmProviderType::OpenAiCompat => {
                use crate::services::llm_providers::openai_compat::OpenAiCompatProvider;
                Arc::new(OpenAiCompatProvider::new(provider_config))
            }
            LlmProviderType::Anthropic => Arc::new(AnthropicProvider::new(provider_config)),
            LlmProviderType::Gemini => Arc::new(GeminiProvider::new(provider_config)),
        };

        // Initialize embedding provider (separate from generation if requested)
        let embed_provider = if let Some(ref embed_model) = config.embed_model {
            let embed_base = config.embed_base_url.clone().or(config.base_url.clone());
            let embed_config = ProviderConfig {
                provider_name: "ollama".to_string(),
                model: embed_model.clone(),
                api_key: config.api_key.clone(),
                base_url: embed_base,
                timeout: config.timeout,
                temperature: 0.0,
                max_tokens: None,
                // The embedding model is reloaded far more often than the
                // generation model (every search_notes / store_note), so it
                // benefits from residency at least as much.
                keep_alive: config.keep_alive.clone(),
                num_ctx: None, // embeddings don't use a generation context window
            };
            // Embeddings always use local Ollama regardless of the active generation provider.
            Arc::new(OllamaProvider::new(embed_config))
                as Arc<dyn crate::services::llm_providers::LlmProvider>
        } else if let Some(ref embed_base) = config.embed_base_url {
            // embed_base_url is set but no dedicated embed_model (OllamaCloud without
            // OLLAMA_EMBED_MODEL).  Before the OllamaCloud→OpenAiCompatProvider refactor,
            // the fallback `provider.clone()` was still an OllamaProvider.  Now it is an
            // OpenAiCompatProvider pointed at the cloud URL, which causes embed() to hit
            // `{cloud_url}/v1/embeddings` — hanging for the full 120-second timeout every
            // time search_notes is called.  Route to local Ollama instead.  If the model
            // isn't pulled locally the call fails fast and vector search is skipped
            // gracefully; BM25 still runs.
            let embed_config = ProviderConfig {
                provider_name: "ollama".to_string(),
                model: config.model.clone(),
                api_key: None, // local Ollama doesn't need auth
                base_url: Some(embed_base.clone()),
                timeout: config.timeout,
                temperature: 0.0,
                max_tokens: None,
                keep_alive: config.keep_alive.clone(),
                num_ctx: None,
            };
            Arc::new(OllamaProvider::new(embed_config))
                as Arc<dyn crate::services::llm_providers::LlmProvider>
        } else {
            provider.clone()
        };

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
        let local = crate::services::provider_registry::get(&self.config.provider_label())
            .map(|e| e.billing == crate::services::provider_registry::Billing::Local)
            .unwrap_or(matches!(
                self.config.provider,
                LlmProviderType::Ollama | LlmProviderType::LlamaCpp
            ));
        match crate::services::gpu_lease::refusal() {
            Some(reason) if local => Err(LlmError::ServerNotReachable(reason)),
            _ => Ok(()),
        }
    }

    /// Embeddings run on host Ollama (bge-m3 lives on the GPU), so a GPU lease
    /// pauses them: a single search would otherwise reload bge-m3 mid-training.
    /// Callers already degrade (search to BM25; a stored note is marked
    /// `embedding_pending` and backfilled on release).
    fn refuse_embeddings_if_gpu_leased(&self) -> Result<(), LlmError> {
        let local = self.config.embed_base_url.is_some()
            || self.config.embed_model.is_some()
            || matches!(self.config.provider, LlmProviderType::Ollama);
        match crate::services::gpu_lease::refusal() {
            Some(reason) if local => Err(LlmError::ServerNotReachable(reason)),
            _ => Ok(()),
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
        let _permit = crate::services::provider_gate::acquire(&self.config.provider_label()).await;
        self.provider
            .generate(prompt, system)
            .await
            .map_err(LlmError::from)
    }

    /// Generate embeddings for a text.
    pub async fn embeddings(&self, text: &str) -> Result<Vec<f32>, LlmError> {
        self.refuse_embeddings_if_gpu_leased()?;
        self.embed_provider
            .embed(text)
            .await
            .map_err(LlmError::from)
    }

    /// Chat with the model using message history.
    pub async fn chat(&self, messages: &[ChatMessage]) -> Result<LlmResponse, LlmError> {
        self.refuse_local_if_gpu_leased()?;
        let _permit = crate::services::provider_gate::acquire(&self.config.provider_label()).await;
        self.provider.chat(messages).await.map_err(LlmError::from)
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

pub fn parse_corrected_body(response: &str) -> Result<Option<serde_json::Value>, LlmError> {
    let json_str = extract_json(response);

    if json_str.trim() == "null" {
        return Ok(None);
    }

    serde_json::from_str(json_str)
        .map(Some)
        .map_err(|e| LlmError::ParseError(format!("Failed to parse corrected body: {}", e)))
}

/// The first complete JSON object in `text` that has every key in `must_have`.
///
/// Parses from each `{` in turn and stops at the end of ONE value, so trailing
/// text cannot break it. Slicing first-`{` to last-`}` could: on 2026-09-26 a
/// model wrote its whole ```json answer twice, the slice spanned both objects,
/// the parse failed, and the fenced raw reply was stored as the report. Keys
/// are required so a broken outer object cannot yield one of its inner ones.
pub fn first_json_object(text: &str, must_have: &[&str]) -> Option<serde_json::Value> {
    first_json_object_strict(text, must_have).or_else(|| {
        // Models write LaTeX and Windows paths into JSON strings (`$\approx$`,
        // `C:\data`), and an invalid escape fails the whole value. Seen
        // 2026-09-30: the brain-exercise `reason` step's answer carried
        // `$\approx$`, the parse failed, the raw JSON became a Task goal, and
        // five evaluator retries of that task all failed. Retry once with each
        // stray backslash escaped.
        let repaired = escape_stray_backslashes(text);
        (repaired != text).then(|| first_json_object_strict(&repaired, must_have))?
    })
}

/// Inside JSON string literals, double every backslash that does not begin a
/// valid escape (`\" \\ \/ \b \f \n \r \t \uXXXX`). Text outside strings is
/// left alone. Never makes valid JSON invalid.
pub fn escape_stray_backslashes(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 8);
    let mut chars = text.chars().peekable();
    let mut in_string = false;
    while let Some(c) = chars.next() {
        if !in_string {
            if c == '"' {
                in_string = true;
            }
            out.push(c);
            continue;
        }
        match c {
            '"' => {
                in_string = false;
                out.push(c);
            }
            '\\' => match chars.peek().copied() {
                Some(n @ ('"' | '\\' | '/' | 'b' | 'f' | 'n' | 'r' | 't')) => {
                    out.push('\\');
                    out.push(n);
                    chars.next();
                }
                Some('u') => {
                    let hex: String = chars.clone().skip(1).take(4).collect();
                    if hex.len() == 4 && hex.chars().all(|h| h.is_ascii_hexdigit()) {
                        out.push('\\');
                    } else {
                        out.push_str("\\\\");
                    }
                }
                _ => out.push_str("\\\\"),
            },
            _ => out.push(c),
        }
    }
    out
}

fn first_json_object_strict(text: &str, must_have: &[&str]) -> Option<serde_json::Value> {
    for (i, _) in text.match_indices('{').take(64) {
        let mut values =
            serde_json::Deserializer::from_str(&text[i..]).into_iter::<serde_json::Value>();
        if let Some(Ok(v)) = values.next()
            && let Some(obj) = v.as_object()
            && must_have.iter().all(|k| obj.contains_key(*k))
        {
            return Some(v);
        }
    }
    None
}

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
        let actual_start = content.find('\n').map(|i| i + 1).unwrap_or(0);
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
        assert_eq!(config.base_url.as_deref(), Some("http://localhost:11434"));
        assert_eq!(config.model, "qwen3.5:4b");
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
    fn test_parse_corrected_body() {
        let response = r#"{"user_id": 123, "name": "test"}"#;
        let body = parse_corrected_body(response).unwrap();
        assert!(body.is_some());
        let body = body.unwrap();
        assert_eq!(body["user_id"], 123);
        assert_eq!(body["name"], "test");
    }

    #[test]
    fn test_parse_corrected_body_null() {
        let response = "null";
        let body = parse_corrected_body(response).unwrap();
        assert!(body.is_none());
    }

    #[test]
    fn test_parse_corrected_body_with_markdown() {
        let response = r#"Here's the fix:
```json
{"user_id": 456}
```"#;
        let body = parse_corrected_body(response).unwrap();
        assert!(body.is_some());
        assert_eq!(body.unwrap()["user_id"], 456);
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
