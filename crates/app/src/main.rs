use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};

use agent_brain::cli::{Cli, Command, TransportType};
use agent_brain::config::{Config, LogFormat};
use agent_brain::logging;
use agent_brain::mcp::{HttpTransport, HttpTransportConfig, McpServer, McpServerCore};
use agent_brain::repository::Neo4jClient;
use agent_brain::services::{
    LlmConfig,
    llm::LlmProviderType,
};

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    // Parse CLI arguments
    let cli = Cli::parse();

    // Build config from environment and override with CLI args
    let mut config = Config::from_env()?;

    config.neo4j_uri = cli.neo4j_uri.clone();
    config.neo4j_user = cli.neo4j_user.clone();
    if let Some(pw) = &cli.neo4j_password {
        config.neo4j_password = pw.clone();
    }
    config.log_level = cli.log_level.clone();
    config.log_format = match cli.log_format.to_lowercase().as_str() {
        "json" => LogFormat::Json,
        _ => LogFormat::Pretty,
    };

    // Initialize logging
    logging::init(&config);

    info!(
        neo4j_uri = %config.neo4j_uri,
        "Starting agent-brain"
    );

    // Execute command
    let result = match cli.command {
        Some(Command::InitDb) => run_init_db(&config).await,
        Some(Command::Serve { transport, bind, api_key }) => {
            run_serve(&config, transport, &bind, api_key).await
        }
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
        &config.neo4j_uri,
        &config.neo4j_user,
        &config.neo4j_password,
    )
    .await?;
    info!("Connected to Neo4j");
    Ok(client)
}

fn build_llm_config(config: &Config) -> LlmConfig {
    let mut base = LlmConfig::default().with_provider(config.llm_provider);

    match config.llm_provider {
        LlmProviderType::Ollama => {
            base = base
                .with_base_url(config.ollama_url.clone())
                .with_model(config.ollama_model.clone());
            if let Some(embed_model) = &config.ollama_embed_model {
                base = base.with_embed_model(embed_model);
            }
        }
        LlmProviderType::Anthropic => {
            if let Some(key) = &config.anthropic_api_key {
                base = base.with_api_key(key);
            }
            if let Some(model) = &config.anthropic_model {
                base = base.with_model(model);
            }
            // Use local embedding model even for cloud generation
            if let Some(embed_model) = &config.ollama_embed_model {
                base = base.with_embed_model(embed_model);
            }
        }
        LlmProviderType::Gemini => {
            if let Some(key) = &config.gemini_api_key {
                base = base.with_api_key(key);
            }
            if let Some(model) = &config.gemini_model {
                base = base.with_model(model);
            }
            // Use local embedding model even for cloud generation
            if let Some(embed_model) = &config.ollama_embed_model {
                base = base.with_embed_model(embed_model);
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

    // Initialize Telemetry
    let telemetry = if let Some(path) = &config.telemetry_db_path {
        match agent_brain::repository::TelemetryClient::new(path) {
            Ok(tc) => {
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

    match transport_type {
        TransportType::Stdio => {
            info!("Starting MCP server on stdio...");

            let server = McpServer::new()
                .with_neo4j(client)
                .with_llm_config(llm_config);

            server.run().await?;
        }
        TransportType::Http => {
            // Parse bind address
            let bind_addr: SocketAddr = bind.parse()
                .map_err(|e| anyhow::anyhow!("Invalid bind address '{}': {}", bind, e))?;

            info!(addr = %bind_addr, "Starting MCP server on HTTP...");

            // Create shared session manager
            let session_config = agent_brain::mcp::SessionConfig::default();
            let session_manager = Arc::new(agent_brain::mcp::SessionManager::with_config(session_config));

            // Create thread-safe server core
            let mut server = McpServerCore::new()
                .with_neo4j(client)
                .with_llm_config(llm_config)
                .with_session_manager(session_manager.clone());

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
    }

    Ok(())
}
