use std::collections::HashMap;

use openapiv3::{
    OpenAPI, Operation, Parameter as OApiParameter, ParameterSchemaOrContent, PathItem,
    ReferenceOr, RequestBody, Response, Schema as OApiSchema, SchemaKind, StatusCode,
};
use thiserror::Error;
use tracing::{debug, info, warn};
use uuid::Uuid;

use crate::models::{Endpoint, HttpMethod, Parameter, ParameterLocation, Resource, Schema};
use crate::repository::Neo4jClient;

#[derive(Debug, Error)]
pub enum OpenApiError {
    #[error("Failed to read file: {0}")]
    FileRead(#[from] std::io::Error),

    #[error("Failed to fetch URL: {0}")]
    Fetch(#[from] reqwest::Error),

    #[error("Failed to parse OpenAPI spec: {0}")]
    Parse(String),

    #[error("Failed to parse JSON: {0}")]
    Json(#[from] serde_json::Error),

    #[error("Failed to parse YAML: {0}")]
    Yaml(#[from] serde_yaml::Error),

    #[error("Repository error: {0}")]
    Repository(#[from] crate::repository::RepositoryError),

    #[error("Unsupported OpenAPI version or format")]
    UnsupportedFormat,
}

/// Result of ingesting an OpenAPI specification.
#[derive(Debug, Clone, Default)]
pub struct IngestResult {
    pub resources_created: usize,
    pub endpoints_created: usize,
    pub schemas_created: usize,
    pub parameters_created: usize,
    pub api_title: String,
    pub api_version: String,
    pub description: Option<String>,
    /// Endpoints created during this ingestion (for context building)
    pub endpoints: Vec<EndpointWithParams>,
}

/// Endpoint with its parameters for context building.
#[derive(Debug, Clone)]
pub struct EndpointWithParams {
    pub endpoint: crate::models::Endpoint,
    pub parameters: Vec<crate::models::Parameter>,
}

/// Parser for OpenAPI specifications.
pub struct OpenApiParser {
    client: Neo4jClient,
    llm: Option<crate::services::LlmClient>,
    /// Cache of schema name -> Schema (with UUID) for linking
    schema_cache: HashMap<String, Schema>,
    /// Cache of resource name -> Resource (with UUID) for linking
    resource_cache: HashMap<String, Resource>,
}

impl OpenApiParser {
    pub fn new(client: Neo4jClient) -> Self {
        Self {
            client,
            llm: None,
            schema_cache: HashMap::new(),
            resource_cache: HashMap::new(),
        }
    }

    /// Set the LLM client for embedding generation.
    pub fn with_llm(mut self, llm: crate::services::LlmClient) -> Self {
        self.llm = Some(llm);
        self
    }

    /// Ingest an OpenAPI spec from a file path or URL.
    pub async fn ingest(&mut self, source: &str) -> Result<IngestResult, OpenApiError> {
        let spec = self.load_spec(source).await?;
        self.process_spec(spec).await
    }

    /// Load an OpenAPI spec from file or URL.
    async fn load_spec(&self, source: &str) -> Result<OpenAPI, OpenApiError> {
        let content = if source.starts_with("http://") || source.starts_with("https://") {
            info!(url = %source, "Fetching OpenAPI spec from URL");
            reqwest::get(source).await?.text().await?
        } else {
            info!(path = %source, "Reading OpenAPI spec from file");
            tokio::fs::read_to_string(source).await?
        };

        self.parse_spec(&content)
    }

    /// Parse OpenAPI spec content (JSON or YAML).
    fn parse_spec(&self, content: &str) -> Result<OpenAPI, OpenApiError> {
        // Try JSON first, then YAML
        if content.trim().starts_with('{') {
            serde_json::from_str(content).map_err(OpenApiError::Json)
        } else {
            serde_yaml::from_str(content).map_err(OpenApiError::Yaml)
        }
    }

    /// Process the parsed OpenAPI spec and ingest into Neo4j.
    async fn process_spec(&mut self, spec: OpenAPI) -> Result<IngestResult, OpenApiError> {
        let mut result = IngestResult {
            api_title: spec.info.title.clone(),
            api_version: spec.info.version.clone(),
            description: spec.info.description.clone(),
            ..Default::default()
        };

        info!(
            title = %result.api_title,
            version = %result.api_version,
            "Processing OpenAPI specification"
        );

        // First pass: Create all schemas from components
        if let Some(components) = &spec.components {
            for (name, schema_ref) in &components.schemas {
                if let ReferenceOr::Item(schema) = schema_ref {
                    self.create_schema(name, schema).await?;
                    result.schemas_created += 1;
                }
            }
        }

        // Second pass: Process all paths and operations
        for (path, path_item_ref) in &spec.paths.paths {
            if let ReferenceOr::Item(path_item) = path_item_ref {
                let (endpoint_count, param_count, endpoints) =
                    self.process_path(path, path_item).await?;
                result.endpoints_created += endpoint_count;
                result.parameters_created += param_count;
                result.endpoints.extend(endpoints);
            }
        }

        result.resources_created = self.resource_cache.len();

        info!(
            resources = result.resources_created,
            endpoints = result.endpoints_created,
            schemas = result.schemas_created,
            parameters = result.parameters_created,
            "Ingestion complete"
        );

        Ok(result)
    }

    /// Create a Schema node from an OpenAPI schema.
    async fn create_schema(
        &mut self,
        name: &str,
        schema: &OApiSchema,
    ) -> Result<Schema, OpenApiError> {
        if let Some(existing) = self.schema_cache.get(name) {
            return Ok(existing.clone());
        }

        let json_structure = serde_json::to_value(schema)
            .unwrap_or_else(|_| serde_json::json!({"error": "Failed to serialize schema"}));

        let schema_node = Schema::new(name, json_structure);

        debug!(name = %name, id = %schema_node.id, "Creating schema");
        self.client.create_schema(&schema_node).await?;

        self.schema_cache
            .insert(name.to_string(), schema_node.clone());
        Ok(schema_node)
    }

    /// Get or create a Resource from a tag name.
    async fn get_or_create_resource(&mut self, tag: &str) -> Result<Resource, OpenApiError> {
        if let Some(existing) = self.resource_cache.get(tag) {
            return Ok(existing.clone());
        }

        // Capitalize first letter for nicer display
        let name = capitalize_first(tag);
        let resource = Resource::new(&name, format!("{} API endpoints", name));

        debug!(name = %name, id = %resource.id, "Creating resource");
        self.client.create_resource(&resource).await?;

        self.resource_cache
            .insert(tag.to_string(), resource.clone());
        Ok(resource)
    }

    /// Process a path and all its operations.
    /// Returns (endpoints_created, parameters_created, endpoints_with_params).
    async fn process_path(
        &mut self,
        path: &str,
        path_item: &PathItem,
    ) -> Result<(usize, usize, Vec<EndpointWithParams>), OpenApiError> {
        let mut endpoints_created = 0;
        let mut parameters_created = 0;
        let mut endpoints = Vec::new();

        // Process each HTTP method
        let operations = [
            (HttpMethod::Get, &path_item.get),
            (HttpMethod::Post, &path_item.post),
            (HttpMethod::Put, &path_item.put),
            (HttpMethod::Patch, &path_item.patch),
            (HttpMethod::Delete, &path_item.delete),
            (HttpMethod::Head, &path_item.head),
            (HttpMethod::Options, &path_item.options),
        ];

        for (method, operation) in operations {
            if let Some(op) = operation {
                let (param_count, endpoint_with_params) =
                    self.process_operation(path, method, op).await?;
                endpoints_created += 1;
                parameters_created += param_count;
                endpoints.push(endpoint_with_params);
            }
        }

        Ok((endpoints_created, parameters_created, endpoints))
    }

    /// Process a single operation (endpoint).
    /// Returns (param_count, endpoint_with_params).
    async fn process_operation(
        &mut self,
        path: &str,
        method: HttpMethod,
        operation: &Operation,
    ) -> Result<(usize, EndpointWithParams), OpenApiError> {
        let summary = operation
            .summary
            .clone()
            .unwrap_or_else(|| format!("{} {}", method, path));
        let operation_id = operation.operation_id.clone();

        let mut endpoint = Endpoint::new(path, method, &summary, operation_id);

        // Generate embedding if LLM is available
        if let Some(llm) = &self.llm {
            let embedding_text = format!("{} {} - {}", method, path, summary);
            if let Ok(emb) = llm.embeddings(&embedding_text).await {
                endpoint.embedding = Some(emb);
            }
        }

        debug!(
            path = %path,
            method = %method,
            id = %endpoint.id,
            "Creating endpoint"
        );
        self.client.create_endpoint(&endpoint).await?;

        // Link to resources based on tags
        let tags = if operation.tags.is_empty() {
            vec!["default".to_string()]
        } else {
            operation.tags.clone()
        };

        for tag in &tags {
            let resource = self.get_or_create_resource(tag).await?;
            self.client
                .link_resource_to_endpoint(resource.id, endpoint.id)
                .await?;
        }

        // Process parameters and collect them
        let mut param_count = 0;
        let mut parameters = Vec::new();
        for param_ref in &operation.parameters {
            if let ReferenceOr::Item(param) = param_ref {
                if let Some(p) = self.process_parameter(&endpoint, param).await? {
                    parameters.push(p);
                }
                param_count += 1;
            }
        }

        // Process request body
        if let Some(ReferenceOr::Item(body)) = &operation.request_body {
            self.process_request_body(&endpoint, body).await?;
        }

        // Process responses
        for (status, response_ref) in &operation.responses.responses {
            if let ReferenceOr::Item(response) = response_ref {
                self.process_response(&endpoint, status, response).await?;
            }
        }

        Ok((
            param_count,
            EndpointWithParams {
                endpoint,
                parameters,
            },
        ))
    }

    /// Process a parameter and link it to the endpoint.
    /// Returns the created Parameter for context building.
    async fn process_parameter(
        &mut self,
        endpoint: &Endpoint,
        param: &OApiParameter,
    ) -> Result<Option<Parameter>, OpenApiError> {
        let param_data = match param {
            OApiParameter::Query { parameter_data, .. } => parameter_data,
            OApiParameter::Path { parameter_data, .. } => parameter_data,
            OApiParameter::Header { parameter_data, .. } => parameter_data,
            OApiParameter::Cookie { parameter_data, .. } => parameter_data,
        };

        let location = match param {
            OApiParameter::Query { .. } => ParameterLocation::Query,
            OApiParameter::Path { .. } => ParameterLocation::Path,
            OApiParameter::Header { .. } => ParameterLocation::Header,
            OApiParameter::Cookie { .. } => ParameterLocation::Query, // Map cookie to query for simplicity
        };

        let param_type = extract_parameter_type(&param_data.format);

        let mut parameter = Parameter::new(&param_data.name, location, param_data.required);

        if let Some(t) = param_type {
            parameter = parameter.with_type(t);
        }

        if let Some(desc) = &param_data.description {
            parameter = parameter.with_description(desc);
        }

        debug!(
            name = %parameter.name,
            location = %parameter.location,
            "Creating parameter"
        );
        self.client.create_parameter(&parameter).await?;
        self.client
            .link_endpoint_to_parameter(endpoint.id, parameter.id)
            .await?;

        Ok(Some(parameter))
    }

    /// Process a request body and link schema to endpoint.
    async fn process_request_body(
        &mut self,
        endpoint: &Endpoint,
        body: &RequestBody,
    ) -> Result<(), OpenApiError> {
        // Look for JSON content
        let Some(media_type) = body.content.get("application/json") else {
            return Ok(());
        };
        let Some(schema_ref) = &media_type.schema else {
            return Ok(());
        };
        if let Some(schema_id) = self.resolve_schema_ref(schema_ref).await? {
            self.client
                .link_endpoint_accepts_schema(endpoint.id, schema_id)
                .await?;
        }
        Ok(())
    }

    /// Process a response and link schema to endpoint.
    async fn process_response(
        &mut self,
        endpoint: &Endpoint,
        status: &StatusCode,
        response: &Response,
    ) -> Result<(), OpenApiError> {
        let status_code: u16 = match status {
            StatusCode::Code(code) => *code,
            StatusCode::Range(_) => return Ok(()), // Skip ranges for now
        };

        // Look for JSON content
        let Some(media_type) = response.content.get("application/json") else {
            return Ok(());
        };
        let Some(schema_ref) = &media_type.schema else {
            return Ok(());
        };
        if let Some(schema_id) = self.resolve_schema_ref(schema_ref).await? {
            self.client
                .link_endpoint_returns_schema(endpoint.id, schema_id, status_code)
                .await?;
        }
        Ok(())
    }

    /// Resolve a schema reference to a Schema UUID.
    async fn resolve_schema_ref(
        &mut self,
        schema_ref: &ReferenceOr<OApiSchema>,
    ) -> Result<Option<Uuid>, OpenApiError> {
        match schema_ref {
            ReferenceOr::Reference { reference } => {
                // Extract schema name from reference like "#/components/schemas/Pet"
                let name = reference
                    .strip_prefix("#/components/schemas/")
                    .unwrap_or(reference);

                if let Some(schema) = self.schema_cache.get(name) {
                    Ok(Some(schema.id))
                } else {
                    warn!(reference = %reference, "Schema reference not found in cache");
                    Ok(None)
                }
            }
            ReferenceOr::Item(schema) => {
                // Inline schema - create with generated name
                let name = format!("InlineSchema_{}", Uuid::new_v4().simple());
                let schema_node = self.create_schema(&name, schema).await?;
                Ok(Some(schema_node.id))
            }
        }
    }
}

/// Extract parameter type from schema format.
fn extract_parameter_type(format: &ParameterSchemaOrContent) -> Option<String> {
    match format {
        ParameterSchemaOrContent::Schema(schema_ref) => {
            if let ReferenceOr::Item(schema) = schema_ref {
                match &schema.schema_kind {
                    SchemaKind::Type(t) => Some(format!("{:?}", t).to_lowercase()),
                    _ => None,
                }
            } else {
                None
            }
        }
        ParameterSchemaOrContent::Content(_) => None,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_capitalize_first() {
        assert_eq!(capitalize_first("pets"), "Pets");
        assert_eq!(capitalize_first(""), "");
        assert_eq!(capitalize_first("a"), "A");
        assert_eq!(
            capitalize_first("already Capitalized"),
            "Already Capitalized"
        );
    }

    #[test]
    fn test_parse_json_spec() {
        let json = r#"{
            "openapi": "3.0.0",
            "info": {"title": "Test", "version": "1.0"},
            "paths": {}
        }"#;

        let result: Result<OpenAPI, _> = serde_json::from_str(json);
        assert!(result.is_ok());

        let spec = result.unwrap();
        assert_eq!(spec.info.title, "Test");
        assert_eq!(spec.info.version, "1.0");
    }

    #[test]
    fn test_parse_yaml_spec() {
        let yaml = r#"
openapi: "3.0.0"
info:
  title: Test
  version: "1.0"
paths: {}
"#;

        let result: Result<OpenAPI, _> = serde_yaml::from_str(yaml);
        assert!(result.is_ok());

        let spec = result.unwrap();
        assert_eq!(spec.info.title, "Test");
        assert_eq!(spec.info.version, "1.0");
    }

    #[test]
    fn test_parse_spec_with_paths() {
        let json = r#"{
            "openapi": "3.0.0",
            "info": {"title": "API", "version": "1.0"},
            "paths": {
                "/users": {
                    "get": {
                        "summary": "List users",
                        "operationId": "listUsers",
                        "tags": ["users"],
                        "responses": {
                            "200": {"description": "Success"}
                        }
                    }
                }
            }
        }"#;

        let spec: OpenAPI = serde_json::from_str(json).unwrap();
        assert_eq!(spec.paths.paths.len(), 1);
        assert!(spec.paths.paths.contains_key("/users"));
    }
}
