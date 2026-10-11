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

use std::path::PathBuf;
use std::sync::Arc;

pub use async_trait::async_trait;
pub use chrono::{DateTime, Utc};
pub use serde_json::{Map, Value};

/// The protocol crate, for anything a tool needs beyond the re-exports.
pub use agent_brain_protocol as protocol;
pub use agent_brain_protocol::{Skill, ToolCallResult, ToolDefinition, parse_args};

/// An in-process plugin: Rust code that contributes tools.
#[async_trait]
pub trait Plugin: Send + Sync {
    /// The bundle folder name (`plugins/<name>/`). Must equal its
    /// `plugin.yaml` `name`.
    fn name(&self) -> &'static str;

    /// The tools this plugin contributes. Called once at startup, after the
    /// core's skills are built; the host refuses the whole plugin if any
    /// tool name is already taken.
    fn tools(&self, ctx: &PluginContext) -> Vec<Arc<dyn Skill>>;

    /// Runs once after the tool registry is built, when the plugin's tools
    /// were accepted: create its graph constraints, seed its nodes. An error
    /// is logged and shown by `GET /api/plugins`; the tools stay registered.
    async fn start(&self, _ctx: &PluginContext) -> anyhow::Result<()> {
        Ok(())
    }

    /// Seed kinds (bundle folder names) this plugin reads, from every loaded
    /// bundle, through [`PluginContext::files`].
    fn seed_kinds(&self) -> &'static [&'static str] {
        &[]
    }

    /// Check one file of a kind from [`Self::seed_kinds`], as `start` would
    /// parse it (`manage_plugin` asks before writing a bundle). `Err` says
    /// what is wrong.
    fn check_seed(&self, _kind: &str, _text: &str) -> Result<(), String> {
        Ok(())
    }

    /// Tools whose result counts as retrieval (consulting the world) for
    /// the chat write guard.
    fn retrieval_tools(&self) -> &'static [&'static str] {
        &[]
    }
}

/// How the host answers an environment lookup.
pub type EnvFn = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

// ── Host capabilities ───────────────────────────────────────────────────────
//
// Each is the core's existing service behind a narrow trait, added when the
// first pack needed it (CORE_AND_PLUGINS_PLAN Phase 4).

/// The knowledge graph. Parameters are a JSON object; rows come back as JSON
/// objects keyed by the query's `AS` aliases. A plugin writes its own labels.
#[async_trait]
pub trait Graph: Send + Sync {
    async fn query(&self, cypher: &str, params: Value) -> anyhow::Result<Vec<Value>>;
}

/// Long-term memory.
#[async_trait]
pub trait Knowledge: Send + Sync {
    /// Store a note; returns its id.
    async fn store_note(
        &self,
        content: &str,
        note_type: &str,
        source_context: Option<&str>,
    ) -> anyhow::Result<String>;
}

/// Text generation through the host's routed provider.
#[async_trait]
pub trait Llm: Send + Sync {
    async fn generate(&self, prompt: &str, system: Option<&str>) -> anyhow::Result<String>;

    /// Generate and parse JSON, re-prompting with the parse error up to
    /// `max_retries` times; an object must carry `required_keys`.
    async fn generate_json(
        &self,
        prompt: &str,
        system: Option<&str>,
        required_keys: &[&str],
        max_retries: u32,
    ) -> anyhow::Result<Value>;
}

/// Running the host's workflows.
#[async_trait]
pub trait Workflows: Send + Sync {
    /// Start a run of `workflow` (a Task); returns the task id. `Err` says
    /// why it was refused (unknown, disabled, unavailable, missing input).
    async fn start_run(
        &self,
        workflow: &str,
        inputs: &Map<String, Value>,
        goal: Option<&str>,
        context: Option<&str>,
    ) -> Result<String, String>;
}

/// Telemetry the host keeps.
pub trait Telemetry: Send + Sync {
    /// Record something the brain could not find or fetch.
    fn log_gap(&self, query: &str, tool: Option<&str>, reason: &str);
}

/// Every loaded bundle's files of one seed kind: `(plugin, path)`, core first.
pub type FilesFn = Arc<dyn Fn(&str) -> Vec<(String, PathBuf)> + Send + Sync>;

/// What the host gives a plugin. Capabilities the host could not provide
/// (no graph configured) are `None`.
#[derive(Clone)]
pub struct PluginContext {
    plugin: String,
    env: EnvFn,
    pub graph: Option<Arc<dyn Graph>>,
    pub knowledge: Option<Arc<dyn Knowledge>>,
    pub llm: Option<Arc<dyn Llm>>,
    pub workflows: Option<Arc<dyn Workflows>>,
    pub telemetry: Option<Arc<dyn Telemetry>>,
    files: Option<FilesFn>,
    day_start: Option<Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>>,
}

impl PluginContext {
    pub fn new(plugin: &str, env: EnvFn) -> Self {
        Self {
            plugin: plugin.to_string(),
            env,
            graph: None,
            knowledge: None,
            llm: None,
            workflows: None,
            telemetry: None,
            files: None,
            day_start: None,
        }
    }

    /// The host side: attach the bundle-file lookup.
    pub fn with_files(mut self, files: FilesFn) -> Self {
        self.files = Some(files);
        self
    }

    /// The host side: attach the clock's local-midnight.
    pub fn with_day_start(mut self, f: Arc<dyn Fn() -> DateTime<Utc> + Send + Sync>) -> Self {
        self.day_start = Some(f);
        self
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

    /// Every loaded bundle's files of `kind` (`.yaml`), core first.
    pub fn files(&self, kind: &str) -> Vec<(String, PathBuf)> {
        self.files.as_ref().map(|f| f(kind)).unwrap_or_default()
    }

    /// The start of today in the owner's time zone, as UTC (UTC midnight
    /// when the host gave no clock).
    pub fn local_day_start_utc(&self) -> DateTime<Utc> {
        match &self.day_start {
            Some(f) => f(),
            None => Utc::now()
                .date_naive()
                .and_hms_opt(0, 0, 0)
                .map(|d| d.and_utc())
                .unwrap_or_else(Utc::now),
        }
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
