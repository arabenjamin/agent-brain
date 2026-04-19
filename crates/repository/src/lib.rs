mod agent_job;
mod client;
mod error;
mod scheduled_task;
mod task;
mod todo;

#[cfg(feature = "telemetry")]
pub mod telemetry;

pub use agent_brain_models::ScheduledTask;
pub use client::Neo4jClient;
pub use error::{RepositoryError, Result};
pub use todo::Todo;

#[cfg(feature = "telemetry")]
pub use telemetry::TelemetryClient;

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

    pub fn get_recent_gaps(&self, _limit: usize) -> anyhow::Result<Vec<(String, String, String)>> {
        Ok(vec![])
    }

    pub fn get_training_examples(
        &self,
        _min_score: Option<i32>,
    ) -> anyhow::Result<Vec<(String, String)>> {
        Ok(vec![])
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

    pub fn get_model_system_prompt(&self, _name: &str) -> anyhow::Result<Option<String>> {
        Ok(None)
    }

    pub fn select_models(
        &self,
        _required_capabilities: &[String],
        _provider_hint: Option<&str>,
        _max_cost_per_1k: Option<f64>,
    ) -> anyhow::Result<Vec<serde_json::Value>> {
        Ok(vec![])
    }

    pub fn record_model_usage(
        &self,
        _model_name: &str,
        _tool_name: Option<&str>,
        _success: bool,
        _duration_ms: Option<i64>,
        _tokens_in: Option<i64>,
        _tokens_out: Option<i64>,
    ) -> anyhow::Result<()> {
        Ok(())
    }

    pub fn get_model_stats(&self, _model_name: &str) -> anyhow::Result<serde_json::Value> {
        Ok(serde_json::json!({ "model": _model_name, "total_calls": 0 }))
    }
}
