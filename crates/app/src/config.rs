pub use agent_brain_graph::logging::{LogFormat, LoggingConfig};
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

/// The brain's active LLM: which provider and model. Endpoints and keys are not
/// here: the provider registry (`models.yaml` `providers:`) names
/// the env vars that hold them, and `model_router::config_for_catalog_entry`
/// reads them, for this config as for every routed one.
#[derive(Debug, Clone)]
pub struct LlmProviderConfig {
    /// A provider-registry name (`LLM_PROVIDER`, folded by `canonical_name`;
    /// default `local`). A name the registry does not know runs on the local engine.
    pub provider: String,
    /// The active model: `ANTHROPIC_MODEL` or `GEMINI_MODEL` for those providers
    /// when set, else `LLM_MODEL`.
    pub model: String,
}

/// The model a provider runs when `LLM_PROVIDER` / `CHAT_LLM_PROVIDER` names it
/// and no model is given: its own `*_MODEL` env var, else `LLM_MODEL`.
pub fn default_model_for(provider: &str) -> String {
    let own = match provider {
        "anthropic" => env::var("ANTHROPIC_MODEL").ok(),
        "gemini" => env::var("GEMINI_MODEL").ok(),
        _ => None,
    };
    own.or_else(|| env::var("LLM_MODEL").ok())
        .or_else(|| agent_brain_llm::engine::model(agent_brain_llm::engine::Role::Generation))
        .unwrap_or_else(|| "local".to_string())
}

/// Optional LLM overrides for the human-facing chat adapter.
///
/// Any field left `None` falls through to the corresponding value in
/// [`LlmProviderConfig`], so an empty `ChatLlmConfig` is a no-op — the
/// chat service uses exactly the same model as the brain's internal LLM.
///
/// Set these when you want a different model for chat than for internal
/// cognitive operations (e.g. cloud Anthropic for chat, the local engine for
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

/// Optional DuckDB telemetry sink and model catalog.
#[derive(Debug, Clone)]
pub struct TelemetryConfig {
    /// Path to the DuckDB file. `None` disables telemetry.
    pub db_path: Option<String>,
    /// Path to the YAML model catalog (default: `models.yaml`).
    pub model_catalog_path: String,
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
                    &env::var("LLM_PROVIDER").unwrap_or_else(|_| "local".to_string()),
                );
                LlmProviderConfig {
                    model: default_model_for(&provider),
                    provider,
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
                provider: "local".to_string(),
                model: "local".to_string(),
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
    fn test_config_defaults() {
        let config = Config::test_config();
        assert_eq!(config.database.uri, "bolt://localhost:7687");
        assert_eq!(config.llm.provider, "local");
        assert_eq!(config.llm.model, "local");
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

    /// The deployed stack has no open defaults (OUTSIDE_REVIEW_PLAN Phase 0):
    /// every published port names a bind address, and Neo4j's password and
    /// the API key come from `.env`, required. Before 2026-10-29 the brain,
    /// the HBI and Neo4j listened on every interface, Neo4j on `password`,
    /// and `MCP_API_KEY` was blanked.
    #[test]
    fn compose_publishes_nothing_open() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docker-compose.yml");
        let text = std::fs::read_to_string(path).expect("read docker-compose.yml");
        let doc: serde_yaml::Value = serde_yaml::from_str(&text).expect("parse docker-compose.yml");
        let services = doc["services"].as_mapping().expect("services");
        for (name, svc) in services {
            let name = name.as_str().unwrap_or_default();
            for port in svc["ports"].as_sequence().into_iter().flatten() {
                let p = port.as_str().unwrap_or_default();
                assert!(
                    p.split(':').count() >= 3,
                    "{name}: port {p:?} publishes on every interface; name a bind address"
                );
            }
            for var in svc["environment"].as_sequence().into_iter().flatten() {
                let Some((key, value)) = var.as_str().and_then(|v| v.split_once('=')) else {
                    continue;
                };
                if matches!(key, "NEO4J_AUTH" | "NEO4J_PASSWORD" | "MCP_API_KEY") {
                    assert!(
                        value.contains("${") && value.contains(":?"),
                        "{name}: {key} must be a required value from .env, got {value:?}"
                    );
                }
            }
        }
    }
}
