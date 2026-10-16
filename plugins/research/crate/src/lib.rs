//! The `research` plugin: paid deep research through Parallel's Task API
//! (`deep_research`), with a monthly spend cap kept on `(:ResearchRun)` nodes
//! and a background collector that delivers reports nobody waited for.
//!
//! The bundle's `plugin.yaml` requires `PARALLEL_API_KEY`, so without it the
//! plugin is skipped instead of offering a tool that can only fail. Prices,
//! waits and the cap are the bundle's `parallel.yaml`.

use std::path::PathBuf;
use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait};
use tracing::{error, warn};

mod client;
mod config;
mod runs;
mod skill;

const KEY_ENV: &str = "PARALLEL_API_KEY";

pub struct Research;

/// The bundle's folder; the source tree's in tests.
fn bundle_root(ctx: &PluginContext) -> PathBuf {
    ctx.root()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/..")))
}

fn parts(ctx: &PluginContext) -> Option<(Arc<client::ParallelClient>, runs::Ledger)> {
    let key = ctx.env(KEY_ENV)?;
    let Some(graph) = ctx.graph.clone() else {
        warn!("research: the host gave no graph (the spend ledger lives there); no tools");
        return None;
    };
    Some((
        Arc::new(client::ParallelClient::new(key)),
        runs::Ledger::new(graph),
    ))
}

#[async_trait]
impl Plugin for Research {
    fn name(&self) -> &'static str {
        "research"
    }

    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        let Some((client, ledger)) = parts(ctx) else {
            return vec![];
        };
        let config = match config::Config::load_from(&bundle_root(ctx)) {
            Ok(c) => c,
            Err(e) => {
                // Without prices there is no cap to enforce: no tool, loudly.
                error!(error = %format!("{e:#}"), "research: parallel.yaml unusable; deep_research not offered");
                return vec![];
            }
        };
        vec![Arc::new(skill::ResearchSkill::new(
            ctx.clone(),
            client,
            ledger,
            config,
        )) as Arc<dyn Skill>]
    }

    async fn start(&self, ctx: &PluginContext) -> anyhow::Result<()> {
        let Some((client, ledger)) = parts(ctx) else {
            return Ok(());
        };
        ledger.ensure_schema().await?;
        runs::spawn_collector(ctx, ledger, client);
        Ok(())
    }

    fn retrieval_tools(&self) -> &'static [&'static str] {
        &["deep_research"]
    }
}
