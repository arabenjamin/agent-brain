//! MCP (Model Context Protocol) server implementation.
//!
//! This module provides a minimal MCP server that exposes the following tools:
//! - `ingest_openapi`: Parse and load OpenAPI specifications into the knowledge graph
//! - `graph_query_endpoint`: Search for API endpoints in the knowledge graph
//! - `execute_http_request`: Execute HTTP requests with optional self-healing

pub mod protocol;
pub mod server;
pub mod tools;
pub mod transport;

pub use protocol::{
    Content, InitializeResult, JsonRpcError, JsonRpcErrorResponse, JsonRpcRequest, JsonRpcResponse,
    RequestId, ToolCallResult, ToolDefinition,
};
pub use server::{McpServer, McpServerConfig, McpServerError};
pub use tools::{ToolHandler, ToolRegistry};
pub use transport::StdioTransport;
