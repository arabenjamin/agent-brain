//! The `websocket` plugin: a WebSocket client whose connections live across
//! tool calls (ws_connect, ws_send, ws_receive, ws_close). It was a core skill
//! until CORE_AND_PLUGINS_PLAN Phase 0 removed it unused; it came back as a
//! plugin on 2026-10-20, now under the net policy (see `skill.rs`).

use std::sync::Arc;

use agent_brain_plugin_api::{Plugin, PluginContext, Skill};

mod skill;

pub use skill::WsSkill;

pub struct Websocket;

impl Plugin for Websocket {
    fn name(&self) -> &'static str {
        "websocket"
    }

    fn tools(&self, _ctx: &PluginContext) -> Vec<Arc<dyn Skill>> {
        vec![Arc::new(WsSkill::new()) as Arc<dyn Skill>]
    }
}
