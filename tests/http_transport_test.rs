//! Integration tests for HTTP transport.
//!
//! These tests verify the HTTP transport implementation:
//! - POST /mcp for JSON-RPC requests
//! - GET /mcp for SSE streaming
//! - DELETE /mcp for session termination
//! - Authentication and header validation

use agent_api::mcp::{
    AuthConfig, ApiKeyAuth, SessionManager, SessionConfig,
};
use axum::{
    body::Body,
    http::{header, Request, StatusCode},
};
use serde_json::{json, Value};

/// Helper to create an initialize request.
fn create_initialize_request() -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": {
                "name": "test-client",
                "version": "1.0.0"
            }
        }
    })
}

/// Helper to create a tools/list request.
fn create_tools_list_request(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "tools/list",
        "params": {}
    })
}

/// Helper to create a ping request.
fn create_ping_request(id: i64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "ping",
        "params": {}
    })
}

/// Helper to create an initialized notification.
fn create_initialized_notification() -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "notifications/initialized",
        "params": {}
    })
}

// ============================================================================
// Session Management Tests
// ============================================================================

#[tokio::test]
async fn test_session_manager_integration() {
    // This tests the session manager in isolation
    let manager = SessionManager::new();

    // Create session
    let session_id = manager.create_session().await.expect("Create session");

    // Verify session exists
    assert!(manager.exists(&session_id).await);

    // Get session (updates last accessed)
    manager.get_session(&session_id).await.expect("Get session");

    // Terminate session
    manager.terminate(&session_id).await.expect("Terminate session");

    // Verify session no longer exists
    assert!(!manager.exists(&session_id).await);
}

#[tokio::test]
async fn test_session_state_transitions() {
    use agent_api::mcp::SessionState;

    let manager = SessionManager::new();
    let session_id = manager.create_session().await.expect("Create session");

    // Initial state should be Created
    let state = manager.get_session_state(&session_id).await.expect("Get state");
    assert_eq!(state, SessionState::Created);

    // Transition through states
    manager.set_session_state(&session_id, SessionState::Initializing).await.expect("Set state");
    let state = manager.get_session_state(&session_id).await.expect("Get state");
    assert_eq!(state, SessionState::Initializing);

    manager.set_session_state(&session_id, SessionState::Running).await.expect("Set state");
    let state = manager.get_session_state(&session_id).await.expect("Get state");
    assert_eq!(state, SessionState::Running);
}

// ============================================================================
// Authentication Tests
// ============================================================================

#[tokio::test]
async fn test_auth_with_valid_api_key() {
    let auth = ApiKeyAuth::with_key("test-api-key-123");

    let request = Request::builder()
        .uri("/mcp")
        .header(header::AUTHORIZATION, "Bearer test-api-key-123")
        .body(Body::empty())
        .unwrap();

    let result = auth.authenticate(&request);
    assert!(result.is_ok(), "Valid API key should pass authentication");
}

#[tokio::test]
async fn test_auth_with_invalid_api_key() {
    let auth = ApiKeyAuth::with_key("correct-key");

    let request = Request::builder()
        .uri("/mcp")
        .header(header::AUTHORIZATION, "Bearer wrong-key")
        .body(Body::empty())
        .unwrap();

    let result = auth.authenticate(&request);
    assert!(result.is_err(), "Invalid API key should fail authentication");
}

#[tokio::test]
async fn test_auth_excluded_paths() {
    let config = AuthConfig {
        api_key: Some("secret".to_string()),
        excluded_paths: vec!["/health".to_string(), "/ready".to_string()],
    };
    let auth = ApiKeyAuth::new(config);

    // Health endpoint should bypass auth
    let request = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();

    let result = auth.authenticate(&request);
    assert!(result.is_ok(), "Excluded path should bypass authentication");
}

// ============================================================================
// HTTP Transport Tests (Placeholder - require HttpTransport implementation)
// ============================================================================

// Note: These tests are structured for when HttpTransport is implemented.
// They currently test the supporting infrastructure.

#[tokio::test]
async fn test_json_rpc_request_parsing() {
    // Test that JSON-RPC requests can be properly parsed
    let init_request = create_initialize_request();

    // Verify structure
    assert_eq!(init_request["jsonrpc"], "2.0");
    assert_eq!(init_request["method"], "initialize");
    assert!(init_request["id"].is_number());
}

#[tokio::test]
async fn test_json_rpc_notification_parsing() {
    // Test notification structure (no id field)
    let notification = create_initialized_notification();

    assert_eq!(notification["jsonrpc"], "2.0");
    assert_eq!(notification["method"], "notifications/initialized");
    assert!(notification.get("id").is_none());
}

#[tokio::test]
async fn test_mcp_protocol_headers() {
    // Test the expected headers for MCP HTTP transport
    let required_headers = vec![
        ("Accept", "application/json, text/event-stream"),
        ("Content-Type", "application/json"),
        ("MCP-Protocol-Version", "2024-11-05"),
    ];

    for (name, value) in required_headers {
        // Verify headers can be constructed
        let request = Request::builder()
            .uri("/mcp")
            .header(name, value)
            .body(Body::empty());

        assert!(request.is_ok(), "Header {}: {} should be valid", name, value);
    }
}

#[tokio::test]
async fn test_session_id_header_format() {
    // Session IDs should be valid UUIDs
    let manager = SessionManager::new();
    let session_id = manager.create_session().await.expect("Create session");

    // Verify it's a valid UUID
    let uuid_result = uuid::Uuid::parse_str(&session_id);
    assert!(uuid_result.is_ok(), "Session ID should be valid UUID");

    // Verify the header can be constructed
    let request = Request::builder()
        .uri("/mcp")
        .header("Mcp-Session-Id", &session_id)
        .body(Body::empty());

    assert!(request.is_ok(), "Session ID header should be valid");
}

// ============================================================================
// SSE Message Format Tests
// ============================================================================

#[tokio::test]
async fn test_sse_message_format() {
    use agent_api::mcp::SseMessage;

    let msg = SseMessage::new(r#"{"jsonrpc":"2.0","id":1,"result":{}}"#.to_string());

    // Should have auto-generated ID
    assert!(msg.id.is_some());
    assert!(uuid::Uuid::parse_str(msg.id.as_ref().unwrap()).is_ok());

    // Default event should be None (becomes "message" when serialized)
    assert!(msg.event.is_none());
}

#[tokio::test]
async fn test_sse_message_with_custom_event() {
    use agent_api::mcp::SseMessage;

    let msg = SseMessage::new("data".to_string()).with_event("notification");

    assert_eq!(msg.event, Some("notification".to_string()));
}

// ============================================================================
// Future HTTP Endpoint Tests (Require HttpTransport implementation)
// ============================================================================

// These are placeholder tests that document expected behavior.
// They will need the actual HttpTransport to run.

/*
#[tokio::test]
async fn test_post_mcp_initialize() {
    // POST /mcp with initialize request should:
    // 1. Return 200 OK
    // 2. Include Mcp-Session-Id header in response
    // 3. Return JSON-RPC response with server capabilities
}

#[tokio::test]
async fn test_post_mcp_without_protocol_version_header() {
    // POST /mcp without MCP-Protocol-Version header should:
    // 1. Return 400 Bad Request
    // 2. Include JSON-RPC error response
}

#[tokio::test]
async fn test_post_mcp_invalid_json() {
    // POST /mcp with invalid JSON should:
    // 1. Return 400 Bad Request
    // 2. Include JSON-RPC parse error
}

#[tokio::test]
async fn test_post_mcp_tool_call() {
    // POST /mcp with tools/call should:
    // 1. Require valid session (Mcp-Session-Id header)
    // 2. Return tool execution result
    // 3. Support SSE streaming if Accept includes text/event-stream
}

#[tokio::test]
async fn test_get_mcp_sse_stream() {
    // GET /mcp should:
    // 1. Require Mcp-Session-Id header
    // 2. Return text/event-stream content type
    // 3. Stream server-initiated messages
}

#[tokio::test]
async fn test_delete_mcp_session() {
    // DELETE /mcp should:
    // 1. Require Mcp-Session-Id header
    // 2. Terminate the session
    // 3. Return 204 No Content on success
}

#[tokio::test]
async fn test_cors_headers() {
    // All responses should include CORS headers:
    // - Access-Control-Allow-Origin
    // - Access-Control-Allow-Methods
    // - Access-Control-Allow-Headers
}

#[tokio::test]
async fn test_health_endpoint() {
    // GET /health should:
    // 1. Return 200 OK
    // 2. Not require authentication
    // 3. Return server health status
}
*/

// ============================================================================
// Test Utilities
// ============================================================================

/// Assert that a JSON value is a valid JSON-RPC 2.0 response.
#[allow(dead_code)]
fn assert_valid_jsonrpc_response(response: &Value) {
    assert_eq!(response["jsonrpc"], "2.0", "Must have jsonrpc: 2.0");
    assert!(response.get("id").is_some(), "Response must have id");
    assert!(
        response.get("result").is_some() || response.get("error").is_some(),
        "Response must have result or error"
    );
}

/// Assert that a JSON value is a valid JSON-RPC 2.0 error.
#[allow(dead_code)]
fn assert_valid_jsonrpc_error(response: &Value) {
    assert_eq!(response["jsonrpc"], "2.0");
    assert!(response.get("error").is_some());
    assert!(response["error"].get("code").is_some());
    assert!(response["error"].get("message").is_some());
}
