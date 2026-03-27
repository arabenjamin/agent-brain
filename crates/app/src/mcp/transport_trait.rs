//! Transport abstraction for MCP server.
//!
//! This module defines the `McpTransport` trait that allows the MCP server
//! to work with different transport backends (stdio, HTTP, etc.).

use async_trait::async_trait;
use tokio::sync::{mpsc, oneshot};

use super::protocol::{JsonRpcNotification, JsonRpcRequest};
use super::transport::OutgoingMessage;

/// Error type for transport operations.
#[derive(Debug, thiserror::Error)]
pub enum TransportError {
    #[error("Transport not started")]
    NotStarted,

    #[error("Transport already running")]
    AlreadyRunning,

    #[error("Send failed: {0}")]
    SendFailed(String),

    #[error("Receive failed: {0}")]
    ReceiveFailed(String),

    #[error("Session not found: {0}")]
    SessionNotFound(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Shutdown in progress")]
    ShuttingDown,
}

/// A message received from a transport.
#[derive(Debug)]
pub enum TransportMessage {
    /// A JSON-RPC request that expects a response.
    Request {
        /// Session ID (None for stdio, Some for HTTP).
        session_id: Option<String>,
        /// The JSON-RPC request.
        request: JsonRpcRequest,
        /// Channel to send the response back.
        response_tx: oneshot::Sender<OutgoingMessage>,
    },
    /// A JSON-RPC notification (no response expected).
    Notification {
        /// Session ID (None for stdio, Some for HTTP).
        session_id: Option<String>,
        /// The JSON-RPC notification.
        notification: JsonRpcNotification,
    },
}

/// Configuration for transport behavior.
#[derive(Debug, Clone)]
pub struct TransportConfig {
    /// Channel buffer size for incoming messages.
    pub channel_buffer_size: usize,
    /// Timeout for send operations in milliseconds.
    pub send_timeout_ms: u64,
}

impl Default for TransportConfig {
    fn default() -> Self {
        Self {
            channel_buffer_size: 32,
            send_timeout_ms: 5000,
        }
    }
}

/// Trait for MCP transport implementations.
///
/// This trait abstracts over different transport mechanisms (stdio, HTTP)
/// allowing the MCP server to work with any compliant transport.
#[async_trait]
pub trait McpTransport: Send + Sync {
    /// Start the transport and return a receiver for incoming messages.
    ///
    /// This should spawn any background tasks needed for the transport
    /// and return a channel receiver for incoming messages.
    async fn start(&self) -> Result<mpsc::Receiver<TransportMessage>, TransportError>;

    /// Send a message to a specific session (or the default for single-session transports).
    ///
    /// For stdio transport, `session_id` is ignored.
    /// For HTTP transport, `session_id` identifies which client session to send to.
    async fn send(
        &self,
        session_id: Option<&str>,
        message: OutgoingMessage,
    ) -> Result<(), TransportError>;

    /// Check if the transport is currently running.
    fn is_running(&self) -> bool;

    /// Gracefully shutdown the transport.
    ///
    /// This should stop any background tasks and close connections.
    async fn shutdown(&self) -> Result<(), TransportError>;

    /// Get the transport name for logging purposes.
    fn name(&self) -> &'static str;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// Mock transport for testing the trait interface.
    struct MockTransport {
        running: Arc<AtomicBool>,
        config: TransportConfig,
    }

    impl MockTransport {
        fn new() -> Self {
            Self {
                running: Arc::new(AtomicBool::new(false)),
                config: TransportConfig::default(),
            }
        }
    }

    #[async_trait]
    impl McpTransport for MockTransport {
        async fn start(&self) -> Result<mpsc::Receiver<TransportMessage>, TransportError> {
            if self.running.load(Ordering::SeqCst) {
                return Err(TransportError::AlreadyRunning);
            }
            self.running.store(true, Ordering::SeqCst);
            let (_tx, rx) = mpsc::channel(self.config.channel_buffer_size);
            Ok(rx)
        }

        async fn send(
            &self,
            _session_id: Option<&str>,
            _message: OutgoingMessage,
        ) -> Result<(), TransportError> {
            if !self.running.load(Ordering::SeqCst) {
                return Err(TransportError::NotStarted);
            }
            Ok(())
        }

        fn is_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }

        async fn shutdown(&self) -> Result<(), TransportError> {
            self.running.store(false, Ordering::SeqCst);
            Ok(())
        }

        fn name(&self) -> &'static str {
            "mock"
        }
    }

    #[test]
    fn test_transport_config_default() {
        let config = TransportConfig::default();
        assert_eq!(config.channel_buffer_size, 32);
        assert_eq!(config.send_timeout_ms, 5000);
    }

    #[test]
    fn test_transport_error_display() {
        let error = TransportError::NotStarted;
        assert_eq!(error.to_string(), "Transport not started");

        let error = TransportError::SessionNotFound("abc123".to_string());
        assert_eq!(error.to_string(), "Session not found: abc123");
    }

    #[tokio::test]
    async fn test_mock_transport_start() {
        let transport = MockTransport::new();
        assert!(!transport.is_running());

        let _rx = transport.start().await.expect("Failed to start");
        assert!(transport.is_running());
    }

    #[tokio::test]
    async fn test_mock_transport_double_start_fails() {
        let transport = MockTransport::new();
        let _rx = transport.start().await.expect("Failed to start");

        let result = transport.start().await;
        assert!(matches!(result, Err(TransportError::AlreadyRunning)));
    }

    #[tokio::test]
    async fn test_mock_transport_send_before_start_fails() {
        let transport = MockTransport::new();
        let message = OutgoingMessage::Response(super::super::protocol::JsonRpcResponse {
            jsonrpc: "2.0".to_string(),
            id: super::super::protocol::RequestId::Number(1),
            result: serde_json::json!({}),
        });

        let result = transport.send(None, message).await;
        assert!(matches!(result, Err(TransportError::NotStarted)));
    }

    #[tokio::test]
    async fn test_mock_transport_shutdown() {
        let transport = MockTransport::new();
        let _rx = transport.start().await.expect("Failed to start");
        assert!(transport.is_running());

        transport.shutdown().await.expect("Failed to shutdown");
        assert!(!transport.is_running());
    }

    #[tokio::test]
    async fn test_mock_transport_name() {
        let transport = MockTransport::new();
        assert_eq!(transport.name(), "mock");
    }

    #[test]
    fn test_transport_message_variants() {
        // Test that TransportMessage can be constructed with both variants
        let (tx, _rx) = oneshot::channel();
        let request_msg = TransportMessage::Request {
            session_id: Some("session-123".to_string()),
            request: JsonRpcRequest {
                jsonrpc: "2.0".to_string(),
                id: super::super::protocol::RequestId::Number(1),
                method: "test".to_string(),
                params: None,
            },
            response_tx: tx,
        };

        // Verify we can match on it
        if let TransportMessage::Request { session_id, .. } = request_msg {
            assert_eq!(session_id, Some("session-123".to_string()));
        } else {
            panic!("Expected Request variant");
        }

        let notification_msg = TransportMessage::Notification {
            session_id: None,
            notification: JsonRpcNotification {
                jsonrpc: "2.0".to_string(),
                method: "notifications/test".to_string(),
                params: None,
            },
        };

        if let TransportMessage::Notification { session_id, .. } = notification_msg {
            assert!(session_id.is_none());
        } else {
            panic!("Expected Notification variant");
        }
    }

    /// Test that McpTransport is object-safe and can be used as dyn trait.
    #[tokio::test]
    async fn test_transport_trait_object_safety() {
        let transport: Box<dyn McpTransport> = Box::new(MockTransport::new());
        assert!(!transport.is_running());
        assert_eq!(transport.name(), "mock");

        let _rx = transport.start().await.expect("Failed to start");
        assert!(transport.is_running());
    }

    /// Test that transport is Send + Sync (required for async contexts).
    #[test]
    fn test_transport_is_send_sync() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<MockTransport>();
    }
}
