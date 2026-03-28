//! JSON-RPC 2.0 and MCP protocol types.
//!
//! All types are defined in the `agent-brain-protocol` crate and re-exported
//! here for backward compatibility.  Code inside this crate can continue to
//! use `crate::mcp::protocol::Content` etc.

pub use agent_brain_protocol::{
    ClientCapabilities,
    // MCP types
    ClientInfo,
    Content,
    IncomingMessage,
    InitializeParams,
    InitializeResult,
    // Constants
    JSONRPC_VERSION,
    JsonRpcError,
    JsonRpcErrorResponse,
    JsonRpcNotification,
    JsonRpcRequest,
    JsonRpcResponse,
    MCP_PROTOCOL_VERSION,
    // JSON-RPC base types
    RequestId,
    ServerCapabilities,
    ServerInfo,
    ToolCallParams,
    ToolCallResult,
    ToolDefinition,
    ToolsCapability,
    ToolsListResult,
    error_codes,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_request_id_from_number() {
        let id: RequestId = 42.into();
        assert_eq!(id, RequestId::Number(42));
    }

    #[test]
    fn test_request_id_from_string() {
        let id: RequestId = "test-id".into();
        assert_eq!(id, RequestId::String("test-id".to_string()));
    }

    #[test]
    fn test_parse_request() {
        let json = r#"{"jsonrpc":"2.0","id":1,"method":"test","params":{"foo":"bar"}}"#;
        let msg = IncomingMessage::parse(json).unwrap();
        assert_eq!(msg.method(), "test");
    }

    #[test]
    fn test_parse_notification() {
        let json = r#"{"jsonrpc":"2.0","method":"initialized"}"#;
        let msg = IncomingMessage::parse(json).unwrap();
        assert_eq!(msg.method(), "initialized");
    }

    #[test]
    fn test_response_serialization() {
        let response = JsonRpcResponse::new(1.into(), serde_json::json!({"result": "ok"}));
        let json = serde_json::to_string(&response).unwrap();
        assert!(json.contains("\"jsonrpc\":\"2.0\""));
        assert!(json.contains("\"id\":1"));
    }

    #[test]
    fn test_error_response() {
        let error = JsonRpcErrorResponse::method_not_found(1.into(), "unknown");
        assert_eq!(error.error.code, error_codes::METHOD_NOT_FOUND);
    }

    #[test]
    fn test_tool_call_result_success() {
        let result = ToolCallResult::success_text("Hello");
        assert!(result.is_error.is_none());
        assert_eq!(result.content.len(), 1);
    }

    #[test]
    fn test_tool_call_result_error() {
        let result = ToolCallResult::error("Something went wrong");
        assert_eq!(result.is_error, Some(true));
    }

    #[test]
    fn test_content_serialization() {
        let content = Content::text("Hello, world!");
        let json = serde_json::to_string(&content).unwrap();
        assert!(json.contains("\"type\":\"text\""));
        assert!(json.contains("\"text\":\"Hello, world!\""));
    }
}
