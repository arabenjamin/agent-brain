use serde::Deserialize;
use std::env;

/// Top-level configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub database: DatabaseConfig,
    pub llm: LlmProviderConfig,
    /// Optional overrides for the chat client adapter's LLM.
    /// When all fields are `None`, chat falls back to `llm` (backward-compatible).
    pub chat_llm: ChatLlmConfig,
    pub logging: LoggingConfig,
    pub telemetry: TelemetryConfig,
}

/// Neo4j connection settings.
#[derive(Debug, Clone)]
pub struct DatabaseConfig {
    pub uri: String,
    pub user: String,
    pub password: String,
}

/// The brain's active LLM: which provider and model, plus the host-Ollama
/// settings every config inherits (embeddings always run there). Endpoints and
/// keys are not here: the provider registry (`models.yaml` `providers:`) names
/// the env vars that hold them, and `model_router::config_for_catalog_entry`
/// reads them, for this config as for every routed one.
#[derive(Debug, Clone)]
pub struct LlmProviderConfig {
    /// A provider-registry name (`LLM_PROVIDER`, folded by `canonical_name`;
    /// default `ollama`). A name the registry does not know runs as host Ollama.
    pub provider: String,
    /// The active model: `ANTHROPIC_MODEL`, `GEMINI_MODEL` or `LLAMACPP_MODEL`
    /// for those providers when set, else `OLLAMA_MODEL`.
    pub model: String,
    /// Host Ollama (default: http://localhost:11434), where embeddings run.
    pub ollama_local_url: String,
    pub ollama_embed_model: Option<String>,
    /// How long Ollama keeps a model resident in VRAM after a request, as a Go
    /// duration string (`30m`, `2h`).  A negative duration (`-1m`) pins the model
    /// indefinitely.  `None` leaves Ollama's own default (5m) in place.
    ///
    /// Chain steps run serially with gaps between them, so on a shared GPU the
    /// model can be evicted between consecutive steps of the same chain and pay a
    /// full reload each time.  Env var: `OLLAMA_KEEP_ALIVE`.
    pub ollama_keep_alive: Option<String>,
}

/// The model a provider runs when `LLM_PROVIDER` / `CHAT_LLM_PROVIDER` names it
/// and no model is given: its own `*_MODEL` env var, else `OLLAMA_MODEL`.
pub fn default_model_for(provider: &str) -> String {
    let own = match provider {
        "anthropic" => env::var("ANTHROPIC_MODEL").ok(),
        "gemini" => env::var("GEMINI_MODEL").ok(),
        "llamacpp" => env::var("LLAMACPP_MODEL").ok(),
        _ => None,
    };
    own.or_else(|| env::var("OLLAMA_MODEL").ok())
        .unwrap_or_else(|| "granite4:latest".to_string())
}

/// Validate a Go duration string (`30m`, `2h`, `-1m`, `1h30m`) before it is sent
/// to Ollama as `keep_alive`.
///
/// Ollama rejects a malformed `keep_alive` with a 400, and `keep_alive` rides on
/// *every* generate/chat/embeddings request — so a typo in the env var would take
/// down all LLM calls rather than degrade one. This fails closed: anything that
/// doesn't parse is dropped with a warning and Ollama keeps its own default.
///
/// Note the bare-number case is deliberately rejected. Ollama accepts a raw JSON
/// *number* as seconds, but we serialize the value as a string, and `"30"` is not
/// a valid Go duration — so requiring a unit suffix keeps the two representations
/// from diverging.
pub(crate) fn validate_go_duration(s: &str) -> Option<String> {
    const UNITS: [&str; 6] = ["ns", "us", "ms", "s", "m", "h"];

    let valid = !s.is_empty() && {
        // Must end in a known unit, and every character must be part of a
        // number/unit sequence (covers compound forms like "1h30m").
        let ends_in_unit = UNITS.iter().any(|u| s.ends_with(u));
        let body_ok = s
            .strip_prefix('-')
            .unwrap_or(s)
            .chars()
            .all(|c| c.is_ascii_digit() || c == '.' || c.is_ascii_alphabetic());
        let has_digit = s.chars().any(|c| c.is_ascii_digit());
        ends_in_unit && body_ok && has_digit
    };

    if valid {
        Some(s.to_string())
    } else {
        tracing::warn!(
            value = %s,
            "OLLAMA_KEEP_ALIVE is not a valid Go duration (e.g. '30m', '2h', '-1m' for indefinite) — ignoring"
        );
        None
    }
}

/// Optional LLM overrides for the human-facing chat adapter.
///
/// Any field left `None` falls through to the corresponding value in
/// [`LlmProviderConfig`], so an empty `ChatLlmConfig` is a no-op — the
/// chat service uses exactly the same model as the brain's internal LLM.
///
/// Set these when you want a different model for chat than for internal
/// cognitive operations (e.g. cloud Anthropic for chat, local Ollama for
/// consolidation/embeddings).
#[derive(Debug, Clone, Default)]
pub struct ChatLlmConfig {
    /// Override the LLM provider for chat: a provider-registry name (e.g.
    /// `"anthropic"`). Env var: `CHAT_LLM_PROVIDER`
    pub provider: Option<String>,
    /// Override the model name for chat (e.g. `"claude-opus-4-5"`).
    /// Env var: `CHAT_LLM_MODEL`
    pub model: Option<String>,
    /// Override the API key for the chat LLM.
    /// Env var: `CHAT_API_KEY`
    pub api_key: Option<String>,
    /// Override the base URL for the chat LLM endpoint.
    /// Env var: `CHAT_LLM_BASE_URL`
    pub base_url: Option<String>,
}

impl ChatLlmConfig {
    /// Returns `true` when any override field is set, meaning chat should use
    /// a dedicated `Arc` separate from the brain's `llm_config`.  When all
    /// fields are `None`, chat should share the brain's Arc so that
    /// `use_model` calls affect it immediately.
    pub fn has_overrides(&self) -> bool {
        self.provider.is_some()
            || self.model.is_some()
            || self.api_key.is_some()
            || self.base_url.is_some()
    }
}

/// Structured logging configuration.
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
}

/// Optional DuckDB telemetry sink and model catalog.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// Path to the DuckDB file. `None` disables telemetry.
    pub db_path: Option<String>,
    /// Path to the YAML model catalog (default: `models.yaml`).
    pub model_catalog_path: String,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

impl Config {
    /// Load configuration from environment variables.
    /// Call `dotenvy::dotenv().ok()` before this to load from .env file.
    pub fn from_env() -> Result<Self, ConfigError> {
        Ok(Self {
            database: DatabaseConfig {
                uri: env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7687".to_string()),
                user: env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".to_string()),
                password: env::var("NEO4J_PASSWORD")
                    .map_err(|_| ConfigError::Missing("NEO4J_PASSWORD"))?,
            },
            llm: {
                let provider = crate::services::provider_registry::canonical_name(
                    &env::var("LLM_PROVIDER").unwrap_or_else(|_| "ollama".to_string()),
                );
                LlmProviderConfig {
                    model: default_model_for(&provider),
                    provider,
                    ollama_local_url: env::var("OLLAMA_LOCAL_URL")
                        .unwrap_or_else(|_| "http://localhost:11434".to_string()),
                    ollama_embed_model: env::var("OLLAMA_EMBED_MODEL").ok(),
                    ollama_keep_alive: env::var("OLLAMA_KEEP_ALIVE")
                        .ok()
                        .and_then(|s| validate_go_duration(s.trim())),
                }
            },
            chat_llm: ChatLlmConfig {
                provider: env::var("CHAT_LLM_PROVIDER")
                    .ok()
                    .map(|s| crate::services::provider_registry::canonical_name(&s)),
                model: env::var("CHAT_LLM_MODEL").ok(),
                api_key: env::var("CHAT_API_KEY").ok(),
                base_url: env::var("CHAT_LLM_BASE_URL").ok(),
            },
            logging: LoggingConfig {
                level: env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string()),
                format: env::var("LOG_FORMAT")
                    .map(|s| match s.to_lowercase().as_str() {
                        "json" => LogFormat::Json,
                        _ => LogFormat::Pretty,
                    })
                    .unwrap_or_default(),
            },
            telemetry: TelemetryConfig {
                db_path: env::var("TELEMETRY_DB_PATH").ok(),
                model_catalog_path: env::var("MODEL_CATALOG_PATH")
                    .unwrap_or_else(|_| "models.yaml".to_string()),
            },
        })
    }

    /// Create a config with default/test values (no env vars required).
    #[cfg(test)]
    pub fn test_config() -> Self {
        Self {
            database: DatabaseConfig {
                uri: "bolt://localhost:7687".to_string(),
                user: "neo4j".to_string(),
                password: "testpassword".to_string(),
            },
            llm: LlmProviderConfig {
                provider: "ollama".to_string(),
                model: "granite4:latest".to_string(),
                ollama_local_url: "http://localhost:11434".to_string(),
                ollama_embed_model: None,
                ollama_keep_alive: None,
            },
            chat_llm: ChatLlmConfig::default(),
            logging: LoggingConfig {
                level: "debug".to_string(),
                format: LogFormat::Pretty,
            },
            telemetry: TelemetryConfig {
                db_path: Some("test_telemetry.db".to_string()),
                model_catalog_path: "models.yaml".to_string(),
            },
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("Missing required environment variable: {0}")]
    Missing(&'static str),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_duration_accepts_valid_forms() {
        for s in ["30m", "2h", "1h30m", "500ms", "-1m", "1.5h"] {
            assert_eq!(validate_go_duration(s), Some(s.to_string()), "rejected {s}");
        }
    }

    #[test]
    fn go_duration_rejects_malformed_values() {
        // A bad keep_alive rides on every request, so anything questionable must
        // fail closed to None rather than 400 the whole LLM path.
        for s in ["", "30", "forever", "5 minutes", "m", "30x"] {
            assert_eq!(validate_go_duration(s), None, "accepted {s:?}");
        }
    }

    #[test]
    fn test_config_defaults() {
        let config = Config::test_config();
        assert_eq!(config.database.uri, "bolt://localhost:7687");
        assert_eq!(config.llm.provider, "ollama");
        assert_eq!(config.llm.model, "granite4:latest");
        assert_eq!(config.logging.format, LogFormat::Pretty);
    }

    #[test]
    fn test_log_format_deserialization() {
        assert_eq!(
            serde_json::from_str::<LogFormat>("\"json\"").unwrap(),
            LogFormat::Json
        );
        assert_eq!(
            serde_json::from_str::<LogFormat>("\"pretty\"").unwrap(),
            LogFormat::Pretty
        );
    }
}
