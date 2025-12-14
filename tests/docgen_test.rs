//! Integration tests for the documentation-to-OpenAPI generation service.

use agent_api::mcp::tools::{ToolHandler, ToolRegistry};
use agent_api::services::{DocGenConfig, LlmConfig};
use serde_json::json;

#[test]
fn test_build_openapi_from_docs_tool_exists() {
    let registry = ToolRegistry::new();
    let tool = registry.get("build_openapi_from_docs");
    assert!(tool.is_some());

    let tool = tool.unwrap();
    assert_eq!(tool.name, "build_openapi_from_docs");
    assert!(tool.description.contains("Generate"));
    assert!(tool.description.contains("OpenAPI"));
    assert!(tool.input_schema["required"]
        .as_array()
        .unwrap()
        .contains(&json!("doc_urls")));
    assert!(tool.input_schema["required"]
        .as_array()
        .unwrap()
        .contains(&json!("api_title")));
}

#[test]
fn test_build_openapi_from_docs_tool_schema() {
    let registry = ToolRegistry::new();
    let tool = registry.get("build_openapi_from_docs").unwrap();

    let props = &tool.input_schema["properties"];

    // Check required properties exist
    assert!(props["doc_urls"].is_object());
    assert!(props["api_title"].is_object());

    // Check optional properties exist
    assert!(props["api_version"].is_object());
    assert!(props["base_url"].is_object());
    assert!(props["output_format"].is_object());
    assert!(props["auto_ingest"].is_object());

    // Check doc_urls is an array type
    assert_eq!(props["doc_urls"]["type"], "array");

    // Check output_format has enum values
    let output_formats = props["output_format"]["enum"].as_array().unwrap();
    assert!(output_formats.contains(&json!("json")));
    assert!(output_formats.contains(&json!("yaml")));
}

#[tokio::test]
async fn test_build_openapi_from_docs_requires_llm() {
    let handler = ToolHandler::new(); // No LLM config

    let result = handler
        .execute(
            "build_openapi_from_docs",
            Some(json!({
                "doc_urls": ["https://example.com/api/docs"],
                "api_title": "Test API"
            })),
        )
        .await;

    // Should return an error because LLM is required
    assert!(result.is_error.unwrap_or(false), "Should error without LLM config");

    if let Some(content) = result.content.first() {
        if let agent_api::mcp::protocol::Content::Text { text } = content {
            assert!(text.contains("LLM") || text.contains("configuration"));
        }
    }
}

#[tokio::test]
async fn test_build_openapi_from_docs_empty_urls() {
    let handler = ToolHandler::new().with_llm_config(LlmConfig::default());

    let result = handler
        .execute(
            "build_openapi_from_docs",
            Some(json!({
                "doc_urls": [],
                "api_title": "Test API"
            })),
        )
        .await;

    // Should return an error for empty URLs
    assert!(result.is_error.unwrap_or(false), "Should error with empty URLs");
}

#[test]
fn test_docgen_config_customization() {
    use std::time::Duration;

    let config = DocGenConfig {
        max_pages: 20,
        request_timeout: Duration::from_secs(60),
        follow_links: false,
        max_depth: 3,
    };

    assert_eq!(config.max_pages, 20);
    assert_eq!(config.request_timeout, Duration::from_secs(60));
    assert!(!config.follow_links);
    assert_eq!(config.max_depth, 3);
}

#[test]
fn test_openapi_spec_structure() {
    use agent_api::services::OpenApiSpec;

    let mut spec = OpenApiSpec::new("Test API", "2.0.0");
    spec.add_server("https://api.example.com", Some("Production"));

    assert_eq!(spec.openapi, "3.0.3");
    assert_eq!(spec.info.title, "Test API");
    assert_eq!(spec.info.version, "2.0.0");
    assert_eq!(spec.servers.len(), 1);
    assert_eq!(spec.servers[0].url, "https://api.example.com");
    assert_eq!(spec.servers[0].description, Some("Production".to_string()));
}

#[test]
fn test_openapi_spec_json_output() {
    use agent_api::services::OpenApiSpec;

    let spec = OpenApiSpec::new("My API", "1.0.0");
    let json = spec.to_json().unwrap();

    assert!(json.contains("\"openapi\": \"3.0.3\""));
    assert!(json.contains("\"title\": \"My API\""));
    assert!(json.contains("\"version\": \"1.0.0\""));
}

#[test]
fn test_openapi_spec_yaml_output() {
    use agent_api::services::OpenApiSpec;

    let spec = OpenApiSpec::new("My API", "1.0.0");
    let yaml = spec.to_yaml().unwrap();

    assert!(yaml.contains("openapi: 3.0.3"));
    assert!(yaml.contains("title: My API"));
    assert!(yaml.contains("version: 1.0.0"));
}

#[test]
fn test_openapi_spec_add_endpoint() {
    use agent_api::services::{OpenApiSpec, docgen::Operation};

    let mut spec = OpenApiSpec::new("Test", "1.0");
    let op = Operation::new("List users");
    spec.add_endpoint("/users", "GET", op);

    assert!(spec.paths.contains_key("/users"));
    let path_item = &spec.paths["/users"];
    assert!(path_item.get.is_some());
    assert!(path_item.post.is_none());

    let get_op = path_item.get.as_ref().unwrap();
    assert_eq!(get_op.summary, Some("List users".to_string()));
}

#[test]
fn test_openapi_spec_multiple_methods_same_path() {
    use agent_api::services::{OpenApiSpec, docgen::Operation};

    let mut spec = OpenApiSpec::new("Test", "1.0");
    spec.add_endpoint("/users", "GET", Operation::new("List users"));
    spec.add_endpoint("/users", "POST", Operation::new("Create user"));

    let path_item = &spec.paths["/users"];
    assert!(path_item.get.is_some());
    assert!(path_item.post.is_some());

    assert_eq!(path_item.get.as_ref().unwrap().summary, Some("List users".to_string()));
    assert_eq!(path_item.post.as_ref().unwrap().summary, Some("Create user".to_string()));
}
