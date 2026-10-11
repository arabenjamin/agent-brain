//! `snapshot_architecture`: record what the brain is made of and what changed
//! since the last time (`arch.rs`). Deterministic: set arithmetic over the
//! meta-graph and the route table, no model. Weekly on `brain-api-sync`.

use std::sync::Arc;

use agent_brain_plugin_api::{Graph, Skill, ToolCallResult, ToolDefinition, async_trait};
use serde_json::{Value, json};

use crate::arch;

pub struct SnapshotSkill {
    graph: Arc<dyn Graph>,
}

impl SnapshotSkill {
    pub fn new(graph: Arc<dyn Graph>) -> Self {
        Self { graph }
    }

    fn def() -> ToolDefinition {
        ToolDefinition {
            name: "snapshot_architecture".to_string(),
            description: "Record a snapshot of the brain's own architecture (skills and tools, \
                          HTTP routes, agents, workflows, MCP servers, declared tools, \
                          providers, services) and return what changed since the previous \
                          snapshot: added and removed names per category, with counts. \
                          Deterministic; the answer to 'what changed in your architecture / \
                          what tools were added this week'. With `dry_run:true` it reports the \
                          change without storing a snapshot."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "dry_run": {
                        "type": "boolean",
                        "description": "Report the diff against the last snapshot without storing a new one."
                    },
                    "trigger": {
                        "type": "string",
                        "description": "Why it was taken, recorded on the snapshot (default: manual)."
                    }
                }
            }),
        }
    }

    async fn run(&self, args: Value) -> ToolCallResult {
        let neo4j = self.graph.as_ref();
        if args["dry_run"].as_bool().unwrap_or(false) {
            let inv = match arch::collect(neo4j).await {
                Ok(i) => i,
                Err(e) => {
                    return ToolCallResult::error(format!("snapshot_architecture failed: {e}"));
                }
            };
            let out = match arch::latest(neo4j).await {
                Ok(Some((s, old))) => {
                    let d = arch::diff(&old, &inv);
                    json!({ "dry_run": true, "since": s.taken_at, "summary": arch::summarize(&d), "diff": d, "counts": inv.counts() })
                }
                Ok(None) => {
                    json!({ "dry_run": true, "since": null, "summary": "No snapshot yet: nothing to compare with.", "counts": inv.counts() })
                }
                Err(e) => {
                    return ToolCallResult::error(format!("snapshot_architecture failed: {e}"));
                }
            };
            return ToolCallResult::success_json(out);
        }
        let trigger = args["trigger"]
            .as_str()
            .filter(|t| !t.is_empty())
            .unwrap_or("manual");
        match arch::take(neo4j, trigger).await {
            Ok(s) => ToolCallResult::success_json(json!({
                "snapshot_id": s.id,
                "taken_at": s.taken_at,
                "previous_id": s.previous_id,
                "summary": s.summary,
                "diff": s.diff,
                "counts": s.counts,
            })),
            Err(e) => ToolCallResult::error(format!("snapshot_architecture failed: {e}")),
        }
    }
}

#[async_trait]
impl Skill for SnapshotSkill {
    fn name(&self) -> &str {
        "Architecture"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        (tool_name == "snapshot_architecture").then_some(())?;
        Some(self.run(arguments.unwrap_or_default()).await)
    }
}
