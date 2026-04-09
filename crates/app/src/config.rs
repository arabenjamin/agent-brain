use serde::Deserialize;
use std::env;

/// Top-level configuration loaded from environment variables.
#[derive(Debug, Clone)]
pub struct Config {
    pub database: DatabaseConfig,
    pub llm: LlmProviderConfig,
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
    pub ollama_embed_model: Option<String>,
    pub ollama_api_key: Option<String>,
    pub anthropic_api_key: Option<String>,
    pub anthropic_model: Option<String>,
    pub gemini_api_key: Option<String>,
    pub gemini_model: Option<String>,
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
                        "gemini"    => crate::services::llm::LlmProviderType::Gemini,
                        "ollama-cloud" | "ollamacloud" => crate::services::llm::LlmProviderType::OllamaCloud,
                        _ => crate::services::llm::LlmProviderType::Ollama,
                    })
                    .unwrap_or_default(),
                ollama_url: env::var("OLLAMA_URL")
                    .unwrap_or_else(|_| "https://ollama.com".to_string()),
                ollama_local_url: env::var("OLLAMA_LOCAL_URL")
                    .unwrap_or_else(|_| "http://localhost:11434".to_string()),
                ollama_model: env::var("OLLAMA_MODEL")
                    .unwrap_or_else(|_| "granite4:latest".to_string()),
                ollama_embed_model: env::var("OLLAMA_EMBED_MODEL").ok(),
                ollama_api_key: env::var("OLLAMA_API_KEY").ok(),
                anthropic_api_key: env::var("ANTHROPIC_API_KEY").ok(),
                anthropic_model: env::var("ANTHROPIC_MODEL").ok(),
                gemini_api_key: env::var("GEMINI_API_KEY").ok(),
                gemini_model: env::var("GEMINI_MODEL").ok(),
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
                ollama_embed_model: None,
                ollama_api_key: None,
                anthropic_api_key: None,
                anthropic_model: None,
                gemini_api_key: None,
                gemini_model: None,
            },
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
