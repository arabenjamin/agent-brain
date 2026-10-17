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
use std::collections::HashMap;
use std::time::{Duration, Instant};

use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use agent_brain_repository::Neo4jClient;
use agent_brain_work::mcp_client::{McpClient, McpServerConfig, content_text};

pub struct McpClientSkill {
    neo4j: Option<Neo4jClient>,
    client: McpClient,
    /// Where an `mcp_call` whose arguments do not fit the remote tool is
    /// recorded (`arg_checks`), as the dispatch check records a local one.
    telemetry: Option<agent_brain_repository::TelemetryClient>,
    /// Each server's tool schemas from its `tools/list`, by server name, kept
    /// for [`SCHEMA_TTL`] so a check costs one listing per server per window.
    schemas: tokio::sync::Mutex<HashMap<String, (Instant, ToolSchemas)>>,
}

/// A server's tools by name, each with its `inputSchema`.
type ToolSchemas = HashMap<String, Value>;

/// How long a server's tool list is reused for argument checks.
const SCHEMA_TTL: Duration = Duration::from_secs(600);

impl McpClientSkill {
    pub fn new(neo4j: Option<Neo4jClient>) -> Self {
        Self {
            neo4j,
            client: McpClient::new(),
            telemetry: None,
            schemas: tokio::sync::Mutex::new(HashMap::new()),
        }
    }

    pub fn with_telemetry(
        mut self,
        telemetry: Option<agent_brain_repository::TelemetryClient>,
    ) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// The server's tools and their input schemas, from cache or a fresh
    /// `tools/list`. None when the server cannot be listed: the call is then
    /// not checked (fail-open, as before this check existed).
    async fn remote_schemas(&self, cfg: &McpServerConfig) -> Option<ToolSchemas> {
        let mut cache = self.schemas.lock().await;
        if let Some((at, m)) = cache.get(&cfg.name)
            && at.elapsed() < SCHEMA_TTL
        {
            return Some(m.clone());
        }
        let tools = self.client.list_tools(cfg).await.ok()?;
        let m: ToolSchemas = tools
            .iter()
            .filter_map(|t| {
                let name = t["name"].as_str()?.to_string();
                let schema = t.get("inputSchema").cloned().unwrap_or_else(|| json!({}));
                Some((name, schema))
            })
            .collect();
        cache.insert(cfg.name.clone(), (Instant::now(), m.clone()));
        Some(m)
    }

    /// Validation Phase 4: an `mcp_call`'s `arguments` against the remote
    /// tool's own `inputSchema` (the dispatch check sees only mcp_call's
    /// envelope). Same checker and mode (`TOOL_ARG_CHECK`) as a local tool;
    /// a misfit is recorded as `mcp_call:<server>/<tool>`. Returns the
    /// refusal in `enforce` mode.
    async fn check_remote_arguments(
        &self,
        cfg: &McpServerConfig,
        tool: &str,
        arguments: &Value,
    ) -> Option<ToolCallResult> {
        use agent_brain_work::validation::ArgCheckMode;
        let mode = ArgCheckMode::from_env();
        if mode == ArgCheckMode::Off {
            return None;
        }
        let schemas = self.remote_schemas(cfg).await?;
        let label = format!("{}/{tool}", cfg.name);
        let problems = remote_problems(&schemas, &cfg.name, tool, arguments);
        if problems.is_empty() {
            return None;
        }
        let refused = mode == ArgCheckMode::Enforce;
        let purpose = agent_brain_llm::routing::RouteContext::current().purpose;
        tracing::warn!(
            tool = %label,
            purpose = purpose.as_str(),
            mode = mode.as_str(),
            problems = %problems.join("; "),
            "mcp_call arguments do not fit the remote tool's schema"
        );
        if let Some(tel) = &self.telemetry {
            let _ = tel.log_arg_check(
                &format!("mcp_call:{label}"),
                purpose.as_str(),
                mode.as_str(),
                refused,
                &problems,
                &arguments.to_string(),
            );
        }
        refused.then(|| {
            ToolCallResult::error(format!(
                "Arguments rejected; {label} was not called. {}",
                problems.join(". ")
            ))
        })
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
            agent_brain_protocol::net_policy::check_url_str(server)
                .map_err(|why| format!("Not requested: {why}"))?;
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
        if let Some(refusal) = self.check_remote_arguments(&cfg, tool, &arguments).await {
            return refusal;
        }

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

/// What is wrong with calling `tool` on `server` with `arguments`, given the
/// server's listed tools: an unknown tool names the ones it has, a known one
/// is checked against its `inputSchema`.
fn remote_problems(
    schemas: &ToolSchemas,
    server: &str,
    tool: &str,
    arguments: &Value,
) -> Vec<String> {
    use agent_brain_work::validation::check_tool_arguments;
    match schemas.get(tool) {
        Some(schema) => check_tool_arguments(&format!("{server}/{tool}"), schema, Some(arguments)),
        None => {
            let mut names: Vec<&str> = schemas.keys().map(String::as_str).collect();
            names.sort();
            vec![format!(
                "`{tool}` is not a tool of {server}; it has: {}",
                names.join(", ")
            )]
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

#[cfg(test)]
mod tests {
    use super::*;

    fn schemas() -> ToolSchemas {
        let mut m = ToolSchemas::new();
        m.insert(
            "search_jobs".into(),
            json!({
                "type": "object",
                "properties": {"query": {"type": "string"}, "remote": {"type": "boolean"}},
                "required": ["query"]
            }),
        );
        m.insert("list_companies".into(), json!({}));
        m
    }

    #[test]
    fn fitting_arguments_pass() {
        let args = json!({"query": "rust", "remote": true});
        assert!(remote_problems(&schemas(), "aidevboard", "search_jobs", &args).is_empty());
    }

    #[test]
    fn a_misspelt_argument_names_the_right_one() {
        let args = json!({"querry": "rust"});
        let p = remote_problems(&schemas(), "aidevboard", "search_jobs", &args).join("; ");
        assert!(
            p.contains("`querry` is not an argument of aidevboard/search_jobs"),
            "{p}"
        );
        assert!(p.contains("did you mean `query`"), "{p}");
    }

    #[test]
    fn a_wrong_type_is_caught() {
        let args = json!({"query": "rust", "remote": "yes"});
        let p = remote_problems(&schemas(), "aidevboard", "search_jobs", &args);
        assert!(!p.is_empty());
    }

    #[test]
    fn an_unknown_tool_lists_what_the_server_has() {
        let p = remote_problems(&schemas(), "aidevboard", "find_jobs", &json!({})).join("; ");
        assert!(p.contains("`find_jobs` is not a tool of aidevboard"), "{p}");
        assert!(p.contains("list_companies, search_jobs"), "{p}");
    }
}
