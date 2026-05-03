#[cfg(feature = "http-transport")]
use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing::{error, info, warn};

use agent_brain::cli::{Cli, Command, TodoAction, TransportType};
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

    // Create log ring buffer and initialize logging.
    // The buffer is passed through to the HTTP transport for GET /api/logs.
    // For stdio transport it's unused but we create it anyway to avoid cfg complexity.
    let log_buffer = agent_brain::logging::LogBuffer::new(500);
    logging::init_with_buffer(&config, Some(log_buffer.clone()));

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
        }) => run_serve(&config, transport, &bind, api_key, log_buffer).await,
        Some(Command::Todo { action, url }) => run_todo(&url, action).await,
        None => {
            // Default to stdio transport when no command specified
            run_serve(
                &config,
                TransportType::Stdio,
                "127.0.0.1:3000",
                None,
                log_buffer,
            )
            .await
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
                .with_base_url(llm.ollama_local_url.clone())
                .with_model(llm.ollama_model.clone());
            if let Some(key) = &llm.ollama_api_key {
                base = base.with_api_key(key);
            }
            if let Some(embed_model) = &llm.ollama_embed_model {
                base = base.with_embed_model(embed_model);
            }
        }
        LlmProviderType::OllamaCloud => {
            base = base
                .with_base_url(llm.ollama_url.clone())
                .with_model(llm.ollama_model.clone());
            if let Some(key) = &llm.ollama_api_key {
                base = base.with_api_key(key);
            }
            // Embeddings always route to local Ollama
            base = base.with_embed_base_url(llm.ollama_local_url.clone());
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
    }
    base
}

/// Build the LLM config for the human-facing chat adapter.
///
/// Starts from the brain's LLM config and applies any `CHAT_LLM_*` overrides
/// from [`Config::chat_llm`].  When all overrides are `None` the result is
/// identical to the brain's config (single-model deployments are unchanged).
fn build_chat_llm_config(config: &Config) -> LlmConfig {
    let chat = &config.chat_llm;

    // No overrides at all → return the exact same config as the brain.
    if chat.provider.is_none()
        && chat.model.is_none()
        && chat.api_key.is_none()
        && chat.base_url.is_none()
    {
        return build_llm_config(config);
    }

    // Apply overrides on top of the brain's base config.
    let mut base = build_llm_config(config);
    if let Some(provider) = chat.provider {
        base = base.with_provider(provider);
    }
    if let Some(ref model) = chat.model {
        base = base.with_model(model);
    }
    if let Some(ref key) = chat.api_key {
        base = base.with_api_key(key);
    }
    if let Some(ref url) = chat.base_url {
        base = base.with_base_url(url.clone());
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

async fn run_todo(url: &str, action: TodoAction) -> Result<()> {
    let client = reqwest::Client::new();
    let base = url.trim_end_matches('/');

    match action {
        TodoAction::Add {
            title,
            description,
            priority,
            due,
            tags,
        } => {
            let tags_vec: Vec<String> = tags
                .unwrap_or_default()
                .split(',')
                .filter(|s| !s.is_empty())
                .map(|s| s.trim().to_string())
                .collect();

            let body = serde_json::json!({
                "title": title,
                "description": description,
                "priority": priority,
                "due_at": due,
                "tags": tags_vec,
            });

            let resp = client
                .post(format!("{base}/todos"))
                .json(&body)
                .send()
                .await?;

            let status = resp.status();
            let json: serde_json::Value = resp.json().await?;
            if status.is_success() {
                println!("{}", serde_json::to_string_pretty(&json)?);
            } else {
                anyhow::bail!("Server error {}: {}", status, json);
            }
        }

        TodoAction::List { status } => {
            let mut req = client.get(format!("{base}/todos"));
            if let Some(s) = status {
                req = req.query(&[("status", s)]);
            }
            let resp = req.send().await?;
            let status_code = resp.status();
            let json: serde_json::Value = resp.json().await?;
            if status_code.is_success() {
                if let Some(todos) = json.get("todos").and_then(|t| t.as_array()) {
                    if todos.is_empty() {
                        println!("No todos found.");
                    } else {
                        for todo in todos {
                            let id = todo["id"].as_str().unwrap_or("?");
                            let title = todo["title"].as_str().unwrap_or("?");
                            let status = todo["status"].as_str().unwrap_or("?");
                            let priority = todo["priority"].as_i64().unwrap_or(2);
                            let pri_label = match priority {
                                0 => "urgent",
                                1 => "high",
                                2 => "normal",
                                _ => "low",
                            };
                            println!("[{status}] [{pri_label}] {title}  (id: {id})");
                        }
                    }
                }
            } else {
                anyhow::bail!("Server error {}: {}", status_code, json);
            }
        }

        TodoAction::Done { id } => {
            let body = serde_json::json!({"status": "done"});
            let resp = client
                .put(format!("{base}/todos/{id}"))
                .json(&body)
                .send()
                .await?;
            let status_code = resp.status();
            let json: serde_json::Value = resp.json().await?;
            if status_code.is_success() {
                println!("Done: {}", json["title"].as_str().unwrap_or(&id));
            } else {
                anyhow::bail!("Server error {}: {}", status_code, json);
            }
        }

        TodoAction::Status { id, status } => {
            let body = serde_json::json!({"status": status});
            let resp = client
                .put(format!("{base}/todos/{id}"))
                .json(&body)
                .send()
                .await?;
            let status_code = resp.status();
            let json: serde_json::Value = resp.json().await?;
            if status_code.is_success() {
                println!(
                    "Updated: {} -> {}",
                    json["title"].as_str().unwrap_or(&id),
                    json["status"].as_str().unwrap_or("?")
                );
            } else {
                anyhow::bail!("Server error {}: {}", status_code, json);
            }
        }

        TodoAction::Delete { id } => {
            let resp = client.delete(format!("{base}/todos/{id}")).send().await?;
            let status_code = resp.status();
            if status_code == 204 {
                println!("Deleted: {id}");
            } else {
                let json: serde_json::Value = resp.json().await.unwrap_or_default();
                anyhow::bail!("Server error {}: {}", status_code, json);
            }
        }
    }

    Ok(())
}

async fn run_serve(
    config: &Config,
    transport_type: TransportType,
    bind: &str,
    api_key: Option<String>,
    log_buffer: Arc<agent_brain::logging::LogBuffer>,
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
        // Ensure the parent directory exists (e.g. when using a named Docker volume).
        let dir_ok = if let Some(parent) = std::path::Path::new(path).parent()
            && !parent.as_os_str().is_empty()
        {
            match std::fs::create_dir_all(parent) {
                Ok(()) => true,
                Err(e) => {
                    error!(
                        path = %path,
                        directory = %parent.display(),
                        error = %e,
                        "Failed to create telemetry directory — DuckDB will not open. \
                         Disabled: SleepSkill (digest_experiences/export_training_data), \
                         QuerySkill (duckdb_query), model usage tracking."
                    );
                    false
                }
            }
        } else {
            true
        };

        if !dir_ok {
            None
        } else {
            match agent_brain::repository::TelemetryClient::new(path) {
                Ok(tc) => {
                    if let Err(e) = catalog.sync_to_duckdb(&tc) {
                        warn!("Could not sync model catalog to DuckDB: {}", e);
                    }
                    info!("Telemetry enabled at {}", path);
                    Some(tc)
                }
                Err(e) => {
                    error!(
                        path = %path,
                        error = %e,
                        "Failed to initialize DuckDB telemetry. \
                         Disabled: SleepSkill (digest_experiences/export_training_data), \
                         QuerySkill (duckdb_query), model usage tracking."
                    );
                    None
                }
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
            let neo4j_for_http = client.clone();
            let mut server = McpServerCore::new()
                .with_neo4j(client)
                .with_llm_config(llm_config)
                .with_session_manager(session_manager.clone())
                .with_system_prompt(system_prompt)
                .with_catalog_path(catalog_path);

            // Only create a separate chat Arc when CHAT_LLM_* overrides are
            // set. Otherwise leave chat_llm_config = None so chat_service()
            // falls back to brain.llm_config — meaning use_model() applies to
            // both the brain and the chat session immediately.
            if config.chat_llm.has_overrides() {
                server = server.with_chat_llm_config(build_chat_llm_config(config));
            }

            if let Some(t) = telemetry {
                server = server.with_telemetry(t);
            }

            // Configure HTTP transport
            let mut http_config = HttpTransportConfig::default()
                .with_bind_addr(bind_addr)
                .with_session_manager(session_manager)
                .with_chat_service(server.chat_service());

            // Wire Neo4j into the HTTP transport for /todos and /scheduled-tasks endpoints.
            http_config = http_config.with_neo4j_client(Arc::new(neo4j_for_http));

            // Wire scheduler handle for /scheduler-config endpoints.
            http_config = http_config.with_scheduler(server.scheduler_handle());

            // Wire context-builder, LLM config, and telemetry for the new /api/* endpoints.
            http_config = http_config.with_context_builder(server.context_builder_handle());
            http_config = http_config.with_llm_config_arc(server.llm_config_arc());
            if let Some(t) = server.telemetry() {
                http_config = http_config.with_telemetry(t);
            }

            // Wire brain event bus into the HTTP transport for SSE job notifications.
            http_config = http_config.with_brain_event_sender(server.brain.event_sender());

            // Wire the log ring buffer for GET /api/logs.
            http_config = http_config.with_log_buffer(log_buffer);

            // Wire the tool registry for GET /api/skills.
            http_config = http_config.with_tool_registry(server.tool_registry_handle());

            if let Some(key) = api_key.filter(|k| !k.is_empty()) {
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
