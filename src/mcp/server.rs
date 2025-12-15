//! MCP server implementation.

use std::sync::Arc;

use serde_json::json;
use thiserror::Error;
use tracing::{debug, error, info, warn};

use crate::repository::Neo4jClient;
use crate::services::{CredentialManager, LlmConfig};

use super::protocol::{
    IncomingMessage, InitializeParams, InitializeResult, JsonRpcErrorResponse, JsonRpcRequest,
    JsonRpcResponse, MCP_PROTOCOL_VERSION, ServerCapabilities, ServerInfo, ToolCallParams,
    ToolsCapability, ToolsListResult, error_codes,
};
use super::tools::{ToolHandler, ToolRegistry};
use super::transport::{OutgoingMessage, StdioTransport};

#[derive(Debug, Error)]
pub enum McpServerError {
    #[error("Transport error: {0}")]
    Transport(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Server not initialized")]
    NotInitialized,

    #[error("Server already initialized")]
    AlreadyInitialized,
}

/// MCP server state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ServerState {
    /// Waiting for initialize request.
    Created,
    /// Initialize received, waiting for initialized notification.
    Initializing,
    /// Ready to handle requests.
    Running,
    /// Shutdown requested.
    ShuttingDown,
}

/// MCP server configuration.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Server name.
    pub name: String,
    /// Server version.
    pub version: String,
    /// Server instructions/description.
    pub instructions: Option<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: "agent-api".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            instructions: Some(
                "Autonomous API Knowledge Graph server. \
                 Ingest OpenAPI specs, query endpoints, and execute requests with self-healing."
                    .to_string(),
            ),
        }
    }
}

/// MCP server for the API Knowledge Graph.
pub struct McpServer {
    config: McpServerConfig,
    state: ServerState,
    tool_registry: ToolRegistry,
    tool_handler: ToolHandler,
}

impl McpServer {
    /// Create a new MCP server with default configuration.
    pub fn new() -> Self {
        Self {
            config: McpServerConfig::default(),
            state: ServerState::Created,
            tool_registry: ToolRegistry::new(),
            tool_handler: ToolHandler::new(),
        }
    }

    /// Create a new MCP server with custom configuration.
    pub fn with_config(config: McpServerConfig) -> Self {
        Self {
            config,
            state: ServerState::Created,
            tool_registry: ToolRegistry::new(),
            tool_handler: ToolHandler::new(),
        }
    }

    /// Set the Neo4j client for database operations.
    pub fn with_neo4j(mut self, neo4j: Neo4jClient) -> Self {
        self.tool_handler = ToolHandler::with_neo4j(neo4j);
        self
    }

    /// Set the LLM configuration for healing.
    pub fn with_llm_config(mut self, config: LlmConfig) -> Self {
        self.tool_handler = self.tool_handler.with_llm_config(config);
        self
    }

    /// Set the credential manager for API authentication.
    pub fn with_credential_manager(mut self, manager: Arc<CredentialManager>) -> Self {
        self.tool_handler = self.tool_handler.with_credential_manager(manager);
        self
    }

    /// Run the MCP server with stdio transport.
    pub async fn run(mut self) -> Result<(), McpServerError> {
        info!(
            name = %self.config.name,
            version = %self.config.version,
            "Starting MCP server"
        );

        let (transport, mut rx) = StdioTransport::new();

        // Main message loop
        while let Some(result) = rx.recv().await {
            match result {
                Ok(message) => {
                    if let Some(response) = self.handle_message(message).await
                        && transport.send(response).await.is_err()
                    {
                        error!("Failed to send response - transport closed");
                        break;
                    }

                    // Check if we should shut down
                    if self.state == ServerState::ShuttingDown {
                        info!("Server shutting down");
                        break;
                    }
                }
                Err(error) => {
                    warn!(error = ?error, "Received malformed message");
                    if transport.send_error(error).await.is_err() {
                        break;
                    }
                }
            }
        }

        info!("MCP server stopped");
        Ok(())
    }

    /// Handle an incoming message and optionally return a response.
    async fn handle_message(&mut self, message: IncomingMessage) -> Option<OutgoingMessage> {
        match message {
            IncomingMessage::Request(request) => Some(self.handle_request(request).await),
            IncomingMessage::Notification(notification) => {
                self.handle_notification(&notification.method);
                None
            }
        }
    }

    /// Handle a JSON-RPC request.
    async fn handle_request(&mut self, request: JsonRpcRequest) -> OutgoingMessage {
        debug!(method = %request.method, id = ?request.id, "Handling request");

        let response = match request.method.as_str() {
            "initialize" => self.handle_initialize(&request),
            "shutdown" => self.handle_shutdown(&request),
            "tools/list" => self.handle_tools_list(&request),
            "tools/call" => self.handle_tools_call(&request).await,
            "ping" => self.handle_ping(&request),
            _ => {
                if self.state != ServerState::Running {
                    Err(JsonRpcErrorResponse::new(
                        Some(request.id.clone()),
                        error_codes::INVALID_REQUEST,
                        "Server not initialized",
                    ))
                } else {
                    Err(JsonRpcErrorResponse::method_not_found(
                        request.id.clone(),
                        &request.method,
                    ))
                }
            }
        };

        match response {
            Ok(result) => OutgoingMessage::Response(result),
            Err(error) => OutgoingMessage::Error(error),
        }
    }

    /// Handle a notification (no response expected).
    fn handle_notification(&mut self, method: &str) {
        debug!(method = %method, "Handling notification");

        match method {
            "notifications/initialized" => {
                if self.state == ServerState::Initializing {
                    self.state = ServerState::Running;
                    info!("Server initialized and ready");
                }
            }
            "notifications/cancelled" => {
                debug!("Request cancelled");
            }
            _ => {
                debug!(method = %method, "Unknown notification");
            }
        }
    }

    // ========================================================================
    // Request Handlers
    // ========================================================================

    fn handle_initialize(
        &mut self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        if self.state != ServerState::Created {
            return Err(JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INVALID_REQUEST,
                "Server already initialized",
            ));
        }

        // Parse initialize params
        let params: InitializeParams = request
            .params
            .as_ref()
            .map(|p| serde_json::from_value(p.clone()))
            .transpose()
            .map_err(|e| {
                JsonRpcErrorResponse::invalid_params(
                    request.id.clone(),
                    format!("Invalid initialize params: {}", e),
                )
            })?
            .ok_or_else(|| {
                JsonRpcErrorResponse::invalid_params(
                    request.id.clone(),
                    "Missing initialize params",
                )
            })?;

        info!(
            client = %params.client_info.name,
            protocol_version = %params.protocol_version,
            "Client connecting"
        );

        self.state = ServerState::Initializing;

        let result = InitializeResult {
            protocol_version: MCP_PROTOCOL_VERSION.to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: false,
                }),
                resources: None,
                prompts: None,
            },
            server_info: ServerInfo {
                name: self.config.name.clone(),
                version: self.config.version.clone(),
            },
            instructions: self.config.instructions.clone(),
        };

        Ok(JsonRpcResponse::new(
            request.id.clone(),
            serde_json::to_value(result).unwrap(),
        ))
    }

    fn handle_shutdown(
        &mut self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        info!("Shutdown requested");
        self.state = ServerState::ShuttingDown;
        Ok(JsonRpcResponse::new(request.id.clone(), json!(null)))
    }

    fn handle_tools_list(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        if self.state != ServerState::Running {
            return Err(JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INVALID_REQUEST,
                "Server not initialized",
            ));
        }

        let result = ToolsListResult {
            tools: self.tool_registry.list().to_vec(),
        };

        Ok(JsonRpcResponse::new(
            request.id.clone(),
            serde_json::to_value(result).unwrap(),
        ))
    }

    async fn handle_tools_call(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        if self.state != ServerState::Running {
            return Err(JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INVALID_REQUEST,
                "Server not initialized",
            ));
        }

        // Parse tool call params
        let params: ToolCallParams = request
            .params
            .as_ref()
            .map(|p| serde_json::from_value(p.clone()))
            .transpose()
            .map_err(|e| {
                JsonRpcErrorResponse::invalid_params(
                    request.id.clone(),
                    format!("Invalid tool call params: {}", e),
                )
            })?
            .ok_or_else(|| {
                JsonRpcErrorResponse::invalid_params(request.id.clone(), "Missing tool call params")
            })?;

        // Check if tool exists
        if self.tool_registry.get(&params.name).is_none() {
            return Err(JsonRpcErrorResponse::invalid_params(
                request.id.clone(),
                format!("Unknown tool: {}", params.name),
            ));
        }

        // Execute the tool
        let result = self
            .tool_handler
            .execute(&params.name, params.arguments)
            .await;

        Ok(JsonRpcResponse::new(
            request.id.clone(),
            serde_json::to_value(result).unwrap(),
        ))
    }

    fn handle_ping(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        Ok(JsonRpcResponse::new(request.id.clone(), json!({})))
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_default() {
        let config = McpServerConfig::default();
        assert_eq!(config.name, "agent-api");
        assert!(config.instructions.is_some());
    }

    #[test]
    fn test_server_creation() {
        let server = McpServer::new();
        assert_eq!(server.state, ServerState::Created);
    }

    #[test]
    fn test_server_with_config() {
        let config = McpServerConfig {
            name: "test-server".to_string(),
            version: "1.0.0".to_string(),
            instructions: None,
        };
        let server = McpServer::with_config(config);
        assert_eq!(server.config.name, "test-server");
    }

    #[test]
    fn test_server_state_transitions() {
        let mut server = McpServer::new();
        assert_eq!(server.state, ServerState::Created);

        // Simulate initialize
        server.state = ServerState::Initializing;
        assert_eq!(server.state, ServerState::Initializing);

        // Simulate initialized notification
        server.handle_notification("notifications/initialized");
        assert_eq!(server.state, ServerState::Running);
    }

    #[test]
    fn test_handle_unknown_notification() {
        let mut server = McpServer::new();
        server.state = ServerState::Running;
        // Should not panic
        server.handle_notification("unknown_notification");
    }

    #[test]
    fn test_initialized_notification_requires_full_method_path() {
        // Regression test: short method name "initialized" should NOT transition state
        let mut server = McpServer::new();
        server.state = ServerState::Initializing;

        // Short name should be ignored (treated as unknown)
        server.handle_notification("initialized");
        assert_eq!(
            server.state,
            ServerState::Initializing,
            "Short method name 'initialized' should not transition state"
        );

        // Full path should work
        server.handle_notification("notifications/initialized");
        assert_eq!(server.state, ServerState::Running);
    }

    #[test]
    fn test_initialized_notification_only_from_initializing_state() {
        let mut server = McpServer::new();

        // From Created state - should not transition
        server.handle_notification("notifications/initialized");
        assert_eq!(server.state, ServerState::Created);

        // From Running state - should stay Running
        server.state = ServerState::Running;
        server.handle_notification("notifications/initialized");
        assert_eq!(server.state, ServerState::Running);

        // From Initializing state - should transition to Running
        server.state = ServerState::Initializing;
        server.handle_notification("notifications/initialized");
        assert_eq!(server.state, ServerState::Running);
    }

    #[test]
    fn test_cancelled_notification_requires_full_method_path() {
        let mut server = McpServer::new();
        server.state = ServerState::Running;

        // Both should be handled without panic, but only full path is recognized
        server.handle_notification("cancelled");
        server.handle_notification("notifications/cancelled");
    }
}
