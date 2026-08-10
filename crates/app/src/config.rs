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
    pub secrets: SecretsConfig,
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

/// LLM provider selection and per-provider settings.
#[derive(Debug, Clone)]
pub struct LlmProviderConfig {
    pub provider: crate::services::llm::LlmProviderType,
    /// Ollama Cloud endpoint (default: https://ollama.com). Used when provider=ollama-cloud.
    pub ollama_url: String,
    /// Local Ollama endpoint (default: http://localhost:11434).
    /// Always used for embeddings, and for provider=ollama.
    pub ollama_local_url: String,
    pub ollama_model: String,
    /// Model used exclusively for background/scheduled jobs on the local Ollama instance.
    /// Defaults to `OLLAMA_LOCAL_MODEL` env var, falls back to `"gemma4:latest"`.
    pub ollama_local_model: String,
    pub ollama_embed_model: Option<String>,
    pub ollama_api_key: Option<String>,
    /// How long Ollama keeps a model resident in VRAM after a request, as a Go
    /// duration string (`30m`, `2h`).  A negative duration (`-1m`) pins the model
    /// indefinitely.  `None` leaves Ollama's own default (5m) in place.
    ///
    /// Chain steps run serially with gaps between them, so on a shared GPU the
    /// model can be evicted between consecutive steps of the same chain and pay a
    /// full reload each time.  Env var: `OLLAMA_KEEP_ALIVE`.
    pub ollama_keep_alive: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub anthropic_model: Option<String>,
    pub gemini_api_key: Option<String>,
    pub gemini_model: Option<String>,
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
    /// Override the LLM provider for chat (e.g. `"anthropic"`).
    /// Env var: `CHAT_LLM_PROVIDER`
    pub provider: Option<crate::services::llm::LlmProviderType>,
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

/// Secret provider backend configuration.
#[derive(Debug, Clone)]
pub struct SecretsConfig {
    pub provider: SecretProviderType,
    /// Path to local encrypted secrets file (local provider).
    pub secrets_file: Option<String>,
    /// Encryption key for local secrets (local provider).
    pub secrets_encryption_key: Option<String>,
    /// Vault server address (vault provider).
    pub vault_address: Option<String>,
    /// Vault auth token (vault provider).
    pub vault_token: Option<String>,
    /// Vault KV mount path (vault provider).
    pub vault_mount_path: Option<String>,
    /// Vault namespace — enterprise only (vault provider).
    pub vault_namespace: Option<String>,
    /// AWS region (aws provider).
    pub aws_region: Option<String>,
    /// Prefix applied to all AWS secret names (aws provider).
    pub aws_secret_prefix: Option<String>,
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

/// Type of secret provider to use.
#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SecretProviderType {
    /// Local encrypted file storage.
    #[default]
    Local,
    /// HashiCorp Vault.
    Vault,
    /// AWS Secrets Manager.
    Aws,
    /// No secret provider (credentials must be passed explicitly).
    None,
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
            llm: LlmProviderConfig {
                provider: env::var("LLM_PROVIDER")
                    .map(|s| match s.to_lowercase().as_str() {
                        "anthropic" => crate::services::llm::LlmProviderType::Anthropic,
                        "gemini" => crate::services::llm::LlmProviderType::Gemini,
                        "ollama-cloud" | "ollamacloud" => {
                            crate::services::llm::LlmProviderType::OllamaCloud
                        }
                        _ => crate::services::llm::LlmProviderType::Ollama,
                    })
                    .unwrap_or_default(),
                ollama_url: env::var("OLLAMA_URL")
                    .unwrap_or_else(|_| "https://ollama.com".to_string()),
                ollama_local_url: env::var("OLLAMA_LOCAL_URL")
                    .unwrap_or_else(|_| "http://localhost:11434".to_string()),
                ollama_model: env::var("OLLAMA_MODEL")
                    .unwrap_or_else(|_| "granite4:latest".to_string()),
                ollama_local_model: env::var("OLLAMA_LOCAL_MODEL")
                    .unwrap_or_else(|_| "gemma4:latest".to_string()),
                ollama_embed_model: env::var("OLLAMA_EMBED_MODEL").ok(),
                ollama_api_key: env::var("OLLAMA_API_KEY").ok(),
                ollama_keep_alive: env::var("OLLAMA_KEEP_ALIVE")
                    .ok()
                    .and_then(|s| validate_go_duration(s.trim())),
                anthropic_api_key: env::var("ANTHROPIC_API_KEY").ok(),
                anthropic_model: env::var("ANTHROPIC_MODEL").ok(),
                gemini_api_key: env::var("GEMINI_API_KEY").ok(),
                gemini_model: env::var("GEMINI_MODEL").ok(),
            },
            chat_llm: ChatLlmConfig {
                provider: env::var("CHAT_LLM_PROVIDER").ok().map(|s| {
                    match s.to_lowercase().as_str() {
                        "anthropic" => crate::services::llm::LlmProviderType::Anthropic,
                        "gemini" => crate::services::llm::LlmProviderType::Gemini,
                        "ollama-cloud" | "ollamacloud" => {
                            crate::services::llm::LlmProviderType::OllamaCloud
                        }
                        _ => crate::services::llm::LlmProviderType::Ollama,
                    }
                }),
                model: env::var("CHAT_LLM_MODEL").ok(),
                api_key: env::var("CHAT_API_KEY").ok(),
                base_url: env::var("CHAT_LLM_BASE_URL").ok(),
            },
            secrets: SecretsConfig {
                provider: env::var("SECRET_PROVIDER")
                    .map(|s| match s.to_lowercase().as_str() {
                        "vault" => SecretProviderType::Vault,
                        "aws" => SecretProviderType::Aws,
                        "none" => SecretProviderType::None,
                        _ => SecretProviderType::Local,
                    })
                    .unwrap_or_default(),
                secrets_file: env::var("SECRETS_FILE").ok(),
                secrets_encryption_key: env::var("SECRETS_ENCRYPTION_KEY").ok(),
                vault_address: env::var("VAULT_ADDR").ok(),
                vault_token: env::var("VAULT_TOKEN").ok(),
                vault_mount_path: env::var("VAULT_MOUNT_PATH").ok(),
                vault_namespace: env::var("VAULT_NAMESPACE").ok(),
                aws_region: env::var("AWS_REGION").ok(),
                aws_secret_prefix: env::var("AWS_SECRET_PREFIX").ok(),
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
                provider: crate::services::llm::LlmProviderType::Ollama,
                ollama_url: "https://ollama.com".to_string(),
                ollama_local_url: "http://localhost:11434".to_string(),
                ollama_model: "granite4:latest".to_string(),
                ollama_local_model: "gemma4:latest".to_string(),
                ollama_embed_model: None,
                ollama_api_key: None,
                ollama_keep_alive: None,
                anthropic_api_key: None,
                anthropic_model: None,
                gemini_api_key: None,
                gemini_model: None,
            },
            chat_llm: ChatLlmConfig::default(),
            secrets: SecretsConfig {
                provider: SecretProviderType::Local,
                secrets_file: Some(".secrets.enc".to_string()),
                secrets_encryption_key: Some("test-key".to_string()),
                vault_address: None,
                vault_token: None,
                vault_mount_path: None,
                vault_namespace: None,
                aws_region: None,
                aws_secret_prefix: None,
            },
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
        assert_eq!(config.llm.ollama_model, "granite4:latest");
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
