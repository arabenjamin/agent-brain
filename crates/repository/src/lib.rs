mod agent_job;
mod client;
mod error;
mod task;
mod temporal;
mod todo;
mod workflow;

#[cfg(feature = "telemetry")]
pub mod telemetry;

pub use agent_brain_models::{WORKFLOW_KINDS, Workflow, WorkflowInput};
pub use client::Neo4jClient;
pub use error::{RepositoryError, Result};
pub use temporal::node_ts;
pub use todo::{TODO_ORIGINS, TODO_STATUSES, Todo, TodoItemPatch};
pub use workflow::{WorkflowDef, WorkflowPatch, YamlSyncOutcome};

#[cfg(feature = "telemetry")]
pub use telemetry::TelemetryClient;

/// One `search_web` engine attempt, as the usage ledger records it.
///
/// The ledger is both quota accounting (what each engine was asked to do,
/// in its budget's units) and the performance record that rotation weights
/// come from: who asked, whether the engine was picked by rotation, pinned
/// or reached as a fallback, and cheap quality hints from the results in
/// hand (dated, inside the requested window, on a known source list).
#[derive(Debug, Clone, Default)]
pub struct SearchAttempt {
    /// Groups the attempts of one `search_web` call.
    pub call_id: String,
    pub engine: String,
    pub query: String,
    pub success: bool,
    pub result_count: Option<i64>,
    pub duration_ms: Option<i64>,
    pub error_kind: Option<String>,
    /// What the attempt cost in the engine's budget unit (USD for Exa, one
    /// call elsewhere); `None` for a failed attempt, which no vendor bills.
    pub units: Option<f64>,
    /// `rotation`, `pinned` (the caller named the engine), `ladder` (rotation
    /// off or no budgeted engine fit), or `fallback` (an earlier rung failed).
    pub picked: String,
    /// The engine the caller named, if any.
    pub requested: Option<String>,
    /// `chat`, `background`, `subagent`, ... (the routing purpose).
    pub purpose: Option<String>,
    /// The queue job's tool (`claim`, `run_agent`, `search_web`, ...).
    pub tool: Option<String>,
    /// The workflow run's Task, for drilling into which workflow searched.
    pub task_id: Option<String>,
    pub recency: Option<String>,
    pub news: bool,
    pub dated: Option<i64>,
    pub in_window: Option<i64>,
    pub known_source: Option<i64>,
    pub domains: Option<i64>,
    /// The result links, in rank order.
    pub links: Vec<String>,
}

/// One engine's recent use, for rotation: units spent since each cutoff
/// (`*_unpriced` counts successful rows written before units were recorded),
/// and attempts in the last hour and minute for rate limits.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct EngineUse {
    pub engine: String,
    pub period_units: f64,
    pub period_unpriced: i64,
    pub day_calls: i64,
    pub hour_calls: i64,
    pub minute_calls: i64,
}

/// Stub TelemetryClient compiled when the `telemetry` feature is disabled.
///
/// Provides the same public API so downstream code compiles unchanged.
/// `new()` always returns an error; all other methods are unreachable at
/// runtime because `new()` never succeeds.
#[cfg(not(feature = "telemetry"))]
#[derive(Clone)]
pub struct TelemetryClient;

#[cfg(not(feature = "telemetry"))]
impl TelemetryClient {
    pub fn new<P: AsRef<std::path::Path>>(_path: P) -> anyhow::Result<Self> {
        anyhow::bail!("compiled without 'telemetry' feature — enable it and set TELEMETRY_DB_PATH")
    }

    pub fn log_interaction(
        &self,
        _prompt: &str,
        _response: &str,
        _tools_used: Option<&serde_json::Value>,
        _success: bool,
        _latency_ms: u64,
        _model: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn log_knowledge_gap(
        &self,
        _query: &str,
        _context: Option<&str>,
        _gap_type: &str,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_model(
        &self,
        _name: &str,
        _provider: &str,
        _model: &str,
        _context_window: i64,
        _cost_input: f64,
        _cost_output: f64,
        _capabilities: &str,
        _system_prompt: Option<&str>,
        _temperature: Option<f64>,
        _max_tokens: Option<i64>,
        _timeout_secs: Option<i64>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn clear_model_registry(&self) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn list_models(&self) -> anyhow::Result<Vec<serde_json::Value>> {
        Ok(vec![])
    }

    pub fn select_models(
        &self,
        _required_capabilities: &[String],
        _provider_hint: Option<&str>,
        _max_cost_per_1k: Option<f64>,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        Ok(vec![])
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_model_usage(
        &self,
        _model_name: &str,
        _tool_name: Option<&str>,
        _success: bool,
        _duration_ms: Option<i64>,
        _tokens_in: Option<i64>,
        _tokens_out: Option<i64>,
        _error_kind: Option<&str>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn get_model_stats(
        &self,
        _model_name: Option<&str>,
        _window_hours: Option<i64>,
    ) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({ "models": [] }))
    }

    pub fn models_with_recent_errors(
        &self,
        _error_kind: &str,
        _hours: i64,
    ) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }

    pub fn search_engines_with_recent_errors(
        &self,
        _error_kind: &str,
        _hours: i64,
    ) -> anyhow::Result<Vec<String>> {
        Ok(vec![])
    }

    pub fn get_search_stats(
        &self,
        _window_hours: Option<i64>,
    ) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({ "engines": [], "by_day": [] }))
    }

    pub fn record_search_attempt(&self, _a: &SearchAttempt) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn engine_use(
        &self,
        _engine: &str,
        _period_start: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<EngineUse> {
        Ok(EngineUse::default())
    }

    pub fn search_links_since(&self, _window_hours: i64) -> anyhow::Result<Vec<(String, String)>> {
        Ok(vec![])
    }
}
