//! The `osint-data` plugin: live conflict-event data from ACLED
//! (`acled_events`). Licence and storage rules live in `client.rs`.
//!
//! The bundle's `plugin.yaml` requires `ACLED_EMAIL` and `ACLED_PASSWORD`, so
//! without them the plugin is skipped with that reason instead of offering a
//! tool that can only fail.

use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill};

mod client;
mod skill;

pub struct OsintData;

impl Plugin for OsintData {
    fn name(&self) -> &'static str {
        "osint-data"
    }

    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        skill::AcledSkill::from_ctx(ctx)
            .map(|s| vec![Arc::new(s) as Arc<dyn Skill>])
            .unwrap_or_default()
    }
}
