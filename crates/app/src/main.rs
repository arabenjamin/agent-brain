#[cfg(feature = "http-transport")]
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};

use agent_brain::cli::{Cli, Command, TransportType};
use agent_brain::config::{Config, LogFormat, LoggingConfig};
use agent_brain::logging;
use agent_brain::mcp::McpServer;
#[cfg(feature = "http-transport")]
use agent_brain::mcp::{HttpTransport, HttpTransportConfig, McpServerCore};
use agent_brain::repository::Neo4jClient;
use agent_brain::services::{LlmConfig, ModelCatalog, llm::LlmProviderType};
use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    // Parse CLI arguments
    let cli = Cli::parse();

    // Build config from environment and override with CLI args
    let mut config = Config::from_env()?;

    config.database.uri = cli.neo4j_uri.clone();
    config.database.user = cli.neo4j_user.clone();
    if let Some(pw) = &cli.neo4j_password {
        config.database.password = pw.clone();
    }
    config.logging = LoggingConfig {
        level: cli.log_level.clone(),
        format: match cli.log_format.to_lowercase().as_str() {
            "json" => LogFormat::Json,
            _ => LogFormat::Pretty,
        },
    };

    // Initialize logging
    logging::init(&config);

    info!(
        neo4j_uri = %config.database.uri,
        "Starting agent-brain"
    );

    // Execute command
    let result = match cli.command {
        Some(Command::InitDb) => run_init_db(&config).await,
        Some(Command::Serve {
            transport,
            bind,
            api_key,
        }) => run_serve(&config, transport, &bind, api_key).await,
        None => {
            // Default to stdio transport when no command specified
            run_serve(&config, TransportType::Stdio, "127.0.0.1:3000", None).await
        }
    };

    if let Err(e) = &result {
        error!(error = %e, "Command failed");
    }

    result
}

async fn connect_neo4j(config: &Config) -> Result<Neo4jClient> {
    info!("Connecting to Neo4j...");
    let client = Neo4jClient::new(
        &config.database.uri,
        &config.database.user,
        &config.database.password,
    )
    .await?;
    info!("Connected to Neo4j");
    Ok(client)
}

fn build_llm_config(config: &Config) -> LlmConfig {
    let llm = &config.llm;
    let mut base = LlmConfig::default().with_provider(llm.provider);

    match llm.provider {
        LlmProviderType::Ollama => {
            base = base
                .with_base_url(llm.ollama_url.clone())
                .with_model(llm.ollama_model.clone());
            if let Some(key) = &llm.ollama_api_key {
                base = base.with_api_key(key);
            }
            if let Some(embed_model) = &llm.ollama_embed_model {
                base = base.with_embed_model(embed_model);
            }
        }
        LlmProviderType::Anthropic => {
            if let Some(key) = &llm.anthropic_api_key {
                base = base.with_api_key(key);
            }
            if let Some(model) = &llm.anthropic_model {
                base = base.with_model(model);
            }
            // Use local embedding model even for cloud generation
            if let Some(embed_model) = &llm.ollama_embed_model {
                base = base.with_embed_model(embed_model);
            }
        }
        LlmProviderType::Gemini => {
            if let Some(key) = &llm.gemini_api_key {
                base = base.with_api_key(key);
            }
            if let Some(model) = &llm.gemini_model {
                base = base.with_model(model);
            }
            // Use local embedding model even for cloud generation
            if let Some(embed_model) = &llm.ollama_embed_model {
                base = base.with_embed_model(embed_model);
            }
        }
        LlmProviderType::VLlm => {
            if let Ok(url) = std::env::var("VLLM_URL") {
                base = base.with_base_url(url);
            }
            if let Ok(model) = std::env::var("VLLM_MODEL") {
                base = base.with_model(model);
            }
            if let Ok(key) = std::env::var("VLLM_API_KEY") {
                base = base.with_api_key(key);
            }
        }
    }
    base
}

async fn run_init_db(config: &Config) -> Result<()> {
    let client = connect_neo4j(config).await?;
    info!("Initializing database schema...");
    client.init_schema().await?;
    info!("Database schema initialized successfully");
    Ok(())
}

async fn run_serve(
    config: &Config,
    transport_type: TransportType,
    bind: &str,
    api_key: Option<String>,
) -> Result<()> {
    let client = connect_neo4j(config).await?;

    // Configure LLM
    let llm_config = build_llm_config(config);

    // Load model catalog and determine active system prompt
    let catalog_path = PathBuf::from(&config.telemetry.model_catalog_path);
    let catalog = ModelCatalog::load_or_default(&catalog_path);
    let active_model = match config.llm.provider {
        LlmProviderType::Anthropic => config
            .llm
            .anthropic_model
            .clone()
            .unwrap_or_else(|| config.llm.ollama_model.clone()),
        LlmProviderType::Gemini => config
            .llm
            .gemini_model
            .clone()
            .unwrap_or_else(|| config.llm.ollama_model.clone()),
        _ => config.llm.ollama_model.clone(),
    };
    // Initialize Telemetry (always attempted; stub returns Err when feature disabled)
    #[allow(unused_variables)]
    let telemetry = if let Some(path) = &config.telemetry.db_path {
        match agent_brain::repository::TelemetryClient::new(path) {
            Ok(tc) => {
                if let Err(e) = catalog.sync_to_duckdb(&tc) {
                    warn!("Could not sync model catalog to DuckDB: {}", e);
                }
                info!("Telemetry enabled at {}", path);
                Some(tc)
            }
            Err(e) => {
                warn!("Failed to initialize telemetry: {}", e);
                None
            }
        }
    } else {
        None
    };

    #[allow(unused_variables)]
    let system_prompt = catalog.resolve_system_prompt(&active_model);

    match transport_type {
        TransportType::Stdio => {
            info!("Starting MCP server on stdio...");

            let server = McpServer::new()
                .with_neo4j(client)
                .with_llm_config(llm_config);

            server.run().await?;
        }
        #[cfg(feature = "http-transport")]
        TransportType::Http => {
            // Parse bind address
            let bind_addr: SocketAddr = bind
                .parse()
                .map_err(|e| anyhow::anyhow!("Invalid bind address '{}': {}", bind, e))?;

            info!(addr = %bind_addr, "Starting MCP server on HTTP...");

            // Create shared session manager
            let session_config = agent_brain::mcp::SessionConfig::default();
            let session_manager = Arc::new(agent_brain::mcp::SessionManager::with_config(
                session_config,
            ));

            // Create thread-safe server core
            let mut server = McpServerCore::new()
                .with_neo4j(client)
                .with_llm_config(llm_config)
                .with_session_manager(session_manager.clone())
                .with_system_prompt(system_prompt)
                .with_catalog_path(catalog_path);

            if let Some(t) = telemetry {
                server = server.with_telemetry(t);
            }

            // Configure HTTP transport
            let mut http_config = HttpTransportConfig::default()
                .with_bind_addr(bind_addr)
                .with_session_manager(session_manager)
                .with_chat_service(server.chat_service());

            if let Some(key) = api_key {
                http_config = http_config.with_api_key(key);
                info!("API key authentication enabled");
            }

            let transport = HttpTransport::with_config(http_config);

            server.run_with_transport(&transport).await?;
        }
        #[cfg(not(feature = "http-transport"))]
        TransportType::Http => {
            anyhow::bail!(
                "HTTP transport is not compiled in. \
                 Rebuild with the 'http-transport' feature enabled."
            );
        }
    }

    Ok(())
}
