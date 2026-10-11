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
/// The HTTP framework a plugin's routes are written in (the host's version).
pub use axum;
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

    /// HTTP routes, served under `/api/plugins/<name>/` (behind the host's
    /// API key). Paths here are relative: `/wiki/coverage` is reached at
    /// `/api/plugins/<name>/wiki/coverage`. Gone when the plugin is off.
    fn routes(&self, _ctx: &PluginContext) -> Option<axum::Router> {
        None
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

    /// [`graph::Query`] form: the rows as [`graph::Row`]s.
    async fn execute(&self, q: graph::Query) -> anyhow::Result<Vec<graph::Row>> {
        let (cypher, params) = q.into_parts();
        Ok(self
            .query(&cypher, params)
            .await?
            .into_iter()
            .map(graph::Row)
            .collect())
    }

    /// [`graph::Query`] form, rows discarded.
    async fn run(&self, q: graph::Query) -> anyhow::Result<()> {
        let (cypher, params) = q.into_parts();
        self.query(&cypher, params).await.map(|_| ())
    }
}

/// A neo4rs-shaped way to write graph calls, so code ported from the core
/// changes little: `query("…").param("k", v)`, `graph.execute(q)`,
/// `row.get::<String>("k")`.
pub mod graph {
    use serde::Serialize;
    use serde::de::DeserializeOwned;
    use serde_json::{Map, Value};

    /// A Cypher query with its parameters.
    #[derive(Debug, Clone)]
    pub struct Query {
        cypher: String,
        params: Map<String, Value>,
    }

    pub fn query(cypher: &str) -> Query {
        Query {
            cypher: cypher.to_string(),
            params: Map::new(),
        }
    }

    impl Query {
        /// Bind `$key`. `None` binds null.
        pub fn param<T: Serialize>(mut self, key: &str, value: T) -> Self {
            self.params.insert(
                key.to_string(),
                serde_json::to_value(value).unwrap_or(Value::Null),
            );
            self
        }

        pub fn into_parts(self) -> (String, Value) {
            (self.cypher, Value::Object(self.params))
        }
    }

    /// One result row, keyed by the query's `AS` aliases.
    #[derive(Debug, Clone)]
    pub struct Row(pub Value);

    impl Row {
        /// The column as `T`; `Err` when absent, null or another type.
        pub fn get<T: DeserializeOwned>(&self, key: &str) -> anyhow::Result<T> {
            match self.0.get(key) {
                None | Some(Value::Null) => anyhow::bail!("column `{key}` is absent or null"),
                Some(v) => serde_json::from_value(v.clone())
                    .map_err(|e| anyhow::anyhow!("column `{key}`: {e}")),
            }
        }
    }
}

/// The core's entities: what plugins that enrich them need.
#[async_trait]
pub trait Entities: Send + Sync {
    /// The merge candidates awaiting review (`{candidates: [{from, into,
    /// judge, …}]}`), optionally only those involving `entity`.
    async fn merge_candidates(&self, entity: Option<&str>, limit: usize) -> anyhow::Result<Value>;

    /// Demonym words from the core's entity rules ("American", "Iranian").
    fn demonym_words(&self) -> Vec<String>;
}

/// Whether the brain is doing foreground work (a job running or queued, a
/// chat turn): idle-time loops back off while it is.
#[async_trait]
pub trait Activity: Send + Sync {
    async fn busy(&self) -> bool;
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

    /// Store a note marked with where its words came from (a model's
    /// synthesis, the brain's own record), which retrieval labels.
    async fn store_note_as(
        &self,
        content: &str,
        note_type: &str,
        source_context: Option<&str>,
        provenance: Provenance,
    ) -> anyhow::Result<String>;

    /// Add an entry to a working-memory session (what a chat or agent is
    /// working with right now).
    async fn push_session_entry(
        &self,
        session_id: &str,
        text: &str,
        role: &str,
    ) -> anyhow::Result<()>;
}

/// Who chose a note's words.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provenance {
    /// A person, or an external system through a tool call.
    UserInput,
    /// A model: synthesis, inference, summary.
    SynthesisInference,
    /// The brain's own code, with no person or model choosing the words.
    SystemRecord,
}

/// The owner's Feed (items to review or act on).
#[async_trait]
pub trait Feed: Send + Sync {
    /// Create an item assigned to the user. The same `origin_kind` +
    /// `origin_ref` returns the existing item instead of a second one.
    /// Returns its id.
    async fn create_item(&self, item: FeedItem) -> anyhow::Result<String>;
}

/// A Feed item to create.
#[derive(Debug, Clone, Default)]
pub struct FeedItem {
    pub title: String,
    pub body: String,
    /// 1 (high) … 3 (low).
    pub priority: i64,
    pub origin_kind: String,
    pub origin_ref: String,
}

/// What the brain is made of, as the host knows it (for self-description).
pub trait Introspection: Send + Sync {
    /// The HTTP transport's router source, as this binary was compiled from
    /// (its `.route(..)` calls are the core's routes).
    fn router_source(&self) -> &'static str;
    /// The live provider registry (`[{name, …}]`).
    fn providers(&self) -> Value;
    /// The core's `components.yaml` services (`[{name, label, kind, …}]`).
    fn components(&self) -> Value;
    /// Tools declared in bundles' `tools/` (`(tool, plugin)`).
    fn declared_tools(&self) -> Vec<(String, String)>;
    /// The brain's own source tree (`CODEBASE_DIR`, else the repo root).
    fn codebase_dir(&self) -> Option<PathBuf>;
    /// Where the brain may write files (`WORKSPACE_DIR`).
    fn workspace_dir(&self) -> Option<PathBuf>;
}

/// Text generation through the host's routed provider.
#[async_trait]
pub trait Llm: Send + Sync {
    /// The model a call made now would use: the workflow step's pinned
    /// model inside a job, else the provider's. For provenance.
    fn model_name(&self) -> String;

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

    /// Enqueue one job running `tool` exactly as the workflow step that
    /// calls it is configured (its pinned model, provider and retries),
    /// with these `arguments`. Returns the job id. `Err` when no workflow
    /// has such a step.
    async fn enqueue_tool_step(
        &self,
        tool: &str,
        arguments: Value,
        description: &str,
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
    pub entities: Option<Arc<dyn Entities>>,
    pub activity: Option<Arc<dyn Activity>>,
    pub feed: Option<Arc<dyn Feed>>,
    pub introspection: Option<Arc<dyn Introspection>>,
    files: Option<FilesFn>,
    root: Option<PathBuf>,
    background: bool,
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
            entities: None,
            activity: None,
            feed: None,
            introspection: None,
            files: None,
            root: None,
            background: false,
            day_start: None,
        }
    }

    /// The host side: whether this process runs background loops.
    pub fn with_background(mut self, allowed: bool) -> Self {
        self.background = allowed;
        self
    }

    /// Whether this process is the deployed brain, which runs idle-time
    /// loops. A stdio session alongside it is not: a plugin's background
    /// loop starts only when this is true.
    pub fn background_allowed(&self) -> bool {
        self.background
    }

    /// The host side: the bundle's folder.
    pub fn with_root(mut self, root: PathBuf) -> Self {
        self.root = Some(root);
        self
    }

    /// The bundle's folder (`plugins/<name>/`), for files the plugin reads
    /// from its own bundle only.
    pub fn root(&self) -> Option<&std::path::Path> {
        self.root.as_deref()
    }

    /// Whether foreground work is running (false when the host gave no
    /// activity signal).
    pub async fn busy(&self) -> bool {
        match &self.activity {
            Some(a) => a.busy().await,
            None => false,
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
