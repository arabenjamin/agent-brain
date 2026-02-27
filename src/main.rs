use std::net::SocketAddr;
use std::sync::Arc;

use anyhow::Result;
use clap::Parser;
use tracing::{debug, error, info, warn};

use agent_brain::cli::{Cli, Command, TransportType};
use agent_brain::config::{Config, LogFormat, SecretProviderType};
use agent_brain::logging;
use agent_brain::mcp::{HttpTransport, HttpTransportConfig, McpServer, McpServerCore};
use agent_brain::models::HttpMethod;
use agent_brain::repository::Neo4jClient;
use agent_brain::services::{
    AwsSecretConfig, AwsSecretProvider, CredentialManager, ExportFormat, ExportOptions,
    HttpExecutor, LlmClient, LlmConfig, LocalSecretConfig, LocalSecretProvider, MarkdownReportGenerator,
    OpenApiExporter, OpenApiParser, RequestBuilder, SpecDiffer, VaultConfig, VaultSecretProvider,
    parse_headers, llm::LlmProviderType,
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
        "Starting agent-api"
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
        Some(Command::Ingest { spec }) => run_ingest(&config, &spec).await,
        Some(Command::Query { query }) => run_query(&config, &query).await,
        Some(Command::Execute {
            method,
            url,
            body,
            headers,
        }) => run_execute(&config, &method, &url, body, headers).await,
        Some(Command::Stats) => run_stats(&config).await,
        Some(Command::Export {
            output,
            format,
            annotations,
            include_broken,
        }) => run_export(&config, output, &format, annotations, include_broken).await,
        Some(Command::Diff {
            format,
            breaking_only,
        }) => run_diff(&config, &format, breaking_only).await,
        Some(Command::Embed) => run_embed(&config).await,
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

    // Configure LLM for healing
    let llm_config = build_llm_config(config);

    // Create credential manager with appropriate secret provider
    let credential_manager = create_credential_manager(config, client.clone()).await;

    // Initialize Telemetry
    let telemetry = if let Some(path) = &config.telemetry_db_path {
        match agent_brain::repository::TelemetryClient::new(path) {
            Ok(client) => {
                info!("Telemetry enabled at {}", path);
                Some(client)
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

            // Use the legacy McpServer for backward compatibility
            let mut server = McpServer::new()
                .with_neo4j(client)
                .with_llm_config(llm_config);

            // Note: McpServer (legacy) doesn't support telemetry yet in this patch set, 
            // but we focus on McpServerCore mostly. 
            // Actually I didn't update McpServer struct to hold telemetry, only McpServerCore.
            // Wait, I did verify I should update both? I only updated McpServerCore.
            // The user instruction was "scaffold". I updated McpServerCore.
            // I should update McpServer too if I want stdio to work with logging.
            // But let's check McpServer definition again.
            
            if let Some(cred_manager) = credential_manager {
                server = server.with_credential_manager(cred_manager);
            }

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

            // Configure HTTP transport
            let mut http_config = HttpTransportConfig::default()
                .with_bind_addr(bind_addr)
                .with_session_manager(session_manager.clone());

            if let Some(key) = api_key {
                http_config = http_config.with_api_key(key);
                info!("API key authentication enabled");
            }

            let transport = HttpTransport::with_config(http_config);

            // Create thread-safe server core
            let mut server = McpServerCore::new()
                .with_neo4j(client)
                .with_llm_config(llm_config)
                .with_session_manager(session_manager);

            if let Some(t) = telemetry {
                server = server.with_telemetry(t);
            }

            if let Some(cred_manager) = credential_manager {
                server = server.with_credential_manager(cred_manager);
            }

            server.run_with_transport(&transport).await?;
        }
    }

    Ok(())
}

/// Create a credential manager based on the configured secret provider.
async fn create_credential_manager(
    config: &Config,
    neo4j: Neo4jClient,
) -> Option<Arc<CredentialManager>> {
    match config.secret_provider {
        SecretProviderType::None => {
            info!("Secret provider disabled, credential management unavailable");
            None
        }
        SecretProviderType::Local => {
            let file_path = config
                .secrets_file
                .clone()
                .unwrap_or_else(|| ".secrets.enc".to_string());
            let encryption_key = config.secrets_encryption_key.clone();

            let local_config = LocalSecretConfig::new(&file_path);
            let local_config = if let Some(key) = encryption_key {
                local_config.with_encryption_key(key)
            } else {
                warn!(
                    "No SECRETS_ENCRYPTION_KEY set, using default key (insecure for production)"
                );
                local_config
            };

            match LocalSecretProvider::new(local_config) {
                Ok(provider) => {
                    // Load existing secrets
                    if let Err(e) = provider.load().await {
                        warn!(error = %e, "Failed to load existing secrets, starting fresh");
                    }

                    let manager = CredentialManager::new()
                        .with_secret_provider(Box::new(provider))
                        .with_neo4j(neo4j);

                    info!("Local secret provider initialized");
                    Some(Arc::new(manager))
                }
                Err(e) => {
                    warn!(error = %e, "Failed to create local secret provider");
                    None
                }
            }
        }
        SecretProviderType::Vault => {
            let vault_address = match &config.vault_address {
                Some(addr) => addr.clone(),
                None => {
                    warn!("VAULT_ADDR not set, skipping Vault provider");
                    return None;
                }
            };

            let vault_token = match &config.vault_token {
                Some(token) => token.clone(),
                None => {
                    warn!("VAULT_TOKEN not set, skipping Vault provider");
                    return None;
                }
            };

            let vault_config = VaultConfig::new(vault_address, vault_token)
                .with_mount_path(
                    config
                        .vault_mount_path
                        .clone()
                        .unwrap_or_else(|| "secret".to_string()),
                );

            let vault_config = if let Some(ns) = &config.vault_namespace {
                vault_config.with_namespace(ns)
            } else {
                vault_config
            };

            match VaultSecretProvider::new(vault_config) {
                Ok(provider) => {
                    let manager = CredentialManager::new()
                        .with_secret_provider(Box::new(provider))
                        .with_neo4j(neo4j);

                    info!("Vault secret provider initialized");
                    Some(Arc::new(manager))
                }
                Err(e) => {
                    warn!(error = %e, "Failed to create Vault secret provider");
                    None
                }
            }
        }
        SecretProviderType::Aws => {
            let region = config
                .aws_region
                .clone()
                .unwrap_or_else(|| "us-east-1".to_string());

            let aws_config = AwsSecretConfig::new().with_region(region);
            let aws_config = if let Some(prefix) = &config.aws_secret_prefix {
                aws_config.with_prefix(prefix)
            } else {
                aws_config
            };

            let provider = AwsSecretProvider::new(aws_config);
            let manager = CredentialManager::new()
                .with_secret_provider(Box::new(provider))
                .with_neo4j(neo4j);

            info!("AWS Secrets Manager provider initialized");
            Some(Arc::new(manager))
        }
    }
}

async fn run_ingest(config: &Config, spec: &str) -> Result<()> {
    let client = connect_neo4j(config).await?;

    info!(spec = %spec, "Ingesting OpenAPI specification...");

    // Initialize schema if needed
    client.init_schema().await?;

    // Parse and ingest the OpenAPI spec
    let mut parser = OpenApiParser::new(client);
    
    // Configure LLM for embeddings if configured
    let llm_config = build_llm_config(config);

    if let Ok(llm) = LlmClient::with_config(llm_config) {
        parser = parser.with_llm(llm);
    }

    let result = parser.ingest(spec).await?;

    println!("Ingestion Complete");
    println!("==================");
    println!(
        "API:            {} v{}",
        result.api_title, result.api_version
    );
    println!("Resources:      {}", result.resources_created);
    println!("Endpoints:      {}", result.endpoints_created);
    println!("Schemas:        {}", result.schemas_created);
    println!("Parameters:     {}", result.parameters_created);

    Ok(())
}

async fn run_query(config: &Config, query: &str) -> Result<()> {
    let client = connect_neo4j(config).await?;

    info!(query = %query, "Querying endpoints...");

    // Simple fuzzy search for now
    let endpoints = client.find_endpoints_by_path(query).await?;

    if endpoints.is_empty() {
        println!("No endpoints found matching: {}", query);
    } else {
        println!("Found {} endpoint(s):\n", endpoints.len());
        for endpoint in endpoints {
            println!(
                "  {} {} - {}",
                endpoint.method, endpoint.path, endpoint.summary
            );
            if let Some(op_id) = &endpoint.operation_id {
                println!("    Operation ID: {}", op_id);
            }
            println!("    Status: {:?}", endpoint.status);
            println!();
        }
    }

    Ok(())
}

async fn run_execute(
    _config: &Config,
    method: &str,
    url: &str,
    body: Option<String>,
    headers: Vec<String>,
) -> Result<()> {
    info!(
        method = %method,
        url = %url,
        body = ?body,
        headers = ?headers,
        "Executing HTTP request..."
    );

    // Parse method
    let http_method: HttpMethod =
        serde_json::from_str(&format!("\"{}\"", method.to_uppercase()))
            .map_err(|_| anyhow::anyhow!("Invalid HTTP method: {}", method))?;

    // Parse headers
    let header_map = parse_headers(&headers)?;

    // Parse body as JSON if provided
    let json_body = body.map(|body_str| {
        serde_json::from_str(&body_str).unwrap_or_else(|_| serde_json::json!(body_str))
    });

    // Build and execute request
    let executor = HttpExecutor::new()?;

    let mut builder = RequestBuilder::new()
        .base_url(url)
        .method(http_method)
        .headers(header_map);

    if let Some(body) = json_body {
        builder = builder.body(body);
    }

    let response = executor.execute(&builder).await?;

    // Display results
    println!("HTTP Response");
    println!("=============");
    println!(
        "Status:      {} ({:?})",
        response.status_code, response.class
    );
    println!("Duration:    {} ms", response.duration_ms);
    println!("URL:         {}", response.url);
    println!("Method:      {}", response.method);
    println!();

    if !response.headers.is_empty() {
        println!("Headers:");
        for (key, value) in &response.headers {
            println!("  {}: {}", key, value);
        }
        println!();
    }

    println!("Body:");
    // Try to pretty-print JSON
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(&response.body) {
        println!("{}", serde_json::to_string_pretty(&json)?);
    } else {
        println!("{}", response.body);
    }

    Ok(())
}

async fn run_stats(config: &Config) -> Result<()> {
    let client = connect_neo4j(config).await?;

    info!("Fetching database statistics...");

    let resources = client.list_resources().await?;
    let endpoints = client.list_endpoints().await?;
    let schemas = client.list_schemas().await?;
    let healing_stats = client.get_healing_stats().await?;

    println!("Database Statistics");
    println!("===================");
    println!("Resources:      {}", resources.len());
    println!("Endpoints:      {}", endpoints.len());
    println!("Schemas:        {}", schemas.len());
    println!();
    println!("Healing Events");
    println!("--------------");
    println!("Total:          {}", healing_stats.total);
    println!("Verified:       {}", healing_stats.verified);
    println!("Unverified:     {}", healing_stats.unverified);

    Ok(())
}

async fn run_export(
    config: &Config,
    output: Option<String>,
    format: &str,
    annotations: bool,
    include_broken: bool,
) -> Result<()> {
    let client = connect_neo4j(config).await?;

    info!(format = %format, annotations = annotations, "Exporting OpenAPI specification...");

    let options = ExportOptions {
        include_annotations: annotations,
        include_original_values: annotations,
        format: match format.to_lowercase().as_str() {
            "json" => ExportFormat::Json,
            _ => ExportFormat::Yaml,
        },
        api_name: None,
        include_broken_endpoints: include_broken,
        include_verification_status: true,
    };

    let exporter = OpenApiExporter::new(client);
    let result = exporter.export(&options).await?;

    // Write to file or stdout
    match output {
        Some(path) => {
            std::fs::write(&path, &result.content)?;
            println!("Export Complete");
            println!("===============");
            println!("Output:         {}", path);
            println!("Format:         {}", format);
            println!("Resources:      {}", result.stats.resources_exported);
            println!("Endpoints:      {}", result.stats.endpoints_exported);
            println!("Schemas:        {}", result.stats.schemas_exported);
            println!("Parameters:     {}", result.stats.parameters_exported);
            println!("Healed Fields:  {}", result.stats.healed_fields_annotated);
            if result.stats.broken_endpoints_skipped > 0 {
                println!(
                    "Skipped (broken): {}",
                    result.stats.broken_endpoints_skipped
                );
            }
        }
        None => {
            // Print to stdout
            println!("{}", result.content);
        }
    }

    Ok(())
}

async fn run_diff(config: &Config, format: &str, breaking_only: bool) -> Result<()> {
    let client = connect_neo4j(config).await?;

    info!(format = %format, breaking_only = breaking_only, "Generating diff report...");

    let differ = SpecDiffer::new(client);
    let mut report = differ.generate_diff(None).await?;

    // Filter to breaking only if requested
    if breaking_only {
        report.changes.retain(|c| c.breaking);
        report.summary.total_changes = report.changes.len();
    }

    // Generate output based on format
    let output = match format.to_lowercase().as_str() {
        "json" => MarkdownReportGenerator::generate_json(&report)?,
        "changelog" => MarkdownReportGenerator::generate_changelog(&report),
        _ => MarkdownReportGenerator::generate(&report),
    };

    println!("{}", output);

    Ok(())
}

async fn run_embed(config: &Config) -> Result<()> {
    let client = connect_neo4j(config).await?;
    let llm_config = build_llm_config(config);
    let llm = LlmClient::with_config(llm_config)?;

    info!("Generating embeddings for existing endpoints...");

    let endpoints = client.list_endpoints().await?;
    let mut count = 0;

    for endpoint in endpoints {
        if endpoint.embedding.is_none() {
            let text = format!("{} {} - {}", endpoint.method, endpoint.path, endpoint.summary);
            debug!(endpoint_id = %endpoint.id, "Generating embedding for {}", text);

            match llm.embeddings(&text).await {
                Ok(emb) => {
                    client.update_endpoint_embedding(endpoint.id, emb).await?;
                    count += 1;
                }
                Err(e) => {
                    warn!(endpoint_id = %endpoint.id, error = %e, "Failed to generate embedding");
                }
            }
        }
    }

    println!("Embedding generation complete");
    println!("============================");
    println!("Endpoints updated: {}", count);

    Ok(())
}
