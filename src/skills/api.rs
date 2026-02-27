//! API Expert Skill - Provides OpenAPI ingestion, discovery, and execution tools.

use std::str::FromStr;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use tracing::{debug, info};

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::models::{ApiCredential, CredentialType, InjectLocation, ParameterLocation, Endpoint};
use crate::repository::Neo4jClient;
use crate::services::{
    ApiContext, ContextStore, CredentialManager, DiscoveryConfig, DiscoveryService, DocGenService,
    EndpointSummary, EndpointWithParams, ExportFormat, ExportOptions, HttpExecutor, LlmClient,
    LlmConfig, MarkdownReportGenerator, MergeStrategy, OpenApiExporter, OpenApiParser,
    ParameterSummary, RepoAnalyzerService, RequestBuilder, SpecDiffer,
    HealingOrchestrator, HealingConfig, RequestContext,
};
use crate::skills::Skill;

/// API Expert Skill implementation.
pub struct ApiSkill {
    neo4j: Option<Neo4jClient>,
    llm_config: Option<LlmConfig>,
    context_store: ContextStore,
    credential_manager: Option<Arc<CredentialManager>>,
}

impl ApiSkill {
    /// Create a new API skill.
    pub fn new(
        neo4j: Option<Neo4jClient>,
        llm_config: Option<LlmConfig>,
        context_store: ContextStore,
        credential_manager: Option<Arc<CredentialManager>>,
    ) -> Self {
        Self {
            neo4j,
            llm_config,
            context_store,
            credential_manager,
        }
    }

    // ========================================================================
    // Tool Definitions (ported from original ToolRegistry)
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
                    },
                    "query": {
                        "type": "string",
                        "description": "Optional natural language query to filter for specific endpoints or schemas"
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

    fn build_openapi_from_repo_def() -> ToolDefinition {
        ToolDefinition {
            name: "build_openapi_from_repo".to_string(),
            description: "Generate an OpenAPI specification by analyzing source code in a Git repository. \
                         Supports GitHub and GitLab repositories (public and private). \
                         Uses LLM to extract API endpoints from code in any language/framework. \
                         Can merge with existing OpenAPI specs found in the repository."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "repo_url": {
                        "type": "string",
                        "description": "Repository URL (e.g., https://github.com/owner/repo)"
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
                    "ref_name": {
                        "type": "string",
                        "description": "Branch, tag, or commit to analyze (default: default branch)"
                    },
                    "subdirectory": {
                        "type": "string",
                        "description": "Subdirectory to analyze (for monorepos)"
                    },
                    "include_patterns": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Glob patterns for files to include (optional)"
                    },
                    "merge_strategy": {
                        "type": "string",
                        "enum": ["enhance", "replace", "ignore"],
                        "description": "How to handle existing specs: 'enhance' (merge), 'replace' (use code only), 'ignore' (skip existing)"
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
                "required": ["repo_url", "api_title"]
            }),
        }
    }

    fn export_openapi_def() -> ToolDefinition {
        ToolDefinition {
            name: "export_openapi".to_string(),
            description: "Export the healed knowledge graph back to an OpenAPI 3.0 specification. \
                         The exported spec includes x-healed-by-ai annotations showing what was \
                         auto-corrected, and x-original-value fields preserving the original values. \
                         Use this to commit the 'healed' documentation back to git."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "format": {
                        "type": "string",
                        "enum": ["yaml", "json"],
                        "description": "Output format (default: yaml, recommended for git)"
                    },
                    "include_annotations": {
                        "type": "boolean",
                        "description": "Include x-healed-by-ai and x-original-value annotations (default: true)"
                    },
                    "include_broken": {
                        "type": "boolean",
                        "description": "Include endpoints marked as broken (default: false)"
                    },
                    "output_path": {
                        "type": "string",
                        "description": "File path to write the spec (returns content if omitted)"
                    }
                }
            }),
        }
    }

    fn diff_api_spec_def() -> ToolDefinition {
        ToolDefinition {
            name: "diff_api_spec".to_string(),
            description: "Compare the original ingested spec against the current healed graph state. \
                         Generates a markdown report showing all documentation drift: parameter renames, \
                         type changes, added/removed fields, and AI corrections. \
                         Use this before committing to see what changed."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "format": {
                        "type": "string",
                        "enum": ["markdown", "json", "changelog"],
                        "description": "Output format: 'markdown' (detailed report), 'json' (structured data), 'changelog' (git commit message)"
                    },
                    "breaking_only": {
                        "type": "boolean",
                        "description": "Only show breaking changes (default: false)"
                    }
                }
            }),
        }
    }

    fn configure_api_credential_def() -> ToolDefinition {
        ToolDefinition {
            name: "configure_api_credential".to_string(),
            description: "Configure API credentials for authentication. \
                         Associates a credential (API key, bearer token, etc.) with an API name. \
                         The actual secret value is stored securely in the configured secret provider."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "api_name": {
                        "type": "string",
                        "description": "Name of the API to configure credentials for (e.g., 'OpenWeatherMap')"
                    },
                    "credential_type": {
                        "type": "string",
                        "enum": ["api_key", "bearer", "basic", "oauth2_client_credentials"],
                        "description": "Type of credential"
                    },
                    "inject_location": {
                        "type": "string",
                        "enum": ["header", "query"],
                        "description": "Where to inject the credential in requests"
                    },
                    "inject_key": {
                        "type": "string",
                        "description": "Header or query parameter name (e.g., 'X-API-Key', 'Authorization', 'appid')"
                    },
                    "secret_value": {
                        "type": "string",
                        "description": "The actual secret value to store"
                    },
                    "description": {
                        "type": "string",
                        "description": "Optional description of the credential"
                    }
                },
                "required": ["api_name", "credential_type", "inject_location", "inject_key", "secret_value"]
            }),
        }
    }

    fn list_api_credentials_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_api_credentials".to_string(),
            description: "List all configured API credentials. \
                         Returns credential metadata (not the actual secrets)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        }
    }

    fn delete_api_credential_def() -> ToolDefinition {
        ToolDefinition {
            name: "delete_api_credential".to_string(),
            description: "Delete an API credential configuration. \
                         Removes both the credential metadata and the stored secret."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "api_name": {
                        "type": "string",
                        "description": "Name of the API to delete credentials for"
                    }
                },
                "required": ["api_name"]
            }),
        }
    }

    // ========================================================================
    // Tool Implementation Logic (ported from original ToolHandler)
    // ========================================================================

    async fn handle_ingest_openapi(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: IngestOpenApiInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let Some(neo4j) = &self.neo4j else {
            return ToolCallResult::error("Database connection not configured");
        };

        info!(source = %input.source, "Ingesting OpenAPI specification");

        if let Err(e) = neo4j.init_schema().await {
            return ToolCallResult::error(format!("Failed to initialize schema: {}", e));
        }

        let mut parser = OpenApiParser::new(neo4j.clone());
        if let Some(llm_config) = &self.llm_config {
            if let Ok(llm) = LlmClient::with_config(llm_config.clone()) {
                parser = parser.with_llm(llm);
            }
        }

        match parser.ingest(&input.source).await {
            Ok(result) => {
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
        let input: QueryEndpointInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let Some(neo4j) = &self.neo4j else {
            return ToolCallResult::error("Database connection not configured");
        };

        info!(query = %input.query, "Querying endpoints with hybrid search");

        // Try semantic search first if LLM is available
        let mut endpoints = Vec::new();
        let mut used_semantic = false;

        if let Some(llm_config) = &self.llm_config {
            if let Ok(llm) = LlmClient::with_config(llm_config.clone()) {
                if let Ok(embedding) = llm.embeddings(&input.query).await {
                    if let Ok(semantic_results) = neo4j.find_endpoints_semantic(embedding, 10).await {
                        if !semantic_results.is_empty() {
                            endpoints = semantic_results;
                            used_semantic = true;
                            debug!(count = endpoints.len(), "Found endpoints via semantic search");
                        }
                    }
                }
            }
        }

        // Fallback to keyword search if semantic failed or wasn't available
        if endpoints.is_empty() {
            match neo4j.find_endpoints_by_path(&input.query).await {
                Ok(results) => {
                    endpoints = results;
                    debug!(count = endpoints.len(), "Found endpoints via keyword search");
                }
                Err(e) => return ToolCallResult::error(format!("Query failed: {}", e)),
            }
        }

        if endpoints.is_empty() {
            return ToolCallResult::success_text(format!(
                "No endpoints found matching: {}",
                input.query
            ));
        }

        let mut results = Vec::new();
        for endpoint in endpoints {
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
            "endpoints": results,
            "search_method": if used_semantic { "semantic" } else { "keyword" }
        });

        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
    }

    async fn handle_execute_request(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ExecuteRequestInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        info!(method = %input.method, url = %input.url, "Executing HTTP request");

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

        // Attempt to find a matching endpoint in Neo4j for self-healing
        let matching_endpoint = if let Some(neo4j) = &self.neo4j {
            self.find_matching_endpoint(neo4j, &input.url, method).await
        } else {
            None
        };

        let mut builder = RequestBuilder::new().base_url(&input.url).method(method);

        if let Some(ref headers) = input.headers {
            builder = builder.headers(headers.clone());
        }

        if let Some(ref body) = input.body {
            builder = builder.body(body.clone());
        }

        let (builder, credentials_injected) = if let Some(cred_manager) = &self.credential_manager {
            let api_detected = cred_manager.detect_api_from_url(&input.url).await.is_some();
            match cred_manager
                .inject_credentials_for_url(&input.url, builder)
                .await
            {
                Ok(updated_builder) => (updated_builder, api_detected),
                Err(e) => {
                    debug!(error = %e, "Failed to inject credentials, rebuilding request");
                    let mut new_builder =
                        RequestBuilder::new().base_url(&input.url).method(method);
                    if let Some(headers) = input.headers.clone() {
                        new_builder = new_builder.headers(headers);
                    }
                    if let Some(body) = input.body.clone() {
                        new_builder = new_builder.body(body);
                    }
                    (new_builder, false)
                }
            }
        } else {
            (builder, false)
        };

        let http = match HttpExecutor::new() {
            Ok(h) => h,
            Err(e) => return ToolCallResult::error(format!("Failed to create HTTP client: {}", e)),
        };

        // If we found a matching endpoint and have LLM config, use the HealingOrchestrator
        if let (Some(endpoint), Some(neo4j), Some(llm_config)) = (matching_endpoint, &self.neo4j, &self.llm_config) {
            info!(endpoint_id = %endpoint.id, path = %endpoint.path, "Using self-healing execution");
            
            let llm = match LlmClient::with_config(llm_config.clone()) {
                Ok(l) => l,
                Err(e) => return ToolCallResult::error(format!("Failed to create LLM client: {}", e)),
            };

            let orchestrator = HealingOrchestrator::with_all(http, llm, neo4j.clone());
            
            // Extract path params if possible (naive matching for now)
            let mut context = RequestContext::new(&input.url);
            if let Some(body) = &input.body {
                context = context.with_body(body.clone());
            }
            if let Some(headers) = &input.headers {
                for (k, v) in headers {
                    context = context.with_header(k, v);
                }
            }

            match orchestrator.execute_with_healing(&endpoint, &context).await {
                Ok(result) => {
                    let mut response_json = json!({
                        "status_code": result.response.status_code,
                        "duration_ms": result.response.duration_ms,
                        "success": result.success,
                        "healed": result.healed,
                        "attempts": result.attempts,
                        "body": try_parse_json(&result.response.body)
                    });

                    if !result.healing_events.is_empty() {
                        response_json["healing_events"] = json!(result.healing_events);
                    }

                    if let Some(analysis) = result.analysis {
                        response_json["error_analysis"] = json!(analysis);
                    }

                    if credentials_injected {
                        response_json["credentials_auto_injected"] = json!(true);
                    }

                    ToolCallResult::success_text(serde_json::to_string_pretty(&response_json).unwrap())
                }
                Err(e) => ToolCallResult::error(format!("Healing execution failed: {}", e)),
            }
        } else {
            // Fallback to simple execution
            match http.execute(&builder).await {
                Ok(response) => {
                    let mut result = json!({
                        "status_code": response.status_code,
                        "status_class": format!("{:?}", response.class),
                        "duration_ms": response.duration_ms,
                        "headers": response.headers,
                        "body": try_parse_json(&response.body)
                    });

                    if credentials_injected {
                        result["credentials_auto_injected"] = json!(true);
                    }

                    ToolCallResult::success_text(serde_json::to_string_pretty(&result).unwrap())
                }
                Err(e) => ToolCallResult::error(format!("Request failed: {}", e)),
            }
        }
    }

    /// Find a matching endpoint in Neo4j based on URL and method.
    async fn find_matching_endpoint(&self, neo4j: &Neo4jClient, url_str: &str, method: crate::models::HttpMethod) -> Option<Endpoint> {
        let parsed_url = match url::Url::parse(url_str) {
            Ok(u) => u,
            Err(_) => return None,
        };
        
        let path = parsed_url.path();
        
        // Try exact match first
        if let Ok(Some(endpoint)) = neo4j.get_endpoint_by_path_method(path, method).await {
            return Some(endpoint);
        }
        
        // Try template match
        if let Ok(endpoints) = neo4j.list_endpoints().await {
            for endpoint in endpoints {
                if endpoint.method == method && self.path_matches_template(path, &endpoint.path) {
                    return Some(endpoint);
                }
            }
        }
        
        None
    }

    /// Simple template matcher: /pet/{petId} matches /pet/123
    fn path_matches_template(&self, path: &str, template: &str) -> bool {
        let path_parts: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
        let temp_parts: Vec<&str> = template.split('/').filter(|s| !s.is_empty()).collect();
        
        if path_parts.len() != temp_parts.len() {
            return false;
        }
        
        for (p, t) in path_parts.iter().zip(temp_parts.iter()) {
            if t.starts_with('{') && t.ends_with('}') {
                continue; // Match any value for template parameter
            }
            if p != t {
                return false;
            }
        }
        
        true
    }

    /// Prune an ApiContext to only include endpoints/schemas relevant to a query.
    fn filter_context(&self, mut ctx: ApiContext, query: &str) -> ApiContext {
        let query_lower = query.to_lowercase();
        
        // Filter endpoints
        ctx.endpoints.retain(|ep| {
            ep.path.to_lowercase().contains(&query_lower) ||
            ep.summary.to_lowercase().contains(&query_lower) ||
            ep.operation_id.as_ref().map(|id| id.to_lowercase().contains(&query_lower)).unwrap_or(false)
        });
        
        // Filter schemas - simple inclusion if mentioned in remaining endpoints
        // (For now, just keep schemas that match the query text)
        ctx.schemas.retain(|s| {
            s.name.to_lowercase().contains(&query_lower) ||
            s.fields.iter().any(|f| f.to_lowercase().contains(&query_lower))
        });
        
        ctx.endpoint_count = ctx.endpoints.len();
        ctx.schema_count = ctx.schemas.len();
        ctx
    }

    async fn handle_get_api_context(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: GetApiContextInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let format = input.format.as_deref().unwrap_or("summary");

        match &input.api_name {
            Some(name) => match self.context_store.get_or_load(name).await {
                Some(mut ctx) => {
                    if let Some(query) = &input.query {
                        ctx = self.filter_context(ctx, query);
                    }
                    self.format_context(&ctx, format)
                }
                None => ToolCallResult::error(format!(
                    "API '{}' not found in context. Use ingest_openapi to load it first.",
                    name
                )),
            },
            None => {
                let mut contexts = self.context_store.get_all().await;
                if contexts.is_empty() {
                    return ToolCallResult::success_text("No APIs loaded in context.");
                }

                if let Some(query) = &input.query {
                    contexts = contexts.into_iter()
                        .map(|ctx| self.filter_context(ctx, query))
                        .filter(|ctx| !ctx.endpoints.is_empty())
                        .collect();
                }

                if contexts.is_empty() {
                    return ToolCallResult::success_text("No matching endpoints found in loaded APIs.");
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
            return ToolCallResult::success_text("No APIs currently loaded.");
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
        let input: ClearApiContextInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        match &input.api_name {
            Some(name) => {
                if self.context_store.contains(name).await {
                    self.context_store.clear(Some(name)).await;
                    ToolCallResult::success_text(format!("Cleared context for API '{}'.", name))
                } else {
                    ToolCallResult::error(format!("API '{}' not found in context.", name))
                }
            }
            None => {
                let count = self.context_store.len().await;
                self.context_store.clear(None).await;
                ToolCallResult::success_text(format!("Cleared {} API context(s).", count))
            }
        }
    }

    async fn handle_discover_openapi(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: DiscoverOpenApiInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let mut service = match DiscoveryService::new() {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Failed to create discovery service: {}", e)),
        };

        if input.use_llm
            && let Some(llm_config) = &self.llm_config
            && let Ok(llm) = LlmClient::with_config(llm_config.clone())
        {
            service = service.with_llm(llm);
        }

        let config = DiscoveryConfig {
            use_llm: input.use_llm && self.llm_config.is_some(),
            ..Default::default()
        };
        service = service.with_config(config);

        let result = match service.discover(&input.base_url).await {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Discovery failed: {}", e)),
        };

        let mut ingested_apis = Vec::new();
        if input.auto_ingest && self.neo4j.is_some() {
            let neo4j = self.neo4j.as_ref().unwrap();
            if let Err(e) = neo4j.init_schema().await {
                return ToolCallResult::error(format!("Failed to initialize schema: {}", e));
            }

            for candidate in &result.candidates {
                let mut parser = OpenApiParser::new(neo4j.clone());
                if let Some(llm_config) = &self.llm_config {
                    if let Ok(llm) = LlmClient::with_config(llm_config.clone()) {
                        parser = parser.with_llm(llm);
                    }
                }

                if let Ok(ingest_result) = parser.ingest(&candidate.url).await {
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
            }
        }

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
        let input: BuildOpenApiFromDocsInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let Some(llm_config) = &self.llm_config else {
            return ToolCallResult::error("LLM configuration required for documentation analysis.");
        };

        let llm = match LlmClient::with_config(llm_config.clone()) {
            Ok(l) => l,
            Err(e) => return ToolCallResult::error(format!("Failed to create LLM client: {}", e)),
        };

        let service = match DocGenService::new(llm) {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Failed to create doc generator: {}", e)),
        };

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

        let spec_output = match input.output_format.as_str() {
            "yaml" => result.spec.to_yaml().unwrap_or_else(|e| format!("YAML error: {}", e)),
            _ => result.spec.to_json().unwrap_or_else(|e| format!("JSON error: {}", e)),
        };

        let mut ingested = false;
        if input.auto_ingest && self.neo4j.is_some() {
            let neo4j = self.neo4j.as_ref().unwrap();
            if let Err(e) = neo4j.init_schema().await {
                return ToolCallResult::error(format!("Failed to initialize schema: {}", e));
            }

            let temp_path = format!("/tmp/generated_spec_{}.json", uuid::Uuid::new_v4());
            if let Ok(()) = std::fs::write(&temp_path, result.spec.to_json().unwrap_or_default()) {
                let mut parser = OpenApiParser::new(neo4j.clone());
                if let Some(llm_config) = &self.llm_config {
                    if let Ok(llm) = LlmClient::with_config(llm_config.clone()) {
                        parser = parser.with_llm(llm);
                    }
                }

                if let Ok(ingest_result) = parser.ingest(&temp_path).await {
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

    async fn handle_build_openapi_from_repo(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: BuildOpenApiFromRepoInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let Some(llm_config) = &self.llm_config else {
            return ToolCallResult::error("LLM configuration required for code analysis.");
        };

        let llm = match LlmClient::with_config(llm_config.clone()) {
            Ok(l) => l,
            Err(e) => return ToolCallResult::error(format!("Failed to create LLM client: {}", e)),
        };

        let mut service = match RepoAnalyzerService::new(llm) {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Failed to create repo analyzer: {}", e)),
        };

        if let Some(patterns) = &input.include_patterns {
            let mut config = crate::services::RepoAnalysisConfig::default();
            config.include_patterns = patterns.clone();
            service = service.with_config(config);
        }

        let token = self.get_repo_token(&input.repo_url).await;
        let merge_strategy = MergeStrategy::from_str(&input.merge_strategy);

        let result = match service
            .analyze(
                &input.repo_url,
                &input.api_title,
                &input.api_version,
                input.base_url.as_deref(),
                merge_strategy,
                token.as_deref(),
                input.subdirectory.as_deref(),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("Repository analysis failed: {}", e)),
        };

        let spec_output = match input.output_format.as_str() {
            "yaml" => result.spec.to_yaml().unwrap_or_else(|e| format!("YAML error: {}", e)),
            _ => result.spec.to_json().unwrap_or_else(|e| format!("JSON error: {}", e)),
        };

        let mut ingested = false;
        if input.auto_ingest && self.neo4j.is_some() {
            let neo4j = self.neo4j.as_ref().unwrap();
            if let Err(e) = neo4j.init_schema().await {
                return ToolCallResult::error(format!("Failed to initialize schema: {}", e));
            }

            let temp_path = format!("/tmp/generated_spec_{}.json", uuid::Uuid::new_v4());
            if let Ok(()) = std::fs::write(&temp_path, result.spec.to_json().unwrap_or_default()) {
                let mut parser = OpenApiParser::new(neo4j.clone());
                if let Some(llm_config) = &self.llm_config {
                    if let Ok(llm) = LlmClient::with_config(llm_config.clone()) {
                        parser = parser.with_llm(llm);
                    }
                }

                if let Ok(ingest_result) = parser.ingest(&temp_path).await {
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

        let mut response = json!({
            "success": true,
            "api_title": input.api_title,
            "api_version": input.api_version,
            "endpoints_found": result.endpoints_found,
            "schemas_found": result.schemas_found,
            "files_analyzed": result.analyzed_files.len(),
            "output_format": input.output_format,
            "spec": spec_output
        });

        if ingested {
            response["auto_ingested"] = json!(true);
            response["context_loaded"] = json!(true);
        }

        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
    }

    async fn handle_export_openapi(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ExportOpenApiInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let Some(neo4j) = &self.neo4j else {
            return ToolCallResult::error("Database connection not configured");
        };

        let options = ExportOptions {
            include_annotations: input.include_annotations,
            include_original_values: input.include_annotations,
            format: match input.format.as_str() {
                "json" => ExportFormat::Json,
                _ => ExportFormat::Yaml,
            },
            api_name: None,
            include_broken_endpoints: input.include_broken,
            include_verification_status: true,
        };

        let exporter = OpenApiExporter::new(neo4j.clone());
        match exporter.export(&options).await {
            Ok(result) => {
                if let Some(ref path) = input.output_path {
                    if let Err(e) = std::fs::write(path, &result.content) {
                        return ToolCallResult::error(format!("Failed to write file: {}", e));
                    }
                    let response = json!({
                        "success": true,
                        "output_path": path,
                        "format": input.format
                    });
                    ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
                } else {
                    let response = json!({
                        "success": true,
                        "format": input.format,
                        "spec": result.content
                    });
                    ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
                }
            }
            Err(e) => ToolCallResult::error(format!("Export failed: {}", e)),
        }
    }

    async fn handle_diff_api_spec(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: DiffApiSpecInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let Some(neo4j) = &self.neo4j else {
            return ToolCallResult::error("Database connection not configured");
        };

        let differ = SpecDiffer::new(neo4j.clone());
        match differ.generate_diff(None).await {
            Ok(mut report) => {
                if input.breaking_only {
                    report.changes.retain(|c| c.breaking);
                    report.summary.total_changes = report.changes.len();
                }

                let output = match input.format.as_str() {
                    "json" => match MarkdownReportGenerator::generate_json(&report) {
                        Ok(json) => json,
                        Err(e) => return ToolCallResult::error(format!("JSON generation failed: {}", e)),
                    },
                    "changelog" => MarkdownReportGenerator::generate_changelog(&report),
                    _ => MarkdownReportGenerator::generate(&report),
                };

                let response = json!({
                    "success": true,
                    "format": input.format,
                    "report": output
                });
                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Diff generation failed: {}", e)),
        }
    }

    async fn handle_configure_api_credential(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ConfigureApiCredentialInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let Some(credential_manager) = &self.credential_manager else {
            return ToolCallResult::error("Credential manager not configured.");
        };

        let Some(neo4j) = &self.neo4j else {
            return ToolCallResult::error("Database connection not configured");
        };

        let credential_type = match input.credential_type.to_lowercase().as_str() {
            "api_key" => CredentialType::ApiKey,
            "bearer" => CredentialType::Bearer,
            "basic" => CredentialType::Basic,
            "oauth2_client_credentials" => CredentialType::OAuth2ClientCredentials,
            _ => return ToolCallResult::error(format!("Invalid credential type: {}", input.credential_type)),
        };

        let inject_location = match input.inject_location.to_lowercase().as_str() {
            "header" => InjectLocation::Header,
            "query" => InjectLocation::Query,
            _ => return ToolCallResult::error(format!("Invalid inject location: {}", input.inject_location)),
        };

        let secret_ref = format!(
            "{}/{}",
            input.api_name.to_lowercase().replace(' ', "-"),
            input.credential_type.to_lowercase()
        );

        let mut credential = ApiCredential::new(
            input.api_name.clone(),
            credential_type,
            inject_location,
            input.inject_key.clone(),
            &secret_ref,
        );

        if let Some(desc) = input.description {
            credential = credential.with_description(desc);
        }

        if let Err(e) = neo4j.create_api_credential(&credential).await {
            return ToolCallResult::error(format!("Failed to store credential metadata: {}", e));
        }

        if let Err(e) = credential_manager
            .store_secret(&credential.secret_ref, &input.secret_value)
            .await
        {
            let _ = neo4j.delete_api_credential(&input.api_name).await;
            return ToolCallResult::error(format!("Failed to store secret: {}", e));
        }

        let response = json!({
            "success": true,
            "api_name": credential.api_name,
            "message": format!("Credential configured for API '{}'", input.api_name)
        });

        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
    }

    async fn handle_list_api_credentials(&self) -> ToolCallResult {
        let Some(neo4j) = &self.neo4j else {
            return ToolCallResult::error("Database connection not configured");
        };

        match neo4j.list_api_credentials().await {
            Ok(credentials) => {
                if credentials.is_empty() {
                    return ToolCallResult::success_text("No API credentials configured.");
                }

                let credential_list: Vec<Value> = credentials
                    .iter()
                    .map(|c| {
                        json!({
                            "api_name": c.api_name,
                            "credential_type": c.credential_type.to_string(),
                            "inject_location": c.inject_location.to_string(),
                            "inject_key": c.inject_key,
                            "active": c.active,
                        })
                    })
                    .collect();

                let response = json!({
                    "count": credential_list.len(),
                    "credentials": credential_list
                });

                ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
            }
            Err(e) => ToolCallResult::error(format!("Failed to list credentials: {}", e)),
        }
    }

    async fn handle_delete_api_credential(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: DeleteApiCredentialInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        let Some(credential_manager) = &self.credential_manager else {
            return ToolCallResult::error("Credential manager not configured.");
        };

        let Some(neo4j) = &self.neo4j else {
            return ToolCallResult::error("Database connection not configured");
        };

        let credential = match neo4j.get_api_credential(&input.api_name).await {
            Ok(c) => c,
            Err(e) => return ToolCallResult::error(format!("Credential not found: {}", e)),
        };

        if let Err(e) = credential_manager.delete_secret(&credential.secret_ref).await {
            debug!(error = %e, "Failed to delete secret, continuing with metadata deletion");
        }

        if let Err(e) = neo4j.delete_api_credential(&input.api_name).await {
            return ToolCallResult::error(format!("Failed to delete credential metadata: {}", e));
        }

        let response = json!({
            "success": true,
            "api_name": input.api_name,
            "message": format!("Credential deleted for API '{}'", input.api_name)
        });

        ToolCallResult::success_text(serde_json::to_string_pretty(&response).unwrap())
    }

    // ========================================================================
    // Helpers
    // ========================================================================

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
                method: ep.endpoint.method,
                path: ep.endpoint.path.clone(),
                summary: ep.endpoint.summary.clone(),
                operation_id: ep.endpoint.operation_id.clone(),
                parameters: param_summary,
            });
        }

        context
    }

    fn format_context(&self, ctx: &ApiContext, format: &str) -> ToolCallResult {
        match format {
            "compact" => ToolCallResult::success_text(ctx.to_compact_summary()),
            "detailed" => {
                let json = self.context_to_json(ctx, true);
                ToolCallResult::success_text(serde_json::to_string_pretty(&json).unwrap())
            }
            _ => {
                let json = self.context_to_json(ctx, false);
                ToolCallResult::success_text(serde_json::to_string_pretty(&json).unwrap())
            }
        }
    }

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

    async fn get_repo_token(&self, repo_url: &str) -> Option<String> {
        let credential_manager = self.credential_manager.as_ref()?;
        let platform = if repo_url.contains("github") {
            "GitHub"
        } else if repo_url.contains("gitlab") {
            "GitLab"
        } else {
            return None;
        };

        match credential_manager.get_credential(platform).await {
            Ok(_cred) => {
                debug!(platform = %platform, "Found credential for repository access");
                None 
            }
            Err(_) => {
                debug!(platform = %platform, "No credential configured for repository");
                None
            }
        }
    }
}

#[async_trait]
impl Skill for ApiSkill {
    fn name(&self) -> &str {
        "API Expert"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::ingest_openapi_def(),
            Self::query_endpoint_def(),
            Self::execute_request_def(),
            Self::get_api_context_def(),
            Self::list_loaded_apis_def(),
            Self::clear_api_context_def(),
            Self::discover_openapi_def(),
            Self::build_openapi_from_docs_def(),
            Self::build_openapi_from_repo_def(),
            Self::export_openapi_def(),
            Self::diff_api_spec_def(),
            Self::configure_api_credential_def(),
            Self::list_api_credentials_def(),
            Self::delete_api_credential_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "ingest_openapi" => Some(self.handle_ingest_openapi(arguments).await),
            "graph_query_endpoint" => Some(self.handle_query_endpoint(arguments).await),
            "execute_http_request" => Some(self.handle_execute_request(arguments).await),
            "get_api_context" => Some(self.handle_get_api_context(arguments).await),
            "list_loaded_apis" => Some(self.handle_list_loaded_apis().await),
            "clear_api_context" => Some(self.handle_clear_api_context(arguments).await),
            "discover_openapi" => Some(self.handle_discover_openapi(arguments).await),
            "build_openapi_from_docs" => Some(self.handle_build_openapi_from_docs(arguments).await),
            "build_openapi_from_repo" => Some(self.handle_build_openapi_from_repo(arguments).await),
            "export_openapi" => Some(self.handle_export_openapi(arguments).await),
            "diff_api_spec" => Some(self.handle_diff_api_spec(arguments).await),
            "configure_api_credential" => Some(self.handle_configure_api_credential(arguments).await),
            "list_api_credentials" => Some(self.handle_list_api_credentials().await),
            "delete_api_credential" => Some(self.handle_delete_api_credential(arguments).await),
            _ => None,
        }
    }
}

// Input structs
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
    #[serde(default)]
    pub query: Option<String>,
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

#[derive(Debug, Deserialize)]
pub struct BuildOpenApiFromRepoInput {
    pub repo_url: String,
    pub api_title: String,
    #[serde(default = "default_api_version")]
    pub api_version: String,
    pub base_url: Option<String>,
    pub ref_name: Option<String>,
    pub subdirectory: Option<String>,
    pub include_patterns: Option<Vec<String>>,
    #[serde(default = "default_merge_strategy")]
    pub merge_strategy: String,
    #[serde(default = "default_output_format")]
    pub output_format: String,
    #[serde(default)]
    pub auto_ingest: bool,
}

fn default_merge_strategy() -> String {
    "enhance".to_string()
}

fn default_api_version() -> String {
    "1.0.0".to_string()
}

fn default_output_format() -> String {
    "json".to_string()
}

#[derive(Debug, Deserialize)]
pub struct ExportOpenApiInput {
    #[serde(default = "default_yaml_format")]
    pub format: String,
    #[serde(default = "default_true")]
    pub include_annotations: bool,
    #[serde(default)]
    pub include_broken: bool,
    pub output_path: Option<String>,
}

fn default_yaml_format() -> String {
    "yaml".to_string()
}

#[derive(Debug, Deserialize)]
pub struct DiffApiSpecInput {
    #[serde(default = "default_markdown_format")]
    pub format: String,
    #[serde(default)]
    pub breaking_only: bool,
}

fn default_markdown_format() -> String {
    "markdown".to_string()
}

#[derive(Debug, Deserialize)]
pub struct ConfigureApiCredentialInput {
    pub api_name: String,
    pub credential_type: String,
    pub inject_location: String,
    pub inject_key: String,
    pub secret_value: String,
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct DeleteApiCredentialInput {
    pub api_name: String,
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
