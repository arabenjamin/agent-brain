//! Outbound MCP client — lets the brain call tools on EXTERNAL MCP servers
//! (aidevboard, worklittle, …). Runs in the brain process, which has network
//! egress; never the sandbox. JSON-RPC over HTTP with a best-effort session
//! handshake, handling both plain-JSON and SSE-framed response bodies.
//!
//! See `project-docs/MCP_CLIENT_PLAN.md`.

use serde_json::{Value, json};
use std::time::Duration;
use tracing::debug;

/// Connection details for one external MCP server, built from a `:McpServer`
/// node (or a bare URL for a keyless server). The secret itself is never held
/// here — only the NAME of the env var that carries it, read at call time.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    pub name: String,
    pub url: String,
    /// `none` | `bearer` | `header`.
    pub auth_scheme: String,
    /// Header name when `auth_scheme == "header"` (e.g. `X-API-Key`).
    pub auth_param: Option<String>,
    /// Name of the env var holding the secret. The value is injected per call.
    pub auth_env_var: Option<String>,
}

impl McpServerConfig {
    /// A keyless server addressed directly by URL.
    pub fn keyless(url: impl Into<String>) -> Self {
        let url = url.into();
        Self {
            name: url.clone(),
            url,
            auth_scheme: "none".to_string(),
            auth_param: None,
            auth_env_var: None,
        }
    }
}

/// Extract the JSON-RPC payload from a response body that is either plain JSON
/// or Server-Sent-Events framing (`event: …` / `data: {json}` lines). MCP
/// streamable-HTTP servers may answer a single request either way. Returns the
/// LAST `data:` payload that parses as JSON (the response frame follows any
/// notifications). Pure — unit-tested without a network.
pub fn extract_json_body(raw: &str) -> Result<Value, String> {
    let trimmed = raw.trim_start();
    if trimmed.starts_with('{') || trimmed.starts_with('[') {
        return serde_json::from_str(trimmed).map_err(|e| format!("invalid JSON body: {e}"));
    }
    // SSE framing: gather `data:` lines; a single frame's data may also span
    // multiple `data:` lines (concatenate with newlines), and there may be
    // several frames — keep the last one that parses.
    let mut last_ok: Option<Value> = None;
    let mut buf: Vec<String> = Vec::new();
    let flush = |buf: &mut Vec<String>, last_ok: &mut Option<Value>| {
        if buf.is_empty() {
            return;
        }
        let joined = buf.join("\n");
        if let Ok(v) = serde_json::from_str::<Value>(&joined) {
            *last_ok = Some(v);
        }
        buf.clear();
    };
    for line in raw.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            buf.push(rest.trim_start().to_string());
        } else if line.trim().is_empty() {
            flush(&mut buf, &mut last_ok);
        }
    }
    flush(&mut buf, &mut last_ok);
    last_ok.ok_or_else(|| "no JSON payload found in response body".to_string())
}

/// Pull the useful text out of an MCP `tools/call` result's `content` array
/// (concatenating every `{type:"text"}` block). Empty when there is none.
pub fn content_text(result: &Value) -> String {
    result
        .get("content")
        .and_then(|c| c.as_array())
        .map(|items| {
            items
                .iter()
                .filter_map(|it| {
                    if it.get("type").and_then(|t| t.as_str()) == Some("text") {
                        it.get("text").and_then(|t| t.as_str())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .join("\n")
        })
        .unwrap_or_default()
}

/// An outbound MCP-over-HTTP client. Cheap to construct; holds a `reqwest::Client`.
pub struct McpClient {
    http: reqwest::Client,
    timeout: Duration,
}

impl Default for McpClient {
    fn default() -> Self {
        Self::new()
    }
}

impl McpClient {
    pub fn new() -> Self {
        Self {
            // Some servers refuse a request with no User-Agent: Exa's hosted
            // MCP answers 403 (found 2026-09-26), and reqwest sends none.
            // Under the network policy: `mcp_call` also takes a bare URL a
            // model chose. The registered servers are public; an internal one
            // goes in HTTP_ALLOW_INTERNAL.
            // A server's key rides on every call, so a redirect never leaves
            // the server's host (`guard_pinned`).
            http: agent_brain_protocol::net_policy::guard_pinned(reqwest::Client::builder())
                .user_agent(concat!("agent-brain/", env!("CARGO_PKG_VERSION")))
                .build()
                .unwrap_or_default(),
            timeout: Duration::from_secs(30),
        }
    }

    /// Put the server's secret on `req`, if the secrets index binds it to the
    /// server's host; a refusal stops the call (`secrets::resolve`).
    fn with_auth(
        &self,
        mut req: reqwest::RequestBuilder,
        cfg: &McpServerConfig,
    ) -> Result<reqwest::RequestBuilder, String> {
        if let Some(env_var) = cfg.auth_env_var.as_deref().filter(|n| !n.trim().is_empty())
            && let Some(secret) = agent_brain_protocol::secrets::resolve(env_var, &cfg.url)
                .map_err(|why| format!("{}: not requested: {why}", cfg.name))?
        {
            match cfg.auth_scheme.as_str() {
                "bearer" => {
                    req = req.header(reqwest::header::AUTHORIZATION, format!("Bearer {secret}"));
                }
                "header" => {
                    if let Some(ref param) = cfg.auth_param {
                        req = req.header(param.as_str(), secret);
                    }
                }
                _ => {}
            }
        }
        Ok(req)
    }

    /// One JSON-RPC call. Returns `(response_json, session_id_header)`.
    async fn rpc(
        &self,
        cfg: &McpServerConfig,
        session: Option<&str>,
        method: &str,
        params: Value,
    ) -> Result<(Value, Option<String>), String> {
        let body = json!({ "jsonrpc": "2.0", "id": 1, "method": method, "params": params });
        let mut req = self
            .http
            .post(&cfg.url)
            .timeout(self.timeout)
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&body);
        if let Some(sid) = session {
            req = req.header("Mcp-Session-Id", sid);
        }
        req = self.with_auth(req, cfg)?;

        let resp =
            req.send().await.map_err(|e| {
                match agent_brain_protocol::net_policy::refusal_in(&e) {
                    Some(why) => format!("{}: not requested: {why}", cfg.name),
                    None => format!("{}: request failed: {e}", cfg.name),
                }
            })?;
        let status = resp.status();
        let sid = resp
            .headers()
            .get("Mcp-Session-Id")
            .and_then(|v| v.to_str().ok())
            .map(|s| s.to_string());
        let text = resp.text().await.unwrap_or_default();
        if !status.is_success() {
            return Err(format!(
                "{}: HTTP {} from {method}: {}",
                cfg.name,
                status.as_u16(),
                text.chars().take(300).collect::<String>()
            ));
        }
        let value = extract_json_body(&text)?;
        if let Some(err) = value.get("error") {
            return Err(format!("{}: {method} error: {err}", cfg.name));
        }
        Ok((value, sid))
    }

    /// Best-effort MCP handshake. Returns a session id if the server issued one.
    /// Never fatal — a server that does not implement `initialize` still serves
    /// `tools/list`/`tools/call`, so a failure here is logged and ignored.
    async fn establish_session(&self, cfg: &McpServerConfig) -> Option<String> {
        let params = json!({
            "protocolVersion": "2025-06-18",
            "capabilities": {},
            "clientInfo": { "name": "agent-brain", "version": "1" }
        });
        match self.rpc(cfg, None, "initialize", params).await {
            Ok((_, sid)) => {
                // Fire-and-forget the initialized notification (no id).
                let note = json!({ "jsonrpc": "2.0", "method": "notifications/initialized", "params": {} });
                let mut req = self
                    .http
                    .post(&cfg.url)
                    .timeout(self.timeout)
                    .header(reqwest::header::CONTENT_TYPE, "application/json")
                    .header(
                        reqwest::header::ACCEPT,
                        "application/json, text/event-stream",
                    )
                    .json(&note);
                if let Some(ref sid) = sid {
                    req = req.header("Mcp-Session-Id", sid.as_str());
                }
                if let Ok(req) = self.with_auth(req, cfg) {
                    let _ = req.send().await;
                }
                sid
            }
            Err(e) => {
                debug!(server = %cfg.name, error = %e, "mcp initialize failed — proceeding without a session");
                None
            }
        }
    }

    /// List the server's tools. Returns the raw `tools` array.
    pub async fn list_tools(&self, cfg: &McpServerConfig) -> Result<Vec<Value>, String> {
        let sid = self.establish_session(cfg).await;
        let (resp, _) = self
            .rpc(cfg, sid.as_deref(), "tools/list", json!({}))
            .await?;
        let tools = resp
            .get("result")
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools)
    }

    /// Call one tool. Returns the JSON-RPC `result` object
    /// (`content`, `structuredContent`, `isError`, …).
    pub async fn call_tool(
        &self,
        cfg: &McpServerConfig,
        tool: &str,
        arguments: Value,
    ) -> Result<Value, String> {
        let sid = self.establish_session(cfg).await;
        let params = json!({ "name": tool, "arguments": arguments });
        let (resp, _) = self.rpc(cfg, sid.as_deref(), "tools/call", params).await?;
        resp.get("result")
            .cloned()
            .ok_or_else(|| format!("{}: tools/call returned no result", cfg.name))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_plain_json_body() {
        let v = extract_json_body(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#).unwrap();
        assert_eq!(v["result"]["ok"], true);
    }

    #[test]
    fn parses_sse_framed_body() {
        let raw = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"n\":42}}\n\n";
        let v = extract_json_body(raw).unwrap();
        assert_eq!(v["result"]["n"], 42);
    }

    #[test]
    fn keeps_last_frame_when_several() {
        let raw = "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\n\
                   data: {\"jsonrpc\":\"2.0\",\"id\":1,\"result\":{\"done\":true}}\n\n";
        let v = extract_json_body(raw).unwrap();
        assert_eq!(v["result"]["done"], true);
    }

    #[test]
    fn empty_body_is_an_error_not_a_panic() {
        assert!(extract_json_body("").is_err());
        assert!(extract_json_body("event: ping\n\n").is_err());
    }

    #[test]
    fn content_text_joins_text_blocks() {
        let result = json!({
            "content": [
                { "type": "text", "text": "line one" },
                { "type": "image", "data": "…" },
                { "type": "text", "text": "line two" }
            ]
        });
        assert_eq!(content_text(&result), "line one\nline two");
    }

    #[test]
    fn content_text_empty_when_none() {
        assert_eq!(content_text(&json!({ "structuredContent": {} })), "");
    }
}
