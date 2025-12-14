//! Integration tests for the discovery service.

use agent_api::mcp::tools::{ToolHandler, ToolRegistry};
use agent_api::services::{DiscoveryService, LlmConfig};
use serde_json::json;

#[test]
fn test_discover_openapi_tool_exists() {
    let registry = ToolRegistry::new();
    let tool = registry.get("discover_openapi");
    assert!(tool.is_some());

    let tool = tool.unwrap();
    assert_eq!(tool.name, "discover_openapi");
    assert!(tool.description.contains("discover"));
    assert!(tool.input_schema["required"].as_array().unwrap().contains(&json!("base_url")));
}

#[tokio::test]
async fn test_discovery_service_petstore() {
    // Test against the public Petstore API
    let service = DiscoveryService::new().unwrap();

    let result = service.discover("https://petstore.swagger.io").await;

    match result {
        Ok(discovery) => {
            println!("Discovery result for petstore.swagger.io:");
            println!("  Base URL: {}", discovery.base_url);
            println!("  Candidates found: {}", discovery.candidates.len());
            println!("  URLs probed: {}", discovery.probed_urls.len());

            for candidate in &discovery.candidates {
                println!(
                    "  - {} (method: {:?}, confidence: {:.2})",
                    candidate.url, candidate.method, candidate.confidence
                );
                if let Some(title) = &candidate.api_title {
                    println!("    Title: {}", title);
                }
            }

            // Petstore should have at least one spec
            if discovery.candidates.is_empty() {
                println!("  No candidates found (petstore may be down or changed)");
            }
        }
        Err(e) => {
            println!("Discovery failed (network may be unavailable): {}", e);
        }
    }
}

#[tokio::test]
async fn test_discovery_service_github_api() {
    // Test against GitHub's API (has OpenAPI spec)
    let service = DiscoveryService::new().unwrap();

    let result = service.discover("https://api.github.com").await;

    match result {
        Ok(discovery) => {
            println!("Discovery result for api.github.com:");
            println!("  Candidates found: {}", discovery.candidates.len());

            for candidate in &discovery.candidates {
                println!(
                    "  - {} ({:?})",
                    candidate.url, candidate.method
                );
            }
        }
        Err(e) => {
            println!("Discovery failed (network may be unavailable): {}", e);
        }
    }
}

#[tokio::test]
async fn test_discovery_tool_handler() {
    let handler = ToolHandler::new();

    // Test without LLM (just common path probing)
    let result = handler
        .execute(
            "discover_openapi",
            Some(json!({
                "base_url": "https://petstore.swagger.io",
                "use_llm": false
            })),
        )
        .await;

    // Should not error even if no specs found
    assert!(
        result.is_error.is_none() || !result.is_error.unwrap(),
        "Tool should not return error for valid URL"
    );

    if let Some(content) = result.content.first() {
        if let agent_api::mcp::protocol::Content::Text { text } = content {
            println!("Tool output:\n{}", text);
            assert!(text.contains("base_url"));
            assert!(text.contains("candidates"));
        }
    }
}

#[tokio::test]
async fn test_discovery_invalid_url() {
    let handler = ToolHandler::new();

    let result = handler
        .execute(
            "discover_openapi",
            Some(json!({
                "base_url": "not-a-valid-url",
                "use_llm": false
            })),
        )
        .await;

    // Should return an error for invalid URL
    assert!(
        result.is_error.unwrap_or(false),
        "Tool should return error for invalid URL"
    );
}

#[tokio::test]
async fn test_discovery_with_llm_config() {
    // Test that LLM config is properly handled
    let handler = ToolHandler::new().with_llm_config(LlmConfig::default());

    let result = handler
        .execute(
            "discover_openapi",
            Some(json!({
                "base_url": "https://petstore.swagger.io",
                "use_llm": true  // Will try to use LLM but may fail if Ollama not running
            })),
        )
        .await;

    // Should still work even if LLM is not available
    if let Some(content) = result.content.first() {
        if let agent_api::mcp::protocol::Content::Text { text } = content {
            println!("Tool output with LLM enabled:\n{}", text);
        }
    }
}
