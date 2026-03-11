//! API context store for managing in-memory API context with DB fallback.
//!
//! This module provides a hybrid caching layer that keeps API summaries in memory
//! for fast access while supporting lazy-loading from Neo4j on cache misses.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, info, warn};

use crate::models::{HttpMethod, ParameterLocation};
use crate::repository::Neo4jClient;

// ============================================================================
// Context Models
// ============================================================================

/// Compact summary of an API endpoint for context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EndpointSummary {
    /// HTTP method (GET, POST, etc.)
    pub method: HttpMethod,
    /// URL path pattern (e.g., "/users/{id}")
    pub path: String,
    /// Brief description of the endpoint
    pub summary: String,
    /// Operation ID if available
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// Parameter names grouped by location
    pub parameters: ParameterSummary,
}

/// Compact parameter summary grouped by location.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ParameterSummary {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub path: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub query: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub header: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub body: Vec<String>,
}

impl ParameterSummary {
    /// Check if there are any parameters.
    pub fn is_empty(&self) -> bool {
        self.path.is_empty()
            && self.query.is_empty()
            && self.header.is_empty()
            && self.body.is_empty()
    }

    /// Get total parameter count.
    pub fn count(&self) -> usize {
        self.path.len() + self.query.len() + self.header.len() + self.body.len()
    }
}

/// Schema summary for context.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchemaSummary {
    /// Schema name
    pub name: String,
    /// List of field names (top-level only for brevity)
    pub fields: Vec<String>,
}

/// Complete API context containing everything needed for LLM to work with an API.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiContext {
    /// API name/title
    pub name: String,
    /// API version
    pub version: String,
    /// Base URL for the API (if known)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// API description
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Source URL or file path where spec was loaded from
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// List of endpoint summaries
    pub endpoints: Vec<EndpointSummary>,
    /// Schema summaries (optional, for detailed context)
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub schemas: Vec<SchemaSummary>,
    /// When this context was loaded
    pub loaded_at: DateTime<Utc>,
    /// Endpoint count for quick reference
    pub endpoint_count: usize,
    /// Schema count for quick reference
    pub schema_count: usize,
}

impl ApiContext {
    /// Create a new API context.
    pub fn new(name: String, version: String) -> Self {
        Self {
            name,
            version,
            base_url: None,
            description: None,
            source: None,
            endpoints: Vec::new(),
            schemas: Vec::new(),
            loaded_at: Utc::now(),
            endpoint_count: 0,
            schema_count: 0,
        }
    }

    /// Set the base URL.
    pub fn with_base_url(mut self, url: impl Into<String>) -> Self {
        self.base_url = Some(url.into());
        self
    }

    /// Set the description.
    pub fn with_description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }

    /// Set the source.
    pub fn with_source(mut self, source: impl Into<String>) -> Self {
        self.source = Some(source.into());
        self
    }

    /// Add an endpoint summary.
    pub fn add_endpoint(&mut self, endpoint: EndpointSummary) {
        self.endpoints.push(endpoint);
        self.endpoint_count = self.endpoints.len();
    }

    /// Add a schema summary.
    pub fn add_schema(&mut self, schema: SchemaSummary) {
        self.schemas.push(schema);
        self.schema_count = self.schemas.len();
    }

    /// Generate a compact text summary suitable for LLM context.
    pub fn to_compact_summary(&self) -> String {
        let mut summary = format!("API: {} v{}\n", self.name, self.version);

        if let Some(base) = &self.base_url {
            summary.push_str(&format!("Base URL: {}\n", base));
        }

        if let Some(desc) = &self.description {
            summary.push_str(&format!("Description: {}\n", desc));
        }

        summary.push_str(&format!("\nEndpoints ({}):\n", self.endpoint_count));

        for ep in &self.endpoints {
            summary.push_str(&format!("  {} {} - {}\n", ep.method, ep.path, ep.summary));

            if !ep.parameters.is_empty() {
                let mut params = Vec::new();
                if !ep.parameters.path.is_empty() {
                    params.push(format!("path: {}", ep.parameters.path.join(", ")));
                }
                if !ep.parameters.query.is_empty() {
                    params.push(format!("query: {}", ep.parameters.query.join(", ")));
                }
                if !ep.parameters.header.is_empty() {
                    params.push(format!("header: {}", ep.parameters.header.join(", ")));
                }
                if !ep.parameters.body.is_empty() {
                    params.push(format!("body: {}", ep.parameters.body.join(", ")));
                }
                summary.push_str(&format!("    Params: {}\n", params.join("; ")));
            }
        }

        if !self.schemas.is_empty() {
            summary.push_str(&format!("\nSchemas ({}):\n", self.schema_count));
            for schema in &self.schemas {
                summary.push_str(&format!(
                    "  {} - fields: {}\n",
                    schema.name,
                    schema.fields.join(", ")
                ));
            }
        }

        summary
    }
}

// ============================================================================
// Context Store
// ============================================================================

/// Thread-safe context store with in-memory caching and optional DB fallback.
#[derive(Clone)]
pub struct ContextStore {
    /// In-memory cache of API contexts, keyed by normalized API name
    contexts: Arc<RwLock<HashMap<String, ApiContext>>>,
    /// Optional Neo4j client for fallback loading
    neo4j: Option<Neo4jClient>,
}

impl ContextStore {
    /// Create a new context store without DB fallback.
    pub fn new() -> Self {
        Self {
            contexts: Arc::new(RwLock::new(HashMap::new())),
            neo4j: None,
        }
    }

    /// Create a context store with Neo4j fallback.
    pub fn with_neo4j(neo4j: Neo4jClient) -> Self {
        Self {
            contexts: Arc::new(RwLock::new(HashMap::new())),
            neo4j: Some(neo4j),
        }
    }

    /// Normalize an API name for consistent key lookup.
    fn normalize_name(name: &str) -> String {
        name.to_lowercase().replace([' ', '_'], "-")
    }

    /// Get a context from memory (fast path).
    pub async fn get(&self, api_name: &str) -> Option<ApiContext> {
        let key = Self::normalize_name(api_name);
        let contexts = self.contexts.read().await;
        contexts.get(&key).cloned()
    }

    /// Get a context, loading from DB if not in memory (slow path with caching).
    pub async fn get_or_load(&self, api_name: &str) -> Option<ApiContext> {
        let key = Self::normalize_name(api_name);

        // Fast path: check memory first
        {
            let contexts = self.contexts.read().await;
            if let Some(ctx) = contexts.get(&key) {
                debug!(api = %api_name, "Context found in memory");
                return Some(ctx.clone());
            }
        }

        // Slow path: try to load from Neo4j
        if let Some(neo4j) = &self.neo4j {
            debug!(api = %api_name, "Context not in memory, loading from Neo4j");
            if let Ok(ctx) = self.load_from_neo4j(neo4j, api_name).await {
                // Cache it for next time
                let mut contexts = self.contexts.write().await;
                contexts.insert(key, ctx.clone());
                info!(api = %api_name, "Context loaded from Neo4j and cached");
                return Some(ctx);
            }
        }

        warn!(api = %api_name, "Context not found in memory or Neo4j");
        None
    }

    /// Store a context in memory.
    pub async fn set(&self, context: ApiContext) {
        let key = Self::normalize_name(&context.name);
        info!(api = %context.name, endpoints = context.endpoint_count, "Storing API context");
        let mut contexts = self.contexts.write().await;
        contexts.insert(key, context);
    }

    /// List all loaded API names.
    pub async fn list_active(&self) -> Vec<String> {
        let contexts = self.contexts.read().await;
        contexts.values().map(|c| c.name.clone()).collect()
    }

    /// Get all loaded contexts.
    pub async fn get_all(&self) -> Vec<ApiContext> {
        let contexts = self.contexts.read().await;
        contexts.values().cloned().collect()
    }

    /// Clear a specific context or all contexts.
    pub async fn clear(&self, api_name: Option<&str>) {
        let mut contexts = self.contexts.write().await;
        match api_name {
            Some(name) => {
                let key = Self::normalize_name(name);
                if contexts.remove(&key).is_some() {
                    info!(api = %name, "Cleared API context");
                }
            }
            None => {
                let count = contexts.len();
                contexts.clear();
                info!(count, "Cleared all API contexts");
            }
        }
    }

    /// Check if a context is loaded.
    pub async fn contains(&self, api_name: &str) -> bool {
        let key = Self::normalize_name(api_name);
        let contexts = self.contexts.read().await;
        contexts.contains_key(&key)
    }

    /// Get the number of loaded contexts.
    pub async fn len(&self) -> usize {
        let contexts = self.contexts.read().await;
        contexts.len()
    }

    /// Check if the store is empty.
    pub async fn is_empty(&self) -> bool {
        let contexts = self.contexts.read().await;
        contexts.is_empty()
    }

    /// Load context from Neo4j by querying endpoints and building a summary.
    async fn load_from_neo4j(
        &self,
        neo4j: &Neo4jClient,
        api_name: &str,
    ) -> Result<ApiContext, ContextError> {
        // Find endpoints matching this API name (by resource name)
        let endpoints = neo4j
            .find_endpoints_by_path(api_name)
            .await
            .map_err(|e| ContextError::DatabaseError(e.to_string()))?;

        if endpoints.is_empty() {
            return Err(ContextError::NotFound(api_name.to_string()));
        }

        let mut context = ApiContext::new(api_name.to_string(), "unknown".to_string());

        for endpoint in endpoints {
            // Get parameters for this endpoint
            let params = neo4j
                .get_parameters_for_endpoint(endpoint.id)
                .await
                .unwrap_or_default();

            let mut param_summary = ParameterSummary::default();
            for param in params {
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
                method: endpoint.method,
                path: endpoint.path,
                summary: endpoint.summary,
                operation_id: endpoint.operation_id,
                parameters: param_summary,
            });
        }

        Ok(context)
    }

    /// Load all available API contexts from Neo4j.
    pub async fn load_all(&self) -> Result<usize, ContextError> {
        let Some(neo4j) = &self.neo4j else {
            return Ok(0);
        };

        info!("Pre-loading all API contexts from Neo4j");
        let resources = neo4j
            .list_resources()
            .await
            .map_err(|e| ContextError::DatabaseError(e.to_string()))?;

        let mut count = 0;
        for resource in resources {
            if let Some(_ctx) = self.get_or_load(&resource.name).await {
                debug!(api = %resource.name, "Pre-loaded API context");
                count += 1;
            }
        }

        info!(loaded = count, "Finished pre-loading API contexts");
        Ok(count)
    }
}

impl Default for ContextStore {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Errors
// ============================================================================

/// Errors that can occur in context operations.
#[derive(Debug, thiserror::Error)]
pub enum ContextError {
    #[error("API context not found: {0}")]
    NotFound(String),

    #[error("Database error: {0}")]
    DatabaseError(String),
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_name() {
        assert_eq!(ContextStore::normalize_name("Petstore API"), "petstore-api");
        assert_eq!(ContextStore::normalize_name("stripe_api"), "stripe-api");
        assert_eq!(ContextStore::normalize_name("GitHub"), "github");
    }

    #[test]
    fn test_api_context_creation() {
        let ctx = ApiContext::new("Test API".to_string(), "1.0.0".to_string())
            .with_base_url("https://api.example.com")
            .with_description("A test API");

        assert_eq!(ctx.name, "Test API");
        assert_eq!(ctx.version, "1.0.0");
        assert_eq!(ctx.base_url, Some("https://api.example.com".to_string()));
        assert_eq!(ctx.description, Some("A test API".to_string()));
        assert_eq!(ctx.endpoint_count, 0);
    }

    #[test]
    fn test_add_endpoint() {
        let mut ctx = ApiContext::new("Test".to_string(), "1.0".to_string());

        ctx.add_endpoint(EndpointSummary {
            method: HttpMethod::Get,
            path: "/users".to_string(),
            summary: "List users".to_string(),
            operation_id: Some("listUsers".to_string()),
            parameters: ParameterSummary::default(),
        });

        assert_eq!(ctx.endpoint_count, 1);
        assert_eq!(ctx.endpoints[0].path, "/users");
    }

    #[test]
    fn test_parameter_summary() {
        let mut params = ParameterSummary::default();
        assert!(params.is_empty());
        assert_eq!(params.count(), 0);

        params.path.push("id".to_string());
        params.query.push("limit".to_string());
        params.query.push("offset".to_string());

        assert!(!params.is_empty());
        assert_eq!(params.count(), 3);
    }

    #[test]
    fn test_compact_summary() {
        let mut ctx = ApiContext::new("Petstore".to_string(), "1.0.0".to_string())
            .with_base_url("https://petstore.example.com");

        ctx.add_endpoint(EndpointSummary {
            method: HttpMethod::Get,
            path: "/pets".to_string(),
            summary: "List all pets".to_string(),
            operation_id: None,
            parameters: ParameterSummary {
                query: vec!["limit".to_string(), "offset".to_string()],
                ..Default::default()
            },
        });

        let summary = ctx.to_compact_summary();
        assert!(summary.contains("Petstore v1.0.0"));
        assert!(summary.contains("GET /pets"));
        assert!(summary.contains("limit, offset"));
    }

    #[tokio::test]
    async fn test_context_store_basic() {
        let store = ContextStore::new();

        assert!(store.is_empty().await);
        assert!(store.get("test").await.is_none());

        let ctx = ApiContext::new("Test API".to_string(), "1.0".to_string());
        store.set(ctx).await;

        assert!(!store.is_empty().await);
        assert_eq!(store.len().await, 1);

        let retrieved = store.get("Test API").await;
        assert!(retrieved.is_some());
        assert_eq!(retrieved.unwrap().name, "Test API");

        // Test case-insensitive lookup
        assert!(store.get("test api").await.is_some());
        assert!(store.get("TEST-API").await.is_some());
    }

    #[tokio::test]
    async fn test_context_store_clear() {
        let store = ContextStore::new();

        store
            .set(ApiContext::new("API1".to_string(), "1.0".to_string()))
            .await;
        store
            .set(ApiContext::new("API2".to_string(), "1.0".to_string()))
            .await;

        assert_eq!(store.len().await, 2);

        store.clear(Some("API1")).await;
        assert_eq!(store.len().await, 1);
        assert!(store.get("API1").await.is_none());
        assert!(store.get("API2").await.is_some());

        store.clear(None).await;
        assert!(store.is_empty().await);
    }

    #[tokio::test]
    async fn test_list_active() {
        let store = ContextStore::new();

        store
            .set(ApiContext::new("Alpha".to_string(), "1.0".to_string()))
            .await;
        store
            .set(ApiContext::new("Beta".to_string(), "2.0".to_string()))
            .await;

        let active = store.list_active().await;
        assert_eq!(active.len(), 2);
        assert!(active.contains(&"Alpha".to_string()));
        assert!(active.contains(&"Beta".to_string()));
    }
}
