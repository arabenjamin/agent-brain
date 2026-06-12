//! Self-model sync — an introspection-generated meta-graph of the brain's own
//! capabilities, written to Neo4j at every startup so the brain (and the
//! future Agent Constructor) can *query* what exists instead of guessing.
//!
//! Generated nodes (never hand-maintained — code, YAML, and the model catalog
//! are the sources of truth; the graph reflects them):
//!
//! - `(:ToolDef {name, description, skill, synced_at})` — from the live tool registry
//! - `(:ContextProfile {name, description, model_preference, provider_hint,
//!   allows_all, synced_at})` — from `contexts/*.yaml` via `ContextBuilderService`
//! - `(:ModelDef {name, provider, model, context_window, cost_per_1k_input,
//!   cost_per_1k_output, capabilities, synced_at})` — from the DuckDB model registry
//! - `(:ContextProfile)-[:ALLOWS]->(:ToolDef)` — profile tool allowlists
//!   (`allows_all: true` instead of edges when the profile's list is empty)
//!
//! Chains and schedules are deliberately NOT duplicated here — they already
//! live in the graph as `(:SchedulerChain)` and `(:ScheduledTask)` nodes.
//!
//! Sync is a full refresh: current entries are MERGEd, entries that no longer
//! exist in their source are deleted (DETACH, so stale ALLOWS edges go too).

use tracing::{info, warn};

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::context_builder::ContextProfile;
use agent_brain_protocol::ToolDefinition;

/// Counts reported after a sync, for the startup log.
#[derive(Debug, Default)]
pub struct SelfModelStats {
    pub tools: usize,
    pub profiles: usize,
    pub models: usize,
}

/// Sync the self-model meta-graph. Non-fatal by design: callers log the error
/// and continue — a missing self-model degrades the constructor, not the brain.
pub async fn sync_self_model(
    neo4j: &Neo4jClient,
    skills_with_tools: &[(String, Vec<ToolDefinition>)],
    profiles: &[ContextProfile],
    telemetry: Option<&TelemetryClient>,
) -> anyhow::Result<SelfModelStats> {
    let now = chrono::Utc::now().to_rfc3339();
    let mut stats = SelfModelStats::default();

    // ── ToolDef ──────────────────────────────────────────────────────────
    let mut tool_names: Vec<String> = Vec::new();
    for (skill_name, tools) in skills_with_tools {
        for t in tools {
            tool_names.push(t.name.clone());
            // Argument names from the JSON schema — the Agent Constructor
            // grounds its plans in these (and validates required ones).
            let arg_names: Vec<String> = t.input_schema["properties"]
                .as_object()
                .map(|m| m.keys().cloned().collect())
                .unwrap_or_default();
            let required_args: Vec<String> = t.input_schema["required"]
                .as_array()
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            neo4j
                .run(
                    neo4rs::query(
                        "MERGE (d:ToolDef {name: $name}) \
                         SET d.description = $description, d.skill = $skill, \
                             d.arg_names = $arg_names, d.required_args = $required_args, \
                             d.synced_at = $now",
                    )
                    .param("name", t.name.as_str())
                    .param("description", t.description.as_str())
                    .param("skill", skill_name.as_str())
                    .param("arg_names", arg_names)
                    .param("required_args", required_args)
                    .param("now", now.as_str()),
                )
                .await?;
            stats.tools += 1;
        }
    }
    neo4j
        .run(
            neo4rs::query("MATCH (d:ToolDef) WHERE NOT d.name IN $names DETACH DELETE d")
                .param("names", tool_names.clone()),
        )
        .await?;

    // ── ContextProfile + ALLOWS edges ────────────────────────────────────
    let profile_names: Vec<String> = profiles.iter().map(|p| p.name.clone()).collect();
    for p in profiles {
        neo4j
            .run(
                neo4rs::query(
                    "MERGE (c:ContextProfile {name: $name}) \
                     SET c.description = $description, \
                         c.model_preference = $model_preference, \
                         c.provider_hint = $provider_hint, \
                         c.allows_all = $allows_all, \
                         c.synced_at = $now",
                )
                .param("name", p.name.as_str())
                .param("description", p.description.as_str())
                .param(
                    "model_preference",
                    p.model_preference.clone().unwrap_or_default(),
                )
                .param("provider_hint", p.provider_hint.clone().unwrap_or_default())
                .param("allows_all", p.tools.is_empty())
                .param("now", now.as_str()),
            )
            .await?;
        // Rebuild the allowlist edges from scratch each sync.
        neo4j
            .run(
                neo4rs::query("MATCH (c:ContextProfile {name: $name})-[r:ALLOWS]->() DELETE r")
                    .param("name", p.name.as_str()),
            )
            .await?;
        if !p.tools.is_empty() {
            neo4j
                .run(
                    neo4rs::query(
                        "MATCH (c:ContextProfile {name: $name}) \
                         UNWIND $tools AS tool_name \
                         MATCH (d:ToolDef {name: tool_name}) \
                         MERGE (c)-[:ALLOWS]->(d)",
                    )
                    .param("name", p.name.as_str())
                    .param("tools", p.tools.clone()),
                )
                .await?;
        }
        stats.profiles += 1;
    }
    neo4j
        .run(
            neo4rs::query("MATCH (c:ContextProfile) WHERE NOT c.name IN $names DETACH DELETE c")
                .param("names", profile_names),
        )
        .await?;

    // ── ModelDef ─────────────────────────────────────────────────────────
    if let Some(tc) = telemetry {
        match tc.list_models() {
            Ok(models) => {
                let mut model_names: Vec<String> = Vec::new();
                for m in &models {
                    let name = m["name"].as_str().unwrap_or_default().to_string();
                    if name.is_empty() {
                        continue;
                    }
                    model_names.push(name.clone());
                    neo4j
                        .run(
                            neo4rs::query(
                                "MERGE (d:ModelDef {name: $name}) \
                                 SET d.provider = $provider, \
                                     d.model = $model, \
                                     d.context_window = $context_window, \
                                     d.cost_per_1k_input = $cin, \
                                     d.cost_per_1k_output = $cout, \
                                     d.capabilities = $capabilities, \
                                     d.synced_at = $now",
                            )
                            .param("name", name.as_str())
                            .param("provider", m["provider"].as_str().unwrap_or_default())
                            .param("model", m["model"].as_str().unwrap_or_default())
                            .param("context_window", m["context_window"].as_i64().unwrap_or(0))
                            .param("cin", m["cost_per_1k_input"].as_f64().unwrap_or(0.0))
                            .param("cout", m["cost_per_1k_output"].as_f64().unwrap_or(0.0))
                            .param("capabilities", m["capabilities"].as_str().unwrap_or("[]"))
                            .param("now", now.as_str()),
                        )
                        .await?;
                    stats.models += 1;
                }
                neo4j
                    .run(
                        neo4rs::query(
                            "MATCH (d:ModelDef) WHERE NOT d.name IN $names DETACH DELETE d",
                        )
                        .param("names", model_names),
                    )
                    .await?;
            }
            Err(e) => {
                warn!(error = %e, "Self-model: model catalog unavailable, skipping ModelDef sync")
            }
        }
    }

    info!(
        tools = stats.tools,
        profiles = stats.profiles,
        models = stats.models,
        "Self-model meta-graph synced"
    );
    Ok(stats)
}
