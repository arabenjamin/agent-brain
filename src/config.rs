use serde::Deserialize;
use std::env;

/// Application configuration loaded from environment variables.
#[derive(Debug, Clone, Deserialize)]
pub struct Config {
    /// Neo4j connection URI (e.g., "bolt://localhost:7687")
    pub neo4j_uri: String,
    /// Neo4j username
    pub neo4j_user: String,
    /// Neo4j password
    pub neo4j_password: String,
    /// Ollama API endpoint (e.g., "http://localhost:11434")
    pub ollama_url: String,
    /// Ollama model to use for text generation (e.g., "granite3.3:8b")
    pub ollama_model: String,
    /// Ollama model to use for embeddings (e.g., "nomic-embed-text"). Falls back to ollama_model if not set.
    pub ollama_embed_model: Option<String>,
    /// Log level (trace, debug, info, warn, error)
    pub log_level: String,
    /// Log format (json, pretty)
    pub log_format: LogFormat,
    /// Secret provider type (local, vault, aws, none)
    pub secret_provider: SecretProviderType,
    /// Path to local secrets file (for local provider)
    pub secrets_file: Option<String>,
    /// Encryption key for local secrets (for local provider)
    pub secrets_encryption_key: Option<String>,
    /// Vault server address (for vault provider)
    pub vault_address: Option<String>,
    /// Vault token (for vault provider)
    pub vault_token: Option<String>,
    /// Vault KV mount path (for vault provider)
    pub vault_mount_path: Option<String>,
    /// Vault namespace (for vault provider, enterprise only)
    pub vault_namespace: Option<String>,
    /// AWS region (for aws provider)
    pub aws_region: Option<String>,
    /// AWS secret name prefix (for aws provider)
    pub aws_secret_prefix: Option<String>,
    /// Path to the DuckDB telemetry database file (e.g. "telemetry.db")
    pub telemetry_db_path: Option<String>,
    /// LLM provider to use (ollama, anthropic, gemini)
    pub llm_provider: crate::services::llm::LlmProviderType,
    /// Anthropic API key
    pub anthropic_api_key: Option<String>,
    /// Anthropic model name
    pub anthropic_model: Option<String>,
    /// Gemini API key
    pub gemini_api_key: Option<String>,
    /// Gemini model name
    pub gemini_model: Option<String>,
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
            neo4j_uri: env::var("NEO4J_URI")
                .unwrap_or_else(|_| "bolt://localhost:7687".to_string()),
            neo4j_user: env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".to_string()),
            neo4j_password: env::var("NEO4J_PASSWORD")
                .map_err(|_| ConfigError::Missing("NEO4J_PASSWORD"))?,
            ollama_url: env::var("OLLAMA_URL")
                .unwrap_or_else(|_| "http://localhost:11434".to_string()),
            ollama_model: env::var("OLLAMA_MODEL").unwrap_or_else(|_| "granite3.3:8b".to_string()),
            ollama_embed_model: env::var("OLLAMA_EMBED_MODEL").ok(),
            log_level: env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string()),
            log_format: env::var("LOG_FORMAT")
                .map(|s| match s.to_lowercase().as_str() {
                    "json" => LogFormat::Json,
                    _ => LogFormat::Pretty,
                })
                .unwrap_or_default(),
            secret_provider: env::var("SECRET_PROVIDER")
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
            telemetry_db_path: env::var("TELEMETRY_DB_PATH").ok(),
            llm_provider: env::var("LLM_PROVIDER")
                .map(|s| match s.to_lowercase().as_str() {
                    "anthropic" => crate::services::llm::LlmProviderType::Anthropic,
                    "gemini" => crate::services::llm::LlmProviderType::Gemini,
                    _ => crate::services::llm::LlmProviderType::Ollama,
                })
                .unwrap_or_default(),
            anthropic_api_key: env::var("ANTHROPIC_API_KEY").ok(),
            anthropic_model: env::var("ANTHROPIC_MODEL").ok(),
            gemini_api_key: env::var("GEMINI_API_KEY").ok(),
            gemini_model: env::var("GEMINI_MODEL").ok(),
        })
    }

    /// Create a config with default/test values (no env vars required).
    #[cfg(test)]
    pub fn test_config() -> Self {
        Self {
            neo4j_uri: "bolt://localhost:7687".to_string(),
            neo4j_user: "neo4j".to_string(),
            neo4j_password: "testpassword".to_string(),
            ollama_url: "http://localhost:11434".to_string(),
            ollama_model: "granite3.3:8b".to_string(),
            ollama_embed_model: None,
            log_level: "debug".to_string(),
            log_format: LogFormat::Pretty,
            secret_provider: SecretProviderType::Local,
            secrets_file: Some(".secrets.enc".to_string()),
            secrets_encryption_key: Some("test-key".to_string()),
            vault_address: None,
            vault_token: None,
            vault_mount_path: None,
            vault_namespace: None,
            aws_region: None,
            aws_secret_prefix: None,
            telemetry_db_path: Some("test_telemetry.db".to_string()),
            llm_provider: crate::services::llm::LlmProviderType::Ollama,
            anthropic_api_key: None,
            anthropic_model: None,
            gemini_api_key: None,
            gemini_model: None,
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
        assert_eq!(config.neo4j_uri, "bolt://localhost:7687");
        assert_eq!(config.ollama_model, "granite3.3:8b");
        assert_eq!(config.log_format, LogFormat::Pretty);
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
