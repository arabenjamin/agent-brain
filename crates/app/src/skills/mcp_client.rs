//! MCP Client Skill — call tools on EXTERNAL MCP servers from the brain.
//!
//! Two passthrough tools make any registered server usable from chat and from
//! chains/schedules: `mcp_list_tools` (no `server` ⇒ the registered servers) and
//! `mcp_call`. Servers
//! are `(:McpServer)` nodes seeded from `mcp-servers/*.yaml`; a bare `https://`
//! URL is also accepted directly for a keyless server. Auth secrets live only in
//! env vars (the node stores the env var NAME). Runs in the brain process, which
//! has egress. See `project-docs/MCP_CLIENT_PLAN.md`.

use async_trait::async_trait;
use serde_json::{Value, json};

use crate::repository::Neo4jClient;
use crate::services::mcp_client::{McpClient, McpServerConfig, content_text};
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

pub struct McpClientSkill {
    neo4j: Option<Neo4jClient>,
    client: McpClient,
}

impl McpClientSkill {
    pub fn new(neo4j: Option<Neo4jClient>) -> Self {
        Self {
            neo4j,
            client: McpClient::new(),
        }
    }

    /// Resolve a `server` argument to a connection config: a registered
    /// `:McpServer` by name, or a bare `https://`/`http://` URL used directly
    /// (keyless). A disabled server is refused.
    async fn resolve_server(&self, server: &str) -> Result<McpServerConfig, String> {
        if let Some(ref neo4j) = self.neo4j {
            let rows = neo4j
                .execute(
                    neo4rs::query(
                        "MATCH (m:McpServer {name: $name}) \
                         RETURN m.url AS url, m.auth_scheme AS auth_scheme, \
                                m.auth_param AS auth_param, m.auth_env_var AS auth_env_var, \
                                m.enabled AS enabled",
                    )
                    .param("name", server),
                )
                .await
                .map_err(|e| format!("failed to read McpServer '{server}': {e}"))?;

            if let Some(row) = rows.first() {
                let enabled: bool = row.get("enabled").unwrap_or(true);
                if !enabled {
                    return Err(format!(
                        "MCP server '{server}' is disabled. Enable it with a Cypher SET before use."
                    ));
                }
                let url: String = row.get("url").unwrap_or_default();
                if url.is_empty() {
                    return Err(format!("MCP server '{server}' has no url."));
                }
                return Ok(McpServerConfig {
                    name: server.to_string(),
                    url,
                    auth_scheme: row
                        .get::<String>("auth_scheme")
                        .unwrap_or_else(|_| "none".into()),
                    auth_param: row.get::<String>("auth_param").ok(),
                    auth_env_var: row.get::<String>("auth_env_var").ok(),
                });
            }
        }

        // Fallback: a bare URL is a keyless server addressed directly.
        if server.starts_with("https://") || server.starts_with("http://") {
            return Ok(McpServerConfig::keyless(server));
        }

        Err(format!(
            "No MCP server named '{server}'. Call `mcp_list_tools` with no `server` to see registered servers, \
             or pass a full https:// URL for a keyless server."
        ))
    }

    fn list_tools_def() -> ToolDefinition {
        ToolDefinition {
            name: "mcp_list_tools".to_string(),
            description: "Without `server`: list the external MCP servers the brain can call \
                          (name, url, description, enabled). With `server`: list the tools that \
                          server exposes (its tools/list), to discover tool names and argument \
                          schemas before mcp_call."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "Registered server name (omit to list them) or a full https:// URL for a keyless server."
                    }
                }
            }),
        }
    }

    fn call_def() -> ToolDefinition {
        ToolDefinition {
            name: "mcp_call".to_string(),
            description: "Call a tool on an external MCP server (its tools/call). Pass the server \
                          name, the tool name (from mcp_list_tools), and the tool's arguments. The \
                          text result is returned under `answer` so it flows through a chain's \
                          {{_prev}}. A remote tool error is returned as a successful call carrying \
                          the error text (is_error=true), so it does not fail the chain. NOTE: \
                          destructive remote tools (e.g. submitting a job application) are gated on \
                          the remote side and require an explicit human-approved token — never \
                          claim such an action succeeded unless the result says so."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "server": {
                        "type": "string",
                        "description": "Registered server name or a full https:// URL."
                    },
                    "tool": {
                        "type": "string",
                        "description": "Tool name to call on that server."
                    },
                    "arguments": {
                        "type": "object",
                        "description": "Arguments object for the remote tool (per its input schema)."
                    }
                },
                "required": ["server", "tool"]
            }),
        }
    }

    async fn handle_list_servers(&self) -> ToolCallResult {
        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };
        let rows = match neo4j
            .execute(neo4rs::query(
                "MATCH (m:McpServer) RETURN m.name AS name, m.url AS url, \
                 m.description AS description, m.enabled AS enabled ORDER BY m.name",
            ))
            .await
        {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("failed to list MCP servers: {e}")),
        };
        let servers: Vec<Value> = rows
            .iter()
            .map(|row| {
                json!({
                    "name": row.get::<String>("name").unwrap_or_default(),
                    "url": row.get::<String>("url").unwrap_or_default(),
                    "description": row.get::<String>("description").unwrap_or_default(),
                    "enabled": row.get::<bool>("enabled").unwrap_or(true),
                })
            })
            .collect();
        ToolCallResult::success_json(json!({ "count": servers.len(), "servers": servers }))
    }

    async fn handle_list_tools(&self, args: Option<Value>) -> ToolCallResult {
        let args = args.unwrap_or_default();
        let Some(server) = args["server"].as_str().filter(|s| !s.trim().is_empty()) else {
            return self.handle_list_servers().await;
        };
        let cfg = match self.resolve_server(server).await {
            Ok(c) => c,
            Err(e) => return ToolCallResult::error(e),
        };
        match self.client.list_tools(&cfg).await {
            Ok(tools) => {
                let summary: Vec<Value> = tools
                    .iter()
                    .map(|t| {
                        json!({
                            "name": t.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                            "description": t.get("description").and_then(|v| v.as_str()).unwrap_or(""),
                            "input_schema": t.get("inputSchema").cloned().unwrap_or(Value::Null),
                        })
                    })
                    .collect();
                ToolCallResult::success_json(json!({
                    "server": cfg.name,
                    "tool_count": summary.len(),
                    "tools": summary,
                }))
            }
            Err(e) => ToolCallResult::error(e),
        }
    }

    async fn handle_call(&self, args: Option<Value>) -> ToolCallResult {
        let args = args.unwrap_or_default();
        let Some(server) = args["server"].as_str() else {
            return ToolCallResult::error("`server` is required".to_string());
        };
        let Some(tool) = args["tool"].as_str() else {
            return ToolCallResult::error("`tool` is required".to_string());
        };
        let arguments = args.get("arguments").cloned().unwrap_or_else(|| json!({}));

        let cfg = match self.resolve_server(server).await {
            Ok(c) => c,
            Err(e) => return ToolCallResult::error(e),
        };

        match self.client.call_tool(&cfg, tool, arguments).await {
            Ok(result) => {
                let text = content_text(&result);
                let is_error = result
                    .get("isError")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                // `answer` carries the text so {{_prev}} flows in a chain. A
                // remote error is a *successful* call with is_error=true (like
                // execute_code) so it does not burn a chain retry.
                let structured = result
                    .get("structuredContent")
                    .cloned()
                    .unwrap_or(Value::Null);
                // `structured_json` is the structured payload as a STRING, so a
                // chain step can pass it via `{{_prev.structured_json}}` into an
                // execute_code / push_context arg and json.loads it — the dotted
                // path resolves to a plain string (safe to embed), where the raw
                // object form would not substitute cleanly into a string arg.
                let structured_json = if structured.is_null() {
                    String::new()
                } else {
                    serde_json::to_string(&structured).unwrap_or_default()
                };
                ToolCallResult::success_json(json!({
                    "server": cfg.name,
                    "tool": tool,
                    "is_error": is_error,
                    "answer": text,
                    "structured": structured,
                    "structured_json": structured_json,
                }))
            }
            Err(e) => ToolCallResult::error(e),
        }
    }
}

#[async_trait]
impl Skill for McpClientSkill {
    fn name(&self) -> &str {
        "MCP Client"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::list_tools_def(), Self::call_def()]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "mcp_list_tools" => Some(self.handle_list_tools(args).await),
            "mcp_call" => Some(self.handle_call(args).await),
            _ => None,
        }
    }
}
