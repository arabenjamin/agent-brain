//! The `epistemics` plugin (CORE_AND_PLUGINS_PLAN Phase 4, pack 6): what the
//! brain's sources assert, and what the evidence says about it. In-tree and
//! on by default (plan §2.4): written against the plugin API so that, if it
//! is ever judged not core, moving it is a folder move.
//!
//! - **Claims** (`claims.rs`, `claim_skill.rs`, the `claim` tool): assertions
//!   pulled out of source notes, stored unverified and attributed, then
//!   verified against independent web evidence (the host's `search`). Status
//!   is derived from evidence edges, never asserted. A "supports" must also
//!   pass the grounding check (`grounding.rs`, a local guardian model).
//! - **Typed relationships** (`relations.rs`, `relation_skill.rs`,
//!   `extract_relations`): between entities, from a closed vocabulary (the
//!   bundle's `relations.yaml`); extracted ones are proposals the person
//!   reviews. Routes (`routes.rs`) under `/api/plugins/epistemics/`.
//!
//! Claims are notes (`note_type: 'claim'`), so the core keeps reading them:
//! retrieval labels them, recall and topics count them, and the core gives
//! them entities on idle time. With the plugin off nothing new is claimed or
//! verified, and what exists stays readable.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait, axum};

mod claim_skill;
mod claims;
mod db;
mod grounding;
mod relation_skill;
mod relations;
mod routes;

pub struct Epistemics;

static ROOT: OnceLock<PathBuf> = OnceLock::new();

/// The bundle's folder (`relations.yaml` lives there); the source tree's in tests.
pub(crate) fn bundle_root() -> PathBuf {
    ROOT.get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/..")))
}

fn db(ctx: &PluginContext) -> Option<db::Db> {
    Some(db::Db::new(ctx.graph.clone()?))
}

#[async_trait]
impl Plugin for Epistemics {
    fn name(&self) -> &'static str {
        "epistemics"
    }

    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        if let Some(r) = ctx.root() {
            let _ = ROOT.set(r.to_path_buf());
        }
        match (db(ctx), ctx.llm.clone()) {
            (Some(d), Some(llm)) => vec![
                Arc::new(claim_skill::ClaimSkill::new(
                    d.clone(),
                    llm.clone(),
                    ctx.clone(),
                )) as Arc<dyn Skill>,
                Arc::new(relation_skill::RelationSkill::new(d, llm)),
            ],
            _ => {
                tracing::warn!("epistemics: the host gave no graph or LLM; no tools");
                vec![]
            }
        }
    }

    /// The merge list's "related" answers become `related_to` relationships
    /// (idempotent; the routes fold again before they read).
    async fn start(&self, ctx: &PluginContext) -> anyhow::Result<()> {
        if let Some(d) = db(ctx) {
            let n = relations::fold_related_entity(&d).await?;
            tracing::info!(
                count = n,
                "epistemics: folded RELATED_ENTITY answers into related_to relationships"
            );
        }
        Ok(())
    }

    fn routes(&self, ctx: &PluginContext) -> Option<axum::Router> {
        Some(routes::router(Arc::new(routes::RouteState {
            db: db(ctx)?,
        })))
    }
}
