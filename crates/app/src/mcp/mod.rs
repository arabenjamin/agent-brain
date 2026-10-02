//! MCP (Model Context Protocol) server implementation.
//!
//! The MCP adapter in front of [`BrainCore`]: the JSON-RPC state machine,
//! session management, and the tool registry that dispatches `tools/call` to
//! the skills (see `crate::skills`).
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

pub use crate::brain_core::BrainCore;
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
