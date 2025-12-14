//! Documentation-to-OpenAPI generation service.
//!
//! This module provides functionality to analyze API documentation (HTML, markdown, text)
//! and generate OpenAPI specifications using LLM-assisted extraction.

use std::collections::HashMap;
use std::time::Duration;

use reqwest::Client;
use scraper::{Html, Selector};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tracing::{debug, info, warn};

use super::llm::{ChatMessage, LlmClient};

// ============================================================================
// Types
// ============================================================================

/// Result of generating an OpenAPI spec from documentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DocGenResult {
    /// The generated OpenAPI specification
    pub spec: OpenApiSpec,
    /// Source URLs that were analyzed
    pub sources: Vec<String>,
    /// Endpoints extracted from documentation
    pub endpoints_found: usize,
    /// Schemas extracted from documentation
    pub schemas_found: usize,
    /// Any warnings during generation
    pub warnings: Vec<String>,
}

/// A simplified OpenAPI specification structure.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenApiSpec {
    pub openapi: String,
    pub info: ApiInfo,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub servers: Vec<Server>,
    pub paths: HashMap<String, PathItem>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub components: Option<Components>,
}

impl OpenApiSpec {
    /// Create a new empty OpenAPI spec.
    pub fn new(title: &str, version: &str) -> Self {
        Self {
            openapi: "3.0.3".to_string(),
            info: ApiInfo {
                title: title.to_string(),
                version: version.to_string(),
                description: None,
            },
            servers: Vec::new(),
            paths: HashMap::new(),
            components: None,
        }
    }

    /// Add a server URL.
    pub fn add_server(&mut self, url: &str, description: Option<&str>) {
        self.servers.push(Server {
            url: url.to_string(),
            description: description.map(|s| s.to_string()),
        });
    }

    /// Add an endpoint.
    pub fn add_endpoint(&mut self, path: &str, method: &str, operation: Operation) {
        let path_item = self.paths.entry(path.to_string()).or_default();
        match method.to_lowercase().as_str() {
            "get" => path_item.get = Some(operation),
            "post" => path_item.post = Some(operation),
            "put" => path_item.put = Some(operation),
            "patch" => path_item.patch = Some(operation),
            "delete" => path_item.delete = Some(operation),
            "head" => path_item.head = Some(operation),
            "options" => path_item.options = Some(operation),
            _ => {}
        }
    }

    /// Convert to JSON string.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// Convert to YAML string.
    pub fn to_yaml(&self) -> Result<String, serde_yaml::Error> {
        serde_yaml::to_string(self)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiInfo {
    pub title: String,
    pub version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Server {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PathItem {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub get: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub post: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub put: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delete: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub head: Option<Operation>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<Operation>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Operation {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub parameters: Vec<Parameter>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub request_body: Option<RequestBody>,
    pub responses: HashMap<String, Response>,
}

impl Operation {
    pub fn new(summary: &str) -> Self {
        let mut responses = HashMap::new();
        responses.insert(
            "200".to_string(),
            Response {
                description: "Successful response".to_string(),
                content: None,
            },
        );

        Self {
            summary: Some(summary.to_string()),
            description: None,
            operation_id: None,
            tags: Vec::new(),
            parameters: Vec::new(),
            request_body: None,
            responses,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Parameter {
    pub name: String,
    #[serde(rename = "in")]
    pub location: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub required: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RequestBody {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub content: HashMap<String, MediaType>,
    #[serde(default)]
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<HashMap<String, MediaType>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MediaType {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub schema: Option<SchemaRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SchemaRef {
    Ref {
        #[serde(rename = "$ref")]
        reference: String,
    },
    Inline(SchemaObject),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaObject {
    #[serde(rename = "type", skip_serializing_if = "Option::is_none")]
    pub schema_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub properties: Option<HashMap<String, SchemaObject>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub items: Option<Box<SchemaObject>>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub required: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Components {
    #[serde(skip_serializing_if = "HashMap::is_empty", default)]
    pub schemas: HashMap<String, SchemaObject>,
}

/// Extracted endpoint information from documentation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedEndpoint {
    pub method: String,
    pub path: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub parameters: Vec<ExtractedParameter>,
    pub request_body: Option<ExtractedRequestBody>,
    pub response: Option<ExtractedResponse>,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedParameter {
    pub name: String,
    pub location: String, // path, query, header, cookie
    pub param_type: Option<String>,
    pub description: Option<String>,
    pub required: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedRequestBody {
    pub content_type: String,
    pub description: Option<String>,
    pub schema: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractedResponse {
    pub status_code: String,
    pub description: Option<String>,
    pub content_type: Option<String>,
    pub schema: Option<Value>,
}

/// Configuration for the doc generator.
#[derive(Debug, Clone)]
pub struct DocGenConfig {
    /// Maximum pages to crawl
    pub max_pages: usize,
    /// Request timeout
    pub request_timeout: Duration,
    /// Whether to follow links to subpages
    pub follow_links: bool,
    /// Maximum depth for link following
    pub max_depth: usize,
}

impl Default for DocGenConfig {
    fn default() -> Self {
        Self {
            max_pages: 10,
            request_timeout: Duration::from_secs(30),
            follow_links: true,
            max_depth: 2,
        }
    }
}

// ============================================================================
// Doc Generator Service
// ============================================================================

/// Service for generating OpenAPI specs from documentation.
pub struct DocGenService {
    client: Client,
    llm: LlmClient,
    config: DocGenConfig,
}

impl DocGenService {
    /// Create a new doc generator service.
    pub fn new(llm: LlmClient) -> Result<Self, DocGenError> {
        let client = Client::builder()
            .timeout(Duration::from_secs(30))
            .user_agent("agent-api/0.1.0 (OpenAPI Generator)")
            .redirect(reqwest::redirect::Policy::limited(5))
            .build()
            .map_err(|e| DocGenError::HttpClient(e.to_string()))?;

        Ok(Self {
            client,
            llm,
            config: DocGenConfig::default(),
        })
    }

    /// Set the configuration.
    pub fn with_config(mut self, config: DocGenConfig) -> Self {
        self.config = config;
        self
    }

    /// Generate an OpenAPI spec from documentation URLs.
    pub async fn generate(
        &self,
        doc_urls: &[String],
        api_title: &str,
        api_version: &str,
        base_url: Option<&str>,
    ) -> Result<DocGenResult, DocGenError> {
        info!(urls = ?doc_urls, title = %api_title, "Starting OpenAPI generation from docs");

        let mut all_content = Vec::new();
        let mut sources = Vec::new();
        let mut warnings = Vec::new();

        // Fetch and extract content from all URLs
        for url in doc_urls {
            match self.fetch_doc_content(url).await {
                Ok(content) => {
                    all_content.push((url.clone(), content));
                    sources.push(url.clone());
                }
                Err(e) => {
                    warn!(url = %url, error = %e, "Failed to fetch documentation");
                    warnings.push(format!("Failed to fetch {}: {}", url, e));
                }
            }
        }

        if all_content.is_empty() {
            return Err(DocGenError::NoContent(
                "No documentation content could be fetched".to_string(),
            ));
        }

        // Use LLM to extract API endpoints from the content
        let extracted = self.extract_endpoints_with_llm(&all_content).await?;

        // Build OpenAPI spec from extracted data
        let mut spec = OpenApiSpec::new(api_title, api_version);

        if let Some(base) = base_url {
            spec.add_server(base, Some("API Server"));
        }

        // Convert extracted endpoints to OpenAPI format
        for endpoint in &extracted {
            let operation = self.build_operation(endpoint);
            spec.add_endpoint(&endpoint.path, &endpoint.method, operation);
        }

        // Extract schemas if present
        let schemas = self
            .extract_schemas_with_llm(&all_content)
            .await
            .unwrap_or_default();
        let schemas_count = schemas.len();
        if !schemas.is_empty() {
            spec.components = Some(Components { schemas });
        }

        let result = DocGenResult {
            spec,
            sources,
            endpoints_found: extracted.len(),
            schemas_found: schemas_count,
            warnings,
        };

        info!(
            endpoints = result.endpoints_found,
            schemas = result.schemas_found,
            "OpenAPI generation complete"
        );

        Ok(result)
    }

    /// Fetch and extract text content from a documentation URL.
    async fn fetch_doc_content(&self, url: &str) -> Result<String, DocGenError> {
        debug!(url = %url, "Fetching documentation");

        let response = self
            .client
            .get(url)
            .header("Accept", "text/html, text/plain, text/markdown")
            .send()
            .await
            .map_err(|e| DocGenError::HttpClient(e.to_string()))?;

        if !response.status().is_success() {
            return Err(DocGenError::HttpClient(format!(
                "HTTP {}: {}",
                response.status(),
                url
            )));
        }

        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|ct| ct.to_str().ok())
            .unwrap_or("")
            .to_string();

        let body = response
            .text()
            .await
            .map_err(|e| DocGenError::HttpClient(e.to_string()))?;

        // Extract text based on content type
        let text = if content_type.contains("html") {
            self.extract_text_from_html(&body)
        } else {
            // Plain text or markdown - use as-is
            body
        };

        Ok(text)
    }

    /// Extract readable text from HTML.
    fn extract_text_from_html(&self, html: &str) -> String {
        let document = Html::parse_document(html);
        let mut text_parts = Vec::new();

        // Extract from main content areas
        let selectors = [
            "main",
            "article",
            ".content",
            ".documentation",
            ".api-docs",
            "#content",
            "body",
        ];

        for selector_str in selectors {
            if let Ok(selector) = Selector::parse(selector_str) {
                for element in document.select(&selector) {
                    let text: String = element.text().collect::<Vec<_>>().join(" ");
                    if !text.trim().is_empty() {
                        text_parts.push(text);
                        break; // Use first matching content area
                    }
                }
                if !text_parts.is_empty() {
                    break;
                }
            }
        }

        // Also look for code blocks which often contain API examples
        if let Ok(code_selector) = Selector::parse("pre, code") {
            for element in document.select(&code_selector) {
                let code: String = element.text().collect();
                if code.contains("GET ")
                    || code.contains("POST ")
                    || code.contains("PUT ")
                    || code.contains("DELETE ")
                    || code.contains("/api/")
                    || code.contains("curl ")
                {
                    text_parts.push(format!("\n```\n{}\n```\n", code));
                }
            }
        }

        let result = text_parts.join("\n\n");

        // Truncate if too long (LLM context limits)
        if result.len() > 15000 {
            result.chars().take(15000).collect()
        } else {
            result
        }
    }

    /// Use LLM to extract API endpoints from documentation content.
    async fn extract_endpoints_with_llm(
        &self,
        content: &[(String, String)],
    ) -> Result<Vec<ExtractedEndpoint>, DocGenError> {
        let combined_content: String = content
            .iter()
            .map(|(url, text)| format!("--- Source: {} ---\n{}\n", url, text))
            .collect::<Vec<_>>()
            .join("\n\n");

        let prompt = format!(
            r#"Analyze the following API documentation and extract all API endpoints.

For each endpoint, provide:
- HTTP method (GET, POST, PUT, PATCH, DELETE)
- URL path (e.g., /users/{{id}})
- Summary (brief description)
- Parameters (path, query, header params with name, type, required)
- Request body (if applicable, with content type and structure)
- Response (status code, description, structure)
- Tags/categories

Documentation:
{}

Return a JSON array of endpoints in this exact format:
```json
[
  {{
    "method": "GET",
    "path": "/users/{{id}}",
    "summary": "Get user by ID",
    "description": "Retrieves a user by their unique identifier",
    "parameters": [
      {{"name": "id", "location": "path", "param_type": "string", "description": "User ID", "required": true}}
    ],
    "request_body": null,
    "response": {{"status_code": "200", "description": "User object", "content_type": "application/json", "schema": null}},
    "tags": ["Users"]
  }}
]
```

Extract ALL endpoints you can find. If you're unsure about details, make reasonable assumptions based on REST conventions.
Return ONLY the JSON array, no other text."#,
            combined_content
        );

        let response = self
            .llm
            .chat(&[ChatMessage::user(&prompt)])
            .await
            .map_err(|e| DocGenError::LlmError(e.to_string()))?;

        self.parse_endpoints_response(&response.text)
    }

    /// Parse the LLM response to extract endpoints.
    fn parse_endpoints_response(
        &self,
        response: &str,
    ) -> Result<Vec<ExtractedEndpoint>, DocGenError> {
        // Try to extract JSON from the response
        let json_str = if let Some(start) = response.find('[') {
            if let Some(end) = response.rfind(']') {
                &response[start..=end]
            } else {
                response
            }
        } else {
            response
        };

        // Clean up common issues
        let cleaned = json_str
            .replace("```json", "")
            .replace("```", "")
            .trim()
            .to_string();

        serde_json::from_str(&cleaned).map_err(|e| {
            debug!(response = %response, "Failed to parse LLM response");
            DocGenError::ParseError(format!("Failed to parse endpoints: {}", e))
        })
    }

    /// Use LLM to extract schema definitions from documentation.
    async fn extract_schemas_with_llm(
        &self,
        content: &[(String, String)],
    ) -> Result<HashMap<String, SchemaObject>, DocGenError> {
        let combined_content: String = content
            .iter()
            .map(|(url, text)| format!("--- Source: {} ---\n{}\n", url, text))
            .collect::<Vec<_>>()
            .join("\n\n");

        let prompt = format!(
            r#"Analyze the following API documentation and extract all data models/schemas.

For each schema, provide:
- Name (e.g., "User", "Product")
- Type (object, array, etc.)
- Properties with their types and descriptions
- Required fields

Documentation:
{}

Return a JSON object mapping schema names to their definitions:
```json
{{
  "User": {{
    "schema_type": "object",
    "description": "A user account",
    "properties": {{
      "id": {{"schema_type": "string", "description": "Unique identifier"}},
      "name": {{"schema_type": "string", "description": "User's full name"}},
      "email": {{"schema_type": "string", "format": "email", "description": "Email address"}}
    }},
    "required": ["id", "name", "email"]
  }}
}}
```

Extract ALL schemas/models you can find. Return ONLY the JSON object, no other text."#,
            combined_content
        );

        let response = self
            .llm
            .chat(&[ChatMessage::user(&prompt)])
            .await
            .map_err(|e| DocGenError::LlmError(e.to_string()))?;

        self.parse_schemas_response(&response.text)
    }

    /// Parse the LLM response to extract schemas.
    fn parse_schemas_response(
        &self,
        response: &str,
    ) -> Result<HashMap<String, SchemaObject>, DocGenError> {
        // Try to extract JSON from the response
        let json_str = if let Some(start) = response.find('{') {
            if let Some(end) = response.rfind('}') {
                &response[start..=end]
            } else {
                response
            }
        } else {
            response
        };

        let cleaned = json_str
            .replace("```json", "")
            .replace("```", "")
            .trim()
            .to_string();

        serde_json::from_str(&cleaned).map_err(|e| {
            debug!(response = %response, "Failed to parse schemas response");
            DocGenError::ParseError(format!("Failed to parse schemas: {}", e))
        })
    }

    /// Build an Operation from extracted endpoint data.
    fn build_operation(&self, endpoint: &ExtractedEndpoint) -> Operation {
        let mut operation = Operation::new(
            endpoint
                .summary
                .as_deref()
                .unwrap_or(&format!("{} {}", endpoint.method, endpoint.path)),
        );

        operation.description = endpoint.description.clone();
        operation.tags = endpoint.tags.clone();

        // Generate operation ID
        let path_parts: Vec<&str> = endpoint
            .path
            .split('/')
            .filter(|s| !s.is_empty() && !s.starts_with('{'))
            .collect();
        let method_lower = endpoint.method.to_lowercase();
        operation.operation_id = Some(format!(
            "{}{}",
            method_lower,
            path_parts
                .iter()
                .map(|s| capitalize_first(s))
                .collect::<String>()
        ));

        // Add parameters
        for param in &endpoint.parameters {
            operation.parameters.push(Parameter {
                name: param.name.clone(),
                location: param.location.clone(),
                description: param.description.clone(),
                required: param.required,
                schema: Some(SchemaRef::Inline(SchemaObject {
                    schema_type: param.param_type.clone(),
                    format: None,
                    description: None,
                    properties: None,
                    items: None,
                    required: Vec::new(),
                })),
            });
        }

        // Add request body
        if let Some(body) = &endpoint.request_body {
            let mut content = HashMap::new();
            content.insert(
                body.content_type.clone(),
                MediaType {
                    schema: body.schema.as_ref().map(|_| {
                        SchemaRef::Inline(SchemaObject {
                            schema_type: Some("object".to_string()),
                            format: None,
                            description: body.description.clone(),
                            properties: None,
                            items: None,
                            required: Vec::new(),
                        })
                    }),
                },
            );
            operation.request_body = Some(RequestBody {
                description: body.description.clone(),
                content,
                required: true,
            });
        }

        // Add response
        if let Some(resp) = &endpoint.response {
            let mut response = Response {
                description: resp
                    .description
                    .clone()
                    .unwrap_or_else(|| "Success".to_string()),
                content: None,
            };

            if let Some(ct) = &resp.content_type {
                let mut content = HashMap::new();
                content.insert(
                    ct.clone(),
                    MediaType {
                        schema: Some(SchemaRef::Inline(SchemaObject {
                            schema_type: Some("object".to_string()),
                            format: None,
                            description: None,
                            properties: None,
                            items: None,
                            required: Vec::new(),
                        })),
                    },
                );
                response.content = Some(content);
            }

            operation
                .responses
                .insert(resp.status_code.clone(), response);
        }

        operation
    }
}

/// Capitalize the first letter of a string.
fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        None => String::new(),
        Some(first) => first.to_uppercase().chain(chars).collect(),
    }
}

// ============================================================================
// Errors
// ============================================================================

/// Errors that can occur during doc generation.
#[derive(Debug, thiserror::Error)]
pub enum DocGenError {
    #[error("HTTP client error: {0}")]
    HttpClient(String),

    #[error("LLM error: {0}")]
    LlmError(String),

    #[error("Parse error: {0}")]
    ParseError(String),

    #[error("No content: {0}")]
    NoContent(String),

    #[error("Invalid configuration: {0}")]
    InvalidConfig(String),
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_openapi_spec_creation() {
        let spec = OpenApiSpec::new("Test API", "1.0.0");
        assert_eq!(spec.openapi, "3.0.3");
        assert_eq!(spec.info.title, "Test API");
        assert_eq!(spec.info.version, "1.0.0");
    }

    #[test]
    fn test_add_server() {
        let mut spec = OpenApiSpec::new("Test", "1.0");
        spec.add_server("https://api.example.com", Some("Production"));
        assert_eq!(spec.servers.len(), 1);
        assert_eq!(spec.servers[0].url, "https://api.example.com");
    }

    #[test]
    fn test_add_endpoint() {
        let mut spec = OpenApiSpec::new("Test", "1.0");
        let op = Operation::new("Get users");
        spec.add_endpoint("/users", "GET", op);

        assert!(spec.paths.contains_key("/users"));
        assert!(spec.paths["/users"].get.is_some());
    }

    #[test]
    fn test_spec_to_json() {
        let mut spec = OpenApiSpec::new("Test API", "1.0.0");
        spec.add_server("https://api.example.com", None);

        let json = spec.to_json().unwrap();
        assert!(json.contains("\"openapi\": \"3.0.3\""));
        assert!(json.contains("\"title\": \"Test API\""));
    }

    #[test]
    fn test_spec_to_yaml() {
        let spec = OpenApiSpec::new("Test API", "1.0.0");
        let yaml = spec.to_yaml().unwrap();
        assert!(yaml.contains("openapi: 3.0.3"));
        assert!(yaml.contains("title: Test API"));
    }

    #[test]
    fn test_operation_creation() {
        let op = Operation::new("Test operation");
        assert_eq!(op.summary, Some("Test operation".to_string()));
        assert!(op.responses.contains_key("200"));
    }

    #[test]
    fn test_docgen_config_default() {
        let config = DocGenConfig::default();
        assert_eq!(config.max_pages, 10);
        assert!(config.follow_links);
        assert_eq!(config.max_depth, 2);
    }

    #[test]
    fn test_capitalize_first() {
        assert_eq!(capitalize_first("hello"), "Hello");
        assert_eq!(capitalize_first("WORLD"), "WORLD");
        assert_eq!(capitalize_first(""), "");
        assert_eq!(capitalize_first("a"), "A");
    }

    #[test]
    fn test_extracted_endpoint_serialization() {
        let endpoint = ExtractedEndpoint {
            method: "GET".to_string(),
            path: "/users".to_string(),
            summary: Some("List users".to_string()),
            description: None,
            parameters: vec![],
            request_body: None,
            response: None,
            tags: vec!["Users".to_string()],
        };

        let json = serde_json::to_string(&endpoint).unwrap();
        assert!(json.contains("GET"));
        assert!(json.contains("/users"));
    }
}
