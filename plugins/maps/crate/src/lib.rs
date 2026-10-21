//! The `maps` plugin (MAP_PLAN.md): `geocode`, and the schema its
//! `(:Place)` cache needs. The spatial views live in the bundle's `views/`.

use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait, graph::query};

mod geocode;

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
        Ok(())
    }
}
