//! JSON-RPC 2.0 and MCP protocol data types.
//!
//! These are pure data types with no dependencies on MCP transport internals.

use serde::{Deserialize, Serialize, de::DeserializeOwned};
use serde_json::Value;

/// JSON-RPC version constant.
pub const JSONRPC_VERSION: &str = "2.0";

/// MCP protocol version we support.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

// ============================================================================
// JSON-RPC 2.0 Base Types
// ============================================================================

/// A JSON-RPC 2.0 request ID (can be string, number, or null).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(untagged)]
pub enum RequestId {
    String(String),
    Number(i64),
}

impl From<i64> for RequestId {
    fn from(n: i64) -> Self {
        RequestId::Number(n)
    }
}

impl From<String> for RequestId {
    fn from(s: String) -> Self {
        RequestId::String(s)
    }
}

impl From<&str> for RequestId {
    fn from(s: &str) -> Self {
        RequestId::String(s.to_string())
    }
}

/// A JSON-RPC 2.0 request message.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcRequest {
    pub jsonrpc: String,
    pub id: RequestId,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 notification (request without id).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcNotification {
    pub jsonrpc: String,
    pub method: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub params: Option<Value>,
}

/// A JSON-RPC 2.0 successful response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcResponse {
    pub jsonrpc: String,
    pub id: RequestId,
    pub result: Value,
}

/// A JSON-RPC 2.0 error response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcErrorResponse {
    pub jsonrpc: String,
    pub id: Option<RequestId>,
    pub error: JsonRpcError,
}

/// A JSON-RPC 2.0 error object.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JsonRpcError {
    pub code: i32,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

/// Standard JSON-RPC error codes.
pub mod error_codes {
    pub const PARSE_ERROR: i32 = -32700;
    pub const INVALID_REQUEST: i32 = -32600;
    pub const METHOD_NOT_FOUND: i32 = -32601;
    pub const INVALID_PARAMS: i32 = -32602;
    pub const INTERNAL_ERROR: i32 = -32603;
}

impl JsonRpcResponse {
    pub fn new(id: RequestId, result: Value) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            result,
        }
    }
}

impl JsonRpcErrorResponse {
    pub fn new(id: Option<RequestId>, code: i32, message: impl Into<String>) -> Self {
        Self {
            jsonrpc: JSONRPC_VERSION.to_string(),
            id,
            error: JsonRpcError {
                code,
                message: message.into(),
                data: None,
            },
        }
    }

    pub fn parse_error(message: impl Into<String>) -> Self {
        Self::new(None, error_codes::PARSE_ERROR, message)
    }

    pub fn invalid_request(id: Option<RequestId>, message: impl Into<String>) -> Self {
        Self::new(id, error_codes::INVALID_REQUEST, message)
    }

    pub fn method_not_found(id: RequestId, method: &str) -> Self {
        Self::new(
            Some(id),
            error_codes::METHOD_NOT_FOUND,
            format!("Method not found: {}", method),
        )
    }

    pub fn invalid_params(id: RequestId, message: impl Into<String>) -> Self {
        Self::new(Some(id), error_codes::INVALID_PARAMS, message)
    }

    pub fn internal_error(id: RequestId, message: impl Into<String>) -> Self {
        Self::new(Some(id), error_codes::INTERNAL_ERROR, message)
    }
}

/// Incoming message that could be a request or notification.
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum IncomingMessage {
    Request(JsonRpcRequest),
    Notification(JsonRpcNotification),
}

impl IncomingMessage {
    /// Parse an incoming JSON-RPC message.
    pub fn parse(json: &str) -> Result<Self, JsonRpcErrorResponse> {
        serde_json::from_str(json).map_err(|e| JsonRpcErrorResponse::parse_error(e.to_string()))
    }

    pub fn method(&self) -> &str {
        match self {
            IncomingMessage::Request(r) => &r.method,
            IncomingMessage::Notification(n) => &n.method,
        }
    }

    pub fn params(&self) -> Option<&Value> {
        match self {
            IncomingMessage::Request(r) => r.params.as_ref(),
            IncomingMessage::Notification(n) => n.params.as_ref(),
        }
    }
}

// ============================================================================
// MCP-Specific Types
// ============================================================================

/// Client information sent during initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClientInfo {
    pub name: String,
    #[serde(default)]
    pub version: Option<String>,
}

/// Server information sent during initialization.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerInfo {
    pub name: String,
    pub version: String,
}

/// Client capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub roots: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sampling: Option<Value>,
}

/// Server capabilities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ServerCapabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsCapability>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resources: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompts: Option<Value>,
}

/// Tools capability configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolsCapability {
    #[serde(default, rename = "listChanged")]
    pub list_changed: bool,
}

/// Initialize request parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct InitializeParams {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    pub client_info: ClientInfo,
}

/// Initialize response result.
#[derive(Debug, Clone, Serialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion")]
    pub protocol_version: String,
    pub capabilities: ServerCapabilities,
    #[serde(rename = "serverInfo")]
    pub server_info: ServerInfo,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub instructions: Option<String>,
}

/// Tool definition for tools/list response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    #[serde(rename = "inputSchema")]
    pub input_schema: Value,
}

/// Tools list response.
#[derive(Debug, Clone, Serialize)]
pub struct ToolsListResult {
    pub tools: Vec<ToolDefinition>,
}

/// Tool call request parameters.
#[derive(Debug, Clone, Deserialize)]
pub struct ToolCallParams {
    pub name: String,
    #[serde(default)]
    pub arguments: Option<Value>,
}

/// Content item in tool result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Content {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "image")]
    Image { data: String, mime_type: String },
    #[serde(rename = "resource")]
    Resource { resource: Value },
}

impl Content {
    pub fn text(text: impl Into<String>) -> Self {
        Content::Text { text: text.into() }
    }
}

/// Tool call response result.
///
/// `content` is what the MODEL reads. `bulk` is a second channel for output
/// whose audience is the **person**, not the model — the complete rows behind a
/// result, delivered without spending the model's context on them.
///
/// The two exist because they answer different questions. A model calling
/// `recall_recent` needs enough to reason and cite; a person who asked "show me
/// everything we learned from video learning" needs all 50 rows, and routing
/// those through a 4B local worker's context in 12-row pages is a round trip
/// that serves nobody: it costs four extra LLM calls, risks a truncated relay,
/// and delivers less than a single query would have. Bulk output bypasses that
/// entirely — the model is told the rows were delivered and given a small
/// sample for continuity, and its context cost stays flat whether the result is
/// 50 rows or 5,000.
///
/// Over MCP, `bulk` is appended to `content` so an external client receives the
/// whole result as one payload. Only the chat loop splits them.
#[derive(Debug, Clone, Serialize)]
pub struct ToolCallResult {
    pub content: Vec<Content>,
    #[serde(default, rename = "isError", skip_serializing_if = "Option::is_none")]
    pub is_error: Option<bool>,
    /// Full output for the human. Never fed to the model. See the type docs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bulk: Option<String>,
}

impl ToolCallResult {
    pub fn success(content: Vec<Content>) -> Self {
        Self {
            content,
            is_error: None,
            bulk: None,
        }
    }

    /// Attach human-facing bulk output that the model will never be given.
    ///
    /// Use it when a tool can produce far more than a model needs but exactly
    /// what a person asked for. The `content` half should then be a receipt:
    /// how many rows were delivered, a small sample, and — importantly — an
    /// instruction not to re-query in order to enumerate what the user can
    /// already see.
    pub fn with_bulk(mut self, bulk: impl Into<String>) -> Self {
        let bulk = bulk.into();
        if !bulk.is_empty() {
            self.bulk = Some(bulk);
        }
        self
    }

    pub fn success_text(text: impl Into<String>) -> Self {
        Self::success(vec![Content::text(text)])
    }

    pub fn success_json<T: Serialize>(value: T) -> Self {
        Self::success_text(serde_json::to_string_pretty(&value).unwrap_or_default())
    }

    pub fn error(message: impl Into<String>) -> Self {
        Self {
            content: vec![Content::text(message)],
            is_error: Some(true),
            bulk: None,
        }
    }

    /// The model-facing text (`content` only — never `bulk`).
    pub fn model_text(&self) -> String {
        self.content
            .iter()
            .map(|c| match c {
                Content::Text { text } => text.as_str(),
                _ => "",
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Everything a person should see: the model-facing text plus any bulk.
    pub fn full_text(&self) -> String {
        match &self.bulk {
            Some(b) => format!("{}\n\n{}", self.model_text(), b),
            None => self.model_text(),
        }
    }
}

/// Parse tool arguments from an optional JSON value into a typed struct.
/// Returns a `ToolCallResult::error` if deserialization fails.
pub fn parse_args<T: DeserializeOwned>(arguments: Option<Value>) -> Result<T, ToolCallResult> {
    let args = arguments.unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(args)
        .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {}", e)))
}
