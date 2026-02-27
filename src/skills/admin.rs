//! AdminSkill — graph maintenance and cleanup tools.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::repository::Neo4jClient;
use crate::services::{ContextStore, LlmClient, LlmConfig};
use crate::skills::Skill;

pub struct AdminSkill {
    neo4j: Neo4jClient,
    context_store: ContextStore,
    llm_config: Option<LlmConfig>,
}

impl AdminSkill {
    pub fn new(neo4j: Neo4jClient, context_store: ContextStore, llm_config: Option<LlmConfig>) -> Self {
        Self { neo4j, context_store, llm_config }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    fn delete_api_def() -> ToolDefinition {
        ToolDefinition {
            name: "delete_api".to_string(),
            description: "Cascade-delete all graph nodes for a specific ingested API: \
                Resource, Endpoints, Parameters, HealingEvents, and exclusively-owned Schemas. \
                Also removes the API from the in-memory context cache. \
                Use dry_run: true to preview what would be deleted without removing anything."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "api_name": {
                        "type": "string",
                        "description": "Name of the API to delete (case-insensitive, matches Resource.name)."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "If true, count what would be deleted without removing anything. Default: false."
                    }
                },
                "required": ["api_name"]
            }),
        }
    }

    fn purge_duplicate_endpoints_def() -> ToolDefinition {
        ToolDefinition {
            name: "purge_duplicate_endpoints".to_string(),
            description: "Find and remove duplicate Endpoint nodes — same path + method under the same Resource. \
                The oldest node (by creation order in the graph) is kept. \
                Use dry_run: true to list duplicates without deleting."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "dry_run": {
                        "type": "boolean",
                        "description": "If true, list duplicate groups without deleting. Default: false."
                    }
                }
            }),
        }
    }

    fn purge_orphaned_schemas_def() -> ToolDefinition {
        ToolDefinition {
            name: "purge_orphaned_schemas".to_string(),
            description: "Delete Schema nodes that are not referenced by any Endpoint \
                (no RETURNS_SCHEMA, ACCEPTS_SCHEMA, or LINKS_TO relationships). \
                These accumulate when APIs are partially deleted or re-ingested. \
                Use dry_run: true to count orphans without deleting."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "dry_run": {
                        "type": "boolean",
                        "description": "If true, count orphans without deleting. Default: false."
                    }
                }
            }),
        }
    }

    fn backfill_endpoint_embeddings_def() -> ToolDefinition {
        ToolDefinition {
            name: "backfill_endpoint_embeddings".to_string(),
            description: "Generate and store vector embeddings for all Endpoint nodes that are \
                missing them. Required for graph_query_endpoint to use semantic search instead of \
                falling back to keyword (CONTAINS) matching. Use dry_run: true to count how many \
                endpoints need embeddings without writing anything."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "dry_run": {
                        "type": "boolean",
                        "description": "If true, count endpoints needing embeddings without generating. Default: false."
                    }
                }
            }),
        }
    }

    fn reset_graph_def() -> ToolDefinition {
        ToolDefinition {
            name: "reset_graph".to_string(),
            description: "Wipe ALL API data from the graph: Resource, Endpoint, Schema, Parameter, \
                and HealingEvent nodes. Knowledge data (Notes, Tasks, Procedures, WorkingMemory, \
                AgentJobs) is preserved. Also clears the in-memory context cache. \
                REQUIRES confirm: true — this cannot be undone."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "confirm": {
                        "type": "boolean",
                        "description": "Must be true to proceed. Acts as a safety guard against accidental resets."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "If true, count what would be deleted without removing anything. Default: false."
                    }
                },
                "required": ["confirm"]
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_delete_api(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            api_name: String,
            #[serde(default)]
            dry_run: bool,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let stats = match self.neo4j.count_api_nodes(&input.api_name).await {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(format!("Failed to count nodes: {}", e)),
        };

        if stats.resources == 0 {
            return ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "found": false,
                    "message": format!("No API named '{}' found in the graph.", input.api_name),
                }))
                .unwrap(),
            );
        }

        if input.dry_run {
            return ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "dry_run": true,
                    "api_name": input.api_name,
                    "would_delete": stats,
                }))
                .unwrap(),
            );
        }

        match self.neo4j.delete_api_cascade(&input.api_name).await {
            Ok(deleted) => {
                // Evict from in-memory context so stale data isn't returned.
                self.context_store.clear(Some(&input.api_name)).await;

                ToolCallResult::success_text(
                    serde_json::to_string_pretty(&json!({
                        "deleted": true,
                        "api_name": input.api_name,
                        "removed": deleted,
                    }))
                    .unwrap(),
                )
            }
            Err(e) => ToolCallResult::error(format!("Delete failed: {}", e)),
        }
    }

    async fn handle_purge_duplicate_endpoints(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            #[serde(default)]
            dry_run: bool,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        if input.dry_run {
            let dupes = match self.neo4j.find_duplicate_endpoints().await {
                Ok(d) => d,
                Err(e) => return ToolCallResult::error(format!("Query failed: {}", e)),
            };

            let total_extra: u32 = dupes.iter().map(|(_, _, _, cnt)| cnt - 1).sum();
            let groups: Vec<Value> = dupes
                .iter()
                .map(|(res, path, method, cnt)| {
                    json!({ "resource": res, "path": path, "method": method, "duplicates": cnt - 1 })
                })
                .collect();

            return ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "dry_run": true,
                    "duplicate_groups": groups.len(),
                    "would_delete": total_extra,
                    "groups": groups,
                }))
                .unwrap(),
            );
        }

        match self.neo4j.purge_duplicate_endpoints().await {
            Ok(deleted) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "deleted": deleted,
                    "message": format!("Removed {} duplicate endpoint(s).", deleted),
                }))
                .unwrap(),
            ),
            Err(e) => ToolCallResult::error(format!("Purge failed: {}", e)),
        }
    }

    async fn handle_purge_orphaned_schemas(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            #[serde(default)]
            dry_run: bool,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        if input.dry_run {
            return match self.neo4j.count_orphaned_schemas().await {
                Ok(cnt) => ToolCallResult::success_text(
                    serde_json::to_string_pretty(&json!({
                        "dry_run": true,
                        "would_delete": cnt,
                    }))
                    .unwrap(),
                ),
                Err(e) => ToolCallResult::error(format!("Count failed: {}", e)),
            };
        }

        match self.neo4j.purge_orphaned_schemas().await {
            Ok(deleted) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "deleted": deleted,
                    "message": format!("Removed {} orphaned schema(s).", deleted),
                }))
                .unwrap(),
            ),
            Err(e) => ToolCallResult::error(format!("Purge failed: {}", e)),
        }
    }

    async fn handle_backfill_endpoint_embeddings(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            #[serde(default)]
            dry_run: bool,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let Some(llm_config) = &self.llm_config else {
            return ToolCallResult::error(
                "LLM not configured — cannot generate embeddings. \
                Set OLLAMA_EMBED_MODEL or configure an LLM provider."
                    .to_string(),
            );
        };

        let llm = match LlmClient::with_config(llm_config.clone()) {
            Ok(l) => l,
            Err(e) => return ToolCallResult::error(format!("LLM init failed: {}", e)),
        };

        let endpoints = match self.neo4j.list_endpoints().await {
            Ok(e) => e,
            Err(e) => return ToolCallResult::error(format!("Failed to list endpoints: {}", e)),
        };

        let total = endpoints.len();
        let needs_embedding: Vec<_> = endpoints.into_iter().filter(|e| e.embedding.is_none()).collect();
        let needs_count = needs_embedding.len();

        if input.dry_run {
            return ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "dry_run": true,
                    "total_endpoints": total,
                    "already_have_embeddings": total - needs_count,
                    "need_embeddings": needs_count,
                }))
                .unwrap(),
            );
        }

        let mut updated = 0u64;
        let mut failed = 0u64;

        for endpoint in needs_embedding {
            let text = format!("{} {} - {}", endpoint.method, endpoint.path, endpoint.summary);
            match llm.embeddings(&text).await {
                Ok(emb) => {
                    if self.neo4j.update_endpoint_embedding(endpoint.id, emb).await.is_ok() {
                        updated += 1;
                    } else {
                        failed += 1;
                    }
                }
                Err(_) => {
                    failed += 1;
                }
            }
        }

        ToolCallResult::success_text(
            serde_json::to_string_pretty(&json!({
                "total_endpoints": total,
                "already_had_embeddings": total - needs_count,
                "updated": updated,
                "failed": failed,
            }))
            .unwrap(),
        )
    }

    async fn handle_reset_graph(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            confirm: bool,
            #[serde(default)]
            dry_run: bool,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        if !input.confirm {
            return ToolCallResult::error(
                "Safety guard: set confirm: true to proceed with reset_graph.".to_string(),
            );
        }

        if input.dry_run {
            return match self.neo4j.count_api_graph().await {
                Ok(stats) => ToolCallResult::success_text(
                    serde_json::to_string_pretty(&json!({
                        "dry_run": true,
                        "would_delete": stats,
                    }))
                    .unwrap(),
                ),
                Err(e) => ToolCallResult::error(format!("Count failed: {}", e)),
            };
        }

        match self.neo4j.reset_api_graph().await {
            Ok(deleted) => {
                // Clear all in-memory API contexts.
                self.context_store.clear(None).await;

                ToolCallResult::success_text(
                    serde_json::to_string_pretty(&json!({
                        "reset": true,
                        "removed": deleted,
                        "message": "All API graph data has been wiped. Knowledge data (Notes, Tasks, etc.) preserved.",
                    }))
                    .unwrap(),
                )
            }
            Err(e) => ToolCallResult::error(format!("Reset failed: {}", e)),
        }
    }
}

#[async_trait]
impl Skill for AdminSkill {
    fn name(&self) -> &str {
        "Graph Admin"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::delete_api_def(),
            Self::purge_duplicate_endpoints_def(),
            Self::purge_orphaned_schemas_def(),
            Self::reset_graph_def(),
            Self::backfill_endpoint_embeddings_def(),
        ]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "delete_api"                       => Some(self.handle_delete_api(args).await),
            "purge_duplicate_endpoints"        => Some(self.handle_purge_duplicate_endpoints(args).await),
            "purge_orphaned_schemas"           => Some(self.handle_purge_orphaned_schemas(args).await),
            "reset_graph"                      => Some(self.handle_reset_graph(args).await),
            "backfill_endpoint_embeddings"     => Some(self.handle_backfill_endpoint_embeddings(args).await),
            _ => None,
        }
    }
}
