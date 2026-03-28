//! MCP (Model Context Protocol) server implementation.
//!
//! This module provides a minimal MCP server that exposes the following tools:
//! - `ingest_openapi`: Parse and load OpenAPI specifications into the knowledge graph
//! - `graph_query_endpoint`: Search for API endpoints in the knowledge graph
//! - `execute_http_request`: Execute HTTP requests with optional self-healing
//!
//! ## Transport Support
//!
//! The server supports multiple transports via the `McpTransport` trait:
//! - `StdioTransport`: Standard input/output (default, for local CLI usage)
//! - `HttpTransport`: Streamable HTTP with SSE (for remote/cloud deployment)

#[cfg(feature = "http-transport")]
pub mod auth;
#[cfg(feature = "http-transport")]
pub mod http_transport;
pub mod protocol;
pub mod server;
pub mod session;
pub mod tools;
pub mod transport;
pub mod transport_trait;

#[cfg(feature = "http-transport")]
pub use auth::{ApiKeyAuth, AuthConfig, AuthError};
#[cfg(feature = "http-transport")]
pub use http_transport::{HttpTransport, HttpTransportConfig};
pub use protocol::{
    Content, InitializeResult, JsonRpcError, JsonRpcErrorResponse, JsonRpcRequest, JsonRpcResponse,
    RequestId, ToolCallResult, ToolDefinition,
};
pub use server::{McpServer, McpServerConfig, McpServerCore, McpServerError, ServerState};
pub use session::{Session, SessionConfig, SessionError, SessionManager, SessionState, SseMessage};
pub use tools::{ToolHandler, ToolRegistry};
pub use transport::StdioTransport;
pub use transport_trait::{McpTransport, TransportConfig, TransportError, TransportMessage};
