//! Integration tests for context management tools.

use agent_api::mcp::tools::{ToolHandler, ToolRegistry};
use agent_api::repository::Neo4jClient;
use serde_json::json;

async fn setup_handler() -> ToolHandler {
    let neo4j = Neo4jClient::new(
        &std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7688".to_string()),
        &std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".to_string()),
        &std::env::var("NEO4J_PASSWORD").unwrap_or_else(|_| "password".to_string()),
    )
    .await
    .expect("Failed to connect to Neo4j");

    neo4j.init_schema().await.expect("Failed to init schema");
    ToolHandler::with_neo4j(neo4j)
}

#[tokio::test]
async fn test_tool_registry_has_eight_tools() {
    let registry = ToolRegistry::new();
    let tools = registry.list();

    assert_eq!(tools.len(), 8, "Expected 8 tools");

    let tool_names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(tool_names.contains(&"ingest_openapi"));
    assert!(tool_names.contains(&"graph_query_endpoint"));
    assert!(tool_names.contains(&"execute_http_request"));
    assert!(tool_names.contains(&"get_api_context"));
    assert!(tool_names.contains(&"list_loaded_apis"));
    assert!(tool_names.contains(&"clear_api_context"));
    assert!(tool_names.contains(&"discover_openapi"));
    assert!(tool_names.contains(&"build_openapi_from_docs"));
}

#[tokio::test]
async fn test_list_loaded_apis_empty() {
    let handler = setup_handler().await;

    // Clear any existing context first
    handler.execute("clear_api_context", Some(json!({}))).await;

    let result = handler.execute("list_loaded_apis", None).await;

    assert!(result.is_error.is_none() || !result.is_error.unwrap());
    let text = result.content.first().unwrap();
    if let agent_api::mcp::protocol::Content::Text { text } = text {
        assert!(text.contains("No APIs currently loaded"));
    }
}

#[tokio::test]
async fn test_ingest_and_get_context() {
    let handler = setup_handler().await;

    // Ingest petstore
    let result = handler
        .execute(
            "ingest_openapi",
            Some(json!({"source": "tests/fixtures/petstore.json"})),
        )
        .await;

    assert!(
        result.is_error.is_none() || !result.is_error.unwrap(),
        "Ingest failed"
    );

    // List loaded APIs - should now have petstore
    let result = handler.execute("list_loaded_apis", None).await;
    let text = result.content.first().unwrap();
    if let agent_api::mcp::protocol::Content::Text { text } = text {
        println!("list_loaded_apis result: {}", text);
        assert!(text.contains("Petstore") || text.contains("count"));
    }

    // Get context for the API
    let result = handler
        .execute("get_api_context", Some(json!({"format": "compact"})))
        .await;

    assert!(result.is_error.is_none() || !result.is_error.unwrap());
    let text = result.content.first().unwrap();
    if let agent_api::mcp::protocol::Content::Text { text } = text {
        println!("get_api_context result: {}", text);
        // Should contain endpoint info
        assert!(text.contains("/pets") || text.contains("Endpoints"));
    }
}

#[tokio::test]
async fn test_clear_api_context() {
    let handler = setup_handler().await;

    // Ingest first
    handler
        .execute(
            "ingest_openapi",
            Some(json!({"source": "tests/fixtures/petstore.json"})),
        )
        .await;

    // Clear all contexts
    let result = handler.execute("clear_api_context", Some(json!({}))).await;

    assert!(result.is_error.is_none() || !result.is_error.unwrap());
    let text = result.content.first().unwrap();
    if let agent_api::mcp::protocol::Content::Text { text } = text {
        println!("clear_api_context result: {}", text);
        assert!(text.contains("Cleared"));
    }

    // Verify it's empty
    let result = handler.execute("list_loaded_apis", None).await;
    let text = result.content.first().unwrap();
    if let agent_api::mcp::protocol::Content::Text { text } = text {
        assert!(text.contains("No APIs currently loaded"));
    }
}

#[tokio::test]
async fn test_get_api_context_formats() {
    let handler = setup_handler().await;

    // Ingest first
    handler
        .execute(
            "ingest_openapi",
            Some(json!({"source": "tests/fixtures/petstore.json"})),
        )
        .await;

    // Test summary format (default)
    let result = handler
        .execute("get_api_context", Some(json!({"format": "summary"})))
        .await;
    assert!(result.is_error.is_none() || !result.is_error.unwrap());
    let text = result.content.first().unwrap();
    if let agent_api::mcp::protocol::Content::Text { text } = text {
        println!("Summary format: {}", text);
        // JSON format should have these keys
        assert!(text.contains("\"name\"") || text.contains("\"endpoints\""));
    }

    // Test compact format
    let result = handler
        .execute("get_api_context", Some(json!({"format": "compact"})))
        .await;
    assert!(result.is_error.is_none() || !result.is_error.unwrap());
    let text = result.content.first().unwrap();
    if let agent_api::mcp::protocol::Content::Text { text } = text {
        println!("Compact format: {}", text);
        // Text format
        assert!(text.contains("API:") || text.contains("Endpoints"));
    }
}
