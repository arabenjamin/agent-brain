//! The `reference` plugin (CORE_AND_PLUGINS_PLAN Phase 4, pack 3): what an
//! entity *is*, from public references.
//!
//! - **Reference links** (`wiki.rs`, `link_entities`): which Wikidata item
//!   and English Wikipedia article an entity is, as proposals the person
//!   confirms; run hourly by the `entity-linking` workflow and on demand.
//! - **Kinds** (`kinds.rs`): the fine kind (country, news outlet, …) walked
//!   up a linked item's classes to the roots in the bundle's `kinds.yaml`,
//!   and the type questions when it disagrees with the filed type.
//! - **Background** (`background.rs`, `wiki_background`): Wikipedia as the
//!   first step of researching a question.
//! - **Routes** (`routes.rs`) under `/api/plugins/reference/`.
//!
//! Every Wikimedia request needs `WIKIMEDIA_CONTACT`; without it the tools
//! and routes say so instead of calling out.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait, axum};

mod background;
mod db;
mod geo;
mod kinds;
mod routes;
mod skill;
mod wiki;

pub struct Reference;

static ROOT: OnceLock<PathBuf> = OnceLock::new();
static DEMONYMS: OnceLock<Vec<String>> = OnceLock::new();

/// The bundle's folder (`kinds.yaml` lives there); the source tree's in tests.
pub(crate) fn bundle_root() -> PathBuf {
    ROOT.get()
        .cloned()
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/..")))
}

/// Demonym words from the core's entity rules, read once at startup.
pub(crate) fn demonym_words() -> Vec<String> {
    DEMONYMS.get().cloned().unwrap_or_default()
}

fn db(ctx: &PluginContext) -> Option<db::Db> {
    Some(db::Db::new(ctx.graph.clone()?, ctx.entities.clone()))
}

#[async_trait]
impl Plugin for Reference {
    fn name(&self) -> &'static str {
        "reference"
    }

    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        if let Some(r) = ctx.root() {
            let _ = ROOT.set(r.to_path_buf());
        }
        if let Some(e) = &ctx.entities {
            let _ = DEMONYMS.set(e.demonym_words());
        }
        match (db(ctx), ctx.llm.clone()) {
            (Some(d), Some(llm)) => {
                vec![
                    Arc::new(skill::WikiSkill::new(d, llm, ctx.knowledge.clone()))
                        as Arc<dyn Skill>,
                ]
            }
            _ => {
                tracing::warn!("reference: the host gave no graph or LLM; no tools");
                vec![]
            }
        }
    }

    async fn start(&self, ctx: &PluginContext) -> anyhow::Result<()> {
        if let Some(d) = db(ctx) {
            geo::spawn(ctx, d);
        }
        Ok(())
    }

    fn routes(&self, ctx: &PluginContext) -> Option<axum::Router> {
        Some(routes::router(Arc::new(routes::RouteState {
            db: db(ctx)?,
            workflows: ctx.workflows.clone(),
        })))
    }
}
