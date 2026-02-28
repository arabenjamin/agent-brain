//! HTTP transport for MCP server using Axum.
//!
//! This module provides an HTTP transport implementation following the MCP spec:
//! - POST /mcp for JSON-RPC requests
//! - GET /mcp for SSE streaming (server-initiated messages)
//! - DELETE /mcp for session termination

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use axum::{
    Router,
    body::Body,
    extract::State,
    http::{header, HeaderMap, HeaderName, HeaderValue, Method, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response, Sse},
    routing::{delete, get, post},
    Json,
};
use axum::response::sse::{Event, KeepAlive};
use futures_util::stream::{Stream, StreamExt};
use serde_json::Value;
use tokio::sync::{mpsc, oneshot, RwLock};
use tokio_stream::wrappers::BroadcastStream;
use tower_http::cors::{Any, CorsLayer};
use tracing::{debug, error, info};

use super::auth::{ApiKeyAuth, AuthError};
use super::protocol::{IncomingMessage, JsonRpcErrorResponse, JsonRpcNotification};
use super::session::{SessionManager, SessionState, SessionConfig};
use super::transport::OutgoingMessage;
use super::transport_trait::{McpTransport, TransportError, TransportMessage};

/// MCP protocol version header name.
pub const MCP_PROTOCOL_VERSION_HEADER: &str = "mcp-protocol-version";
/// MCP session ID header name.
pub const MCP_SESSION_ID_HEADER: &str = "mcp-session-id";
/// Expected MCP protocol version.
pub const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

/// Configuration for the HTTP transport.
#[derive(Debug, Clone)]
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
            .allow_methods([Method::GET, Method::POST, Method::DELETE, Method::OPTIONS])
            .allow_headers([
                header::CONTENT_TYPE,
                header::ACCEPT,
                header::AUTHORIZATION,
                mcp_protocol_version_header.clone(),
                mcp_session_id_header.clone(),
            ])
            .expose_headers([mcp_session_id_header]);

        Router::new()
            .route("/mcp", post(handle_post_mcp))
            .route("/mcp", get(handle_get_mcp))
            .route("/mcp", delete(handle_delete_mcp))
            .route("/health", get(handle_health))
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
            sessions,
            auth,
            message_tx,
            config: self.config.clone(),
        });

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
        debug!("Request missing {}, assuming {}", MCP_PROTOCOL_VERSION_HEADER, MCP_PROTOCOL_VERSION);
    }

    // Get or create session
    let session_id = match headers.get(MCP_SESSION_ID_HEADER).and_then(|v| v.to_str().ok()) {
        Some(id) => {
            // Validate existing session
            if !state.sessions.exists(id).await {
                return Err(McpHttpError::SessionNotFound(id.to_string()));
            }
            state.sessions.get_session(id).await.map_err(|e| {
                McpHttpError::SessionError(e.to_string())
            })?;
            id.to_string()
        }
        None => {
            // Create new session for initialize request
            state.sessions.create_session().await.map_err(|e| {
                McpHttpError::SessionError(e.to_string())
            })?
        }
    };

    // Parse the JSON-RPC message
    let incoming = IncomingMessage::parse(&body.to_string()).map_err(|e| {
        McpHttpError::JsonRpcError(e)
    })?;

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

            state.message_tx.send(msg).await.map_err(|_| {
                McpHttpError::InternalError("Server unavailable".to_string())
            })?;

            // Wait for response
            let response = response_rx.await.map_err(|_| {
                McpHttpError::InternalError("No response from server".to_string())
            })?;

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

            state.message_tx.send(msg).await.map_err(|_| {
                McpHttpError::InternalError("Server unavailable".to_string())
            })?;

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
    let rx = state.sessions.subscribe(session_id).await.map_err(|e| {
        McpHttpError::SessionError(e.to_string())
    })?;

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
    state.sessions.terminate(session_id).await.map_err(|e| {
        McpHttpError::SessionError(e.to_string())
    })?;

    Ok(StatusCode::NO_CONTENT)
}

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
        let config = HttpTransportConfig::default()
            .with_bind_addr(([0, 0, 0, 0], 9000).into());
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
        let result = transport.send(None, OutgoingMessage::Response(
            super::super::protocol::JsonRpcResponse::new(
                super::super::protocol::RequestId::Number(1),
                serde_json::json!({}),
            )
        )).await;
        assert!(matches!(result, Err(TransportError::NotStarted)));
    }
}
