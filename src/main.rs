use anyhow::Result;
use clap::Parser;
use tracing::{error, info};

use agent_api::cli::{Cli, Command};
use agent_api::config::{Config, LogFormat};
use agent_api::logging;
use agent_api::models::HttpMethod;
use agent_api::repository::Neo4jClient;
use agent_api::services::{HttpExecutor, OpenApiParser, RequestBuilder, parse_headers};

#[tokio::main]
async fn main() -> Result<()> {
    // Load .env file if present
    dotenvy::dotenv().ok();

    // Parse CLI arguments
    let cli = Cli::parse();

    // Build config from CLI args (which can come from env vars via clap)
    let config = Config {
        neo4j_uri: cli.neo4j_uri.clone(),
        neo4j_user: cli.neo4j_user.clone(),
        neo4j_password: cli
            .neo4j_password
            .clone()
            .ok_or_else(|| anyhow::anyhow!("NEO4J_PASSWORD is required"))?,
        ollama_url: std::env::var("OLLAMA_URL")
            .unwrap_or_else(|_| "http://localhost:11434".to_string()),
        ollama_model: std::env::var("OLLAMA_MODEL").unwrap_or_else(|_| "llama3".to_string()),
        log_level: cli.log_level.clone(),
        log_format: match cli.log_format.to_lowercase().as_str() {
            "json" => LogFormat::Json,
            _ => LogFormat::Pretty,
        },
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
        Some(Command::Serve) | None => run_serve(&config).await,
        Some(Command::Ingest { spec }) => run_ingest(&config, &spec).await,
        Some(Command::Query { query }) => run_query(&config, &query).await,
        Some(Command::Execute {
            method,
            url,
            body,
            headers,
        }) => run_execute(&config, &method, &url, body, headers).await,
        Some(Command::Stats) => run_stats(&config).await,
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

async fn run_init_db(config: &Config) -> Result<()> {
    let client = connect_neo4j(config).await?;
    info!("Initializing database schema...");
    client.init_schema().await?;
    info!("Database schema initialized successfully");
    Ok(())
}

async fn run_serve(config: &Config) -> Result<()> {
    let _client = connect_neo4j(config).await?;

    info!("Starting MCP server on stdio...");

    // TODO: Initialize MCP server with stdio transport
    // For now, just wait indefinitely (placeholder for MCP server loop)
    info!("MCP server is not yet implemented. Press Ctrl+C to exit.");

    // Wait for shutdown signal
    tokio::signal::ctrl_c().await?;
    info!("Shutting down...");

    Ok(())
}

async fn run_ingest(config: &Config, spec: &str) -> Result<()> {
    let client = connect_neo4j(config).await?;

    info!(spec = %spec, "Ingesting OpenAPI specification...");

    // Initialize schema if needed
    client.init_schema().await?;

    // Parse and ingest the OpenAPI spec
    let mut parser = OpenApiParser::new(client);
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
