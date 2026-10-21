//! WebSocket Skill — connect to WebSocket endpoints, send and receive messages.
//!
//! Connections are held in-process, keyed by a caller-supplied or generated
//! ID, and survive across tool calls for the server's lifetime (not restarts).
//!
//! A URL a model chose is judged by the net policy before any socket opens:
//! `net_policy::connect_addrs` refuses internal addresses (Neo4j, the
//! sandboxes, the LAN, the tailnet) unless the owner allowed the host with
//! `HTTP_ALLOW_INTERNAL`, and returns the checked addresses. The socket goes
//! to one of those, never to a second lookup of the name, so a DNS answer
//! that changes between check and connect cannot steer it inside.
//!
//! Limits keep a chatty feed from flooding a model's context: at most
//! [`MAX_CONNECTIONS`] open at once, a message is cut at [`MAX_MESSAGE_CHARS`]
//! (marked, with its full size), and one receive drains at most
//! [`MAX_BATCH`] messages.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, RwLock};
use tokio::time::{Instant, timeout};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};
use tracing::{info, warn};
use uuid::Uuid;

use agent_brain_plugin_api::protocol::net_policy;
use agent_brain_plugin_api::{Skill, ToolCallResult, ToolDefinition, parse_args};

/// Open connections at once; a forgotten one holds a socket until closed.
const MAX_CONNECTIONS: usize = 8;
/// A received message longer than this is cut, and says so.
const MAX_MESSAGE_CHARS: usize = 16_000;
/// Messages one `ws_receive` may drain.
const MAX_BATCH: usize = 20;
/// The largest frame the client accepts at all.
const MAX_FRAME_BYTES: usize = 4 << 20;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_RECEIVE_MS: u64 = 5_000;
const MAX_RECEIVE_MS: u64 = 60_000;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;

struct WsConn {
    url: String,
    stream: Mutex<Option<WsStream>>,
}

pub struct WsSkill {
    connections: Arc<RwLock<HashMap<String, Arc<WsConn>>>>,
}

impl Default for WsSkill {
    fn default() -> Self {
        Self::new()
    }
}

/// A message as the model sees it: text, cut at [`MAX_MESSAGE_CHARS`].
fn render(msg: Message) -> Value {
    let (kind, text) = match msg {
        Message::Text(t) => ("text", t.to_string()),
        Message::Binary(b) => ("binary", format!("<binary {} bytes>", b.len())),
        Message::Ping(_) => ("ping", String::new()),
        Message::Pong(_) => ("pong", String::new()),
        Message::Close(c) => (
            "close",
            c.map(|f| format!("{} {}", f.code, f.reason))
                .unwrap_or_default(),
        ),
        Message::Frame(_) => ("frame", String::new()),
    };
    let chars = text.chars().count();
    if chars > MAX_MESSAGE_CHARS {
        let cut: String = text.chars().take(MAX_MESSAGE_CHARS).collect();
        json!({"kind": kind, "text": cut, "truncated": true, "full_chars": chars})
    } else {
        json!({"kind": kind, "text": text})
    }
}

impl WsSkill {
    pub fn new() -> Self {
        Self {
            connections: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// The open connection IDs, for an error that names what does exist.
    async fn open_ids(&self) -> String {
        let conns = self.connections.read().await;
        if conns.is_empty() {
            return "none are open".to_string();
        }
        let mut ids: Vec<String> = conns
            .iter()
            .map(|(id, c)| format!("{id} ({})", c.url))
            .collect();
        ids.sort();
        format!("open: {}", ids.join(", "))
    }

    async fn get(&self, id: &str) -> Result<Arc<WsConn>, ToolCallResult> {
        if let Some(c) = self.connections.read().await.get(id) {
            return Ok(Arc::clone(c));
        }
        Err(ToolCallResult::error(format!(
            "No connection '{id}' ({}). Call ws_connect first.",
            self.open_ids().await
        )))
    }

    // ── Tool definitions ─────────────────────────────────────────────────────

    fn ws_connect_def() -> ToolDefinition {
        ToolDefinition {
            name: "ws_connect".to_string(),
            description: "Open a WebSocket connection (ws:// or wss://) that stays open across \
                          tool calls. Returns a connection_id for ws_send, ws_receive and ws_close. \
                          For streaming feeds and push APIs http_request cannot read. Internal \
                          hosts are refused unless the owner allowed them. Close it when done."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "WebSocket URL (ws:// or wss://)"
                    },
                    "connection_id": {
                        "type": "string",
                        "description": "Optional stable ID for this connection. Generated if omitted."
                    }
                },
                "required": ["url"]
            }),
        }
    }

    fn ws_send_def() -> ToolDefinition {
        ToolDefinition {
            name: "ws_send".to_string(),
            description: "Send a text message over an open WebSocket connection (JSON goes \
                          as its text)."
                .to_string(),
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
            description: format!(
                "Receive from an open WebSocket connection: waits up to timeout_ms for the \
                 first message, then returns it with any already waiting, up to max_messages \
                 (default 1, at most {MAX_BATCH}). A message over {MAX_MESSAGE_CHARS} chars is \
                 cut and marked truncated. No message in time is not an error: `reason` says \
                 timeout."
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "connection_id": {
                        "type": "string",
                        "description": "Connection ID returned by ws_connect"
                    },
                    "timeout_ms": {
                        "type": "integer",
                        "description": format!("Max milliseconds to wait for the first message (default {DEFAULT_RECEIVE_MS}, at most {MAX_RECEIVE_MS})")
                    },
                    "max_messages": {
                        "type": "integer",
                        "description": format!("Messages to return at most (default 1, at most {MAX_BATCH})")
                    }
                },
                "required": ["connection_id"]
            }),
        }
    }

    fn ws_close_def() -> ToolDefinition {
        ToolDefinition {
            name: "ws_close".to_string(),
            description: "Close an open WebSocket connection and forget it.".to_string(),
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

    // ── Handlers ─────────────────────────────────────────────────────────────

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

        let url = match url::Url::parse(&input.url) {
            Ok(u) if matches!(u.scheme(), "ws" | "wss") => u,
            Ok(u) => {
                return ToolCallResult::error(format!(
                    "ws_connect takes a ws:// or wss:// URL, not {}:. Use http_request for http.",
                    u.scheme()
                ));
            }
            Err(e) => return ToolCallResult::error(format!("Not a URL: {e}")),
        };
        let conn_id = input
            .connection_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        {
            let conns = self.connections.read().await;
            if conns.contains_key(&conn_id) {
                return ToolCallResult::error(format!(
                    "Connection '{conn_id}' already exists. Close it first."
                ));
            }
            if conns.len() >= MAX_CONNECTIONS {
                drop(conns);
                return ToolCallResult::error(format!(
                    "{MAX_CONNECTIONS} connections are already open ({}). Close one first.",
                    self.open_ids().await
                ));
            }
        }

        let addrs = match net_policy::connect_addrs(&url).await {
            Ok(a) => a,
            Err(e) => return ToolCallResult::error(e),
        };

        info!(conn_id = %conn_id, url = %url, "Connecting WebSocket");
        let opened = timeout(CONNECT_TIMEOUT, async {
            let mut last = format!("{} resolved to no address", url);
            for addr in &addrs {
                match TcpStream::connect(addr).await {
                    Ok(tcp) => {
                        let config = WebSocketConfig::default()
                            .max_message_size(Some(MAX_FRAME_BYTES))
                            .max_frame_size(Some(MAX_FRAME_BYTES));
                        return tokio_tungstenite::client_async_tls_with_config(
                            url.as_str(),
                            tcp,
                            Some(config),
                            None,
                        )
                        .await
                        .map(|(ws, _)| ws)
                        .map_err(|e| format!("WebSocket handshake failed: {e}"));
                    }
                    Err(e) => last = format!("TCP connect to {addr} failed: {e}"),
                }
            }
            Err(last)
        })
        .await;
        let ws = match opened {
            Ok(Ok(ws)) => ws,
            Ok(Err(e)) => {
                warn!(error = %e, url = %url, "WebSocket connect failed");
                return ToolCallResult::error(e);
            }
            Err(_) => {
                return ToolCallResult::error(format!(
                    "WebSocket connect timed out after {}s",
                    CONNECT_TIMEOUT.as_secs()
                ));
            }
        };

        let conn = Arc::new(WsConn {
            url: url.to_string(),
            stream: Mutex::new(Some(ws)),
        });
        {
            let mut conns = self.connections.write().await;
            // Another call may have taken the ID or the last slot meanwhile.
            if conns.contains_key(&conn_id) || conns.len() >= MAX_CONNECTIONS {
                drop(conns);
                if let Some(mut s) = conn.stream.lock().await.take() {
                    let _ = s.close(None).await;
                }
                return ToolCallResult::error(format!(
                    "Connection '{conn_id}' could not be stored ({}).",
                    self.open_ids().await
                ));
            }
            conns.insert(conn_id.clone(), conn);
        }

        info!(conn_id = %conn_id, "WebSocket connected");
        ToolCallResult::success_json(json!({
            "connection_id": conn_id,
            "url": url.as_str(),
            "status": "connected"
        }))
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
        let conn = match self.get(&input.connection_id).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        let mut guard = conn.stream.lock().await;
        let Some(stream) = guard.as_mut() else {
            return ToolCallResult::error("Connection is closed.".to_string());
        };
        match stream
            .send(Message::Text(input.message.clone().into()))
            .await
        {
            Ok(_) => {
                info!(conn_id = %input.connection_id, bytes = input.message.len(), "WS message sent");
                ToolCallResult::success_json(json!({
                    "connection_id": input.connection_id,
                    "sent_bytes": input.message.len()
                }))
            }
            Err(e) => ToolCallResult::error(format!("Send failed: {e}")),
        }
    }

    async fn handle_ws_receive(&self, arguments: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            connection_id: String,
            timeout_ms: Option<u64>,
            max_messages: Option<usize>,
        }
        let input: Input = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };
        let wait = Duration::from_millis(
            input
                .timeout_ms
                .unwrap_or(DEFAULT_RECEIVE_MS)
                .min(MAX_RECEIVE_MS),
        );
        let max = input.max_messages.unwrap_or(1).clamp(1, MAX_BATCH);
        let conn = match self.get(&input.connection_id).await {
            Ok(c) => c,
            Err(e) => return e,
        };
        let mut guard = conn.stream.lock().await;
        let Some(stream) = guard.as_mut() else {
            return ToolCallResult::error("Connection is closed.".to_string());
        };

        // Wait for the first message; then take only what is already waiting.
        let deadline = Instant::now() + wait;
        let mut messages = Vec::new();
        let mut reason = None;
        while messages.len() < max {
            let left = if messages.is_empty() {
                deadline.saturating_duration_since(Instant::now())
            } else {
                Duration::from_millis(50)
            };
            match timeout(left, stream.next()).await {
                Ok(Some(Ok(msg))) => {
                    let closed = matches!(msg, Message::Close(_));
                    messages.push(render(msg));
                    if closed {
                        reason = Some("connection closed by server");
                        break;
                    }
                }
                Ok(Some(Err(e))) => {
                    if messages.is_empty() {
                        return ToolCallResult::error(format!("Receive error: {e}"));
                    }
                    reason = Some("receive error after the messages shown");
                    break;
                }
                Ok(None) => {
                    reason = Some("connection closed by server");
                    break;
                }
                Err(_) => {
                    if messages.is_empty() {
                        reason = Some("timeout");
                    }
                    break;
                }
            }
        }
        info!(conn_id = %input.connection_id, n = messages.len(), "WS messages received");
        let mut out = json!({
            "connection_id": input.connection_id,
            "count": messages.len(),
            "messages": messages,
        });
        if let Some(r) = reason {
            out["reason"] = json!(r);
        }
        ToolCallResult::success_json(out)
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
        let removed = self.connections.write().await.remove(&input.connection_id);
        let Some(conn) = removed else {
            return ToolCallResult::error(format!(
                "No connection '{}' ({}).",
                input.connection_id,
                self.open_ids().await
            ));
        };
        if let Some(mut stream) = conn.stream.lock().await.take() {
            let _ = stream.close(None).await;
        }
        info!(conn_id = %input.connection_id, "WebSocket closed");
        ToolCallResult::success_json(json!({
            "connection_id": input.connection_id,
            "status": "closed"
        }))
    }
}

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
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "ws_connect" => Some(self.handle_ws_connect(arguments).await),
            "ws_send" => Some(self.handle_ws_send(arguments).await),
            "ws_receive" => Some(self.handle_ws_receive(arguments).await),
            "ws_close" => Some(self.handle_ws_close(arguments).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text(r: &ToolCallResult) -> String {
        serde_json::to_string(r).unwrap()
    }

    async fn call(s: &WsSkill, tool: &str, args: Value) -> ToolCallResult {
        s.execute(tool, Some(args)).await.unwrap()
    }

    #[tokio::test]
    async fn internal_hosts_are_refused_before_any_socket_opens() {
        let s = WsSkill::new();
        for url in [
            "ws://127.0.0.1:8081/ws",
            "ws://localhost:7687/",
            "ws://[::1]/",
        ] {
            let r = text(&call(&s, "ws_connect", json!({"url": url})).await);
            assert!(r.contains("[net policy]"), "{url}: {r}");
        }
        assert!(s.connections.read().await.is_empty());
    }

    #[tokio::test]
    async fn only_websocket_urls_are_taken() {
        let s = WsSkill::new();
        let r = text(&call(&s, "ws_connect", json!({"url": "https://example.com"})).await);
        assert!(
            r.contains("ws:// or wss://") && r.contains("http_request"),
            "{r}"
        );
        let r = text(&call(&s, "ws_connect", json!({"url": "not a url"})).await);
        assert!(r.contains("Not a URL"), "{r}");
    }

    #[tokio::test]
    async fn an_unknown_connection_names_what_is_open() {
        let s = WsSkill::new();
        for tool in ["ws_send", "ws_receive", "ws_close"] {
            let r = text(&call(&s, tool, json!({"connection_id": "x", "message": "m"})).await);
            assert!(
                r.contains("No connection 'x'") && r.contains("none are open"),
                "{tool}: {r}"
            );
        }
    }

    #[test]
    fn a_long_message_is_cut_and_says_so() {
        let v = render(Message::Text("é".repeat(MAX_MESSAGE_CHARS + 5).into()));
        assert_eq!(v["truncated"], json!(true));
        assert_eq!(v["full_chars"], json!(MAX_MESSAGE_CHARS + 5));
        assert_eq!(
            v["text"].as_str().unwrap().chars().count(),
            MAX_MESSAGE_CHARS
        );
        let v = render(Message::Text("hi".into()));
        assert_eq!(v, json!({"kind": "text", "text": "hi"}));
    }

    #[test]
    fn four_tools() {
        let names: Vec<String> = WsSkill::new().tools().into_iter().map(|t| t.name).collect();
        assert_eq!(names, ["ws_connect", "ws_send", "ws_receive", "ws_close"]);
    }
}
