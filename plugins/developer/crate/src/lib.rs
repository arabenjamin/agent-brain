//! The `developer` plugin (CORE_AND_PLUGINS_PLAN Phase 4, pack 4): the brain
//! working on its own code.
//!
//! - **Codebase tools** (`codebase.rs`): read, search and git-log the source,
//!   write docs and proposals (a Feed item, never a code change), the
//!   workspace pair, and `analyze_own_structure`.
//! - **The architecture view** (`arch.rs`, `routes.rs`, `snapshot.rs`): the
//!   inventory, the bundle's `layout.yaml`, weekly snapshots and their diffs,
//!   at `/api/plugins/developer/architecture`. The core keeps what it needs
//!   itself (components, tombstones) and hands the router source over.
//! - **Dev notes** (`dev_notes.rs`, `dev_docs.rs`): the brain's engineering
//!   rules, history and plan decisions as `:DevNote`, for coding agents
//!   (`dev_docs`, `scripts/dev_docs.sh`). Seeded from `devdocs/devnotes.jsonl`
//!   when the graph has none; the file is rewritten after every write.
//! - Workflows `codebase` (doc-update), `brain-api-sync`, `diagnose`,
//!   `capability-mining`; profiles `code-analyst`, `self-analyst`.

use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait, axum};

mod arch;
mod codebase;
mod dev_docs;
mod dev_notes;
mod routes;
mod snapshot;

pub struct Developer;

#[async_trait]
impl Plugin for Developer {
    fn name(&self) -> &'static str {
        "developer"
    }

    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        arch::init(
            ctx.introspection.clone(),
            ctx.root().map(|r| r.to_path_buf()),
        );
        let mut skills: Vec<Arc<dyn Skill>> = vec![Arc::new(codebase::CodebaseSkill::new(ctx))];
        if let Some(g) = ctx.graph.clone() {
            skills.push(Arc::new(snapshot::SnapshotSkill::new(g)));
        }
        if let Some(notes) = dev_notes(ctx) {
            skills.push(Arc::new(dev_docs::DevDocsSkill::new(notes)));
        }
        skills
    }

    fn routes(&self, ctx: &PluginContext) -> Option<axum::Router> {
        Some(routes::router(ctx.graph.clone()?))
    }

    /// The snapshots' schema, then a baseline: the view's "what changed"
    /// needs one, so take the first snapshot now rather than after the first
    /// weekly run.
    async fn start(&self, ctx: &PluginContext) -> anyhow::Result<()> {
        let Some(g) = &ctx.graph else {
            return Ok(());
        };
        for stmt in arch::SCHEMA.iter().chain(dev_notes::SCHEMA) {
            g.query(stmt, serde_json::json!({})).await?;
        }
        // Seed an empty graph from git and embed what lacks a vector, off the
        // startup path (the embedder may still be loading).
        if let Some(notes) = dev_notes(ctx) {
            tokio::spawn(async move {
                match notes.seed_and_backfill().await {
                    Ok((seeded, embedded)) => tracing::info!(seeded, embedded, "dev notes ready"),
                    Err(e) => tracing::warn!(error = %e, "dev notes: seed/backfill failed"),
                }
            });
        }
        if let Some(s) = arch::ensure_baseline(g.as_ref()).await? {
            tracing::info!(summary = %s.summary, "Architecture baseline snapshot taken");
        }
        Ok(())
    }
}

/// The dev-note store, when the host gives a graph and an embedder.
fn dev_notes(ctx: &PluginContext) -> Option<Arc<dev_notes::DevNotes>> {
    Some(Arc::new(dev_notes::DevNotes::new(
        ctx.graph.clone()?,
        ctx.llm.clone()?,
        dev_notes::seed_file(ctx.env("CODEBASE_DIR")),
    )))
}
