//! MCP tool definitions and handlers.

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, info};

use crate::models::ParameterLocation;
use crate::repository::Neo4jClient;
use crate::services::{
    ApiContext, ContextStore, DiscoveryConfig, DiscoveryService, DocGenService, EndpointSummary,
    EndpointWithParams, HealingConfig, HealingOrchestrator, HttpExecutor, LlmClient, LlmConfig,
    OpenApiParser, ParameterSummary,
};

use super::protocol::{ToolCallResult, ToolDefinition};

/// Tool registry containing all available tools.
pub struct ToolRegistry {
    tools: Vec<ToolDefinition>,
}

impl ToolRegistry {
    /// Create a new tool registry with all available tools.
    pub fn new() -> Self {
        Self {
            tools: vec![
                Self::ingest_openapi_def(),
                Self::query_endpoint_def(),
                Self::execute_request_def(),
                Self::get_api_context_def(),
                Self::list_loaded_apis_def(),
                Self::clear_api_context_def(),
                Self::discover_openapi_def(),
                Self::build_openapi_from_docs_def(),
            ],
        }
    }

    /// Get all tool definitions.
    pub fn list(&self) -> &[ToolDefinition] {
        &self.tools
    }

    /// Get a tool definition by name.
    pub fn get(&self, name: &str) -> Option<&ToolDefinition> {
        self.tools.iter().find(|t| t.name == name)
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn ingest_openapi_def() -> ToolDefinition {
        ToolDefinition {
            name: "ingest_openapi".to_string(),
            description: "Parse an OpenAPI specification and load it into the knowledge graph. \
                         Accepts either a URL or a local file path."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "source": {
                        "type": "string",
                        "description": "URL or file path to the OpenAPI specification (JSON or YAML)"
                    }
                },
                "required": ["source"]
            }),
        }
    }

    fn query_endpoint_def() -> ToolDefinition {
        ToolDefinition {
            name: "graph_query_endpoint".to_string(),
            description: "Search the knowledge graph for API endpoints matching a query. \
                         Returns endpoint details including path, method, parameters, and schemas."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Natural language query or path pattern to search for (e.g., 'create user', '/users', 'POST')"
                    }
                },
                "required": ["query"]
            }),
        }
    }

    fn execute_request_def() -> ToolDefinition {
        ToolDefinition {
            name: "execute_http_request".to_string(),
            description: "Execute an HTTP request against an API endpoint. \
                         Supports automatic error analysis and self-healing when documentation is incorrect."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "method": {
                        "type": "string",
                        "enum": ["GET", "POST", "PUT", "PATCH", "DELETE", "HEAD", "OPTIONS"],
                        "description": "HTTP method"
                    },
                    "url": {
                        "type": "string",
                        "description": "Full URL to request"
                    },
                    "headers": {
                        "type": "object",
                        "description": "HTTP headers as key-value pairs",
                        "additionalProperties": { "type": "string" }
                    },
                    "body": {
                        "type": "object",
                        "description": "Request body (for POST, PUT, PATCH)"
                    }
                },
                "required": ["method", "url"]
            }),
        }
    }

    fn get_api_context_def() -> ToolDefinition {
        ToolDefinition {
            name: "get_api_context".to_string(),
            description: "Get a summary of loaded API(s) for context. Returns endpoints, methods, \
                         and parameters in a format suitable for understanding and working with the API. \
                         Use this after ingesting an API to get its structure."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "api_name": {
                        "type": "string",
                        "description": "Name of the API to get context for (optional - returns all loaded APIs if omitted)"
                    },
                    "format": {
                        "type": "string",
                        "enum": ["summary", "detailed", "compact"],
                        "description": "Output format: 'summary' (default) for structured JSON, 'detailed' includes schemas, 'compact' for text overview"
                    }
                }
            }),
        }
    }

    fn list_loaded_apis_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_loaded_apis".to_string(),
            description: "List all APIs currently loaded in the context store. \
                         Shows which APIs are available for querying without hitting the database."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn clear_api_context_def() -> ToolDefinition {
        ToolDefinition {
            name: "clear_api_context".to_string(),
            description: "Remove an API from the in-memory context store. \
                         The API data remains in the database and can be reloaded."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "api_name": {
                        "type": "string",
                        "description": "Name of the API to clear (optional - clears all if omitted)"
                    }
                }
            }),
        }
    }

    fn discover_openapi_def() -> ToolDefinition {
        ToolDefinition {
            name: "discover_openapi".to_string(),
            description: "Automatically discover OpenAPI specifications for an API. \
                         Probes common paths (e.g., /openapi.json, /swagger.json), \
                         parses HTML documentation pages for spec links, and uses \
                         LLM to intelligently suggest additional locations based on \
                         the API's structure and responses."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "base_url": {
                        "type": "string",
                        "description": "Base URL of the API to discover specs for (e.g., https://api.example.com)"
                    },
                    "use_llm": {
                        "type": "boolean",
                        "description": "Whether to use LLM for intelligent discovery suggestions (default: true)"
                    },
                    "auto_ingest": {
                        "type": "boolean",
                        "description": "Automatically ingest discovered specs into the knowledge graph (default: false)"
                    }
                },
                "required": ["base_url"]
            }),
        }
    }

    fn build_openapi_from_docs_def() -> ToolDefinition {
        ToolDefinition {
            name: "build_openapi_from_docs".to_string(),
            description: "Generate an OpenAPI specification from API documentation pages. \
                         Uses LLM to analyze HTML, markdown, or text documentation and \
                         extract API endpoints, parameters, request/response schemas. \
                         Outputs a valid OpenAPI 3.0 spec in JSON or YAML format."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "doc_urls": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "URLs of documentation pages to analyze"
                    },
                    "api_title": {
                        "type": "string",
                        "description": "Title for the generated API spec"
                    },
                    "api_version": {
                        "type": "string",
                        "description": "Version for the generated API spec (default: 1.0.0)"
                    },
                    "base_url": {
                        "type": "string",
                        "description": "Base URL of the API server (optional)"
                    },
                    "output_format": {
                        "type": "string",
                        "enum": ["json", "yaml"],
                        "description": "Output format for the spec (default: json)"
                    },
                    "auto_ingest": {
                        "type": "boolean",
                        "description": "Automatically ingest the generated spec into the knowledge graph (default: false)"
                    }
                },
                "required": ["doc_urls", "api_title"]
            }),
        }
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tool Input Types
// ============================================================================

#[derive(Debug, Deserialize)]
pub struct IngestOpenApiInput {
    pub source: String,
}

#[derive(Debug, Deserialize)]
pub struct QueryEndpointInput {
    pub query: String,
}

#[derive(Debug, Deserialize)]
pub struct ExecuteRequestInput {
    pub method: String,
    pub url: String,
    #[serde(default)]
    pub headers: Option<std::collections::HashMap<String, String>>,
    #[serde(default)]
    pub body: Option<Value>,
}

#[derive(Debug, Deserialize)]
pub struct GetApiContextInput {
    pub api_name: Option<String>,
    #[serde(default)]
    pub format: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ClearApiContextInput {
    pub api_name: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DiscoverOpenApiInput {
    pub base_url: String,
    #[serde(default = "default_true")]
    pub use_llm: bool,
    #[serde(default)]
    pub auto_ingest: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Deserialize)]
pub struct BuildOpenApiFromDocsInput {
    pub doc_urls: Vec<String>,
    pub api_title: String,
    #[serde(default = "default_api_version")]
    pub api_version: String,
    pub base_url: Option<String>,
    #[serde(default = "default_output_format")]
    pub output_format: String,
    #[serde(default)]
    pub auto_ingest: bool,
}

fn default_api_version() -> String {
    "1.0.0".to_string()
}

fn default_output_format() -> String {
    "json".to_string()
}

// ============================================================================
// Tool Handler
// ============================================================================

/// Handler for executing MCP tools.
pub struct ToolHandler {
    neo4j: Option<Neo4jClient>,
    llm_config: Option<LlmConfig>,
    context_store: ContextStore,
}

impl ToolHandler {
    /// Create a new tool handler without database connection.
    pub fn new() -> Self {
        Self {
            neo4j: None,
            llm_config: None,
            context_store: ContextStore::new(),
        }
    }

    /// Create a tool handler with Neo4j connection.
    pub fn with_neo4j(neo4j: Neo4jClient) -> Self {
        let context_store = ContextStore::with_neo4j(neo4j.clone());
        Self {
            neo4j: Some(neo4j),
            llm_config: None,
            context_store,
        }
    }

    /// Set the LLM configuration for healing.
    pub fn with_llm_config(mut self, config: LlmConfig) -> Self {
        self.llm_config = Some(config);
        self
    }

    /// Get a reference to the context store.
    pub fn context_store(&self) -> &ContextStore {
        &self.context_store
    }

    /// Execute a tool by name with the given arguments.
    pub async fn execute(&self, name: &str, arguments: Option<Value>) -> ToolCallResult {
        debug!(tool = %name, "Executing tool");

        match name {
            "ingest_openapi" => self.handle_ingest_openapi(arguments).await,
            "graph_query_endpoint" => self.handle_query_endpoint(arguments).await,
            "execute_http_request" => self.handle_execute_request(arguments).await,
            "get_api_context" => self.handle_get_api_context(arguments).await,
            "list_loaded_apis" => self.handle_list_loaded_apis().await,
            "clear_api_context" => self.handle_clear_api_context(arguments).await,
            "discover_openapi" => self.handle_discover_openapi(arguments).await,
            "build_openapi_from_docs" => self.handle_build_openapi_from_docs(arguments).await,
            _ => ToolCallResult::error(format!("Unknown tool: {}", name)),
        }
    }

    // ========================================================================
    // Tool Implementations
    // ========================================================================

    async fn handle_ingest_openapi(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: IngestOpenApiInput = match Self::parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let Some(neo4j) = &self.neo4j else {
            return ToolCallResult::error("Database connection not configured");
        };

        info!(source = %input.source, "Ingesting OpenAPI specification");

        // Initialize schema if needed
        if let Err(e) = neo4j.init_schema().await {
            return ToolCallResult::error(format!("Failed to initialize schema: {}", e));
        }

        // Parse and ingest
        let mut parser = OpenApiParser::new(neo4j.clone());
        match parser.ingest(&input.source).await {
            Ok(result) => {
                // Build and store API context from the ingested endpoints
                let context = self.build_api_context(
                    &result.api_title,
                    &result.api_version,
                    Some(&input.source),
                    result.description.as_deref(),
                    &result.endpoints,
                );

                self.context_store.set(context).await;

                let response = json!({
                    "success": true,
                    "api_title": result.api_title,
                    "api_version": result.api_version,
                    "resources_created": result.resources_created,
                    "endpoints_created": result.endpoints_created,
                    "schemas_created": result.schemas_created,
                    "parameters_created": result.parameters_created,
                    "context_loaded": true
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to ingest OpenAPI spec: {}", e)),
        }
    }

    async fn handle_query_endpoint(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: QueryEndpointInput = match Self::parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let Some(neo4j) = &self.neo4j else {
            return ToolCallResult::error("Database connection not configured");
        };

        info!(query = %input.query, "Querying endpoints");

        match neo4j.find_endpoints_by_path(&input.query).await {
            Ok(endpoints) => {
                if endpoints.is_empty() {
                    return ToolCallResult::success_text(format!(
                        "No endpoints found matching: {}",
                        input.query
                    ));
                }

                let mut results = Vec::new();
                for endpoint in endpoints {
                    // Get parameters for this endpoint
                    let params = neo4j
                        .get_parameters_for_endpoint(endpoint.id)
                        .await
                        .unwrap_or_default();

                    let param_list: Vec<_> = params
                        .iter()
                        .map(|p| {
                            json!({
                                "name": p.name,
                                "location": p.location.to_string(),
                                "required": p.required,
                                "type": p.param_type
                            })
                        })
                        .collect();

                    results.push(json!({
                        "id": endpoint.id.to_string(),
                        "path": endpoint.path,
                        "method": endpoint.method.to_string(),
                        "summary": endpoint.summary,
                        "operation_id": endpoint.operation_id,
                        "status": format!("{:?}", endpoint.status),
                        "parameters": param_list
                    }));
                }

                let response = json!({
                    "count": results.len(),
                    "endpoints": results
                });

                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Query failed: {}", e)),
        }
    }

    async fn handle_execute_request(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ExecuteRequestInput = match Self::parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        info!(method = %input.method, url = %input.url, "Executing HTTP request");

        // Parse method
        let method = match input.method.to_uppercase().as_str() {
            "GET" => crate::models::HttpMethod::Get,
            "POST" => crate::models::HttpMethod::Post,
            "PUT" => crate::models::HttpMethod::Put,
            "PATCH" => crate::models::HttpMethod::Patch,
            "DELETE" => crate::models::HttpMethod::Delete,
            "HEAD" => crate::models::HttpMethod::Head,
            "OPTIONS" => crate::models::HttpMethod::Options,
            _ => return ToolCallResult::error(format!("Invalid HTTP method: {}", input.method)),
        };

        // Create HTTP executor
        let http = match HttpExecutor::new() {
            Ok(h) => h,
            Err(e) => return ToolCallResult::error(format!("Failed to create HTTP client: {}", e)),
        };

        // Build orchestrator with optional LLM for healing
        let mut orchestrator = HealingOrchestrator::new(http);

        if let Some(llm_config) = &self.llm_config
            && let Ok(llm) = LlmClient::with_config(llm_config.clone())
        {
            orchestrator = orchestrator
                .with_llm(llm)
                .with_config(HealingConfig::default());
        }

        if let Some(neo4j) = &self.neo4j {
            orchestrator = orchestrator.with_neo4j(neo4j.clone());
        }

        // Execute the request
        match orchestrator
            .execute_simple(method, &input.url, input.body.clone())
            .await
        {
            Ok(response) => {
                let result = json!({
                    "status_code": response.status_code,
                    "status_class": format!("{:?}", response.class),
                    "duration_ms": response.duration_ms,
                    "headers": response.headers,
                    "body": Self::try_parse_json(&response.body)
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&result).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Request failed: {}", e)),
        }
    }

    async fn handle_get_api_context(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: GetApiContextInput = match Self::parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let format = input.format.as_deref().unwrap_or("summary");

        match &input.api_name {
            Some(name) => {
                // Get specific API context
                match self.context_store.get_or_load(name).await {
                    Some(ctx) => self.format_context(&ctx, format),
                    None => ToolCallResult::error(format!(
                        "API '{}' not found in context. Use ingest_openapi to load it first, or list_loaded_apis to see available APIs.",
                        name
                    )),
                }
            }
            None => {
                // Get all loaded contexts
                let contexts = self.context_store.get_all().await;
                if contexts.is_empty() {
                    return ToolCallResult::success_text(
                        "No APIs loaded in context. Use ingest_openapi to load an API specification."
                    );
                }

                match format {
                    "compact" => {
                        let mut output = String::new();
                        for ctx in &contexts {
                            output.push_str(&ctx.to_compact_summary());
                            output.push_str("\n---\n\n");
                        }
                        ToolCallResult::success_text(output)
                    }
                    _ => {
                        let summaries: Vec<Value> = contexts
                            .iter()
                            .map(|ctx| self.context_to_json(ctx, format == "detailed"))
                            .collect();
                        let response = json!({
                            "count": summaries.len(),
                            "apis": summaries
                        });
                        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
                    }
                }
            }
        }
    }

    async fn handle_list_loaded_apis(&self) -> ToolCallResult {
        let contexts = self.context_store.get_all().await;

        if contexts.is_empty() {
            return ToolCallResult::success_text(
                "No APIs currently loaded. Use ingest_openapi to load an API specification."
            );
        }

        let api_list: Vec<Value> = contexts
            .iter()
            .map(|ctx| {
                json!({
                    "name": ctx.name,
                    "version": ctx.version,
                    "endpoint_count": ctx.endpoint_count,
                    "schema_count": ctx.schema_count,
                    "loaded_at": ctx.loaded_at.to_rfc3339()
                })
            })
            .collect();

        let response = json!({
            "count": api_list.len(),
            "apis": api_list
        });

        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
    }

    async fn handle_clear_api_context(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ClearApiContextInput = match Self::parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        match &input.api_name {
            Some(name) => {
                if self.context_store.contains(name).await {
                    self.context_store.clear(Some(name)).await;
                    ToolCallResult::success_text(format!(
                        "Cleared context for API '{}'. The API data remains in the database and can be reloaded.",
                        name
                    ))
                } else {
                    ToolCallResult::error(format!(
                        "API '{}' not found in context. Use list_loaded_apis to see available APIs.",
                        name
                    ))
                }
            }
            None => {
                let count = self.context_store.len().await;
                self.context_store.clear(None).await;
                ToolCallResult::success_text(format!(
                    "Cleared {} API context(s). API data remains in the database and can be reloaded.",
                    count
                ))
            }
        }
    }

    async fn handle_discover_openapi(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: DiscoverOpenApiInput = match Self::parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        info!(base_url = %input.base_url, use_llm = input.use_llm, "Discovering OpenAPI specifications");

        // Create discovery service
        let mut service = match DiscoveryService::new() {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Failed to create discovery service: {}", e)),
        };

        // Configure LLM if requested
        if input.use_llm {
            if let Some(llm_config) = &self.llm_config {
                if let Ok(llm) = LlmClient::with_config(llm_config.clone()) {
                    service = service.with_llm(llm);
                }
            }
        }

        // Configure discovery
        let config = DiscoveryConfig {
            use_llm: input.use_llm && self.llm_config.is_some(),
            ..Default::default()
        };
        service = service.with_config(config);

        // Run discovery
        let result = match service.discover(&input.base_url).await {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Discovery failed: {}", e)),
        };

        // Auto-ingest if requested and we have a database connection
        let mut ingested_apis = Vec::new();
        if input.auto_ingest && self.neo4j.is_some() {
            let neo4j = self.neo4j.as_ref().unwrap();

            // Initialize schema if needed
            if let Err(e) = neo4j.init_schema().await {
                return ToolCallResult::error(format!("Failed to initialize schema: {}", e));
            }

            for candidate in &result.candidates {
                let mut parser = OpenApiParser::new(neo4j.clone());
                match parser.ingest(&candidate.url).await {
                    Ok(ingest_result) => {
                        // Build and store context
                        let context = self.build_api_context(
                            &ingest_result.api_title,
                            &ingest_result.api_version,
                            Some(&candidate.url),
                            ingest_result.description.as_deref(),
                            &ingest_result.endpoints,
                        );
                        self.context_store.set(context).await;

                        ingested_apis.push(json!({
                            "url": candidate.url,
                            "api_title": ingest_result.api_title,
                            "api_version": ingest_result.api_version,
                            "endpoints_created": ingest_result.endpoints_created
                        }));
                    }
                    Err(e) => {
                        debug!(url = %candidate.url, error = %e, "Failed to ingest discovered spec");
                    }
                }
            }
        }

        // Build response
        let candidates: Vec<Value> = result
            .candidates
            .iter()
            .map(|c| {
                json!({
                    "url": c.url,
                    "method": format!("{:?}", c.method),
                    "confidence": c.confidence,
                    "format": c.format,
                    "api_title": c.api_title,
                    "api_version": c.api_version
                })
            })
            .collect();

        let mut response = json!({
            "base_url": result.base_url,
            "candidates_found": candidates.len(),
            "candidates": candidates,
            "urls_probed": result.probed_urls.len()
        });

        if !result.errors.is_empty() {
            response["errors"] = json!(result.errors);
        }

        if !ingested_apis.is_empty() {
            response["auto_ingested"] = json!(ingested_apis);
        }

        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
    }

    async fn handle_build_openapi_from_docs(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: BuildOpenApiFromDocsInput = match Self::parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        info!(
            urls = ?input.doc_urls,
            title = %input.api_title,
            "Building OpenAPI spec from documentation"
        );

        // Require LLM configuration for this tool
        let Some(llm_config) = &self.llm_config else {
            return ToolCallResult::error(
                "LLM configuration required for documentation analysis. \
                 Configure an LLM provider (Ollama or Anthropic) to use this tool."
            );
        };

        let llm = match LlmClient::with_config(llm_config.clone()) {
            Ok(l) => l,
            Err(e) => return ToolCallResult::error(format!("Failed to create LLM client: {}", e)),
        };

        // Create the doc generator service
        let service = match DocGenService::new(llm) {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Failed to create doc generator: {}", e)),
        };

        // Generate the OpenAPI spec
        let result = match service
            .generate(
                &input.doc_urls,
                &input.api_title,
                &input.api_version,
                input.base_url.as_deref(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Documentation analysis failed: {}", e)),
        };

        // Format the spec based on requested format
        let spec_output = match input.output_format.as_str() {
            "yaml" => result.spec.to_yaml().unwrap_or_else(|e| format!("YAML error: {}", e)),
            _ => result.spec.to_json().unwrap_or_else(|e| format!("JSON error: {}", e)),
        };

        // Auto-ingest if requested and we have a database connection
        let mut ingested = false;
        if input.auto_ingest && self.neo4j.is_some() {
            let neo4j = self.neo4j.as_ref().unwrap();

            // Initialize schema if needed
            if let Err(e) = neo4j.init_schema().await {
                return ToolCallResult::error(format!("Failed to initialize schema: {}", e));
            }

            // Write spec to temp file for ingestion
            let temp_path = format!("/tmp/generated_spec_{}.json", uuid::Uuid::new_v4());
            if let Ok(()) = std::fs::write(&temp_path, result.spec.to_json().unwrap_or_default()) {
                let mut parser = OpenApiParser::new(neo4j.clone());
                if let Ok(ingest_result) = parser.ingest(&temp_path).await {
                    // Build and store context
                    let context = self.build_api_context(
                        &ingest_result.api_title,
                        &ingest_result.api_version,
                        None,
                        ingest_result.description.as_deref(),
                        &ingest_result.endpoints,
                    );
                    self.context_store.set(context).await;
                    ingested = true;
                }
                let _ = std::fs::remove_file(&temp_path);
            }
        }

        // Build response
        let mut response = json!({
            "success": true,
            "api_title": input.api_title,
            "api_version": input.api_version,
            "endpoints_found": result.endpoints_found,
            "schemas_found": result.schemas_found,
            "sources_analyzed": result.sources.len(),
            "output_format": input.output_format,
            "spec": spec_output
        });

        if !result.warnings.is_empty() {
            response["warnings"] = json!(result.warnings);
        }

        if ingested {
            response["auto_ingested"] = json!(true);
            response["context_loaded"] = json!(true);
        }

        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
    }

    // ========================================================================
    // Helpers
    // ========================================================================

    /// Build an ApiContext from freshly ingested endpoints.
    fn build_api_context(
        &self,
        api_name: &str,
        api_version: &str,
        source: Option<&str>,
        description: Option<&str>,
        endpoints: &[EndpointWithParams],
    ) -> ApiContext {
        let mut context = ApiContext::new(api_name.to_string(), api_version.to_string());

        if let Some(src) = source {
            context = context.with_source(src);
        }

        if let Some(desc) = description {
            context = context.with_description(desc);
        }

        for ep in endpoints {
            let mut param_summary = ParameterSummary::default();
            for param in &ep.parameters {
                let name = if param.required {
                    format!("{}*", param.name)
                } else {
                    param.name.clone()
                };

                match param.location {
                    ParameterLocation::Path => param_summary.path.push(name),
                    ParameterLocation::Query => param_summary.query.push(name),
                    ParameterLocation::Header => param_summary.header.push(name),
                    ParameterLocation::Body => param_summary.body.push(name),
                }
            }

            context.add_endpoint(EndpointSummary {
                method: ep.endpoint.method.clone(),
                path: ep.endpoint.path.clone(),
                summary: ep.endpoint.summary.clone(),
                operation_id: ep.endpoint.operation_id.clone(),
                parameters: param_summary,
            });
        }

        context
    }

    /// Format a context for output based on requested format.
    fn format_context(&self, ctx: &ApiContext, format: &str) -> ToolCallResult {
        match format {
            "compact" => ToolCallResult::success_text(ctx.to_compact_summary()),
            "detailed" => {
                let json = self.context_to_json(ctx, true);
                ToolCallResult::success_text(serde_json::to_string_pretty(&json).unwrap())
            }
            _ => {
                // "summary" - default
                let json = self.context_to_json(ctx, false);
                ToolCallResult::success_text(serde_json::to_string_pretty(&json).unwrap())
            }
        }
    }

    /// Convert context to JSON representation.
    fn context_to_json(&self, ctx: &ApiContext, include_schemas: bool) -> Value {
        let endpoints: Vec<Value> = ctx
            .endpoints
            .iter()
            .map(|ep| {
                let mut ep_json = json!({
                    "method": ep.method.to_string(),
                    "path": ep.path,
                    "summary": ep.summary
                });

                if let Some(op_id) = &ep.operation_id {
                    ep_json["operation_id"] = json!(op_id);
                }

                if !ep.parameters.is_empty() {
                    let mut params = json!({});
                    if !ep.parameters.path.is_empty() {
                        params["path"] = json!(ep.parameters.path);
                    }
                    if !ep.parameters.query.is_empty() {
                        params["query"] = json!(ep.parameters.query);
                    }
                    if !ep.parameters.header.is_empty() {
                        params["header"] = json!(ep.parameters.header);
                    }
                    if !ep.parameters.body.is_empty() {
                        params["body"] = json!(ep.parameters.body);
                    }
                    ep_json["parameters"] = params;
                }

                ep_json
            })
            .collect();

        let mut result = json!({
            "name": ctx.name,
            "version": ctx.version,
            "endpoint_count": ctx.endpoint_count,
            "endpoints": endpoints,
            "loaded_at": ctx.loaded_at.to_rfc3339()
        });

        if let Some(base) = &ctx.base_url {
            result["base_url"] = json!(base);
        }

        if let Some(desc) = &ctx.description {
            result["description"] = json!(desc);
        }

        if let Some(src) = &ctx.source {
            result["source"] = json!(src);
        }

        if include_schemas && !ctx.schemas.is_empty() {
            let schemas: Vec<Value> = ctx
                .schemas
                .iter()
                .map(|s| {
                    json!({
                        "name": s.name,
                        "fields": s.fields
                    })
                })
                .collect();
            result["schemas"] = json!(schemas);
            result["schema_count"] = json!(ctx.schema_count);
        }

        result
    }

    fn parse_args<T: for<'de> Deserialize<'de>>(
        arguments: Option<Value>,
    ) -> Result<T, ToolCallResult> {
        let args = arguments.unwrap_or(Value::Object(Default::default()));
        serde_json::from_value(args)
            .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {}", e)))
    }

    fn try_parse_json(text: &str) -> Value {
        serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.to_string()))
    }
}

impl Default for ToolHandler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_registry_creation() {
        let registry = ToolRegistry::new();
        assert_eq!(registry.list().len(), 8);
    }

    #[test]
    fn test_tool_registry_get() {
        let registry = ToolRegistry::new();
        assert!(registry.get("ingest_openapi").is_some());
        assert!(registry.get("graph_query_endpoint").is_some());
        assert!(registry.get("execute_http_request").is_some());
        assert!(registry.get("get_api_context").is_some());
        assert!(registry.get("list_loaded_apis").is_some());
        assert!(registry.get("clear_api_context").is_some());
        assert!(registry.get("discover_openapi").is_some());
        assert!(registry.get("build_openapi_from_docs").is_some());
        assert!(registry.get("unknown_tool").is_none());
    }

    #[test]
    fn test_tool_definitions_have_required_fields() {
        let registry = ToolRegistry::new();
        for tool in registry.list() {
            assert!(!tool.name.is_empty());
            assert!(!tool.description.is_empty());
            assert!(tool.input_schema.is_object());
        }
    }

    #[test]
    fn test_ingest_input_parsing() {
        let json = json!({"source": "https://example.com/openapi.json"});
        let input: IngestOpenApiInput = serde_json::from_value(json).unwrap();
        assert_eq!(input.source, "https://example.com/openapi.json");
    }

    #[test]
    fn test_query_input_parsing() {
        let json = json!({"query": "create user"});
        let input: QueryEndpointInput = serde_json::from_value(json).unwrap();
        assert_eq!(input.query, "create user");
    }

    #[test]
    fn test_execute_input_parsing() {
        let json = json!({
            "method": "POST",
            "url": "https://api.example.com/users",
            "headers": {"Authorization": "Bearer token"},
            "body": {"name": "test"}
        });
        let input: ExecuteRequestInput = serde_json::from_value(json).unwrap();
        assert_eq!(input.method, "POST");
        assert_eq!(input.url, "https://api.example.com/users");
        assert!(input.headers.is_some());
        assert!(input.body.is_some());
    }

    #[test]
    fn test_execute_input_minimal() {
        let json = json!({"method": "GET", "url": "https://example.com"});
        let input: ExecuteRequestInput = serde_json::from_value(json).unwrap();
        assert_eq!(input.method, "GET");
        assert!(input.headers.is_none());
        assert!(input.body.is_none());
    }

    #[test]
    fn test_tool_handler_creation() {
        let handler = ToolHandler::new();
        assert!(handler.neo4j.is_none());
    }

    #[tokio::test]
    async fn test_unknown_tool_returns_error() {
        let handler = ToolHandler::new();
        let result = handler.execute("unknown_tool", None).await;
        assert_eq!(result.is_error, Some(true));
    }
}
