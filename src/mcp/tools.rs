//! MCP tool definitions and handlers.

use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{debug, info};

use crate::repository::Neo4jClient;
use crate::services::{
    HealingConfig, HealingOrchestrator, HttpExecutor, LlmClient, LlmConfig, OpenApiParser,
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

// ============================================================================
// Tool Handler
// ============================================================================

/// Handler for executing MCP tools.
pub struct ToolHandler {
    neo4j: Option<Neo4jClient>,
    llm_config: Option<LlmConfig>,
}

impl ToolHandler {
    /// Create a new tool handler without database connection.
    pub fn new() -> Self {
        Self {
            neo4j: None,
            llm_config: None,
        }
    }

    /// Create a tool handler with Neo4j connection.
    pub fn with_neo4j(neo4j: Neo4jClient) -> Self {
        Self {
            neo4j: Some(neo4j),
            llm_config: None,
        }
    }

    /// Set the LLM configuration for healing.
    pub fn with_llm_config(mut self, config: LlmConfig) -> Self {
        self.llm_config = Some(config);
        self
    }

    /// Execute a tool by name with the given arguments.
    pub async fn execute(&self, name: &str, arguments: Option<Value>) -> ToolCallResult {
        debug!(tool = %name, "Executing tool");

        match name {
            "ingest_openapi" => self.handle_ingest_openapi(arguments).await,
            "graph_query_endpoint" => self.handle_query_endpoint(arguments).await,
            "execute_http_request" => self.handle_execute_request(arguments).await,
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
                let response = json!({
                    "success": true,
                    "api_title": result.api_title,
                    "api_version": result.api_version,
                    "resources_created": result.resources_created,
                    "endpoints_created": result.endpoints_created,
                    "schemas_created": result.schemas_created,
                    "parameters_created": result.parameters_created
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

    // ========================================================================
    // Helpers
    // ========================================================================

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
        assert_eq!(registry.list().len(), 3);
    }

    #[test]
    fn test_tool_registry_get() {
        let registry = ToolRegistry::new();
        assert!(registry.get("ingest_openapi").is_some());
        assert!(registry.get("graph_query_endpoint").is_some());
        assert!(registry.get("execute_http_request").is_some());
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
