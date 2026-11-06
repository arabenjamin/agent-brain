//! The `mcp-client` plugin (CODEBASE_SPLIT_PLAN Phase 3): the brain calling
//! tools on external MCP servers (Exa, Tavily, the owner's own).
//!
//! - `skill.rs`: `mcp_list_tools` and `mcp_call`, over `(:McpServer)` nodes
//!   the host seeds from the bundle's `mcp-servers/*.yaml` (graph-owned after
//!   the first seed). A call's arguments are checked against the remote
//!   tool's schema with the host's checker (`ArgCheck`).
//! - `client.rs`: JSON-RPC over HTTP, plain or SSE-framed, under the network
//!   policy; a server's key comes from its env var at call time.
//!
//! Stamped from `templates/plugin` (`scripts/plugin_template.sh`); the input
//! schemas are the bundle's `schemas/tools/<tool>.json`.

use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill};

mod client;
mod skill;

pub struct McpClient;

impl Plugin for McpClient {
    fn name(&self) -> &'static str {
        "mcp-client"
    }

    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        vec![Arc::new(skill::McpClientSkill::new(
            ctx.graph.clone(),
            ctx.arg_check.clone(),
        )) as Arc<dyn Skill>]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_plugin_yaml() {
        let want = format!("name: {}", McpClient.name());
        assert!(
            include_str!("../../plugin.yaml").lines().any(|l| l == want),
            "plugin.yaml has no `{want}` line"
        );
    }
}
