//! The `alias-judge` plugin: a local classifier's verdict on each entity
//! merge candidate (HBI plan §11.10), judged on idle time and stored as
//! `ALIAS_VERDICT`. The person still decides every merge; the core sorts its
//! review list by the stored verdict (`entity_resolution::rank`). No tools:
//! the plugin is its background loop.

use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill, async_trait};

mod judge;

pub struct AliasJudge;

#[async_trait]
impl Plugin for AliasJudge {
    fn name(&self) -> &'static str {
        "alias-judge"
    }

    fn tools(&self, _ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        vec![]
    }

    async fn start(&self, ctx: &PluginContext) -> anyhow::Result<()> {
        judge::spawn(ctx);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_matches_plugin_yaml() {
        let want = format!("name: {}", AliasJudge.name());
        assert!(
            include_str!("../../plugin.yaml").lines().any(|l| l == want),
            "plugin.yaml has no `{want}` line"
        );
    }
}
