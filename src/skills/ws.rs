//! WebSocket Skill — connect to WebSocket endpoints, send and receive messages.
//!
//! Connections are stored in-process (keyed by a caller-supplied or auto-generated ID)
//! and survive across multiple tool calls within the same server lifetime.
//! Useful for real-time interaction with the Gizmo signaling server or any WS endpoint.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Mutex, RwLock};
use tokio::time::timeout;
use tokio_tungstenite::{connect_async, tungstenite::Message};
use tracing::{info, warn};
use uuid::Uuid;

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::skills::Skill;

// ============================================================================
// Connection store
// ============================================================================

type WsStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

struct WsConn {
    url: String,
    stream: Mutex<Option<WsStream>>,
}

// ============================================================================
// Skill struct
// ============================================================================

pub struct WsSkill {
    connections: Arc<RwLock<HashMap<String, Arc<WsConn>>>>,
}

impl Default for WsSkill {
    fn default() -> Self {
        Self::new()
    }
}

impl WsSkill {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    // ========================================================================
    // Tool definitions
    // ========================================================================

    fn ws_connect_def() -> ToolDefinition {
        ToolDefinition {
            name: "ws_connect".to_string(),
            description: "Connect to a WebSocket endpoint. Returns a connection_id used by \
                          ws_send, ws_receive, and ws_close. \
                          Useful for the Gizmo signaling server (ws://host:8081/ws) or \
                          any other WS endpoint."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "WebSocket URL to connect to (ws:// or wss://)"
                    },
                    "connection_id": {
                        "type": "string",
                        "description": "Optional stable ID for this connection. Auto-generated if omitted."
                    }
                },
                "required": ["url"]
            }),
        }
    }

    fn ws_send_def() -> ToolDefinition {
        ToolDefinition {
            name: "ws_send".to_string(),
            description: "Send a text message over an open WebSocket connection.".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "connection_id": {
                        "type": "string",
                        "description": "Connection ID returned by ws_connect"
                    },
                    "message": {
                        "type": "string",
                        "description": "Text message to send"
                    }
                },
                "required": ["connection_id", "message"]
            }),
        }
    }

    fn ws_receive_def() -> ToolDefinition {
        ToolDefinition {
            name: "ws_receive".to_string(),
            description: "Receive the next message from an open WebSocket connection. \
                          Blocks until a message arrives or the timeout elapses."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "connection_id": {
                        "type": "string",
                        "description": "Connection ID returned by ws_connect"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": "Max milliseconds to wait (default: 5000)"
                    }
                },
                "required": ["connection_id"]
            }),
        }
    }

    fn ws_close_def() -> ToolDefinition {
        ToolDefinition {
            name: "ws_close".to_string(),
            description: "Close an open WebSocket connection and remove it from the store."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "connection_id": {
                        "type": "string",
                        "description": "Connection ID returned by ws_connect"
                    }
                },
                "required": ["connection_id"]
            }),
        }
    }

    // ========================================================================
    // Handlers
    // ========================================================================

    async fn handle_ws_connect(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            url: String,
            connection_id: Option<String>,
        }
        let input: Input = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let conn_id = input
            .connection_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        // Check for existing open connection with same ID.
        {
            let conns = self.connections.read().await;
            if conns.contains_key(&conn_id) {
                return ToolCallResult::error(format!(
                    "Connection '{}' already exists. Close it first.",
                    conn_id
                ));
            }
        }

        info!(conn_id = %conn_id, url = %input.url, "Connecting WebSocket");

        let (ws_stream, _response) = match connect_async(&input.url).await {
            Ok(pair) => pair,
            Err(e) => {
                warn!(error = %e, url = %input.url, "WebSocket connect failed");
                return ToolCallResult::error(format!("WebSocket connect failed: {}", e));
            }
        };

        let conn = Arc::new(WsConn {
            url: input.url.clone(),
            stream: Mutex::new(Some(ws_stream)),
        });

        self.connections.write().await.insert(conn_id.clone(), conn);

        info!(conn_id = %conn_id, "WebSocket connected");
        ToolCallResult::success_text(
            serde_json::to_string_pretty(&json!({
                "connection_id": conn_id,
                "url": input.url,
                "status": "connected"
            }))
            .unwrap(),
        )
    }

    async fn handle_ws_send(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            connection_id: String,
            message: String,
        }
        let input: Input = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let conn = {
            let conns = self.connections.read().await;
            match conns.get(&input.connection_id) {
                Some(c) => Arc::clone(c),
                None => {
                    return ToolCallResult::error(format!(
                        "No connection '{}'. Call ws_connect first.",
                        input.connection_id
                    ));
                }
            }
        };

        let mut guard = conn.stream.lock().await;
        let stream = match guard.as_mut() {
            Some(s) => s,
            None => return ToolCallResult::error("Connection is closed.".to_string()),
        };

        match stream
            .send(Message::Text(input.message.clone().into()))
            .await
        {
            Ok(_) => {
                info!(conn_id = %input.connection_id, bytes = input.message.len(), "WS message sent");
                ToolCallResult::success_text(
                    serde_json::to_string_pretty(&json!({
                        "connection_id": input.connection_id,
                        "sent": input.message
                    }))
                    .unwrap(),
                )
            }
            Err(e) => ToolCallResult::error(format!("Send failed: {}", e)),
        }
    }

    async fn handle_ws_receive(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            connection_id: String,
            timeout_ms: Option<u64>,
        }
        let input: Input = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let wait = Duration::from_millis(input.timeout_ms.unwrap_or(5000));

        let conn = {
            let conns = self.connections.read().await;
            match conns.get(&input.connection_id) {
                Some(c) => Arc::clone(c),
                None => {
                    return ToolCallResult::error(format!(
                        "No connection '{}'. Call ws_connect first.",
                        input.connection_id
                    ));
                }
            }
        };

        let mut guard = conn.stream.lock().await;
        let stream = match guard.as_mut() {
            Some(s) => s,
            None => return ToolCallResult::error("Connection is closed.".to_string()),
        };

        match timeout(wait, stream.next()).await {
            Ok(Some(Ok(msg))) => {
                let text = match msg {
                    Message::Text(t) => t.to_string(),
                    Message::Binary(b) => format!("<binary {} bytes>", b.len()),
                    Message::Ping(_) => "<ping>".to_string(),
                    Message::Pong(_) => "<pong>".to_string(),
                    Message::Close(_) => "<close>".to_string(),
                    _ => "<unknown>".to_string(),
                };
                info!(conn_id = %input.connection_id, "WS message received");
                ToolCallResult::success_text(
                    serde_json::to_string_pretty(&json!({
                        "connection_id": input.connection_id,
                        "message": text
                    }))
                    .unwrap(),
                )
            }
            Ok(Some(Err(e))) => ToolCallResult::error(format!("Receive error: {}", e)),
            Ok(None) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "connection_id": input.connection_id,
                    "message": null,
                    "reason": "connection closed by server"
                }))
                .unwrap(),
            ),
            Err(_) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "connection_id": input.connection_id,
                    "message": null,
                    "reason": "timeout"
                }))
                .unwrap(),
            ),
        }
    }

    async fn handle_ws_close(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            connection_id: String,
        }
        let input: Input = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        let conn = self.connections.write().await.remove(&input.connection_id);
        match conn {
            None => ToolCallResult::error(format!("No connection '{}'.", input.connection_id)),
            Some(c) => {
                // Send a graceful close frame if stream is still open.
                let mut guard = c.stream.lock().await;
                if let Some(stream) = guard.as_mut() {
                    let _ = stream.close(None).await;
                }
                *guard = None;
                info!(conn_id = %input.connection_id, "WebSocket closed");
                ToolCallResult::success_text(
                    serde_json::to_string_pretty(&json!({
                        "connection_id": input.connection_id,
                        "status": "closed"
                    }))
                    .unwrap(),
                )
            }
        }
    }

    async fn handle_ws_list(&self, _arguments: Option<Value>) -> ToolCallResult {
        let conns = self.connections.read().await;
        let list: Vec<Value> = conns
            .iter()
            .map(|(id, c)| {
                json!({
                    "connection_id": id,
                    "url": c.url,
                })
            })
            .collect();
        ToolCallResult::success_text(
            serde_json::to_string_pretty(&json!({ "connections": list })).unwrap(),
        )
    }

    fn ws_list_def() -> ToolDefinition {
        ToolDefinition {
            name: "ws_list".to_string(),
            description: "List all currently open WebSocket connections.".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }
}

// ============================================================================
// Skill trait impl
// ============================================================================

#[async_trait]
impl Skill for WsSkill {
    fn name(&self) -> &str {
        "WebSocket"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::ws_connect_def(),
            Self::ws_send_def(),
            Self::ws_receive_def(),
            Self::ws_close_def(),
            Self::ws_list_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "ws_connect" => Some(self.handle_ws_connect(arguments).await),
            "ws_send" => Some(self.handle_ws_send(arguments).await),
            "ws_receive" => Some(self.handle_ws_receive(arguments).await),
            "ws_close" => Some(self.handle_ws_close(arguments).await),
            "ws_list" => Some(self.handle_ws_list(arguments).await),
            _ => None,
        }
    }
}

// ============================================================================
// Helpers
// ============================================================================

fn parse_args<T: for<'de> Deserialize<'de>>(arguments: Option<Value>) -> Result<T, ToolCallResult> {
    let args = arguments.unwrap_or(Value::Object(Default::default()));
    serde_json::from_value(args)
        .map_err(|e| ToolCallResult::error(format!("Invalid arguments: {}", e)))
}
