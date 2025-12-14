//! OpenAPI specification exporter from the knowledge graph.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use thiserror::Error;
use tracing::{debug, info};

use crate::models::{
    Endpoint, EndpointStatus, HealingAction, HealingEvent, Parameter, ParameterLocation, Resource,
    Schema,
};
use crate::repository::Neo4jClient;

use super::builder::OpenApiBuilder;

/// Errors that can occur during export.
#[derive(Debug, Error)]
pub enum ExportError {
    #[error("Database error: {0}")]
    Database(String),

    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("API not found: {0}")]
    ApiNotFound(String),

    #[error("Invalid graph state: {0}")]
    InvalidState(String),

    #[error("Repository error: {0}")]
    Repository(#[from] crate::repository::RepositoryError),
}

/// Configuration for spec export.
#[derive(Debug, Clone)]
pub struct ExportOptions {
    /// Include x-healed-by-ai annotations on modified fields.
    pub include_annotations: bool,

    /// Include x-original-* values for healed fields.
    pub include_original_values: bool,

    /// Output format (YAML preferred for git).
    pub format: ExportFormat,

    /// Export specific API by name, or all if None.
    pub api_name: Option<String>,

    /// Include endpoints marked as 'broken'.
    pub include_broken_endpoints: bool,

    /// Add x-last-verified timestamp on verified endpoints.
    pub include_verification_status: bool,
}

impl Default for ExportOptions {
    fn default() -> Self {
        Self {
            include_annotations: true,
            include_original_values: true,
            format: ExportFormat::Yaml,
            api_name: None,
            include_broken_endpoints: false,
            include_verification_status: true,
        }
    }
}

/// Output format for the exported specification.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExportFormat {
    #[default]
    Yaml,
    Json,
}

/// Result of exporting the graph to OpenAPI.
#[derive(Debug)]
pub struct ExportResult {
    /// The raw OpenAPI spec as serde_json::Value.
    pub spec: serde_json::Value,

    /// Serialized content (YAML or JSON).
    pub content: String,

    /// Export statistics.
    pub stats: ExportStats,

    /// Warnings encountered during export.
    pub warnings: Vec<String>,
}

/// Statistics about the export operation.
#[derive(Debug, Default)]
pub struct ExportStats {
    pub resources_exported: usize,
    pub endpoints_exported: usize,
    pub parameters_exported: usize,
    pub schemas_exported: usize,
    pub healed_fields_annotated: usize,
    pub broken_endpoints_skipped: usize,
}

/// Healing metadata to attach as x-extensions.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealingAnnotation {
    pub healed_by_ai: bool,
    pub original_value: Option<serde_json::Value>,
    pub healing_reason: Option<String>,
    pub healed_at: Option<DateTime<Utc>>,
    pub confidence: Option<f32>,
}

/// Internal struct for endpoint with all related data.
#[derive(Debug, Clone)]
pub struct EndpointExportData {
    pub endpoint: Endpoint,
    pub resource_name: String,
    pub parameters: Vec<Parameter>,
    pub request_schema: Option<Schema>,
    pub response_schemas: HashMap<u16, Schema>,
    pub healing_events: Vec<HealingEvent>,
}

/// Main exporter service.
pub struct OpenApiExporter {
    neo4j: Neo4jClient,
}

impl OpenApiExporter {
    /// Create a new exporter with a Neo4j connection.
    pub fn new(neo4j: Neo4jClient) -> Self {
        Self { neo4j }
    }

    /// Export the knowledge graph to an OpenAPI specification.
    pub async fn export(&self, options: &ExportOptions) -> Result<ExportResult, ExportError> {
        let mut stats = ExportStats::default();
        let mut warnings = Vec::new();

        info!("Starting OpenAPI export");

        // Step 1: Fetch all resources
        let resources = self.fetch_resources().await?;
        debug!("Fetched {} resources", resources.len());

        // Step 2: Fetch all endpoints with their data
        let mut endpoint_data = Vec::new();
        for resource in &resources {
            let endpoints = self.fetch_endpoints_for_resource(resource, options).await?;
            for ep in endpoints {
                if ep.endpoint.status == EndpointStatus::Broken && !options.include_broken_endpoints
                {
                    stats.broken_endpoints_skipped += 1;
                    continue;
                }
                endpoint_data.push(ep);
            }
        }
        debug!("Fetched {} endpoints", endpoint_data.len());

        // Step 3: Fetch all schemas
        let schemas = self.fetch_schemas().await?;
        debug!("Fetched {} schemas", schemas.len());

        // Step 4: Build the OpenAPI spec using the builder
        let mut builder = OpenApiBuilder::new()
            .with_title("Exported API")
            .with_version("1.0.0")
            .with_description("Exported from API Knowledge Graph");

        // Add export metadata
        builder =
            builder.with_extension("x-exported-from", serde_json::json!("api-knowledge-graph"));
        builder =
            builder.with_extension("x-exported-at", serde_json::json!(Utc::now().to_rfc3339()));

        // Add schemas to components
        for schema in &schemas {
            stats.schemas_exported += 1;
            builder = builder.with_schema(&schema.name, schema.json_structure.clone());
        }

        // Group endpoints by path
        let mut paths_map: HashMap<String, Vec<&EndpointExportData>> = HashMap::new();
        for ep_data in &endpoint_data {
            paths_map
                .entry(ep_data.endpoint.path.clone())
                .or_default()
                .push(ep_data);
        }

        // Add paths and operations
        for (path, endpoints) in paths_map {
            for ep_data in endpoints {
                stats.endpoints_exported += 1;
                stats.resources_exported = resources.len();

                let operation = self.build_operation(ep_data, options, &mut stats, &mut warnings);
                builder = builder.with_operation(&path, ep_data.endpoint.method, operation);
            }
        }

        stats.resources_exported = resources.len();

        // Step 5: Build and serialize the spec
        let spec = builder.build();

        let content = match options.format {
            ExportFormat::Yaml => serde_yaml::to_string(&spec)
                .map_err(|e| ExportError::Serialization(e.to_string()))?,
            ExportFormat::Json => serde_json::to_string_pretty(&spec)
                .map_err(|e| ExportError::Serialization(e.to_string()))?,
        };

        info!(
            endpoints = stats.endpoints_exported,
            schemas = stats.schemas_exported,
            healed = stats.healed_fields_annotated,
            "Export complete"
        );

        Ok(ExportResult {
            spec,
            content,
            stats,
            warnings,
        })
    }

    /// Fetch all resources from the database.
    async fn fetch_resources(&self) -> Result<Vec<Resource>, ExportError> {
        self.neo4j.list_resources().await.map_err(ExportError::from)
    }

    /// Fetch all endpoints for a resource with their parameters and schemas.
    async fn fetch_endpoints_for_resource(
        &self,
        resource: &Resource,
        options: &ExportOptions,
    ) -> Result<Vec<EndpointExportData>, ExportError> {
        let endpoints = self.neo4j.get_endpoints_for_resource(resource.id).await?;
        let mut result = Vec::new();

        for endpoint in endpoints {
            // Fetch parameters
            let parameters = self.neo4j.get_parameters_for_endpoint(endpoint.id).await?;

            // Fetch request schema
            let request_schema = self.neo4j.get_request_schema(endpoint.id).await?;

            // Fetch response schemas (we only fetch 200 for now)
            let mut response_schemas = HashMap::new();
            if let Some(schema) = self.neo4j.get_response_schema(endpoint.id, 200).await? {
                response_schemas.insert(200, schema);
            }

            // Fetch healing events if annotations are enabled
            let healing_events = if options.include_annotations {
                self.neo4j
                    .get_healing_history(endpoint.id)
                    .await
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

            result.push(EndpointExportData {
                endpoint,
                resource_name: resource.name.clone(),
                parameters,
                request_schema,
                response_schemas,
                healing_events,
            });
        }

        Ok(result)
    }

    /// Fetch all schemas from the database.
    async fn fetch_schemas(&self) -> Result<Vec<Schema>, ExportError> {
        self.neo4j.list_schemas().await.map_err(ExportError::from)
    }

    /// Build an operation object from endpoint data.
    fn build_operation(
        &self,
        ep_data: &EndpointExportData,
        options: &ExportOptions,
        stats: &mut ExportStats,
        _warnings: &mut Vec<String>,
    ) -> serde_json::Value {
        let endpoint = &ep_data.endpoint;
        let mut operation = serde_json::json!({
            "summary": endpoint.summary,
            "tags": [ep_data.resource_name]
        });

        // Add operation ID if present
        if let Some(ref op_id) = endpoint.operation_id {
            operation["operationId"] = serde_json::json!(op_id);
        }

        // Add verification status extensions
        if options.include_verification_status {
            if let Some(status) = endpoint.last_verified_status {
                operation["x-last-verified-status"] = serde_json::json!(status);
            }
            operation["x-endpoint-status"] = serde_json::json!(format!("{:?}", endpoint.status));
        }

        // Mark if endpoint was healed
        if endpoint.healed_by_ai && options.include_annotations {
            operation["x-healed-by-ai"] = serde_json::json!(true);
        }

        // Build parameters array
        let mut params = Vec::new();
        for param in &ep_data.parameters {
            stats.parameters_exported += 1;
            let mut param_obj = self.build_parameter(param);

            // Check if this parameter was healed
            if options.include_annotations
                && let Some(annotation) =
                    self.find_parameter_healing(param, &ep_data.healing_events)
            {
                stats.healed_fields_annotated += 1;
                param_obj["x-healed-by-ai"] = serde_json::json!(true);

                if let Some(reason) = annotation.healing_reason {
                    param_obj["x-healing-reason"] = serde_json::json!(reason);
                }

                if options.include_original_values
                    && let Some(original) = annotation.original_value
                {
                    param_obj["x-original-value"] = original;
                }

                if let Some(healed_at) = annotation.healed_at {
                    param_obj["x-healed-at"] = serde_json::json!(healed_at.to_rfc3339());
                }
            }

            params.push(param_obj);
        }

        if !params.is_empty() {
            operation["parameters"] = serde_json::json!(params);
        }

        // Add request body if present
        if let Some(ref schema) = ep_data.request_schema {
            operation["requestBody"] = serde_json::json!({
                "required": true,
                "content": {
                    "application/json": {
                        "schema": {
                            "$ref": format!("#/components/schemas/{}", schema.name)
                        }
                    }
                }
            });
        }

        // Build responses
        let mut responses = serde_json::json!({});

        // Add response schemas
        for (status_code, schema) in &ep_data.response_schemas {
            responses[status_code.to_string()] = serde_json::json!({
                "description": format!("Response for status {}", status_code),
                "content": {
                    "application/json": {
                        "schema": {
                            "$ref": format!("#/components/schemas/{}", schema.name)
                        }
                    }
                }
            });
        }

        // Ensure at least a default response
        if ep_data.response_schemas.is_empty() {
            responses["200"] = serde_json::json!({
                "description": "Successful response"
            });
        }

        operation["responses"] = responses;

        operation
    }

    /// Build a parameter object from the Parameter model.
    fn build_parameter(&self, param: &Parameter) -> serde_json::Value {
        let location = match param.location {
            ParameterLocation::Query => "query",
            ParameterLocation::Path => "path",
            ParameterLocation::Header => "header",
            ParameterLocation::Body => "query", // Body params are handled separately
        };

        let mut param_obj = serde_json::json!({
            "name": param.name,
            "in": location,
            "required": param.required
        });

        // Add schema with type
        let param_type = param.param_type.as_deref().unwrap_or("string");
        param_obj["schema"] = serde_json::json!({
            "type": param_type
        });

        // Add description if present
        if let Some(ref desc) = param.description {
            param_obj["description"] = serde_json::json!(desc);
        }

        param_obj
    }

    /// Find healing annotation for a parameter from healing events.
    fn find_parameter_healing(
        &self,
        param: &Parameter,
        events: &[HealingEvent],
    ) -> Option<HealingAnnotation> {
        for event in events {
            match &event.action {
                HealingAction::RenameParameter {
                    new_name, old_name, ..
                } if new_name == &param.name => {
                    return Some(HealingAnnotation {
                        healed_by_ai: true,
                        original_value: Some(serde_json::json!({"name": old_name})),
                        healing_reason: Some(event.trigger_error.clone()),
                        healed_at: Some(event.timestamp),
                        confidence: None,
                    });
                }
                HealingAction::ChangeParameterType {
                    param_name,
                    old_type,
                    ..
                } if param_name == &param.name => {
                    return Some(HealingAnnotation {
                        healed_by_ai: true,
                        original_value: Some(serde_json::json!({"type": old_type})),
                        healing_reason: Some(event.trigger_error.clone()),
                        healed_at: Some(event.timestamp),
                        confidence: None,
                    });
                }
                HealingAction::AddMissingParameter { param_name, .. }
                    if param_name == &param.name =>
                {
                    return Some(HealingAnnotation {
                        healed_by_ai: true,
                        original_value: None, // Didn't exist before
                        healing_reason: Some(event.trigger_error.clone()),
                        healed_at: Some(event.timestamp),
                        confidence: None,
                    });
                }
                _ => continue,
            }
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_export_options_default() {
        let options = ExportOptions::default();
        assert!(options.include_annotations);
        assert!(options.include_original_values);
        assert_eq!(options.format, ExportFormat::Yaml);
        assert!(options.api_name.is_none());
        assert!(!options.include_broken_endpoints);
        assert!(options.include_verification_status);
    }

    #[test]
    fn test_export_format_default() {
        let format = ExportFormat::default();
        assert_eq!(format, ExportFormat::Yaml);
    }

    #[test]
    fn test_healing_annotation_serialization() {
        let annotation = HealingAnnotation {
            healed_by_ai: true,
            original_value: Some(serde_json::json!({"name": "old_param"})),
            healing_reason: Some("API returned 400".to_string()),
            healed_at: Some(Utc::now()),
            confidence: Some(0.95),
        };

        let json = serde_json::to_string(&annotation).unwrap();
        assert!(json.contains("healed_by_ai"));
        assert!(json.contains("old_param"));
    }
}
