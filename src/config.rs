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
    /// Ollama model to use (e.g., "llama3", "mistral")
    pub ollama_model: String,
    /// Log level (trace, debug, info, warn, error)
    pub log_level: String,
    /// Log format (json, pretty)
    pub log_format: LogFormat,
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
            neo4j_uri: env::var("NEO4J_URI")
                .unwrap_or_else(|_| "bolt://localhost:7687".to_string()),
            neo4j_user: env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".to_string()),
            neo4j_password: env::var("NEO4J_PASSWORD")
                .map_err(|_| ConfigError::Missing("NEO4J_PASSWORD"))?,
            ollama_url: env::var("OLLAMA_URL")
                .unwrap_or_else(|_| "http://localhost:11434".to_string()),
            ollama_model: env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3".to_string()),
            log_level: env::var("LOG_LEVEL").unwrap_or_else(|_| "info".to_string()),
            log_format: env::var("LOG_FORMAT")
                .map(|s| match s.to_lowercase().as_str() {
                    "json" => LogFormat::Json,
                    _ => LogFormat::Pretty,
                })
                .unwrap_or_default(),
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
            ollama_model: "llama3".to_string(),
            log_level: "debug".to_string(),
            log_format: LogFormat::Pretty,
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
        assert_eq!(config.ollama_model, "llama3");
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
