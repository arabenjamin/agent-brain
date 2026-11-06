//! The `maps` plugin (MAP_PLAN.md): `geocode`, and the schema its
//! `(:Place)` cache needs (Phase 2); map layers and the Map panel's routes
//! (Phase 3, `layers.rs`, `routes.rs`); ship and aircraft tracks over the
//! watch areas, and `area_traffic` (Phase 4, `tracks/`). The spatial views
//! live in the bundle's `views/`, the shipped layers in `layers/`; watch
//! areas come from other bundles' `areas/` (the NIA's are private).

use std::sync::{Arc, OnceLock};

use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait};

mod geocode;
pub mod layers;
mod routes;
pub mod tracks;

pub use geocode::GeocodeSkill;

pub struct Maps;

/// The tracks store and collectors, shared by the tool, the routes and the
/// collectors (opened once, by whichever asks first).
static TRACKS: OnceLock<tracks::Tracks> = OnceLock::new();

fn tracks(ctx: &PluginContext) -> tracks::Tracks {
    TRACKS.get_or_init(|| tracks::Tracks::open(ctx)).clone()
}

#[async_trait]
impl Plugin for Maps {
    fn name(&self) -> &'static str {
        "maps"
    }

    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        if let Some(g) = ctx.graph.clone() {
            vec![
                Arc::new(GeocodeSkill::new(g, |k| ctx.env(k))) as Arc<dyn Skill>,
                Arc::new(tracks::tool::AreaTrafficSkill::new(tracks(ctx))),
            ]
        } else {
            tracing::warn!("maps: the host gave no graph; no tools");
            vec![]
        }
    }

    async fn start(&self, ctx: &PluginContext) -> anyhow::Result<()> {
        if let Some(g) = &ctx.graph {
            // Places' and watch areas' constraints: schemas/graph/schema.cypher,
            // before the areas are seeded below.
            agent_brain_plugin_api::graph::apply_schema(
                g.as_ref(),
                agent_brain_plugin_api::protocol::graph_schema!(plugin),
            )
            .await?;
        }
        for bad in routes::load_all(ctx).1 {
            tracing::warn!(%bad, "maps: a layer did not load");
        }
        let t = tracks(ctx);
        // The bundles' area files into the graph, before the collectors read it.
        if let Err(e) = t.seed().await {
            tracing::warn!(error = %e, "maps: watch areas not seeded");
        }
        t.spawn();
        Ok(())
    }

    fn seed_kinds(&self) -> &'static [&'static str] {
        &["layers", "areas"]
    }

    fn check_seed(&self, kind: &str, text: &str) -> Result<(), String> {
        match kind {
            "layers" => layers::Layer::parse(text).map(|_| ()),
            "areas" => tracks::Area::parse(text).map(|_| ()),
            other => Err(format!("the maps plugin does not read '{other}'")),
        }
    }

    fn routes(&self, ctx: &PluginContext) -> Option<agent_brain_plugin_api::axum::Router> {
        Some(routes::router(Arc::new(routes::RouteState {
            graph: ctx.graph.clone()?,
            ctx: ctx.clone(),
            tracks: tracks(ctx),
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_plugin_yaml() {
        let want = format!("name: {}", Maps.name());
        assert!(
            include_str!("../../plugin.yaml").lines().any(|l| l == want),
            "plugin.yaml has no `{want}` line"
        );
    }
}
