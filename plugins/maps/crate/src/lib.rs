//! The `maps` plugin (MAP_PLAN.md): `geocode`, and the schema its
//! `(:Place)` cache needs (Phase 2); map layers and the Map panel's routes
//! (Phase 3, `layers.rs`, `routes.rs`). The spatial views live in the
//! bundle's `views/`, the shipped layers in `layers/`.

use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait, graph::query};

mod geocode;
pub mod layers;
mod routes;

pub use geocode::GeocodeSkill;

pub struct Maps;

#[async_trait]
impl Plugin for Maps {
    fn name(&self) -> &'static str {
        "maps"
    }

    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        match ctx.graph.clone() {
            Some(g) => vec![Arc::new(GeocodeSkill::new(g, |k| ctx.env(k))) as Arc<dyn Skill>],
            None => {
                tracing::warn!("maps: the host gave no graph; no tools");
                vec![]
            }
        }
    }

    async fn start(&self, ctx: &PluginContext) -> anyhow::Result<()> {
        if let Some(g) = &ctx.graph {
            g.run(query(
                "CREATE CONSTRAINT place_key IF NOT EXISTS FOR (p:Place) REQUIRE p.key IS UNIQUE",
            ))
            .await?;
        }
        for bad in routes::load_all(ctx).1 {
            tracing::warn!(%bad, "maps: a layer did not load");
        }
        Ok(())
    }

    fn seed_kinds(&self) -> &'static [&'static str] {
        &["layers"]
    }

    fn check_seed(&self, kind: &str, text: &str) -> Result<(), String> {
        match kind {
            "layers" => layers::Layer::parse(text).map(|_| ()),
            other => Err(format!("the maps plugin does not read '{other}'")),
        }
    }

    fn routes(&self, ctx: &PluginContext) -> Option<agent_brain_plugin_api::axum::Router> {
        Some(routes::router(Arc::new(routes::RouteState {
            graph: ctx.graph.clone()?,
            ctx: ctx.clone(),
        })))
    }
}
