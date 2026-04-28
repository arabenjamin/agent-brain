//! Session management for HTTP transport.
//!
//! This module provides session management for the HTTP transport layer.
//! Each HTTP client gets a unique session ID that persists across requests.

use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{RwLock, broadcast};
use uuid::Uuid;

use agent_brain_protocol::SseNotifier;
use async_trait::async_trait;

/// Error type for session operations.
#[derive(Debug, thiserror::Error)]
pub enum SessionError {
    #[error("Session not found: {0}")]
    NotFound(String),

    #[error("Session expired: {0}")]
    Expired(String),

    #[error("Maximum sessions reached: {0}")]
    MaxSessionsReached(usize),

    #[error("Invalid session ID format")]
    InvalidFormat,
}

use super::server::ServerState;

/// Server state for a session (mirrors McpServer state machine).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SessionState {
    /// Session created, waiting for initialize request.
    #[default]
    Created,
    /// Initialize received, waiting for initialized notification.
    Initializing,
    /// Fully initialized and ready.
    Running,
    /// Shutdown requested.
    ShuttingDown,
}

impl From<ServerState> for SessionState {
    fn from(state: ServerState) -> Self {
        match state {
            ServerState::Created => SessionState::Created,
            ServerState::Initializing => SessionState::Initializing,
            ServerState::Running => SessionState::Running,
            ServerState::ShuttingDown => SessionState::ShuttingDown,
        }
    }
}

impl From<SessionState> for ServerState {
    fn from(state: SessionState) -> Self {
        match state {
            SessionState::Created => ServerState::Created,
            SessionState::Initializing => ServerState::Initializing,
            SessionState::Running => ServerState::Running,
            SessionState::ShuttingDown => ServerState::ShuttingDown,
        }
    }
}

/// Message that can be sent via SSE to clients.
#[derive(Debug, Clone)]
pub struct SseMessage {
    /// The JSON-RPC message content.
    pub data: String,
    /// Optional event ID for resumability.
    pub id: Option<String>,
    /// Event type (default: "message").
    pub event: Option<String>,
}

impl SseMessage {
    /// Create a new SSE message with auto-generated ID.
    pub fn new(data: String) -> Self {
        Self {
            data,
            id: Some(Uuid::new_v4().to_string()),
            event: None,
        }
    }

    /// Create a message with a specific event type.
    pub fn with_event(mut self, event: impl Into<String>) -> Self {
        self.event = Some(event.into());
        self
    }
}

/// A client session for HTTP transport.
#[derive(Debug)]
pub struct Session {
    /// Unique session identifier (UUID v4).
    pub id: String,
    /// When the session was created.
    pub created_at: DateTime<Utc>,
    /// When the session was last accessed.
    pub last_accessed: DateTime<Utc>,
    /// Current session state.
    pub state: SessionState,
    /// Broadcast channel for SSE messages to this session.
    pub sse_tx: broadcast::Sender<SseMessage>,
}

impl Session {
    /// Create a new session with a random UUID.
    pub fn new() -> Self {
        let (sse_tx, _) = broadcast::channel(32);
        Self {
            id: Uuid::new_v4().to_string(),
            created_at: Utc::now(),
            last_accessed: Utc::now(),
            state: SessionState::default(),
            sse_tx,
        }
    }

    /// Check if the session has expired based on the given timeout.
    pub fn is_expired(&self, timeout: Duration) -> bool {
        let elapsed = Utc::now()
            .signed_duration_since(self.last_accessed)
            .to_std()
            .unwrap_or(Duration::MAX);
        elapsed > timeout
    }

    /// Update the last accessed timestamp.
    pub fn touch(&mut self) {
        self.last_accessed = Utc::now();
    }

    /// Subscribe to SSE messages for this session.
    pub fn subscribe(&self) -> broadcast::Receiver<SseMessage> {
        self.sse_tx.subscribe()
    }

    /// Send an SSE message to this session's subscribers.
    pub fn send_sse(
        &self,
        message: SseMessage,
    ) -> Result<usize, broadcast::error::SendError<SseMessage>> {
        self.sse_tx.send(message)
    }
}

impl Default for Session {
    fn default() -> Self {
        Self::new()
    }
}

/// Configuration for the session manager.
#[derive(Debug, Clone)]
pub struct SessionConfig {
    /// Maximum number of concurrent sessions.
    pub max_sessions: usize,
    /// Session timeout duration.
    pub session_timeout: Duration,
    /// Interval for cleanup of expired sessions.
    pub cleanup_interval: Duration,
}

impl Default for SessionConfig {
    fn default() -> Self {
        Self {
            max_sessions: 1000,
            session_timeout: Duration::from_secs(3600), // 1 hour
            cleanup_interval: Duration::from_secs(60),  // 1 minute
        }
    }
}

/// Manages HTTP transport sessions.
#[derive(Debug)]
pub struct SessionManager {
    sessions: Arc<RwLock<HashMap<String, Session>>>,
    config: SessionConfig,
}

impl SessionManager {
    /// Create a new session manager with default configuration.
    pub fn new() -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            config: SessionConfig::default(),
        }
    }

    /// Create a new session manager with custom configuration.
    pub fn with_config(config: SessionConfig) -> Self {
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    /// Create a new session and return its ID.
    pub async fn create_session(&self) -> Result<String, SessionError> {
        let mut sessions = self.sessions.write().await;

        if sessions.len() >= self.config.max_sessions {
            return Err(SessionError::MaxSessionsReached(self.config.max_sessions));
        }

        let session = Session::new();
        let id = session.id.clone();
        sessions.insert(id.clone(), session);

        Ok(id)
    }

    /// Resurrect (or create) a session with a specific ID.
    ///
    /// Used to auto-heal stale sessions after a server restart: the client
    /// already holds the session ID, so we recreate the server-side state
    /// under that exact ID rather than rejecting the request.
    pub async fn resurrect_session(
        &self,
        id: &str,
        state: SessionState,
    ) -> Result<(), SessionError> {
        let mut sessions = self.sessions.write().await;

        if sessions.len() >= self.config.max_sessions {
            return Err(SessionError::MaxSessionsReached(self.config.max_sessions));
        }

        let (sse_tx, _) = tokio::sync::broadcast::channel(32);
        let session = Session {
            id: id.to_string(),
            created_at: chrono::Utc::now(),
            last_accessed: chrono::Utc::now(),
            state,
            sse_tx,
        };
        sessions.insert(id.to_string(), session);
        Ok(())
    }

    /// Get a session by ID, updating last accessed time.
    pub async fn get_session(&self, id: &str) -> Result<(), SessionError> {
        let mut sessions = self.sessions.write().await;

        let session = sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;

        if session.is_expired(self.config.session_timeout) {
            sessions.remove(id);
            return Err(SessionError::Expired(id.to_string()));
        }

        session.touch();
        Ok(())
    }

    /// Get the state of a session.
    pub async fn get_session_state(&self, id: &str) -> Result<SessionState, SessionError> {
        let sessions = self.sessions.read().await;
        sessions
            .get(id)
            .map(|s| s.state)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))
    }

    /// Update the state of a session.
    pub async fn set_session_state(
        &self,
        id: &str,
        state: SessionState,
    ) -> Result<(), SessionError> {
        let mut sessions = self.sessions.write().await;
        let session = sessions
            .get_mut(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        session.state = state;
        session.touch();
        Ok(())
    }

    /// Subscribe to SSE messages for a session.
    pub async fn subscribe(
        &self,
        id: &str,
    ) -> Result<broadcast::Receiver<SseMessage>, SessionError> {
        let sessions = self.sessions.read().await;
        let session = sessions
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        Ok(session.subscribe())
    }

    /// Send an SSE message to a session.
    pub async fn send_sse(&self, id: &str, message: SseMessage) -> Result<usize, SessionError> {
        let sessions = self.sessions.read().await;
        let session = sessions
            .get(id)
            .ok_or_else(|| SessionError::NotFound(id.to_string()))?;
        session
            .send_sse(message)
            .map_err(|_| SessionError::NotFound(id.to_string()))
    }

    /// Terminate a session.
    pub async fn terminate(&self, id: &str) -> Result<(), SessionError> {
        let mut sessions = self.sessions.write().await;
        sessions
            .remove(id)
            .map(|_| ())
            .ok_or_else(|| SessionError::NotFound(id.to_string()))
    }

    /// Check if a session exists.
    pub async fn exists(&self, id: &str) -> bool {
        let sessions = self.sessions.read().await;
        sessions.contains_key(id)
    }

    /// Get the number of active sessions.
    pub async fn count(&self) -> usize {
        let sessions = self.sessions.read().await;
        sessions.len()
    }

    /// Remove all expired sessions.
    pub async fn cleanup_expired(&self) -> usize {
        let mut sessions = self.sessions.write().await;
        let timeout = self.config.session_timeout;
        let before = sessions.len();
        sessions.retain(|_, session| !session.is_expired(timeout));
        before - sessions.len()
    }

    /// Get the session configuration.
    pub fn config(&self) -> &SessionConfig {
        &self.config
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl SseNotifier for SessionManager {
    async fn notify(&self, session_id: &str, event: &str, data: serde_json::Value) {
        let payload = serde_json::to_string(&data).unwrap_or_default();
        let message = SseMessage::new(payload).with_event(event.to_string());
        // Best-effort: ignore send errors (e.g. no subscriber).
        let _ = self.send_sse(session_id, message).await;
    }
}

impl SessionManager {
    /// Broadcast an SSE event to every active session.
    /// Used for brain-initiated notifications that have no target session.
    pub async fn notify_all(&self, event: &str, data: serde_json::Value) {
        let payload = serde_json::to_string(&data).unwrap_or_default();
        let sessions = self.sessions.read().await;
        for session in sessions.values() {
            let msg = SseMessage::new(payload.clone()).with_event(event.to_string());
            // Best-effort
            let _ = session.send_sse(msg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn test_session_state_default() {
        assert_eq!(SessionState::default(), SessionState::Created);
    }

    #[test]
    fn test_session_error_display() {
        let error = SessionError::NotFound("abc123".to_string());
        assert_eq!(error.to_string(), "Session not found: abc123");

        let error = SessionError::MaxSessionsReached(100);
        assert_eq!(error.to_string(), "Maximum sessions reached: 100");
    }

    #[test]
    fn test_session_config_default() {
        let config = SessionConfig::default();
        assert_eq!(config.max_sessions, 1000);
        assert_eq!(config.session_timeout, Duration::from_secs(3600));
        assert_eq!(config.cleanup_interval, Duration::from_secs(60));
    }

    #[test]
    fn test_session_creation() {
        let session = Session::new();

        // ID should be a valid UUID format
        assert!(Uuid::parse_str(&session.id).is_ok());
        assert_eq!(session.state, SessionState::Created);
    }

    #[test]
    fn test_session_touch() {
        let mut session = Session::new();
        let initial_accessed = session.last_accessed;

        std::thread::sleep(Duration::from_millis(10));
        session.touch();

        assert!(session.last_accessed > initial_accessed);
    }

    #[test]
    fn test_session_expiration() {
        let session = Session::new();

        // Should not be expired with long timeout
        assert!(!session.is_expired(Duration::from_secs(3600)));

        // Should be expired with very short timeout
        std::thread::sleep(Duration::from_millis(10));
        assert!(session.is_expired(Duration::from_millis(1)));
    }

    #[test]
    fn test_sse_message_creation() {
        let msg = SseMessage::new("test data".to_string());
        assert_eq!(msg.data, "test data");
        assert!(msg.id.is_some());
        assert!(msg.event.is_none());
    }

    #[test]
    fn test_sse_message_with_event() {
        let msg = SseMessage::new("data".to_string()).with_event("custom");
        assert_eq!(msg.event, Some("custom".to_string()));
    }

    #[tokio::test]
    async fn test_session_manager_create_session() {
        let manager = SessionManager::new();

        let id = manager
            .create_session()
            .await
            .expect("Failed to create session");
        assert!(Uuid::parse_str(&id).is_ok());
        assert_eq!(manager.count().await, 1);
    }

    #[tokio::test]
    async fn test_session_manager_get_nonexistent() {
        let manager = SessionManager::new();

        let result = manager.get_session("nonexistent").await;
        assert!(matches!(result, Err(SessionError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_session_manager_get_session() {
        let manager = SessionManager::new();

        let id = manager
            .create_session()
            .await
            .expect("Failed to create session");
        let result = manager.get_session(&id).await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_session_manager_terminate() {
        let manager = SessionManager::new();

        let id = manager
            .create_session()
            .await
            .expect("Failed to create session");
        assert!(manager.exists(&id).await);

        manager.terminate(&id).await.expect("Failed to terminate");
        assert!(!manager.exists(&id).await);
    }

    #[tokio::test]
    async fn test_session_manager_terminate_nonexistent() {
        let manager = SessionManager::new();

        let result = manager.terminate("nonexistent").await;
        assert!(matches!(result, Err(SessionError::NotFound(_))));
    }

    #[tokio::test]
    async fn test_session_manager_state_transitions() {
        let manager = SessionManager::new();

        let id = manager
            .create_session()
            .await
            .expect("Failed to create session");

        // Initial state
        let state = manager
            .get_session_state(&id)
            .await
            .expect("Failed to get state");
        assert_eq!(state, SessionState::Created);

        // Transition to Initializing
        manager
            .set_session_state(&id, SessionState::Initializing)
            .await
            .expect("Failed to set state");
        let state = manager
            .get_session_state(&id)
            .await
            .expect("Failed to get state");
        assert_eq!(state, SessionState::Initializing);

        // Transition to Running
        manager
            .set_session_state(&id, SessionState::Running)
            .await
            .expect("Failed to set state");
        let state = manager
            .get_session_state(&id)
            .await
            .expect("Failed to get state");
        assert_eq!(state, SessionState::Running);
    }

    #[tokio::test]
    async fn test_session_manager_max_sessions() {
        let config = SessionConfig {
            max_sessions: 2,
            ..Default::default()
        };
        let manager = SessionManager::with_config(config);

        // Create up to max
        manager.create_session().await.expect("First session");
        manager.create_session().await.expect("Second session");

        // Third should fail
        let result = manager.create_session().await;
        assert!(matches!(result, Err(SessionError::MaxSessionsReached(2))));
    }

    #[tokio::test]
    async fn test_session_manager_cleanup_expired() {
        let config = SessionConfig {
            session_timeout: Duration::from_millis(10),
            ..Default::default()
        };
        let manager = SessionManager::with_config(config);

        // Create sessions
        manager.create_session().await.expect("First session");
        manager.create_session().await.expect("Second session");
        assert_eq!(manager.count().await, 2);

        // Wait for expiration
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Cleanup should remove both
        let removed = manager.cleanup_expired().await;
        assert_eq!(removed, 2);
        assert_eq!(manager.count().await, 0);
    }

    #[tokio::test]
    async fn test_session_manager_expired_session_removed_on_access() {
        let config = SessionConfig {
            session_timeout: Duration::from_millis(10),
            ..Default::default()
        };
        let manager = SessionManager::with_config(config);

        let id = manager.create_session().await.expect("Create session");

        // Wait for expiration
        tokio::time::sleep(Duration::from_millis(20)).await;

        // Access should fail and remove session
        let result = manager.get_session(&id).await;
        assert!(matches!(result, Err(SessionError::Expired(_))));

        // Session should be removed
        assert!(!manager.exists(&id).await);
    }

    #[tokio::test]
    async fn test_session_manager_concurrent_access() {
        let manager = Arc::new(SessionManager::new());

        // Spawn multiple tasks creating sessions
        let mut handles = vec![];
        for _ in 0..10 {
            let manager_clone = Arc::clone(&manager);
            handles.push(tokio::spawn(
                async move { manager_clone.create_session().await },
            ));
        }

        // Wait for all to complete
        let results: Vec<_> = futures_util::future::join_all(handles).await;

        // All should succeed
        for result in results {
            assert!(result.expect("Task panicked").is_ok());
        }

        assert_eq!(manager.count().await, 10);
    }

    #[tokio::test]
    async fn test_session_sse_subscribe_and_send() {
        let manager = SessionManager::new();
        let id = manager.create_session().await.expect("Create session");

        // Subscribe
        let mut rx = manager.subscribe(&id).await.expect("Subscribe");

        // Send message
        let msg = SseMessage::new("test".to_string());
        let sent = manager.send_sse(&id, msg).await.expect("Send");
        assert_eq!(sent, 1);

        // Receive
        let received = rx.recv().await.expect("Receive");
        assert_eq!(received.data, "test");
    }
}
