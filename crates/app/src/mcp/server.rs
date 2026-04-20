//! MCP server implementation.
//!
//! [`McpServerCore`] is the MCP protocol adapter.  It owns only the things
//! specific to the MCP protocol — the JSON-RPC state machine, the session
//! manager for HTTP SSE, and the optional per-connection tool-profile filter.
//!
//! All brain logic (skill registry, LLM config, background jobs, storage) lives
//! in [`BrainCore`] which `McpServerCore` holds and delegates to.

use std::sync::Arc;

use serde_json::json;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::brain_core::BrainCore;
use crate::clients::chat::ChatService;
use crate::repository::Neo4jClient;
use crate::services::{LlmConfig, SchedulerService};

use super::protocol::{
    IncomingMessage, InitializeParams, InitializeResult, JsonRpcErrorResponse, JsonRpcRequest,
    JsonRpcResponse, MCP_PROTOCOL_VERSION, ServerCapabilities, ServerInfo, ToolCallParams,
    ToolsCapability, ToolsListResult, error_codes,
};
use super::session::{SessionManager, SessionState};
use super::transport::{OutgoingMessage, StdioTransport};
use super::transport_trait::{McpTransport, TransportMessage};

// Re-export sub-configs so existing external callers (e.g. tests) that import
// them from `crate::mcp::server` still compile.
pub use crate::brain_core::{CodebaseConfig, JobServices, SearchConfig, StorageConfig};

// ============================================================================
// Errors / State
// ============================================================================

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
pub enum ServerState {
    /// Waiting for initialize request.
    Created,
    /// Initialize received, waiting for initialized notification.
    Initializing,
    /// Ready to handle requests.
    Running,
    /// Shutdown requested.
    ShuttingDown,
}

// ============================================================================
// McpServerConfig
// ============================================================================

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
            name: "agent-brain".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            instructions: Some(
                "General Intelligence Agent Core with Graph RAG and MCP. \
                 Manage long-term memory, execute tasks, and learn from feedback."
                    .to_string(),
            ),
        }
    }
}

// ============================================================================
// McpServerCore — MCP protocol adapter
// ============================================================================

/// Thread-safe MCP protocol adapter.
///
/// Owns the MCP handshake state machine and session management.  All brain
/// logic is delegated to the inner [`BrainCore`].
pub struct McpServerCore {
    config: McpServerConfig,
    pub(crate) state: Arc<RwLock<ServerState>>,
    session_manager: Option<Arc<SessionManager>>,
    /// Optional context-profile name used to filter `tools/list` responses.
    mcp_tool_profile: Option<String>,
    /// The brain — owns storage, LLM, skills, scheduler, queue.
    pub brain: BrainCore,
    /// Separate LLM config for the chat client adapter.
    ///
    /// When `None`, [`chat_service`] falls back to `brain.llm_config` so that
    /// existing single-model deployments continue to work without any config
    /// changes.  Set via [`with_chat_llm_config`] to run a different model
    /// (e.g. cloud Anthropic) for human chat while keeping brain internals on
    /// local Ollama.
    chat_llm_config: Option<Arc<RwLock<Option<LlmConfig>>>>,
}

impl McpServerCore {
    /// Create a new server core with default configuration.
    pub fn new() -> Self {
        Self::with_config(McpServerConfig::default())
    }

    /// Create a new server core with custom configuration.
    pub fn with_config(config: McpServerConfig) -> Self {
        Self {
            config,
            state: Arc::new(RwLock::new(ServerState::Created)),
            session_manager: None,
            mcp_tool_profile: std::env::var("MCP_TOOL_PROFILE").ok(),
            brain: BrainCore::new(),
            chat_llm_config: None,
        }
    }

    // ── Builder methods ───────────────────────────────────────────────────

    /// Set the session manager for HTTP transport.
    pub fn with_session_manager(mut self, manager: Arc<SessionManager>) -> Self {
        self.session_manager = Some(manager);
        self
    }

    /// Set the Neo4j client for database operations.
    pub fn with_neo4j(mut self, neo4j: Neo4jClient) -> Self {
        self.brain = self.brain.with_neo4j(neo4j);
        self
    }

    /// Set the Telemetry client for logging.
    pub fn with_telemetry(mut self, telemetry: crate::repository::TelemetryClient) -> Self {
        self.brain = self.brain.with_telemetry(telemetry);
        self
    }

    /// Set the LLM configuration.
    pub fn with_llm_config(mut self, config: LlmConfig) -> Self {
        self.brain = self.brain.with_llm_config(config);
        self
    }

    /// Set the Brave API Key for searching.
    pub fn with_brave_api_key(mut self, key: impl Into<String>) -> Self {
        self.brain = self.brain.with_brave_api_key(key);
        self
    }

    /// Set the Google API Key and CX for searching.
    pub fn with_google_config(mut self, key: impl Into<String>, cx: impl Into<String>) -> Self {
        self.brain = self.brain.with_google_config(key, cx);
        self
    }

    /// Set the SerpApi Key for searching.
    pub fn with_serpapi_key(mut self, key: impl Into<String>) -> Self {
        self.brain = self.brain.with_serpapi_key(key);
        self
    }

    /// Set the active system prompt (from model catalog).
    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.brain = self.brain.with_system_prompt(prompt);
        self
    }

    /// Set the path to models.yaml (forwarded to ModelSkill for hot-reload).
    pub fn with_catalog_path(mut self, path: std::path::PathBuf) -> Self {
        self.brain = self.brain.with_catalog_path(path);
        self
    }

    // ── Delegated brain accessors ─────────────────────────────────────────

    /// Set a separate LLM configuration for the human-facing chat adapter.
    ///
    /// When called, [`chat_service`] will use this config instead of
    /// `brain.llm_config`, allowing chat to run a different provider/model
    /// than the brain's internal cognitive operations.
    ///
    /// When NOT called, chat falls back to `brain.llm_config` (backward-
    /// compatible: existing single-model deployments need no changes).
    pub fn with_chat_llm_config(mut self, config: LlmConfig) -> Self {
        self.chat_llm_config = Some(Arc::new(RwLock::new(Some(config))));
        self
    }

    /// Return a live [`ChatService`] wired to the brain's tool registry.
    ///
    /// Uses the dedicated chat LLM config when one was provided via
    /// [`with_chat_llm_config`], otherwise shares `brain.llm_config`.
    pub fn chat_service(&self) -> Arc<ChatService> {
        let llm = self
            .chat_llm_config
            .as_ref()
            .map(Arc::clone)
            .unwrap_or_else(|| Arc::clone(&self.brain.llm_config));

        ChatService::with_context_builder(
            Arc::clone(&self.brain.tool_handler),
            Arc::clone(&self.brain.tool_registry),
            llm,
            Arc::clone(&self.brain.context_builder_svc),
        )
    }

    /// Return the scheduler `Arc` handle so callers can attach it to HTTP
    /// transport config before `run_with_transport` is called.
    pub fn scheduler_handle(&self) -> Arc<RwLock<Option<Arc<SchedulerService>>>> {
        self.brain.scheduler_handle()
    }

    /// Return the context-builder handle for wiring into the REST adapter.
    pub fn context_builder_handle(
        &self,
    ) -> Arc<RwLock<Option<Arc<crate::services::ContextBuilderService>>>> {
        self.brain.context_builder_handle()
    }

    /// Return the live LLM-config Arc for the `/api/models` REST endpoint.
    pub fn llm_config_arc(&self) -> Arc<RwLock<Option<crate::services::LlmConfig>>> {
        self.brain.llm_config_arc()
    }

    /// Return a clone of the telemetry client for the `/api/models` REST endpoint.
    pub fn telemetry(&self) -> Option<crate::repository::TelemetryClient> {
        self.brain.telemetry()
    }

    /// Return the tool registry Arc for the `/api/skills` REST endpoint.
    pub fn tool_registry_handle(
        &self,
    ) -> Arc<RwLock<crate::mcp::tools::ToolRegistry>> {
        self.brain.tool_registry_handle()
    }

    // ── MCP state management ──────────────────────────────────────────────

    /// Get the current server state.
    pub async fn get_state(&self) -> ServerState {
        *self.state.read().await
    }

    /// Get the state for a specific session, falling back to global state.
    pub async fn get_session_state(&self, session_id: Option<&str>) -> ServerState {
        if let (Some(id), Some(manager)) = (session_id, &self.session_manager)
            && let Ok(state) = manager.get_session_state(id).await
        {
            return ServerState::from(state);
        }
        *self.state.read().await
    }

    /// Update the state for a specific session and the global state.
    pub async fn update_session_state(&self, session_id: Option<&str>, new_state: ServerState) {
        if let (Some(id), Some(manager)) = (session_id, &self.session_manager) {
            let _ = manager
                .set_session_state(id, SessionState::from(new_state))
                .await;
        }
        // Always update global state for backward compatibility with stdio
        let mut state = self.state.write().await;
        *state = new_state;
    }

    /// Check if the server is shutting down.
    pub async fn is_shutting_down(&self) -> bool {
        *self.state.read().await == ServerState::ShuttingDown
    }

    // ── Initialization ────────────────────────────────────────────────────

    /// Build skills and run the boot protocol.  Called automatically by
    /// `run_with_transport`; expose publicly for callers that need to
    /// pre-initialize (e.g. test harnesses, the stdio path).
    pub async fn initialize(&self) {
        self.brain.initialize().await;
    }

    /// Build skills without running the boot protocol.  Used by the legacy
    /// stdio path (`McpServer::run`) which handles the boot protocol itself.
    pub async fn build_skills(&self) {
        self.brain.build_skills().await;
    }

    // ── Transport runner ──────────────────────────────────────────────────

    /// Run the server with a specific transport implementation.
    pub async fn run_with_transport<T: McpTransport>(
        &self,
        transport: &T,
    ) -> Result<(), McpServerError> {
        self.initialize().await;

        info!(
            name = %self.config.name,
            version = %self.config.version,
            transport = %transport.name(),
            "Starting MCP server"
        );

        let mut rx = transport
            .start()
            .await
            .map_err(|e| McpServerError::Transport(e.to_string()))?;

        // Main message loop
        while let Some(msg) = rx.recv().await {
            match msg {
                TransportMessage::Request {
                    session_id: _,
                    request,
                    response_tx,
                } => {
                    let response = self.handle_request(request).await;
                    let _ = response_tx.send(response);

                    if self.is_shutting_down().await {
                        info!("Server shutting down");
                        break;
                    }
                }
                TransportMessage::Notification {
                    session_id: _,
                    notification,
                } => {
                    self.handle_notification(&notification.method).await;
                }
            }
        }

        transport
            .shutdown()
            .await
            .map_err(|e| McpServerError::Transport(e.to_string()))?;

        info!("MCP server stopped");
        Ok(())
    }

    // ── Request / notification handlers ──────────────────────────────────

    /// Handle an incoming JSON-RPC request (thread-safe).
    pub async fn handle_request(&self, request: JsonRpcRequest) -> OutgoingMessage {
        debug!(method = %request.method, id = ?request.id, "Handling request");

        let response = match request.method.as_str() {
            "initialize" => self.handle_initialize(&request).await,
            "shutdown" => self.handle_shutdown(&request).await,
            "tools/list" => self.handle_tools_list(&request).await,
            "tools/call" => self.handle_tools_call(&request).await,
            "ping" => self.handle_ping(&request),
            _ => {
                let state = self.get_state().await;
                if state != ServerState::Running {
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

    /// Handle a notification (thread-safe, no response expected).
    pub async fn handle_notification(&self, method: &str) {
        debug!(method = %method, "Handling notification");

        match method {
            "notifications/initialized" => {
                let mut state = self.state.write().await;
                if *state == ServerState::Initializing || *state == ServerState::Created {
                    *state = ServerState::Running;
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

    // ── Individual request handlers ───────────────────────────────────────

    async fn handle_initialize(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        let current_state = {
            let state = self.state.read().await;
            *state
        };

        if current_state == ServerState::ShuttingDown {
            return Err(JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INVALID_REQUEST,
                "Server is shutting down",
            ));
        }

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
            already_running = (current_state == ServerState::Running),
            "Client connecting"
        );

        if current_state == ServerState::Created {
            let mut state = self.state.write().await;
            *state = ServerState::Initializing;
        }

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

    async fn handle_shutdown(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        info!("Shutdown requested");
        {
            let mut state = self.state.write().await;
            *state = ServerState::ShuttingDown;
        }
        Ok(JsonRpcResponse::new(request.id.clone(), json!(null)))
    }

    async fn handle_tools_list(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        let state = self.get_state().await;
        if state != ServerState::Running {
            return Err(JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INVALID_REQUEST,
                "Server not initialized",
            ));
        }

        let tools = if let Some(profile_name) = &self.mcp_tool_profile {
            self.brain.list_tools_filtered(profile_name).await
        } else {
            self.brain.list_tools().await
        };

        let result = ToolsListResult { tools };
        Ok(JsonRpcResponse::new(
            request.id.clone(),
            serde_json::to_value(result).unwrap(),
        ))
    }

    async fn handle_tools_call(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        let state = self.get_state().await;
        if state != ServerState::Running {
            return Err(JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INVALID_REQUEST,
                "Server not initialized",
            ));
        }

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

        // Existence check (releases lock before the execute await).
        if !self.brain.has_tool(&params.name).await {
            return Err(JsonRpcErrorResponse::invalid_params(
                request.id.clone(),
                format!("Unknown tool: {}", params.name),
            ));
        }

        // Execute through brain.
        let result = self
            .brain
            .try_execute_tool(&params.name, params.arguments)
            .await
            .map_err(|e| {
                JsonRpcErrorResponse::new(Some(request.id.clone()), error_codes::INTERNAL_ERROR, &e)
            })?;

        // Notify the scheduler that activity occurred (wakes sleep mode).
        self.brain.notify_scheduler_activity().await;

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

impl Default for McpServerCore {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Legacy McpServer — wraps McpServerCore for stdio backward compatibility
// ============================================================================

/// MCP server for the Agent Brain (stdio transport).
///
/// Thin wrapper around [`McpServerCore`] maintained for backward compatibility
/// with the legacy stdio transport path.  For new code, use `McpServerCore`
/// directly with an explicit transport.
pub struct McpServer {
    core: McpServerCore,
}

impl McpServer {
    pub fn new() -> Self {
        Self {
            core: McpServerCore::new(),
        }
    }

    pub fn with_config(config: McpServerConfig) -> Self {
        Self {
            core: McpServerCore::with_config(config),
        }
    }

    pub fn with_neo4j(mut self, neo4j: Neo4jClient) -> Self {
        self.core = self.core.with_neo4j(neo4j);
        self
    }

    pub fn with_llm_config(mut self, config: LlmConfig) -> Self {
        self.core = self.core.with_llm_config(config);
        self
    }

    /// Run the MCP server with stdio transport.
    pub async fn run(self) -> Result<(), McpServerError> {
        self.core.build_skills().await;

        info!(
            name = %self.core.config.name,
            version = %self.core.config.version,
            "Starting MCP server (stdio)"
        );

        let (transport, mut rx) = StdioTransport::new();

        while let Some(result) = rx.recv().await {
            match result {
                Ok(message) => {
                    if let Some(response) = self.handle_message(message).await
                        && transport.send(response).await.is_err()
                    {
                        error!("Failed to send response - transport closed");
                        break;
                    }
                    if self.core.is_shutting_down().await {
                        info!("Server shutting down");
                        break;
                    }
                }
                Err(error) => {
                    warn!(error = ?error, "Received malformed message");
                    if transport.send(OutgoingMessage::Error(error)).await.is_err() {
                        break;
                    }
                }
            }
        }

        info!("MCP server stopped");
        Ok(())
    }

    async fn handle_message(&self, message: IncomingMessage) -> Option<OutgoingMessage> {
        match message {
            IncomingMessage::Request(request) => Some(self.core.handle_request(request).await),
            IncomingMessage::Notification(notification) => {
                self.core.handle_notification(&notification.method).await;
                None
            }
        }
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_server_core_initial_state() {
        let server = McpServerCore::new();
        assert_eq!(server.get_state().await, ServerState::Created);
    }

    #[tokio::test]
    async fn test_server_core_initialized_notification_only_from_initializing_state() {
        let server = McpServerCore::new();

        // From Created state - transitions to Running (session resurrection support)
        server
            .handle_notification("notifications/initialized")
            .await;
        assert_eq!(server.get_state().await, ServerState::Running);

        // From Running state - should stay Running
        {
            let mut state = server.state.write().await;
            *state = ServerState::Running;
        }
        server
            .handle_notification("notifications/initialized")
            .await;
        assert_eq!(server.get_state().await, ServerState::Running);

        // From Initializing state - should transition to Running
        {
            let mut state = server.state.write().await;
            *state = ServerState::Initializing;
        }
        server
            .handle_notification("notifications/initialized")
            .await;
        assert_eq!(server.get_state().await, ServerState::Running);
    }
}
