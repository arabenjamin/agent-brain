//! OpenAPI specification builder helpers.

use serde_json::{Value, json};
use std::collections::HashMap;

use crate::models::HttpMethod;

/// Builder for constructing OpenAPI 3.0 specifications.
#[derive(Debug, Clone)]
pub struct OpenApiBuilder {
    title: String,
    version: String,
    description: Option<String>,
    servers: Vec<ServerInfo>,
    paths: HashMap<String, PathItemBuilder>,
    schemas: HashMap<String, Value>,
    extensions: HashMap<String, Value>,
}

/// Server information for the OpenAPI spec.
#[derive(Debug, Clone)]
pub struct ServerInfo {
    pub url: String,
    pub description: Option<String>,
}

/// Builder for a path item (all operations on a path).
#[derive(Debug, Clone, Default)]
pub struct PathItemBuilder {
    get: Option<Value>,
    post: Option<Value>,
    put: Option<Value>,
    patch: Option<Value>,
    delete: Option<Value>,
    head: Option<Value>,
    options: Option<Value>,
}

impl OpenApiBuilder {
    /// Create a new OpenAPI builder with defaults.
    pub fn new() -> Self {
        Self {
            title: "API".to_string(),
            version: "1.0.0".to_string(),
            description: None,
            servers: Vec::new(),
            paths: HashMap::new(),
            schemas: HashMap::new(),
            extensions: HashMap::new(),
        }
    }

    /// Set the API title.
    pub fn with_title(mut self, title: impl Into<String>) -> Self {
        self.title = title.into();
        self
    }

    /// Set the API version.
    pub fn with_version(mut self, version: impl Into<String>) -> Self {
        self.version = version.into();
        self
    }

    /// Set the API description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = Some(description.into());
        self
    }

    /// Add a server URL.
    pub fn with_server(mut self, url: impl Into<String>, description: Option<String>) -> Self {
        self.servers.push(ServerInfo {
            url: url.into(),
            description,
        });
        self
    }

    /// Add a top-level extension (x-* field).
    pub fn with_extension(mut self, name: impl Into<String>, value: Value) -> Self {
        self.extensions.insert(name.into(), value);
        self
    }

    /// Add a schema to components.
    pub fn with_schema(mut self, name: impl Into<String>, schema: Value) -> Self {
        self.schemas.insert(name.into(), schema);
        self
    }

    /// Add an operation to a path.
    pub fn with_operation(
        mut self,
        path: impl Into<String>,
        method: HttpMethod,
        operation: Value,
    ) -> Self {
        let path_str = path.into();
        let path_item = self.paths.entry(path_str).or_default();

        match method {
            HttpMethod::Get => path_item.get = Some(operation),
            HttpMethod::Post => path_item.post = Some(operation),
            HttpMethod::Put => path_item.put = Some(operation),
            HttpMethod::Patch => path_item.patch = Some(operation),
            HttpMethod::Delete => path_item.delete = Some(operation),
            HttpMethod::Head => path_item.head = Some(operation),
            HttpMethod::Options => path_item.options = Some(operation),
        }

        self
    }

    /// Build the final OpenAPI specification as JSON.
    pub fn build(self) -> Value {
        let mut spec = json!({
            "openapi": "3.0.3",
            "info": {
                "title": self.title,
                "version": self.version
            }
        });

        // Add description if present
        if let Some(desc) = &self.description {
            spec["info"]["description"] = json!(desc);
        }

        // Add servers if present
        if !self.servers.is_empty() {
            let servers: Vec<Value> = self
                .servers
                .iter()
                .map(|s| {
                    let mut server = json!({"url": s.url});
                    if let Some(ref desc) = s.description {
                        server["description"] = json!(desc);
                    }
                    server
                })
                .collect();
            spec["servers"] = json!(servers);
        }

        // Add extensions
        for (key, value) in &self.extensions {
            spec[key] = value.clone();
        }

        // Build paths
        let mut paths = json!({});
        for (path, path_item) in &self.paths {
            let mut item = json!({});

            if let Some(ref op) = path_item.get {
                item["get"] = op.clone();
            }
            if let Some(ref op) = path_item.post {
                item["post"] = op.clone();
            }
            if let Some(ref op) = path_item.put {
                item["put"] = op.clone();
            }
            if let Some(ref op) = path_item.patch {
                item["patch"] = op.clone();
            }
            if let Some(ref op) = path_item.delete {
                item["delete"] = op.clone();
            }
            if let Some(ref op) = path_item.head {
                item["head"] = op.clone();
            }
            if let Some(ref op) = path_item.options {
                item["options"] = op.clone();
            }

            paths[path] = item;
        }
        spec["paths"] = paths;

        // Build components with schemas
        if !self.schemas.is_empty() {
            let mut schemas_obj = json!({});
            for (name, schema) in &self.schemas {
                schemas_obj[name] = schema.clone();
            }
            spec["components"] = json!({
                "schemas": schemas_obj
            });
        }

        spec
    }

    /// Build and serialize to YAML.
    pub fn build_yaml(self) -> Result<String, serde_yaml::Error> {
        let spec = self.build();
        serde_yaml::to_string(&spec)
    }

    /// Build and serialize to JSON.
    pub fn build_json(self) -> Result<String, serde_json::Error> {
        let spec = self.build();
        serde_json::to_string_pretty(&spec)
    }
}

impl Default for OpenApiBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// Helper to create a parameter object.
#[allow(dead_code)]
pub fn build_parameter(
    name: &str,
    location: &str,
    required: bool,
    param_type: Option<&str>,
    description: Option<&str>,
) -> Value {
    let mut param = json!({
        "name": name,
        "in": location,
        "required": required,
        "schema": {
            "type": param_type.unwrap_or("string")
        }
    });

    if let Some(desc) = description {
        param["description"] = json!(desc);
    }

    param
}

/// Helper to create a response object.
#[allow(dead_code)]
pub fn build_response(
    description: &str,
    schema_ref: Option<&str>,
    content_type: Option<&str>,
) -> Value {
    let mut response = json!({
        "description": description
    });

    if let Some(schema) = schema_ref {
        let ct = content_type.unwrap_or("application/json");
        response["content"] = json!({
            ct: {
                "schema": {
                    "$ref": format!("#/components/schemas/{}", schema)
                }
            }
        });
    }

    response
}

/// Helper to create a request body object.
#[allow(dead_code)]
pub fn build_request_body(schema_ref: &str, required: bool, content_type: Option<&str>) -> Value {
    let ct = content_type.unwrap_or("application/json");
    json!({
        "required": required,
        "content": {
            ct: {
                "schema": {
                    "$ref": format!("#/components/schemas/{}", schema_ref)
                }
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_builder_basic() {
        let spec = OpenApiBuilder::new()
            .with_title("Test API")
            .with_version("2.0.0")
            .build();

        assert_eq!(spec["openapi"], "3.0.3");
        assert_eq!(spec["info"]["title"], "Test API");
        assert_eq!(spec["info"]["version"], "2.0.0");
    }

    #[test]
    fn test_builder_with_description() {
        let spec = OpenApiBuilder::new()
            .with_title("Test")
            .with_version("1.0.0")
            .with_description("A test API")
            .build();

        assert_eq!(spec["info"]["description"], "A test API");
    }

    #[test]
    fn test_builder_with_servers() {
        let spec = OpenApiBuilder::new()
            .with_title("Test")
            .with_version("1.0.0")
            .with_server("https://api.example.com", Some("Production".to_string()))
            .with_server("https://staging.example.com", None)
            .build();

        assert!(spec["servers"].is_array());
        assert_eq!(spec["servers"][0]["url"], "https://api.example.com");
        assert_eq!(spec["servers"][0]["description"], "Production");
        assert_eq!(spec["servers"][1]["url"], "https://staging.example.com");
    }

    #[test]
    fn test_builder_with_extension() {
        let spec = OpenApiBuilder::new()
            .with_title("Test")
            .with_version("1.0.0")
            .with_extension("x-custom", json!("value"))
            .build();

        assert_eq!(spec["x-custom"], "value");
    }

    #[test]
    fn test_builder_with_schema() {
        let spec = OpenApiBuilder::new()
            .with_title("Test")
            .with_version("1.0.0")
            .with_schema(
                "User",
                json!({
                    "type": "object",
                    "properties": {
                        "id": {"type": "integer"},
                        "name": {"type": "string"}
                    }
                }),
            )
            .build();

        assert!(spec["components"]["schemas"]["User"].is_object());
        assert_eq!(spec["components"]["schemas"]["User"]["type"], "object");
    }

    #[test]
    fn test_builder_with_operation() {
        let operation = json!({
            "summary": "Get users",
            "responses": {
                "200": {"description": "Success"}
            }
        });

        let spec = OpenApiBuilder::new()
            .with_title("Test")
            .with_version("1.0.0")
            .with_operation("/users", HttpMethod::Get, operation)
            .build();

        assert!(spec["paths"]["/users"]["get"].is_object());
        assert_eq!(spec["paths"]["/users"]["get"]["summary"], "Get users");
    }

    #[test]
    fn test_build_parameter() {
        let param = build_parameter("userId", "path", true, Some("integer"), Some("The user ID"));

        assert_eq!(param["name"], "userId");
        assert_eq!(param["in"], "path");
        assert_eq!(param["required"], true);
        assert_eq!(param["schema"]["type"], "integer");
        assert_eq!(param["description"], "The user ID");
    }

    #[test]
    fn test_build_response() {
        let response = build_response("Success", Some("User"), None);

        assert_eq!(response["description"], "Success");
        assert!(
            response["content"]["application/json"]["schema"]["$ref"]
                .as_str()
                .unwrap()
                .contains("User")
        );
    }

    #[test]
    fn test_build_request_body() {
        let body = build_request_body("CreateUserRequest", true, None);

        assert_eq!(body["required"], true);
        assert!(
            body["content"]["application/json"]["schema"]["$ref"]
                .as_str()
                .unwrap()
                .contains("CreateUserRequest")
        );
    }

    #[test]
    fn test_builder_yaml_output() {
        let yaml = OpenApiBuilder::new()
            .with_title("Test")
            .with_version("1.0.0")
            .build_yaml()
            .unwrap();

        assert!(yaml.contains("openapi:"));
        assert!(yaml.contains("Test"));
    }

    #[test]
    fn test_builder_json_output() {
        let json_str = OpenApiBuilder::new()
            .with_title("Test")
            .with_version("1.0.0")
            .build_json()
            .unwrap();

        assert!(json_str.contains("\"openapi\":"));
        assert!(json_str.contains("Test"));
    }
}
