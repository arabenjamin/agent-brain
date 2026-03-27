//! Stdio transport for MCP server.

use std::io::{self, BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, RwLock};
use tracing::{debug, error, trace};

use super::protocol::{IncomingMessage, JsonRpcErrorResponse, JsonRpcResponse};
use super::transport_trait::{McpTransport, TransportConfig, TransportError, TransportMessage};

/// Message to send to the client.
#[derive(Debug, Clone)]
pub enum OutgoingMessage {
    Response(JsonRpcResponse),
    Error(JsonRpcErrorResponse),
}

impl OutgoingMessage {
    pub fn to_json(&self) -> String {
        match self {
            OutgoingMessage::Response(r) => serde_json::to_string(r).unwrap(),
            OutgoingMessage::Error(e) => serde_json::to_string(e).unwrap(),
        }
    }
}

/// Stdio transport for reading/writing JSON-RPC messages.
///
/// This transport uses stdin/stdout for communication, suitable for
/// local CLI usage where the MCP server is spawned as a subprocess.
pub struct StdioTransport {
    config: TransportConfig,
    running: Arc<AtomicBool>,
    out_tx: Arc<RwLock<Option<mpsc::Sender<OutgoingMessage>>>>,
}

impl StdioTransport {
    /// Create a new stdio transport with default configuration.
    pub fn create() -> Self {
        Self {
            config: TransportConfig::default(),
            running: Arc::new(AtomicBool::new(false)),
            out_tx: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a new stdio transport with custom configuration.
    pub fn with_config(config: TransportConfig) -> Self {
        Self {
            config,
            running: Arc::new(AtomicBool::new(false)),
            out_tx: Arc::new(RwLock::new(None)),
        }
    }

    /// Create a new stdio transport and spawn reader/writer tasks.
    /// Returns the transport and a receiver for incoming messages.
    ///
    /// This is the legacy constructor for backward compatibility.
    pub fn new() -> (
        Self,
        mpsc::Receiver<Result<IncomingMessage, JsonRpcErrorResponse>>,
    ) {
        let (in_tx, in_rx) = mpsc::channel(32);
        let (out_tx, out_rx) = mpsc::channel(32);

        // Spawn reader task
        tokio::spawn(Self::reader_task(in_tx));

        // Spawn writer task
        tokio::spawn(Self::writer_task(out_rx));

        let transport = Self {
            config: TransportConfig::default(),
            running: Arc::new(AtomicBool::new(true)),
            out_tx: Arc::new(RwLock::new(Some(out_tx))),
        };

        (transport, in_rx)
    }

    /// Send a response to the client.
    ///
    /// This is the original method signature for backward compatibility.
    pub async fn send(
        &self,
        msg: OutgoingMessage,
    ) -> Result<(), mpsc::error::SendError<OutgoingMessage>> {
        let guard = self.out_tx.read().await;
        if let Some(tx) = guard.as_ref() {
            tx.send(msg).await
        } else {
            Err(mpsc::error::SendError(msg))
        }
    }

    /// Send a successful response.
    pub async fn send_response(
        &self,
        response: JsonRpcResponse,
    ) -> Result<(), mpsc::error::SendError<OutgoingMessage>> {
        self.send(OutgoingMessage::Response(response)).await
    }

    /// Send an error response.
    pub async fn send_error(
        &self,
        error: JsonRpcErrorResponse,
    ) -> Result<(), mpsc::error::SendError<OutgoingMessage>> {
        self.send(OutgoingMessage::Error(error)).await
    }

    /// Reader task that reads lines from stdin and parses them as JSON-RPC messages.
    async fn reader_task(tx: mpsc::Sender<Result<IncomingMessage, JsonRpcErrorResponse>>) {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    // EOF
                    debug!("Stdin closed (EOF)");
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    trace!(message = %trimmed, "Received message");

                    let result = IncomingMessage::parse(trimmed);
                    if tx.send(result).await.is_err() {
                        // Receiver dropped
                        break;
                    }
                }
                Err(e) => {
                    error!(error = %e, "Failed to read from stdin");
                    break;
                }
            }
        }
    }

    /// Reader task for trait implementation that emits TransportMessage.
    async fn reader_task_trait(
        tx: mpsc::Sender<TransportMessage>,
        running: Arc<AtomicBool>,
        out_tx: Arc<RwLock<Option<mpsc::Sender<OutgoingMessage>>>>,
    ) {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        let mut line = String::new();

        while running.load(Ordering::SeqCst) {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) => {
                    debug!("Stdin closed (EOF)");
                    break;
                }
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }

                    trace!(message = %trimmed, "Received message");

                    match IncomingMessage::parse(trimmed) {
                        Ok(IncomingMessage::Request(request)) => {
                            // Create a response channel
                            let (response_tx, response_rx) = oneshot::channel();
                            let msg = TransportMessage::Request {
                                session_id: None, // stdio has no sessions
                                request,
                                response_tx,
                            };

                            if tx.send(msg).await.is_err() {
                                break;
                            }

                            // Wait for response and send it
                            if let Ok(response) = response_rx.await {
                                let guard = out_tx.read().await;
                                if let Some(sender) = guard.as_ref() {
                                    let _ = sender.send(response).await;
                                }
                            }
                        }
                        Ok(IncomingMessage::Notification(notification)) => {
                            let msg = TransportMessage::Notification {
                                session_id: None,
                                notification,
                            };
                            if tx.send(msg).await.is_err() {
                                break;
                            }
                        }
                        Err(error) => {
                            // Send error response
                            let guard = out_tx.read().await;
                            if let Some(sender) = guard.as_ref() {
                                let _ = sender.send(OutgoingMessage::Error(error)).await;
                            }
                        }
                    }
                }
                Err(e) => {
                    error!(error = %e, "Failed to read from stdin");
                    break;
                }
            }
        }

        running.store(false, Ordering::SeqCst);
    }

    /// Writer task that writes JSON-RPC messages to stdout.
    async fn writer_task(mut rx: mpsc::Receiver<OutgoingMessage>) {
        let mut stdout = tokio::io::stdout();

        while let Some(msg) = rx.recv().await {
            let json = msg.to_json();
            trace!(message = %json, "Sending message");

            // Write message followed by newline
            if let Err(e) = stdout.write_all(json.as_bytes()).await {
                error!(error = %e, "Failed to write to stdout");
                break;
            }
            if let Err(e) = stdout.write_all(b"\n").await {
                error!(error = %e, "Failed to write newline to stdout");
                break;
            }
            if let Err(e) = stdout.flush().await {
                error!(error = %e, "Failed to flush stdout");
                break;
            }
        }
    }
}

impl Default for StdioTransport {
    fn default() -> Self {
        Self::create()
    }
}

#[async_trait]
impl McpTransport for StdioTransport {
    async fn start(&self) -> Result<mpsc::Receiver<TransportMessage>, TransportError> {
        if self.running.load(Ordering::SeqCst) {
            return Err(TransportError::AlreadyRunning);
        }

        let (in_tx, in_rx) = mpsc::channel(self.config.channel_buffer_size);
        let (out_tx, out_rx) = mpsc::channel(self.config.channel_buffer_size);

        // Store the output sender
        {
            let mut guard = self.out_tx.write().await;
            *guard = Some(out_tx);
        }

        self.running.store(true, Ordering::SeqCst);

        // Spawn writer task
        tokio::spawn(Self::writer_task(out_rx));

        // Spawn reader task with trait message format
        let running = Arc::clone(&self.running);
        let out_tx = Arc::clone(&self.out_tx);
        tokio::spawn(Self::reader_task_trait(in_tx, running, out_tx));

        Ok(in_rx)
    }

    async fn send(
        &self,
        _session_id: Option<&str>,
        message: OutgoingMessage,
    ) -> Result<(), TransportError> {
        if !self.running.load(Ordering::SeqCst) {
            return Err(TransportError::NotStarted);
        }

        let guard = self.out_tx.read().await;
        if let Some(tx) = guard.as_ref() {
            tx.send(message)
                .await
                .map_err(|e| TransportError::SendFailed(e.to_string()))
        } else {
            Err(TransportError::NotStarted)
        }
    }

    fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    async fn shutdown(&self) -> Result<(), TransportError> {
        self.running.store(false, Ordering::SeqCst);
        // Drop the sender to close the writer task
        let mut guard = self.out_tx.write().await;
        *guard = None;
        Ok(())
    }

    fn name(&self) -> &'static str {
        "stdio"
    }
}

/// Synchronous stdio transport for simpler use cases.
pub struct SyncStdioTransport;

impl SyncStdioTransport {
    /// Read a single message from stdin (blocking).
    pub fn read_message() -> Result<IncomingMessage, JsonRpcErrorResponse> {
        let stdin = io::stdin();
        let mut line = String::new();

        stdin
            .lock()
            .read_line(&mut line)
            .map_err(|e| JsonRpcErrorResponse::parse_error(format!("IO error: {}", e)))?;

        IncomingMessage::parse(line.trim())
    }

    /// Write a response to stdout (blocking).
    pub fn write_response(response: &JsonRpcResponse) -> io::Result<()> {
        let json = serde_json::to_string(response).unwrap();
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{}", json)?;
        stdout.flush()
    }

    /// Write an error to stdout (blocking).
    pub fn write_error(error: &JsonRpcErrorResponse) -> io::Result<()> {
        let json = serde_json::to_string(error).unwrap();
        let mut stdout = io::stdout().lock();
        writeln!(stdout, "{}", json)?;
        stdout.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_outgoing_message_response() {
        let response = JsonRpcResponse::new(1.into(), serde_json::json!({"status": "ok"}));
        let msg = OutgoingMessage::Response(response);
        let json = msg.to_json();
        assert!(json.contains("\"result\""));
    }

    #[test]
    fn test_outgoing_message_error() {
        let error = JsonRpcErrorResponse::method_not_found(1.into(), "test");
        let msg = OutgoingMessage::Error(error);
        let json = msg.to_json();
        assert!(json.contains("\"error\""));
    }
}
