//! Stdio transport for MCP server.

use std::io::{self, BufRead, Write};

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tracing::{debug, error, trace};

use super::protocol::{IncomingMessage, JsonRpcErrorResponse, JsonRpcResponse};

/// Message to send to the client.
#[derive(Debug)]
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
pub struct StdioTransport {
    tx: mpsc::Sender<OutgoingMessage>,
}

impl StdioTransport {
    /// Create a new stdio transport and spawn reader/writer tasks.
    /// Returns the transport and a receiver for incoming messages.
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

        (Self { tx: out_tx }, in_rx)
    }

    /// Send a response to the client.
    pub async fn send(
        &self,
        msg: OutgoingMessage,
    ) -> Result<(), mpsc::error::SendError<OutgoingMessage>> {
        self.tx.send(msg).await
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
