//! The `sandbox` plugin: `execute_code`, the client for the offline Python and
//! Rust sandboxes (CORE_AND_PLUGINS_PLAN 6g; it was a core skill until then).
//! The isolation contract lives in docker-compose.yml and `skill.rs`.

use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill};

mod skill;

pub use skill::ExecSkill;

pub struct Sandbox;

impl Plugin for Sandbox {
    fn name(&self) -> &'static str {
        "sandbox"
    }

    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        ExecSkill::from_env(|k| ctx.env(k))
            .map(|s| vec![Arc::new(s) as Arc<dyn Skill>])
            .unwrap_or_default()
    }
}
