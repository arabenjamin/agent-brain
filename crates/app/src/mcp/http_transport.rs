//! HTTP transport for MCP server using Axum.
//!
//! This module provides an HTTP transport implementation following the MCP spec:
//! - POST /mcp for JSON-RPC requests
//! - GET /mcp for SSE streaming (server-initiated messages)
//! - DELETE /mcp for session termination

use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use axum::response::sse::{Event, KeepAlive};
use axum::{
    Json, Router,
    body::Body,
    extract::State,
    http::{HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode, header},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse},
    routing::{delete, get, post},
};
use futures_util::stream::{Stream, StreamExt};
use serde_json::Value;
use tokio::sync::{RwLock, broadcast, mpsc, oneshot};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, info, warn};

use crate::brain_core::BrainEvent;
use crate::clients::{ChatEvent, ChatRequest, ChatService, RestAdapter};
use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::LlmConfig;
use crate::services::context_builder::ContextBuilderService;
use crate::services::scheduler::SchedulerService;
use agent_brain_protocol::SseNotifier as _;

use super::auth::{ApiKeyAuth, AuthError};
use super::protocol::{IncomingMessage, JsonRpcErrorResponse, JsonRpcNotification};
use super::session::{SessionConfig, SessionManager, SessionState};
use super::transport::OutgoingMessage;
use super::transport_trait::{McpTransport, TransportError, TransportMessage};

/// MCP protocol version header name.
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
/// MCP session ID header name.
pub const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
/// Expected MCP protocol version.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Configuration for the HTTP transport.
#[derive(Clone)]
pub struct HttpTransportConfig {
    /// Address to bind the HTTP server to.
    pub bind_addr: SocketAddr,
    /// Optional API key for authentication.
    pub api_key: Option<String>,
    /// Allowed CORS origins (empty means allow all).
    pub allowed_origins: Vec<String>,
    /// Session timeout duration.
    pub session_timeout: Duration,
    /// Maximum number of concurrent sessions.
    pub max_sessions: usize,
    /// Channel buffer size for messages.
    pub channel_buffer_size: usize,
    /// Optional shared session manager.
    pub session_manager: Option<Arc<SessionManager>>,
    /// Optional chat service for the `/chat` SSE endpoint.
    pub chat_service: Option<Arc<ChatService>>,
    /// Optional Neo4j client for the `/todos` and `/scheduled-tasks` REST endpoints.
    pub neo4j: Option<Arc<Neo4jClient>>,
    /// Optional scheduler service handle for the `/scheduler-config` REST endpoints.
    /// Pass `server.scheduler_handle()` before calling `run_with_transport`.
    pub scheduler: Option<Arc<RwLock<Option<Arc<SchedulerService>>>>>,
    /// Optional brain event bus sender.  When set, the HTTP transport spawns a
    /// background task that subscribes to [`BrainEvent`]s and pushes
    /// `notifications/agent_job` SSE messages to the owning session.
    pub brain_event_sender: Option<broadcast::Sender<BrainEvent>>,
    /// Optional context-builder handle for the `/api/contexts` REST endpoints.
    pub context_builder: Option<Arc<RwLock<Option<Arc<ContextBuilderService>>>>>,
    /// Optional live LLM-config for the `/api/models` REST endpoint.
    pub llm_config: Option<Arc<RwLock<Option<LlmConfig>>>>,
    /// Optional telemetry client for the `/api/models` REST endpoint.
    pub telemetry: Option<TelemetryClient>,
    /// Optional in-memory log ring buffer for the `GET /api/logs` endpoint.
    pub log_buffer: Option<Arc<crate::logging::LogBuffer>>,
    /// Optional tool registry for the `GET /api/skills` endpoint.
    pub tool_registry: Option<Arc<RwLock<crate::mcp::tools::ToolRegistry>>>,
}

impl Default for HttpTransportConfig {
    fn default() -> Self {
        Self {
            bind_addr: ([127, 0, 0, 1], 3000).into(),
            api_key: None,
            allowed_origins: vec![],
            session_timeout: Duration::from_secs(3600),
            max_sessions: 1000,
            channel_buffer_size: 32,
            session_manager: None,
            chat_service: None,
            neo4j: None,
            scheduler: None,
            brain_event_sender: None,
            context_builder: None,
            llm_config: None,
            telemetry: None,
            log_buffer: None,
            tool_registry: None,
        }
    }
}

impl HttpTransportConfig {
    /// Create a new config with the specified bind address.
    pub fn with_bind_addr(mut self, addr: SocketAddr) -> Self {
        self.bind_addr = addr;
        self
    }

    /// Set the API key for authentication.
    pub fn with_api_key(mut self, key: impl Into<String>) -> Self {
        self.api_key = Some(key.into());
        self
    }

    /// Set allowed CORS origins.
    pub fn with_origins(mut self, origins: Vec<String>) -> Self {
        self.allowed_origins = origins;
        self
    }

    /// Set the shared session manager.
    pub fn with_session_manager(mut self, manager: Arc<SessionManager>) -> Self {
        self.session_manager = Some(manager);
        self
    }

    /// Attach a [`ChatService`] to enable the `/chat` SSE endpoint.
    pub fn with_chat_service(mut self, svc: Arc<ChatService>) -> Self {
        self.chat_service = Some(svc);
        self
    }

    /// Attach a [`Neo4jClient`] to enable the `/todos` and `/scheduled-tasks` REST endpoints.
    pub fn with_neo4j_client(mut self, neo4j: Arc<Neo4jClient>) -> Self {
        self.neo4j = Some(neo4j);
        self
    }

    /// Attach the scheduler handle to enable the `/scheduler-config` REST endpoints.
    pub fn with_scheduler(mut self, handle: Arc<RwLock<Option<Arc<SchedulerService>>>>) -> Self {
        self.scheduler = Some(handle);
        self
    }

    /// Attach the brain event bus so the transport can push job notifications
    /// to client sessions via SSE.
    pub fn with_brain_event_sender(mut self, sender: broadcast::Sender<BrainEvent>) -> Self {
        self.brain_event_sender = Some(sender);
        self
    }

    /// Attach the context-builder handle to enable the `/api/contexts` endpoints.
    pub fn with_context_builder(
        mut self,
        handle: Arc<RwLock<Option<Arc<ContextBuilderService>>>>,
    ) -> Self {
        self.context_builder = Some(handle);
        self
    }

    /// Attach the live LLM-config Arc to enable the `/api/models` endpoint.
    pub fn with_llm_config_arc(mut self, cfg: Arc<RwLock<Option<LlmConfig>>>) -> Self {
        self.llm_config = Some(cfg);
        self
    }

    /// Attach the telemetry client to enable model catalog reads on `/api/models`.
    pub fn with_telemetry(mut self, telemetry: TelemetryClient) -> Self {
        self.telemetry = Some(telemetry);
        self
    }

    pub fn with_log_buffer(mut self, buf: Arc<crate::logging::LogBuffer>) -> Self {
        self.log_buffer = Some(buf);
        self
    }

    pub fn with_tool_registry(
        mut self,
        registry: Arc<RwLock<crate::mcp::tools::ToolRegistry>>,
    ) -> Self {
        self.tool_registry = Some(registry);
        self
    }
}

/// Shared state for the HTTP transport.
#[allow(dead_code)]
struct HttpTransportState {
    /// Session manager for tracking client sessions.
    sessions: Arc<SessionManager>,
    /// API key authenticator.
    auth: Arc<ApiKeyAuth>,
    /// Channel to send messages to the server core.
    message_tx: mpsc::Sender<TransportMessage>,
    /// Configuration.
    config: HttpTransportConfig,
    /// Optional chat service for the `/chat` SSE endpoint.
    chat_service: Option<Arc<ChatService>>,
    /// Optional Neo4j client for the `/todos` and `/scheduled-tasks` REST endpoints.
    neo4j: Option<Arc<Neo4jClient>>,
    /// Optional scheduler handle for the `/scheduler-config` REST endpoints.
    scheduler: Option<Arc<RwLock<Option<Arc<SchedulerService>>>>>,
    /// Optional context-builder handle for `/api/contexts` endpoints.
    context_builder: Option<Arc<RwLock<Option<Arc<ContextBuilderService>>>>>,
    /// Optional live LLM-config for `/api/models`.
    llm_config: Option<Arc<RwLock<Option<LlmConfig>>>>,
    /// Optional telemetry client for `/api/models`.
    telemetry: Option<TelemetryClient>,
    /// Optional in-memory log ring buffer for `GET /api/logs`.
    log_buffer: Option<Arc<crate::logging::LogBuffer>>,
    /// Optional tool registry for `GET /api/skills`.
    tool_registry: Option<Arc<RwLock<crate::mcp::tools::ToolRegistry>>>,
}

/// HTTP transport for MCP server.
pub struct HttpTransport {
    config: HttpTransportConfig,
    running: Arc<AtomicBool>,
    shutdown_tx: Arc<RwLock<Option<oneshot::Sender<()>>>>,
}

impl HttpTransport {
    /// Create a new HTTP transport with default configuration.
    pub fn new() -> Self {
        Self {
            config: HttpTransportConfig::default(),
            running: Arc::new(AtomicBool::new(false)),
            shutdown_tx: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a new HTTP transport with custom configuration.
    pub fn with_config(config: HttpTransportConfig) -> Self {
        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            shutdown_tx: Arc::new(RwLock::new(None)),
        }
    }

    /// Build the Axum router with all MCP endpoints.
    fn build_router(state: Arc<HttpTransportState>) -> Router {
        // Custom header names for MCP
        let mcp_protocol_version_header = HeaderName::from_static(MCP_PROTOCOL_VERSION_HEADER);
        let mcp_session_id_header = HeaderName::from_static(MCP_SESSION_ID_HEADER);

        // Build CORS layer
        let cors = CorsLayer::new()
            .allow_origin(Any)
            .allow_methods([
                Method::GET,
                Method::POST,
                Method::PUT,
                Method::DELETE,
                Method::OPTIONS,
            ])
            .allow_headers([
                header::CONTENT_TYPE,
                header::ACCEPT,
                header::AUTHORIZATION,
                mcp_protocol_version_header.clone(),
                mcp_session_id_header.clone(),
            ])
            .expose_headers([mcp_session_id_header]);

        use crate::clients::rest as rest_handlers;

        // Build REST state — injected as an Extension so REST handlers can
        // extract it without coupling to HttpTransportState.
        // (In Axum 0.8 routers with different state types cannot be merged,
        // so we wire REST routes directly here and inject the state via layer.)
        let rest_state = RestAdapter::new()
            .with_neo4j_opt(state.neo4j.clone())
            .with_scheduler_opt(state.scheduler.clone())
            .with_context_builder_opt(state.context_builder.clone())
            .with_llm_config_opt(state.llm_config.clone())
            .with_telemetry_opt(state.telemetry.clone())
            .with_log_buffer_opt(state.log_buffer.clone())
            .with_tool_registry_opt(state.tool_registry.clone())
            .build_state();

        Router::new()
            .route("/mcp", post(handle_post_mcp))
            .route("/mcp", get(handle_get_mcp))
            .route("/mcp", delete(handle_delete_mcp))
            .route("/chat", post(handle_post_chat))
            .route("/health", get(handle_health))
            // --- REST routes (handlers defined in clients/rest.rs) ---
            .route(
                "/todos",
                get(rest_handlers::handle_list_todos).post(rest_handlers::handle_create_todo),
            )
            .route(
                "/todos/{id}",
                get(rest_handlers::handle_get_todo)
                    .put(rest_handlers::handle_update_todo)
                    .delete(rest_handlers::handle_delete_todo),
            )
            .route(
                "/scheduled-tasks",
                get(rest_handlers::handle_list_scheduled_tasks)
                    .post(rest_handlers::handle_create_scheduled_task),
            )
            .route(
                "/scheduled-tasks/{id}",
                get(rest_handlers::handle_get_scheduled_task)
                    .put(rest_handlers::handle_update_scheduled_task)
                    .delete(rest_handlers::handle_delete_scheduled_task),
            )
            .route(
                "/scheduler-config",
                get(rest_handlers::handle_get_scheduler_config)
                    .put(rest_handlers::handle_put_scheduler_config),
            )
            // --- read-only API endpoints (formerly MCP tools) ---
            .route("/api/graph", get(rest_handlers::handle_get_graph))
            .route("/api/health", get(rest_handlers::handle_api_health))
            .route(
                "/api/scheduler/status",
                get(rest_handlers::handle_get_scheduler_status),
            )
            .route("/api/queue/status", get(rest_handlers::handle_queue_status))
            .route("/api/queue/drain", post(rest_handlers::handle_queue_drain))
            .route(
                "/api/scheduler/chains",
                get(rest_handlers::handle_list_scheduler_chains),
            )
            .route(
                "/api/contexts",
                get(rest_handlers::handle_list_context_profiles),
            )
            .route(
                "/api/contexts/{name}",
                get(rest_handlers::handle_get_context_profile),
            )
            .route(
                "/api/http-contexts",
                get(rest_handlers::handle_list_http_contexts),
            )
            .route("/api/models", get(rest_handlers::handle_list_models))
            .route("/api/sessions", get(rest_handlers::handle_list_sessions))
            .route(
                "/api/sessions/{id}/entries",
                get(rest_handlers::handle_get_session_entries),
            )
            .route("/api/jobs", get(rest_handlers::handle_list_jobs))
            .route("/api/tasks", get(rest_handlers::handle_list_tasks))
            .route(
                "/api/notes",
                get(rest_handlers::handle_list_notes).post(rest_handlers::handle_create_note),
            )
            .route(
                "/api/notes/{id}",
                get(rest_handlers::handle_get_note)
                    .put(rest_handlers::handle_update_note)
                    .delete(rest_handlers::handle_delete_note),
            )
            .route(
                "/api/notes/{id}/related",
                get(rest_handlers::handle_get_related_notes),
            )
            .route(
                "/api/tools/dynamic",
                get(rest_handlers::handle_list_dynamic_tools),
            )
            .route("/api/logs", get(rest_handlers::handle_get_logs))
            .route("/api/skills", get(rest_handlers::handle_list_skills))
            .route(
                "/api/notifications",
                get(rest_handlers::handle_list_notifications),
            )
            .route(
                "/api/notifications/read-all",
                axum::routing::post(rest_handlers::handle_mark_all_notifications_read),
            )
            .route(
                "/api/notifications/{id}/read",
                axum::routing::post(rest_handlers::handle_mark_notification_read),
            )
            // Inject REST state for the handlers above.
            .layer(axum::Extension(rest_state))
            .layer(cors)
            .layer(middleware::from_fn_with_state(
                state.clone(),
                auth_middleware,
            ))
            .with_state(state)
    }
}

impl Default for HttpTransport {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl McpTransport for HttpTransport {
    async fn start(&self) -> Result<mpsc::Receiver<TransportMessage>, TransportError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(TransportError::AlreadyRunning);
        }

        let (message_tx, message_rx) = mpsc::channel(self.config.channel_buffer_size);
        let (shutdown_tx, shutdown_rx) = oneshot::channel();

        // Store shutdown sender
        {
            let mut guard = self.shutdown_tx.write().await;
            *guard = Some(shutdown_tx);
        }

        // Create or use shared session manager
        let sessions = if let Some(ref manager) = self.config.session_manager {
            manager.clone()
        } else {
            let session_config = SessionConfig {
                max_sessions: self.config.max_sessions,
                session_timeout: self.config.session_timeout,
                ..Default::default()
            };
            Arc::new(SessionManager::with_config(session_config))
        };

        // Create authenticator
        let auth = if let Some(ref key) = self.config.api_key {
            Arc::new(ApiKeyAuth::with_key(key))
        } else {
            Arc::new(ApiKeyAuth::disabled())
        };

        // Create shared state
        let state = Arc::new(HttpTransportState {
            sessions: sessions.clone(),
            auth,
            message_tx,
            chat_service: self.config.chat_service.clone(),
            neo4j: self.config.neo4j.clone(),
            scheduler: self.config.scheduler.clone(),
            context_builder: self.config.context_builder.clone(),
            llm_config: self.config.llm_config.clone(),
            telemetry: self.config.telemetry.clone(),
            log_buffer: self.config.log_buffer.clone(),
            tool_registry: self.config.tool_registry.clone(),
            config: self.config.clone(),
        });

        // Spawn brain-event relay: subscribes to BrainEvents and forwards job
        // notifications to the owning session as SSE `notifications/agent_job`.
        if let Some(ref sender) = self.config.brain_event_sender {
            let mut event_rx = sender.subscribe();
            let sessions_for_relay = sessions.clone();
            tokio::spawn(async move {
                loop {
                    match event_rx.recv().await {
                        Ok(event) => {
                            relay_brain_event(event, &sessions_for_relay).await;
                        }
                        Err(broadcast::error::RecvError::Lagged(n)) => {
                            warn!(
                                skipped = n,
                                "Brain event relay lagged — some notifications dropped"
                            );
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            info!("Brain event channel closed — relay task exiting");
                            break;
                        }
                    }
                }
            });
        }

        // Build router
        let router = Self::build_router(state);

        // Start server
        let bind_addr = self.config.bind_addr;
        let running = Arc::clone(&self.running);
        running.store(true, Ordering::SeqCst);

        tokio::spawn(async move {
            info!(addr = %bind_addr, "Starting HTTP transport");

            let listener = match tokio::net::TcpListener::bind(bind_addr).await {
                Ok(l) => l,
                Err(e) => {
                    error!(error = %e, "Failed to bind HTTP server");
                    running.store(false, Ordering::SeqCst);
                    return;
                }
            };

            let server = axum::serve(listener, router).with_graceful_shutdown(async move {
                let _ = shutdown_rx.await;
                info!("HTTP transport shutdown signal received");
            });

            if let Err(e) = server.await {
                error!(error = %e, "HTTP server error");
            }

            running.store(false, Ordering::SeqCst);
            info!("HTTP transport stopped");
        });

        Ok(message_rx)
    }

    async fn send(
        &self,
        session_id: Option<&str>,
        _message: OutgoingMessage,
    ) -> Result<(), TransportError> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(TransportError::NotStarted);
        }

        // For HTTP transport, responses are sent directly in the request handler.
        // This method is used for server-initiated messages via SSE.
        // We would need access to the session manager to send SSE messages.
        // For now, this is a no-op as the architecture handles responses differently.

        debug!(session_id = ?session_id, "Send called on HTTP transport (no-op for request/response)");
        Ok(())
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    async fn shutdown(&self) -> Result<(), TransportError> {
        let mut guard = self.shutdown_tx.write().await;
        if let Some(tx) = guard.take() {
            let _ = tx.send(());
        }
        self.running.store(false, Ordering::SeqCst);
        Ok(())
    }

    fn name(&self) -> &'static str {
        "http"
    }
}

// ============================================================================
// Brain-event relay
// ============================================================================

/// Convert a [`BrainEvent`] into an SSE `notifications/agent_job` push and
/// deliver it to the session identified inside the event, if any.
async fn relay_brain_event(event: BrainEvent, sessions: &SessionManager) {
    let (session_id, params) = match &event {
        BrainEvent::JobCompleted {
            job_id,
            tool_name,
            session_id,
            result_preview,
        } => {
            let Some(sid) = session_id else { return };
            let mut p = serde_json::json!({
                "job_id":    job_id,
                "tool_name": tool_name,
                "status":    "completed",
            });
            if let Some(preview) = result_preview {
                p["result_preview"] = serde_json::Value::String(preview.clone());
            }
            (sid.clone(), p)
        }
        BrainEvent::JobFailed {
            job_id,
            tool_name,
            session_id,
            error,
        } => {
            let Some(sid) = session_id else { return };
            (
                sid.clone(),
                serde_json::json!({
                    "job_id":    job_id,
                    "tool_name": tool_name,
                    "status":    "failed",
                    "error":     error,
                }),
            )
        }
        BrainEvent::JobDead {
            job_id,
            tool_name,
            session_id,
            error,
        } => {
            let Some(sid) = session_id else { return };
            (
                sid.clone(),
                serde_json::json!({
                    "job_id":    job_id,
                    "tool_name": tool_name,
                    "status":    "dead",
                    "error":     error,
                }),
            )
        }
        BrainEvent::AgentChatInitiated {
            notification_id,
            message,
            related_session_id,
        } => {
            let data = serde_json::json!({
                "jsonrpc": "2.0",
                "method":  "notifications/agent_chat",
                "params": {
                    "notification_id": notification_id,
                    "message": message,
                    "related_session_id": related_session_id,
                },
            });
            // Push to ALL connected sessions — this is a user-facing notification.
            sessions.notify_all("agent_chat", data).await;
            return;
        }
        // Scheduler events are broadcast-only — no per-session routing yet.
        BrainEvent::SchedulerTick { .. }
        | BrainEvent::SchedulerSleepEntered
        | BrainEvent::SchedulerSleepExited => return,
    };

    let data = serde_json::json!({
        "jsonrpc": "2.0",
        "method":  "notifications/agent_job",
        "params":  params,
    });

    sessions.notify(&session_id, "agent_job", data).await;
}

// ============================================================================
// Middleware
// ============================================================================

/// Authentication middleware.
async fn auth_middleware(
    State(state): State<Arc<HttpTransportState>>,
    request: Request<Body>,
    next: Next,
) -> Result<Response, AuthError> {
    state.auth.authenticate(&request)?;
    Ok(next.run(request).await)
}

// ============================================================================
// Route Handlers
// ============================================================================

/// Health check endpoint.
async fn handle_health() -> impl IntoResponse {
    Json(serde_json::json!({
        "status": "healthy"
    }))
}

/// POST /mcp - Handle JSON-RPC requests.
async fn handle_post_mcp(
    State(state): State<Arc<HttpTransportState>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<impl IntoResponse, McpHttpError> {
    // Accept requests without the protocol version header for compatibility with
    // clients that don't implement it (e.g. OpenWebUI). Log at debug level only.
    let protocol_version = headers
        .get(MCP_PROTOCOL_VERSION_HEADER)
        .and_then(|v| v.to_str().ok());

    if protocol_version.is_none() {
        debug!(
            "Request missing {}, assuming {}",
            MCP_PROTOCOL_VERSION_HEADER, MCP_PROTOCOL_VERSION
        );
    }

    // Get or create session
    let session_id = match headers
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        Some(id) => {
            if !state.sessions.exists(id).await {
                // Session not found — server likely restarted. Resurrect the session
                // under the same ID so the client can continue without reinitialising.
                info!(session_id = %id, "Resurrecting stale session after server restart");
                state
                    .sessions
                    .resurrect_session(id, SessionState::Running)
                    .await
                    .map_err(|e| McpHttpError::SessionError(e.to_string()))?;
                // Send synthetic notifications/initialized so the server core
                // accepts tool calls on this resurrected session.
                let synthetic = TransportMessage::Notification {
                    session_id: Some(id.to_string()),
                    notification: JsonRpcNotification {
                        jsonrpc: "2.0".to_string(),
                        method: "notifications/initialized".to_string(),
                        params: None,
                    },
                };
                let _ = state.message_tx.send(synthetic).await;
            } else {
                state
                    .sessions
                    .get_session(id)
                    .await
                    .map_err(|e| McpHttpError::SessionError(e.to_string()))?;
            }
            id.to_string()
        }
        None => {
            // No session ID — create a new session for this client.
            state
                .sessions
                .create_session()
                .await
                .map_err(|e| McpHttpError::SessionError(e.to_string()))?
        }
    };

    // Parse the JSON-RPC message
    let incoming = IncomingMessage::parse(&body.to_string()).map_err(McpHttpError::JsonRpcError)?;

    // Process the message
    match incoming {
        IncomingMessage::Request(request) => {
            let method = request.method.clone();

            // Create response channel
            let (response_tx, response_rx) = oneshot::channel();

            // Send to server core
            let msg = TransportMessage::Request {
                session_id: Some(session_id.clone()),
                request,
                response_tx,
            };

            state
                .message_tx
                .send(msg)
                .await
                .map_err(|_| McpHttpError::InternalError("Server unavailable".to_string()))?;

            // Wait for response
            let response = response_rx
                .await
                .map_err(|_| McpHttpError::InternalError("No response from server".to_string()))?;

            // On initialize, advance the session straight to Running and also
            // send a synthetic notifications/initialized to the server core so
            // that clients which don't send the notification explicitly (e.g.
            // OpenWebUI) can immediately use tools/list and tools/call.
            if method == "initialize" {
                let _ = state
                    .sessions
                    .set_session_state(&session_id, SessionState::Running)
                    .await;
                let synthetic = TransportMessage::Notification {
                    session_id: Some(session_id.clone()),
                    notification: JsonRpcNotification {
                        jsonrpc: "2.0".to_string(),
                        method: "notifications/initialized".to_string(),
                        params: None,
                    },
                };
                let _ = state.message_tx.send(synthetic).await;
            }

            // Build response with session ID header
            let mut response_headers = HeaderMap::new();
            response_headers.insert(
                MCP_SESSION_ID_HEADER,
                HeaderValue::from_str(&session_id).unwrap(),
            );

            let json_response = match response {
                OutgoingMessage::Response(r) => serde_json::to_value(r).unwrap(),
                OutgoingMessage::Error(e) => serde_json::to_value(e).unwrap(),
            };

            Ok((response_headers, Json(json_response)))
        }
        IncomingMessage::Notification(notification) => {
            // Send notification to server core
            let msg = TransportMessage::Notification {
                session_id: Some(session_id.clone()),
                notification,
            };

            state
                .message_tx
                .send(msg)
                .await
                .map_err(|_| McpHttpError::InternalError("Server unavailable".to_string()))?;

            // Notifications don't get responses
            Ok((HeaderMap::new(), Json(serde_json::json!(null))))
        }
    }
}

/// GET /mcp - SSE stream for server-initiated messages.
async fn handle_get_mcp(
    State(state): State<Arc<HttpTransportState>>,
    headers: HeaderMap,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, McpHttpError> {
    // Require session ID
    let session_id = headers
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or(McpHttpError::MissingSessionId)?;

    // Validate session
    if !state.sessions.exists(session_id).await {
        return Err(McpHttpError::SessionNotFound(session_id.to_string()));
    }

    // Subscribe to session's SSE messages
    let rx = state
        .sessions
        .subscribe(session_id)
        .await
        .map_err(|e| McpHttpError::SessionError(e.to_string()))?;

    // Convert broadcast receiver to stream
    let stream = BroadcastStream::new(rx).filter_map(|result| async move {
        match result {
            Ok(msg) => {
                let mut event = Event::default().data(msg.data);
                if let Some(id) = msg.id {
                    event = event.id(id);
                }
                if let Some(event_type) = msg.event {
                    event = event.event(event_type);
                }
                Some(Ok(event))
            }
            Err(_) => None, // Skip lagged messages
        }
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// POST /chat - Agentic chat with SSE streaming.
///
/// Accepts `ChatRequest` JSON, runs the server-side LLM ↔ tool loop, and
/// streams `ChatEvent` objects as SSE events.
async fn handle_post_chat(
    State(state): State<Arc<HttpTransportState>>,
    _headers: HeaderMap,
    Json(body): Json<Value>,
) -> Result<Sse<impl Stream<Item = Result<Event, std::convert::Infallible>>>, McpHttpError> {
    let svc = state
        .chat_service
        .as_ref()
        .ok_or_else(|| McpHttpError::InternalError("Chat service not available".into()))?
        .clone();

    let request: ChatRequest = serde_json::from_value(body)
        .map_err(|e| McpHttpError::InternalError(format!("Invalid chat request: {e}")))?;

    let (tx, rx) = tokio::sync::mpsc::channel::<ChatEvent>(64);
    tokio::spawn(async move {
        svc.run(request, tx).await;
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(|event| {
        let evt_type = match &event {
            ChatEvent::Thinking { .. } => "thinking",
            ChatEvent::ToolCall { .. } => "tool_call",
            ChatEvent::ToolResult { .. } => "tool_result",
            ChatEvent::Token { .. } => "token",
            ChatEvent::Message { .. } => "message",
            ChatEvent::Error { .. } => "error",
            ChatEvent::Done => "done",
        };
        let data = serde_json::to_string(&event).unwrap_or_default();
        Ok(Event::default().event(evt_type).data(data))
    });

    Ok(Sse::new(stream).keep_alive(KeepAlive::default()))
}

/// DELETE /mcp - Terminate session.
async fn handle_delete_mcp(
    State(state): State<Arc<HttpTransportState>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, McpHttpError> {
    // Require session ID
    let session_id = headers
        .get(MCP_SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .ok_or(McpHttpError::MissingSessionId)?;

    // Terminate session
    state
        .sessions
        .terminate(session_id)
        .await
        .map_err(|e| McpHttpError::SessionError(e.to_string()))?;

    Ok(StatusCode::NO_CONTENT)
}

// (REST handlers are in clients/rest.rs — merged via RestAdapter::into_router())

// ============================================================================
// Error Types
// ============================================================================

/// HTTP transport errors.
#[derive(Debug)]
pub enum McpHttpError {
    MissingProtocolVersion,
    MissingSessionId,
    SessionNotFound(String),
    SessionError(String),
    JsonRpcError(JsonRpcErrorResponse),
    InternalError(String),
}

impl IntoResponse for McpHttpError {
    fn into_response(self) -> Response {
        let (status, error) = match self {
            McpHttpError::MissingProtocolVersion => (
                StatusCode::BAD_REQUEST,
                JsonRpcErrorResponse::new(
                    None,
                    -32600,
                    format!("Missing {} header", MCP_PROTOCOL_VERSION_HEADER),
                ),
            ),
            McpHttpError::MissingSessionId => (
                StatusCode::BAD_REQUEST,
                JsonRpcErrorResponse::new(
                    None,
                    -32600,
                    format!("Missing {} header", MCP_SESSION_ID_HEADER),
                ),
            ),
            McpHttpError::SessionNotFound(id) => (
                StatusCode::NOT_FOUND,
                JsonRpcErrorResponse::new(None, -32000, format!("Session not found: {}", id)),
            ),
            McpHttpError::SessionError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                JsonRpcErrorResponse::new(None, -32000, msg),
            ),
            McpHttpError::JsonRpcError(e) => (StatusCode::BAD_REQUEST, e),
            McpHttpError::InternalError(msg) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                JsonRpcErrorResponse::new(None, -32000, msg),
            ),
        };

        (status, Json(error)).into_response()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_http_transport_config_default() {
        let config = HttpTransportConfig::default();
        assert_eq!(config.bind_addr, ([127, 0, 0, 1], 3000).into());
        assert!(config.api_key.is_none());
        assert!(config.allowed_origins.is_empty());
        assert_eq!(config.session_timeout, Duration::from_secs(3600));
        assert_eq!(config.max_sessions, 1000);
    }

    #[test]
    fn test_http_transport_config_builder() {
        let config = HttpTransportConfig::default()
            .with_bind_addr(([0, 0, 0, 0], 8080).into())
            .with_api_key("test-key")
            .with_origins(vec!["http://localhost:3000".to_string()]);

        assert_eq!(config.bind_addr, ([0, 0, 0, 0], 8080).into());
        assert_eq!(config.api_key, Some("test-key".to_string()));
        assert_eq!(config.allowed_origins.len(), 1);
    }

    #[test]
    fn test_http_transport_creation() {
        let transport = HttpTransport::new();
        assert!(!transport.is_running());
        assert_eq!(transport.name(), "http");
    }

    #[test]
    fn test_http_transport_with_config() {
        let config = HttpTransportConfig::default().with_bind_addr(([0, 0, 0, 0], 9000).into());
        let transport = HttpTransport::with_config(config);
        assert_eq!(transport.config.bind_addr, ([0, 0, 0, 0], 9000).into());
    }

    #[test]
    fn test_mcp_http_error_into_response() {
        let error = McpHttpError::MissingProtocolVersion;
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let error = McpHttpError::SessionNotFound("test".to_string());
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn test_http_transport_not_started() {
        let transport = HttpTransport::new();
        let result = transport
            .send(
                None,
                OutgoingMessage::Response(super::super::protocol::JsonRpcResponse::new(
                    super::super::protocol::RequestId::Number(1),
                    serde_json::json!({}),
                )),
            )
            .await;
        assert!(matches!(result, Err(TransportError::NotStarted)));
    }
}
