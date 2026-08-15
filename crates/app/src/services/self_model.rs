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

use std::path::Path;

use tokio::process::Command;
use tracing::{info, warn};

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::context_builder::ContextProfile;
use agent_brain_protocol::ToolDefinition;

/// The running code version, read from git at startup.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeVersion {
    pub sha: String,
    pub subject: String,
    pub branch: String,
    /// True when the working tree has uncommitted changes — the running binary
    /// may not correspond to `sha` alone.
    pub dirty: bool,
}

/// Run a git command in `dir`, returning trimmed stdout, or `None` on failure.
async fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir);
    for a in args {
        cmd.arg(a);
    }
    match cmd.output().await {
        Ok(out) if out.status.success() => {
            Some(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        Ok(out) => {
            warn!(
                args = ?args,
                stderr = %String::from_utf8_lossy(&out.stderr).trim(),
                "Self-model: git command failed"
            );
            None
        }
        Err(e) => {
            warn!(args = ?args, error = %e, "Self-model: could not run git");
            None
        }
    }
}

/// Read the running code version from the codebase working copy.
pub async fn read_code_version(codebase_dir: &Path) -> Option<CodeVersion> {
    let sha = git(codebase_dir, &["rev-parse", "--short", "HEAD"]).await?;
    let subject = git(codebase_dir, &["log", "-1", "--pretty=%s"])
        .await
        .unwrap_or_default();
    let branch = git(codebase_dir, &["rev-parse", "--abbrev-ref", "HEAD"])
        .await
        .unwrap_or_else(|| "unknown".to_string());
    // `--quiet` exits 1 when there are unstaged changes; an error here is the
    // signal, not a failure, so this deliberately does not use `git()`.
    let dirty = Command::new("git")
        .arg("-C")
        .arg(codebase_dir)
        .args(["status", "--porcelain"])
        .output()
        .await
        .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
        .unwrap_or(false);
    Some(CodeVersion {
        sha,
        subject,
        branch,
        dirty,
    })
}

/// Record which commit this process is running, and note it when it changes.
///
/// This replaces the `post-commit` → `scripts/self_update.py` round-trip. That
/// script fired on `git commit`, so it recorded a sha the running process did
/// not yet contain, it re-counted tools over HTTP (reporting 0 — it parsed a
/// Markdown document as JSON inside a bare `except: pass`), and it raced the
/// rebuild it was triggered by. Reading HEAD here, in-process at startup, means
/// the recorded version is the code that is *actually loaded*, and it works for
/// every deploy path rather than only a local `git commit`.
///
/// `(:BrainVersion {id:'current'})` is a singleton — always "what is running".
/// An episodic note is written ONLY when the sha changes, so restarts are free;
/// the old hook wrote one note per commit regardless (44 accumulated).
pub async fn sync_code_version(
    neo4j: &Neo4jClient,
    codebase_dir: &Path,
) -> anyhow::Result<Option<CodeVersion>> {
    let Some(version) = read_code_version(codebase_dir).await else {
        // Not a git checkout (a released image, say). Absence of git is normal,
        // not an error — the rest of the self-model is unaffected.
        return Ok(None);
    };

    let previous: Option<String> = neo4j
        .execute(neo4rs::query(
            "MATCH (v:BrainVersion {id: 'current'}) RETURN v.sha AS sha",
        ))
        .await?
        .first()
        .and_then(|r| r.get::<String>("sha").ok());

    let changed = previous.as_deref() != Some(version.sha.as_str());
    let now = chrono::Utc::now().to_rfc3339();

    neo4j
        .run(
            neo4rs::query(
                "MERGE (v:BrainVersion {id: 'current'}) \
                 SET v.sha = $sha, v.subject = $subject, v.branch = $branch, \
                     v.dirty = $dirty, v.seen_at = datetime($now) \
                 FOREACH (_ IN CASE WHEN $changed THEN [1] ELSE [] END | \
                     SET v.deployed_at = datetime($now))",
            )
            .param("sha", version.sha.as_str())
            .param("subject", version.subject.as_str())
            .param("branch", version.branch.as_str())
            .param("dirty", version.dirty)
            .param("now", now.as_str())
            .param("changed", changed),
        )
        .await?;

    if !changed {
        info!(sha = %version.sha, branch = %version.branch, "Running code version unchanged");
        return Ok(Some(version));
    }

    // Changed files are what make the note useful to reason over later.
    let files = git(
        codebase_dir,
        &["diff-tree", "--no-commit-id", "-r", "--name-only", "HEAD"],
    )
    .await
    .unwrap_or_default();
    let file_list: String = files.lines().take(40).collect::<Vec<_>>().join("\n");
    let extra = files.lines().count().saturating_sub(40);

    let note = format!(
        "Now running commit {} on branch {}{}.\nSubject: {}\nChanged files:\n{}{}",
        version.sha,
        version.branch,
        if version.dirty {
            " (working tree dirty — running binary may include uncommitted changes)"
        } else {
            ""
        },
        version.subject,
        if file_list.is_empty() {
            "(none reported)".to_string()
        } else {
            file_list
        },
        if extra > 0 {
            format!("\n… and {extra} more")
        } else {
            String::new()
        },
    );

    match neo4j
        .store_episodic_note(&note, Some(&format!("code_version {}", version.sha)))
        .await
    {
        Ok(_) => info!(
            sha = %version.sha,
            previous = previous.as_deref().unwrap_or("<none>"),
            branch = %version.branch,
            dirty = version.dirty,
            "Running code version CHANGED — recorded"
        ),
        Err(e) => warn!(error = %e, "Failed to store code-version note"),
    }

    Ok(Some(version))
}

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
                             d.synced_at = datetime($now)",
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
                         c.synced_at = datetime($now)",
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
                                     d.synced_at = datetime($now)",
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
