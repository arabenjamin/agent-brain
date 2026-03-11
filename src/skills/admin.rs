//! AdminSkill — graph maintenance, cleanup, snapshot, and integrity tools.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::mcp::protocol::{ToolCallResult, ToolDefinition};
use crate::mcp::tools::ToolRegistry;
use crate::repository::Neo4jClient;
use crate::services::{ContextStore, LlmClient, LlmConfig, SnapshotService};
use crate::skills::Skill;

pub struct AdminSkill {
    neo4j: Neo4jClient,
    context_store: ContextStore,
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
    snapshot_svc: Option<Arc<SnapshotService>>,
    tool_registry: Arc<RwLock<ToolRegistry>>,
}

impl AdminSkill {
    pub fn new(
        neo4j: Neo4jClient,
        context_store: ContextStore,
        llm_config: Arc<RwLock<Option<LlmConfig>>>,
        snapshot_svc: Option<Arc<SnapshotService>>,
        tool_registry: Arc<RwLock<ToolRegistry>>,
    ) -> Self {
        Self {
            neo4j,
            context_store,
            llm_config,
            snapshot_svc,
            tool_registry,
        }
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
            description:
                "Wipe ALL API data from the graph: Resource, Endpoint, Schema, Parameter, \
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

    fn snapshot_knowledge_def() -> ToolDefinition {
        ToolDefinition {
            name: "snapshot_knowledge".to_string(),
            description: "Take a compressed snapshot of the full knowledge graph (Notes, Tasks, \
                Entities, Procedures, and their relationships). Saved as a .json.gz file in the \
                snapshot directory. Embeddings are excluded — run backfill_endpoint_embeddings \
                after restoring if needed."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "label": {
                        "type": "string",
                        "description": "Optional label to embed in the filename (e.g. 'before_prune'). Default: none."
                    }
                }
            }),
        }
    }

    fn restore_knowledge_def() -> ToolDefinition {
        ToolDefinition {
            name: "restore_knowledge".to_string(),
            description: "Restore knowledge graph data from a .json.gz snapshot file. \
                Uses MERGE semantics — safe to run on a non-empty graph (existing nodes preserved). \
                Use dry_run: true to preview counts without writing anything. \
                After restore, run backfill_endpoint_embeddings to regenerate embeddings."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "file": {
                        "type": "string",
                        "description": "Path to the snapshot file (use list_snapshots to discover available files)."
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "If true, return restore counts without writing anything. Default: false."
                    }
                },
                "required": ["file"]
            }),
        }
    }

    fn list_snapshots_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_snapshots".to_string(),
            description: "List all available knowledge graph snapshot files, sorted newest-first. \
                Returns file names, export timestamps, node counts, and file sizes."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn verify_knowledge_integrity_def() -> ToolDefinition {
        ToolDefinition {
            name: "verify_knowledge_integrity".to_string(),
            description: "Scan the knowledge graph for common integrity issues: \
                empty or too-short notes, orphaned chunk notes (PART_OF pointing to missing parent), \
                hallucinated consolidated notes (content starting with label prefixes like 'Note ' or '[Memory'), \
                and exact-duplicate note content. Returns counts and IDs for each issue category."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content_min_length": {
                        "type": "integer",
                        "description": "Minimum content length (chars) before a note is flagged as too short. Default: 10."
                    }
                }
            }),
        }
    }

    fn analyze_own_structure_def() -> ToolDefinition {
        ToolDefinition {
            name: "analyze_own_structure".to_string(),
            description: "Walk the src/ directory to count Rust source files per module \
                (skills, services, repository, models, mcp), read the live tool registry to \
                count registered tools, and return a JSON structure report. \
                Optionally stores the report as a procedural knowledge note."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "store_as_note": {
                        "type": "boolean",
                        "description": "If true, store the analysis result as a semantic note. Default: false."
                    }
                }
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
                self.context_store.clear(Some(&input.api_name)).await;

                ToolCallResult::success_text(
                    serde_json::to_string_pretty(&json!({
                        "deleted": true,
                        "api_name": input.api_name,
                        "count": deleted,
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
                    "count": deleted,
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
                    "count": deleted,
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

        let llm_config = self.llm_config.read().await.clone();
        let Some(llm_config) = llm_config else {
            return ToolCallResult::error(
                "LLM not configured — cannot generate embeddings. \
                Set OLLAMA_EMBED_MODEL or configure an LLM provider."
                    .to_string(),
            );
        };

        let llm = match LlmClient::with_config(llm_config) {
            Ok(l) => l,
            Err(e) => return ToolCallResult::error(format!("LLM init failed: {}", e)),
        };

        let endpoints = match self.neo4j.list_endpoints().await {
            Ok(e) => e,
            Err(e) => return ToolCallResult::error(format!("Failed to list endpoints: {}", e)),
        };

        let total = endpoints.len();
        let needs_embedding: Vec<_> = endpoints
            .into_iter()
            .filter(|e| e.embedding.is_none())
            .collect();
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
            let text = format!(
                "{} {} - {}",
                endpoint.method, endpoint.path, endpoint.summary
            );
            match llm.embeddings(&text).await {
                Ok(emb) => {
                    if self
                        .neo4j
                        .update_endpoint_embedding(endpoint.id, emb)
                        .await
                        .is_ok()
                    {
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
                self.context_store.clear(None).await;

                ToolCallResult::success_text(
                    serde_json::to_string_pretty(&json!({
                        "reset": true,
                        "count": deleted,
                        "message": "All API graph data has been wiped. Knowledge data (Notes, Tasks, etc.) preserved.",
                    }))
                    .unwrap(),
                )
            }
            Err(e) => ToolCallResult::error(format!("Reset failed: {}", e)),
        }
    }

    async fn handle_snapshot_knowledge(&self, args: Option<Value>) -> ToolCallResult {
        let svc = match &self.snapshot_svc {
            Some(s) => s,
            None => {
                return ToolCallResult::error(
                    "Snapshot service not available (Neo4j required).".to_string(),
                );
            }
        };

        #[derive(Deserialize, Default)]
        struct Input {
            label: Option<String>,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        match svc.take_snapshot(input.label.as_deref()).await {
            Ok((_, meta)) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "file": meta.file_name,
                    "file_path": meta.file_path,
                    "exported_at": meta.exported_at,
                    "schema_version": meta.schema_version,
                    "notes": meta.note_count,
                    "tasks": meta.task_count,
                    "entities": meta.entity_count,
                    "procedures": meta.procedure_count,
                    "relationships": meta.relationship_count,
                    "size_bytes": meta.size_bytes,
                }))
                .unwrap(),
            ),
            Err(e) => ToolCallResult::error(format!("Snapshot failed: {}", e)),
        }
    }

    async fn handle_restore_knowledge(&self, args: Option<Value>) -> ToolCallResult {
        let svc = match &self.snapshot_svc {
            Some(s) => s,
            None => {
                return ToolCallResult::error(
                    "Snapshot service not available (Neo4j required).".to_string(),
                );
            }
        };

        #[derive(Deserialize)]
        struct Input {
            file: String,
            #[serde(default)]
            dry_run: bool,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let path = Path::new(&input.file);
        match svc.restore_snapshot(path, input.dry_run).await {
            Ok(stats) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "notes_restored": stats.notes_restored,
                    "tasks_restored": stats.tasks_restored,
                    "entities_restored": stats.entities_restored,
                    "procedures_restored": stats.procedures_restored,
                    "relationships_restored": stats.relationships_restored,
                    "dry_run": stats.dry_run,
                    "message": if stats.dry_run {
                        "Dry run complete — no data was written.".to_string()
                    } else {
                        "Restore complete. Run backfill_endpoint_embeddings to regenerate embeddings.".to_string()
                    },
                }))
                .unwrap(),
            ),
            Err(e) => ToolCallResult::error(format!("Restore failed: {}", e)),
        }
    }

    async fn handle_list_snapshots(&self, _args: Option<Value>) -> ToolCallResult {
        let svc = match &self.snapshot_svc {
            Some(s) => s,
            None => {
                return ToolCallResult::error(
                    "Snapshot service not available (Neo4j required).".to_string(),
                );
            }
        };

        match svc.list_snapshots().await {
            Ok(snapshots) => {
                let items: Vec<Value> = snapshots
                    .iter()
                    .map(|m| {
                        json!({
                            "file": m.file_name,
                            "file_path": m.file_path,
                            "exported_at": m.exported_at,
                            "schema_version": m.schema_version,
                            "notes": m.note_count,
                            "tasks": m.task_count,
                            "entities": m.entity_count,
                            "procedures": m.procedure_count,
                            "relationships": m.relationship_count,
                            "size_bytes": m.size_bytes,
                        })
                    })
                    .collect();

                ToolCallResult::success_text(
                    serde_json::to_string_pretty(&json!({
                        "count": items.len(),
                        "snapshots": items,
                    }))
                    .unwrap(),
                )
            }
            Err(e) => ToolCallResult::error(format!("Failed to list snapshots: {}", e)),
        }
    }

    async fn handle_verify_knowledge_integrity(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            content_min_length: Option<i64>,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let min_len = input.content_min_length.unwrap_or(10);

        match self.neo4j.check_knowledge_integrity(min_len).await {
            Ok(report) => ToolCallResult::success_text(
                serde_json::to_string_pretty(&json!({
                    "total_issues": report.total_issues,
                    "checks": {
                        "empty_notes": {
                            "count": report.empty_notes.len(),
                            "items": report.empty_notes,
                        },
                        "orphaned_chunks": {
                            "count": report.orphaned_chunks.len(),
                            "items": report.orphaned_chunks,
                        },
                        "suspicious_consolidated": {
                            "count": report.suspicious_consolidated.len(),
                            "items": report.suspicious_consolidated,
                        },
                        "duplicate_notes": {
                            "count": report.duplicate_notes.len(),
                            "items": report.duplicate_notes,
                            "note": "Limited to 50 pairs to avoid timeout",
                        },
                    },
                    "message": if report.total_issues == 0 {
                        "Knowledge graph integrity check passed — no issues found.".to_string()
                    } else {
                        format!("Found {} issue(s). Use delete_note to remove corrupted entries.", report.total_issues)
                    },
                }))
                .unwrap(),
            ),
            Err(e) => ToolCallResult::error(format!("Integrity check failed: {}", e)),
        }
    }
    async fn handle_analyze_own_structure(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            #[serde(default)]
            store_as_note: bool,
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {e}")),
        };

        // Walk src/ and count .rs files per top-level module dir.
        let src_path = std::path::Path::new("src");
        let mut module_counts: std::collections::BTreeMap<String, usize> =
            std::collections::BTreeMap::new();

        fn count_rs(dir: &std::path::Path) -> usize {
            let Ok(rd) = std::fs::read_dir(dir) else {
                return 0;
            };
            let mut n = 0usize;
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    n += count_rs(&p);
                } else if p.extension().and_then(|e| e.to_str()) == Some("rs") {
                    n += 1;
                }
            }
            n
        }

        if let Ok(rd) = std::fs::read_dir(src_path) {
            for entry in rd.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    let name = p
                        .file_name()
                        .and_then(|n| n.to_str())
                        .unwrap_or("")
                        .to_string();
                    module_counts.insert(name, count_rs(&p));
                }
            }
        }

        // Count total .rs files in src/ root.
        let root_rs = std::fs::read_dir(src_path)
            .map(|rd| {
                rd.flatten()
                    .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("rs"))
                    .count()
            })
            .unwrap_or(0);

        // Count registered tools from in-memory registry.
        let total_tools = {
            let reg = self.tool_registry.read().await;
            reg.list().len()
        };

        let report = json!({
            "total_tools": total_tools,
            "modules": module_counts,
            "root_rs_files": root_rs,
        });

        if input.store_as_note {
            let content = format!(
                "Agent Brain structure analysis:\n- Total registered tools: {}\n- Source modules: {}\n- Root .rs files: {}",
                total_tools,
                module_counts
                    .iter()
                    .map(|(k, v)| format!("{k}: {v} files"))
                    .collect::<Vec<_>>()
                    .join(", "),
                root_rs,
            );
            // Store via neo4j directly (KnowledgeService would need the tool handler).
            let _ = self
                .neo4j
                .execute(
                    neo4rs::query(
                        "CREATE (n:Note {
                        id: randomUUID(),
                        content: $content,
                        note_type: 'semantic',
                        created_at: datetime(),
                        updated_at: datetime(),
                        access_count: 0,
                        next_review_at: datetime() + duration({days: 30}),
                        review_interval_days: 30
                    })",
                    )
                    .param("content", content),
                )
                .await;
        }

        ToolCallResult::success_text(serde_json::to_string_pretty(&report).unwrap())
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
            Self::snapshot_knowledge_def(),
            Self::restore_knowledge_def(),
            Self::list_snapshots_def(),
            Self::verify_knowledge_integrity_def(),
            Self::analyze_own_structure_def(),
        ]
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "delete_api" => Some(self.handle_delete_api(args).await),
            "purge_duplicate_endpoints" => Some(self.handle_purge_duplicate_endpoints(args).await),
            "purge_orphaned_schemas" => Some(self.handle_purge_orphaned_schemas(args).await),
            "reset_graph" => Some(self.handle_reset_graph(args).await),
            "backfill_endpoint_embeddings" => {
                Some(self.handle_backfill_endpoint_embeddings(args).await)
            }
            "snapshot_knowledge" => Some(self.handle_snapshot_knowledge(args).await),
            "restore_knowledge" => Some(self.handle_restore_knowledge(args).await),
            "list_snapshots" => Some(self.handle_list_snapshots(args).await),
            "verify_knowledge_integrity" => {
                Some(self.handle_verify_knowledge_integrity(args).await)
            }
            "analyze_own_structure" => Some(self.handle_analyze_own_structure(args).await),
            _ => None,
        }
    }
}
