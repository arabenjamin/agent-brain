//! Seed `(:McpServer)` nodes from `mcp-servers/*.yaml` — the external MCP
//! servers the brain may call (see `skills/mcp_client.rs`).
//!
//! ON CREATE only: the graph owns each server after first creation, so runtime
//! edits (enable/disable, url change) via `neo4j_query` persist across restarts.
//! Delete a node to re-seed it from its YAML. Non-fatal throughout — a missing
//! directory or bad file is logged and skipped; the MCP-client tools simply have
//! fewer servers to offer. The secret itself is NEVER stored: only `auth_env_var`
//! (the env var name) is, mirroring the ApiContext pattern.
//! Returns the number of servers newly created.

use std::path::Path;

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::repository::Neo4jClient;

#[derive(Debug, Deserialize)]
struct McpServerFile {
    name: String,
    url: String,
    #[serde(default)]
    description: String,
    /// `none` | `bearer` | `header`.
    #[serde(default = "default_auth_scheme")]
    auth_scheme: String,
    /// Header name when `auth_scheme: header` (e.g. `X-API-Key`).
    #[serde(default)]
    auth_param: Option<String>,
    /// Name of the env var carrying the secret (value injected at call time).
    #[serde(default)]
    auth_env_var: Option<String>,
    /// Seed disabled for human review (like schedules `enabled: false`).
    #[serde(default = "default_true")]
    enabled: bool,
}

fn default_auth_scheme() -> String {
    "none".to_string()
}
fn default_true() -> bool {
    true
}

pub async fn seed_mcp_servers_from_dir(neo4j: &Neo4jClient, dir: &Path) -> usize {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "seed_mcp_servers: cannot read directory");
            return 0;
        }
    };

    let mut created = 0usize;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_mcp_servers: cannot read file");
                continue;
            }
        };
        let srv: McpServerFile = match serde_yaml::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_mcp_servers: YAML parse error");
                continue;
            }
        };

        // ON CREATE only — graph-owned after first creation.
        let cypher = "MERGE (m:McpServer {name: $name}) \
                      ON CREATE SET m.url = $url, m.description = $description, \
                                    m.auth_scheme = $auth_scheme, m.auth_param = $auth_param, \
                                    m.auth_env_var = $auth_env_var, m.enabled = $enabled, \
                                    m.created_at = datetime()";
        // MERGE alone can't report create-vs-match through `run`; check existence
        // first for an accurate created count and an idempotent log line.
        let existed = neo4j
            .execute(
                neo4rs::query("MATCH (m:McpServer {name:$name}) RETURN count(m) AS c")
                    .param("name", srv.name.as_str()),
            )
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("c").ok()))
            .map(|c| c > 0)
            .unwrap_or(false);

        let q = neo4rs::query(cypher)
            .param("name", srv.name.as_str())
            .param("url", srv.url.as_str())
            .param("description", srv.description.as_str())
            .param("auth_scheme", srv.auth_scheme.as_str())
            .param("auth_param", srv.auth_param.clone().unwrap_or_default())
            .param("auth_env_var", srv.auth_env_var.clone().unwrap_or_default())
            .param("enabled", srv.enabled);

        match neo4j.run(q).await {
            Ok(()) => {
                if existed {
                    debug!(name = %srv.name, "McpServer already exists — graph copy kept");
                } else {
                    info!(name = %srv.name, url = %srv.url, "Seeded McpServer (graph-owned from now on)");
                    created += 1;
                }
            }
            Err(e) => warn!(name = %srv.name, error = %e, "seed_mcp_servers: seed failed"),
        }
    }

    created
}
