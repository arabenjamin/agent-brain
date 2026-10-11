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
//! - Workflows `codebase` (doc-update), `brain-api-sync`, `diagnose`,
//!   `capability-mining`; profiles `code-analyst`, `self-analyst`.

use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait, axum};

mod arch;
mod codebase;
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
        for stmt in arch::SCHEMA {
            g.query(stmt, serde_json::json!({})).await?;
        }
        if let Some(s) = arch::ensure_baseline(g.as_ref()).await? {
            tracing::info!(summary = %s.summary, "Architecture baseline snapshot taken");
        }
        Ok(())
    }
}
