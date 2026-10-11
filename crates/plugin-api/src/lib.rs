//! The API an in-process plugin is written against (CORE_AND_PLUGINS_PLAN
//! Phase 3c). A plugin crate depends on this crate and external crates only,
//! never on the brain's `app` or `repository` crates: that is what lets a
//! pack be disabled, moved or replaced without touching the core.
//!
//! A plugin is a folder `plugins/<name>/` with a `plugin.yaml`, any seed dirs,
//! and optionally a `crate/` implementing [`Plugin`]. The host instantiates
//! the plugin only when its bundle loaded (its `requires` were met).
//!
//! [`PluginContext`] is the host's side. It grows with its consumers: each
//! capability (graph, knowledge, LLM, notify, …) is added when the first pack
//! that needs it moves out of the core, not before.

use std::sync::Arc;

/// The protocol crate, for anything a tool needs beyond the re-exports.
pub use agent_brain_protocol as protocol;
pub use agent_brain_protocol::{Skill, ToolCallResult, ToolDefinition, parse_args};

/// An in-process plugin: Rust code that contributes tools.
pub trait Plugin: Send + Sync {
    /// The bundle folder name (`plugins/<name>/`). Must equal its
    /// `plugin.yaml` `name`.
    fn name(&self) -> &'static str;

    /// The tools this plugin contributes. Called once at startup, after the
    /// core's skills are built; the host refuses the whole plugin if any
    /// tool name is already taken.
    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>>;
}

/// How the host answers an environment lookup.
pub type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

/// What the host gives a plugin.
#[derive(Clone)]
pub struct PluginContext {
    plugin: String,
    env: EnvFn,
}

impl PluginContext {
    pub fn new(plugin: &str, env: EnvFn) -> Self {
        Self {
            plugin: plugin.to_string(),
            env,
        }
    }

    /// The plugin this context belongs to.
    pub fn plugin(&self) -> &str {
        &self.plugin
    }

    /// An environment value, trimmed; `None` when unset or blank. Secrets
    /// reach plugins this way, never through their YAML.
    pub fn env(&self, key: &str) -> Option<String> {
        (self.env)(key)
            .map(|v| v.trim().to_string())
            .filter(|v| !v.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn env_is_trimmed_and_blank_is_absent() {
        let ctx = PluginContext::new(
            "p",
            Arc::new(|k: &str| match k {
                "SET" => Some("  v ".into()),
                "BLANK" => Some("  ".into()),
                _ => None,
            }),
        );
        assert_eq!(ctx.plugin(), "p");
        assert_eq!(ctx.env("SET").as_deref(), Some("v"));
        assert_eq!(ctx.env("BLANK"), None);
        assert_eq!(ctx.env("UNSET"), None);
    }
}
