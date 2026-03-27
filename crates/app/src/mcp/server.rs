//! MCP server implementation.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::{ChatService, LlmConfig};
use crate::services::queue::QueueService;
use crate::services::SchedulerService;
use crate::skills::{
    Skill,
    agent::AgentSkill,
    dynamic::DynamicSkill,
    knowledge::KnowledgeSkill,
    model::ModelSkill,
    procedure::ProcedureSkill,
    scheduler::SchedulerSkill,
    sleep::SleepSkill,
    task::TaskSkill,
    search::SearchSkill,
    working_memory::WorkingMemorySkill,
};

use super::session::{SessionManager, SessionState};
use super::protocol::{
    IncomingMessage, InitializeParams, InitializeResult, JsonRpcErrorResponse, JsonRpcRequest,
    JsonRpcResponse, MCP_PROTOCOL_VERSION, ServerCapabilities, ServerInfo, ToolCallParams,
    ToolsCapability, ToolsListResult, error_codes,
};
use super::tools::{ToolHandler, ToolRegistry};
use super::transport::{OutgoingMessage, StdioTransport};
use super::transport_trait::{McpTransport, TransportMessage};

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

/// Thread-safe MCP server core that works with any transport.
///
/// This struct contains the shared state that can be safely accessed
/// from multiple async tasks concurrently.
pub struct McpServerCore {
    config: McpServerConfig,
    pub(crate) state: Arc<RwLock<ServerState>>,
    tool_registry: Arc<RwLock<ToolRegistry>>,
    tool_handler: Arc<RwLock<Option<ToolHandler>>>,

    // Session management for HTTP
    session_manager: Option<Arc<SessionManager>>,

    // Configuration state needed to build skills
    neo4j: Option<Neo4jClient>,
    telemetry: Option<TelemetryClient>,
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
    // Search Config
    brave_api_key: Option<String>,
    google_api_key: Option<String>,
    google_cx: Option<String>,
    serpapi_key: Option<String>,

    // Sleep / training-data export directory
    dataset_dir: PathBuf,

    // Background job queue (created in build_skills when neo4j is available)
    queue_service: Arc<RwLock<Option<Arc<QueueService>>>>,

    // Autonomous scheduler (created in build_skills after queue is ready)
    scheduler_service: Arc<RwLock<Option<Arc<SchedulerService>>>>,
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
            tool_registry: Arc::new(RwLock::new(ToolRegistry::new())),
            tool_handler: Arc::new(RwLock::new(None)),
            session_manager: None,
            neo4j: None,
            telemetry: None,
            llm_config: Arc::new(RwLock::new(None)),
            brave_api_key: std::env::var("BRAVE_API_KEY").ok(),
            google_api_key: std::env::var("GOOGLE_API_KEY").ok(),
            google_cx: std::env::var("GOOGLE_CX").ok(),
            serpapi_key: std::env::var("SERPAPI_KEY").ok(),
            dataset_dir: std::env::var("DATASET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./datasets")),
            queue_service: Arc::new(RwLock::new(None)),
            scheduler_service: Arc::new(RwLock::new(None)),
        }
    }

    /// Set the session manager for HTTP transport.
    pub fn with_session_manager(mut self, manager: Arc<SessionManager>) -> Self {
        self.session_manager = Some(manager);
        self
    }

    /// Get the current server state.
    pub async fn get_state(&self) -> ServerState {
        *self.state.read().await
    }

    /// Get the state for a specific session, falling back to global state.
    pub async fn get_session_state(&self, session_id: Option<&str>) -> ServerState {
        if let (Some(id), Some(manager)) = (session_id, &self.session_manager) {
            if let Ok(state) = manager.get_session_state(id).await {
                return ServerState::from(state);
            }
        }
        *self.state.read().await
    }

    /// Update the state for a specific session and the global state.
    pub async fn update_session_state(&self, session_id: Option<&str>, new_state: ServerState) {
        if let (Some(id), Some(manager)) = (session_id, &self.session_manager) {
            let _ = manager.set_session_state(id, SessionState::from(new_state)).await;
        }

        // Always update global state for backward compatibility with stdio
        let mut state = self.state.write().await;
        *state = new_state;
    }

    /// Set the Neo4j client for database operations.
    pub fn with_neo4j(mut self, neo4j: Neo4jClient) -> Self {
        self.neo4j = Some(neo4j);
        self
    }

    /// Set the Telemetry client for logging.
    pub fn with_telemetry(mut self, telemetry: TelemetryClient) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    /// Set the LLM configuration for healing.
    pub fn with_llm_config(mut self, config: LlmConfig) -> Self {
        self.llm_config = Arc::new(RwLock::new(Some(config)));
        self
    }

    /// Set the Brave API Key for searching.
    pub fn with_brave_api_key(mut self, key: impl Into<String>) -> Self {
        self.brave_api_key = Some(key.into());
        self
    }

    /// Set the Google API Key and CX for searching.
    pub fn with_google_config(mut self, key: impl Into<String>, cx: impl Into<String>) -> Self {
        self.google_api_key = Some(key.into());
        self.google_cx = Some(cx.into());
        self
    }

    /// Set the SerpApi Key for searching.
    pub fn with_serpapi_key(mut self, key: impl Into<String>) -> Self {
        self.serpapi_key = Some(key.into());
        self
    }

    /// Create a [`ChatService`] backed by this server's live tool handler,
    /// registry, and LLM config.
    ///
    /// Safe to call before or after [`build_skills`] — the `Arc` references
    /// will always see the most up-to-date state.
    pub fn chat_service(&self) -> Arc<ChatService> {
        ChatService::new(
            Arc::clone(&self.tool_handler),
            Arc::clone(&self.tool_registry),
            Arc::clone(&self.llm_config),
        )
    }

    /// Build the skills and initialize the tool handler.
    /// This should be called before running the server.
    pub async fn build_skills(&self) {
        // Build DynamicSkill first (before taking locks) so we can share the Arc.
        // Both the registry clone and the handler original share the same tools_map.
        let dynamic_skill = if let Some(neo4j) = &self.neo4j {
            let d = DynamicSkill::new(neo4j.clone(), self.tool_handler.clone());
            d.load_from_neo4j().await;
            Some(d)
        } else {
            None
        };

        // Create (or reuse) QueueService when Neo4j is available.
        let queue_arc: Option<Arc<QueueService>> = if let Some(neo4j) = &self.neo4j {
            let mut qs_guard = self.queue_service.write().await;
            if qs_guard.is_none() {
                let sse_notifier: Option<Arc<dyn agent_brain_protocol::SseNotifier>> =
                    self.session_manager.as_ref().map(|sm| Arc::clone(sm) as Arc<dyn agent_brain_protocol::SseNotifier>);
                let qs = Arc::new(QueueService::new(
                    neo4j.clone(),
                    self.tool_handler.clone(),
                    sse_notifier,
                ));
                qs.recover().await;
                *qs_guard = Some(Arc::clone(&qs));
            }
            qs_guard.as_ref().map(Arc::clone)
        } else {
            None
        };

        // Create (or reuse) SchedulerService when Neo4j + Queue are available.
        let scheduler_arc: Option<Arc<SchedulerService>> =
            if let (Some(neo4j), Some(qs)) = (&self.neo4j, &queue_arc) {
                let mut g = self.scheduler_service.write().await;
                if g.is_none() {
                    *g = Some(SchedulerService::new(neo4j.clone(), Arc::clone(qs)));
                }
                g.as_ref().map(Arc::clone)
            } else {
                None
            };

        let mut registry = self.tool_registry.write().await;

        // Clear registry to allow safe re-registration on reload.
        registry.clear();

        // Register Knowledge Skill
        if let Some(neo4j) = &self.neo4j {
            let knowledge_skill = KnowledgeSkill::new(neo4j.clone(), Arc::clone(&self.llm_config));
            registry.register_skill(Box::new(knowledge_skill));
        }

        // Register Task Skill
        let task_skill = TaskSkill::new(
            Arc::clone(&self.llm_config),
            self.neo4j.clone(),
            queue_arc.as_ref().map(Arc::clone),
        );
        registry.register_skill(Box::new(task_skill));

        // Register Procedure Skill
        if let Some(neo4j) = &self.neo4j {
            let procedure_skill = ProcedureSkill::new(neo4j.clone());
            registry.register_skill(Box::new(procedure_skill));
        }

        // Register Search Skill
        let search_skill = SearchSkill::new(
            self.telemetry.clone(),
            self.brave_api_key.clone(),
            self.google_api_key.clone(),
            self.google_cx.clone(),
            self.serpapi_key.clone(),
        );
        registry.register_skill(Box::new(search_skill));

        // Register Model Skill (shares Arc<RwLock<>> so runtime provider changes propagate)
        let model_skill = ModelSkill::new(self.llm_config.clone(), self.neo4j.clone());
        registry.register_skill(Box::new(model_skill));

        // Register Sleep Skill (requires telemetry / DuckDB)
        if let Some(ref telemetry) = self.telemetry {
            let sleep_skill = SleepSkill::new(telemetry.clone(), self.dataset_dir.clone());
            registry.register_skill(Box::new(sleep_skill));
        }

        // Register Working Memory Skill
        if let Some(neo4j) = &self.neo4j {
            let wm_skill = WorkingMemorySkill::new(neo4j.clone(), Arc::clone(&self.llm_config));
            registry.register_skill(Box::new(wm_skill));
        }

        // Register Agent Skill (queue management)
        if let Some(ref qs) = queue_arc {
            registry.register_skill(Box::new(AgentSkill::new(Arc::clone(qs))));
        }

        // Register Scheduler Skill
        if let Some(ref sched) = scheduler_arc {
            registry.register_skill(Box::new(SchedulerSkill::new(Arc::clone(sched))));
        }

        // Register DynamicSkill in registry (shared-map clone — registry sees live updates)
        if let Some(ref d) = dynamic_skill {
            registry.register_skill(Box::new(d.clone_shared()));
        }


        drop(registry);

        // Build handler skills list (re-creates non-dynamic skills; DynamicSkill original goes here)
        let mut skills: Vec<Box<dyn Skill>> = Vec::new();

        if let Some(neo4j) = &self.neo4j {
            skills.push(Box::new(KnowledgeSkill::new(
                neo4j.clone(),
                Arc::clone(&self.llm_config),
            )));
        }

        skills.push(Box::new(TaskSkill::new(
            Arc::clone(&self.llm_config),
            self.neo4j.clone(),
            queue_arc.as_ref().map(Arc::clone),
        )));

        if let Some(neo4j) = &self.neo4j {
            skills.push(Box::new(ProcedureSkill::new(neo4j.clone())));
        }

        skills.push(Box::new(SearchSkill::new(
            self.telemetry.clone(),
            self.brave_api_key.clone(),
            self.google_api_key.clone(),
            self.google_cx.clone(),
            self.serpapi_key.clone(),
        )));

        skills.push(Box::new(ModelSkill::new(self.llm_config.clone(), self.neo4j.clone())));

        if let Some(ref telemetry) = self.telemetry {
            skills.push(Box::new(SleepSkill::new(telemetry.clone(), self.dataset_dir.clone())));
        }

        if let Some(neo4j) = &self.neo4j {
            skills.push(Box::new(WorkingMemorySkill::new(neo4j.clone(), Arc::clone(&self.llm_config))));
        }

        // Agent Skill (queue management)
        if let Some(ref qs) = queue_arc {
            skills.push(Box::new(AgentSkill::new(Arc::clone(qs))));
        }

        // Scheduler Skill (autonomous self-improvement loop)
        if let Some(ref sched) = scheduler_arc {
            skills.push(Box::new(SchedulerSkill::new(Arc::clone(sched))));
        }

        // Push original DynamicSkill to handler (shares tools_map with registry clone)
        if let Some(d) = dynamic_skill {
            skills.push(Box::new(d));
        }


        let mut handler = self.tool_handler.write().await;
        let mut tool_handler = ToolHandler::new(skills);
        if let Some(ref tel) = self.telemetry {
            tool_handler = tool_handler.with_telemetry(tel.clone());
        }
        *handler = Some(tool_handler);

        // Spawn the queue coordinator now that the tool handler is populated.
        if let Some(qs) = queue_arc {
            QueueService::spawn_coordinator(qs);
        }
    }

    /// Check if the server is shutting down.
    pub async fn is_shutting_down(&self) -> bool {
        *self.state.read().await == ServerState::ShuttingDown
    }

    /// Run the server with a specific transport implementation.
    pub async fn run_with_transport<T: McpTransport>(
        &self,
        transport: &T,
    ) -> Result<(), McpServerError> {
        // Ensure skills are built
        self.build_skills().await;

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
                    // Send response back through the oneshot channel
                    let _ = response_tx.send(response);

                    // Check if we should shut down
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
                if *state == ServerState::Initializing {
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

    // ========================================================================
    // Request Handlers (thread-safe versions)
    // ========================================================================

    async fn handle_initialize(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        let current_state = {
            let state = self.state.read().await;
            *state
        };

        // Reject only if shutting down.
        if current_state == ServerState::ShuttingDown {
            return Err(JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INVALID_REQUEST,
                "Server is shutting down",
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
            already_running = (current_state == ServerState::Running),
            "Client connecting"
        );

        // Advance to Initializing only from Created state; already-running server
        // stays Running so existing sessions are not disrupted.
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

        let tools = {
            let registry = self.tool_registry.read().await;
            registry.list()
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

        // Check if tool exists (release lock before await)
        {
            let registry = self.tool_registry.read().await;
            if registry.get(&params.name).is_none() {
                return Err(JsonRpcErrorResponse::invalid_params(
                    request.id.clone(),
                    format!("Unknown tool: {}", params.name),
                ));
            }
        }

        // Clone handler to avoid holding the lock across the await
        let handler = {
            let guard = self.tool_handler.read().await;
            guard.clone()
        };

        let handler = handler.ok_or_else(|| {
            JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INTERNAL_ERROR,
                "Tool handler not initialized",
            )
        })?;

        // Execute the tool (lock is released)
        let result = handler.execute(&params.name, params.arguments).await;

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

/// MCP server for the API Knowledge Graph.
///
/// This is a thin wrapper around `McpServerCore` maintained for backward
/// compatibility with the legacy stdio transport path. For new code, use
/// `McpServerCore` directly with an explicit transport.
pub struct McpServer {
    core: McpServerCore,
}

impl McpServer {
    /// Create a new MCP server with default configuration.
    pub fn new() -> Self {
        Self {
            core: McpServerCore::new(),
        }
    }

    /// Create a new MCP server with custom configuration.
    pub fn with_config(config: McpServerConfig) -> Self {
        Self {
            core: McpServerCore::with_config(config),
        }
    }

    /// Set the Neo4j client for database operations.
    pub fn with_neo4j(mut self, neo4j: Neo4jClient) -> Self {
        self.core = self.core.with_neo4j(neo4j);
        self
    }

    /// Set the LLM configuration for healing.
    pub fn with_llm_config(mut self, config: LlmConfig) -> Self {
        self.core = self.core.with_llm_config(config);
        self
    }

    /// Run the MCP server with stdio transport.
    pub async fn run(self) -> Result<(), McpServerError> {
        // Build skills first
        self.core.build_skills().await;

        info!(
            name = %self.core.config.name,
            version = %self.core.config.version,
            "Starting MCP server (stdio)"
        );

        let (transport, mut rx) = StdioTransport::new();

        // Main message loop
        while let Some(result) = rx.recv().await {
            match result {
                Ok(message) => {
                    if let Some(response) = self.handle_message(message).await {
                        if transport.send(response).await.is_err() {
                            error!("Failed to send response - transport closed");
                            break;
                        }
                    }

                    // Check if we should shut down
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

    /// Handle an incoming message and optionally return a response.
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_default() {
        let config = McpServerConfig::default();
        assert_eq!(config.name, "agent-brain");
        assert!(config.instructions.is_some());
    }

    #[test]
    fn test_server_creation() {
        let _server = McpServer::new();
    }

    #[test]
    fn test_server_with_config() {
        let config = McpServerConfig {
            name: "test-server".to_string(),
            version: "1.0.0".to_string(),
            instructions: None,
        };
        let server = McpServer::with_config(config);
        assert_eq!(server.core.config.name, "test-server");
    }

    // ========================================================================
    // McpServerCore Tests (thread-safe version)
    // ========================================================================

    #[tokio::test]
    async fn test_server_core_creation() {
        let server = McpServerCore::new();
        assert_eq!(server.get_state().await, ServerState::Created);
    }

    #[tokio::test]
    async fn test_server_core_with_config() {
        let config = McpServerConfig {
            name: "test-server".to_string(),
            version: "1.0.0".to_string(),
            instructions: None,
        };
        let server = McpServerCore::with_config(config);
        assert_eq!(server.config.name, "test-server");
    }

    #[tokio::test]
    async fn test_server_core_state_transitions() {
        let server = McpServerCore::new();
        assert_eq!(server.get_state().await, ServerState::Created);

        // Simulate initialize
        {
            let mut state = server.state.write().await;
            *state = ServerState::Initializing;
        }
        assert_eq!(server.get_state().await, ServerState::Initializing);

        // Simulate initialized notification
        server.handle_notification("notifications/initialized").await;
        assert_eq!(server.get_state().await, ServerState::Running);
    }

    #[tokio::test]
    async fn test_server_core_is_shutting_down() {
        let server = McpServerCore::new();
        assert!(!server.is_shutting_down().await);

        {
            let mut state = server.state.write().await;
            *state = ServerState::ShuttingDown;
        }
        assert!(server.is_shutting_down().await);
    }

    #[tokio::test]
    async fn test_server_core_concurrent_state_access() {
        use std::sync::Arc;

        let server = Arc::new(McpServerCore::new());

        // Spawn multiple tasks reading state concurrently
        let mut handles = vec![];
        for _ in 0..10 {
            let server_clone = Arc::clone(&server);
            handles.push(tokio::spawn(async move {
                server_clone.get_state().await
            }));
        }

        // All should return Created
        for handle in handles {
            let state = handle.await.expect("Task panicked");
            assert_eq!(state, ServerState::Created);
        }
    }

    #[tokio::test]
    async fn test_server_core_handle_notification_thread_safe() {
        let server = McpServerCore::new();

        // Set state to Initializing
        {
            let mut state = server.state.write().await;
            *state = ServerState::Initializing;
        }

        // Handle notification should transition state
        server.handle_notification("notifications/initialized").await;
        assert_eq!(server.get_state().await, ServerState::Running);
    }

    #[tokio::test]
    async fn test_server_core_ping_request() {
        let server = McpServerCore::new();

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: super::super::protocol::RequestId::Number(1),
            method: "ping".to_string(),
            params: None,
        };

        let response = server.handle_request(request).await;
        match response {
            OutgoingMessage::Response(r) => {
                assert_eq!(r.result, serde_json::json!({}));
            }
            OutgoingMessage::Error(_) => panic!("Expected response, got error"),
        }
    }

    #[tokio::test]
    async fn test_server_core_initialized_notification_only_from_initializing_state() {
        let server = McpServerCore::new();

        // From Created state - should not transition
        server.handle_notification("notifications/initialized").await;
        assert_eq!(server.get_state().await, ServerState::Created);

        // From Running state - should stay Running
        {
            let mut state = server.state.write().await;
            *state = ServerState::Running;
        }
        server.handle_notification("notifications/initialized").await;
        assert_eq!(server.get_state().await, ServerState::Running);

        // From Initializing state - should transition to Running
        {
            let mut state = server.state.write().await;
            *state = ServerState::Initializing;
        }
        server.handle_notification("notifications/initialized").await;
        assert_eq!(server.get_state().await, ServerState::Running);
    }
}
