use serde::{Deserialize, Serialize};

/// A recurring task managed by the scheduler.
///
/// Each `ScheduledTask` stores its own job chain (`steps` as a JSON string)
/// and recurrence interval.  When `next_run_at <= now()` and `enabled = true`
/// the scheduler creates a one-off `Task` node as a run record, enqueues the
/// chain, and updates `last_run_at` / `next_run_at`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ScheduledTask {
    /// UUID string — unique identifier.
    pub id: String,
    /// Human-readable name, also used as the `goal` of the spawned Task node.
    pub name: String,
    /// Optional description shown in list views and LLM context.
    pub description: Option<String>,
    /// Whether the scheduler will dispatch this task when due.
    pub enabled: bool,
    /// Recurrence period in whole seconds (e.g. 86400 = daily, 604800 = weekly).
    pub interval_seconds: i64,
    /// JSON-serialised `Vec<ChainStep>` — the job chain to execute.
    pub steps: String,
    /// RFC3339 timestamp of the last successful dispatch (`None` = never run).
    pub last_run_at: Option<String>,
    /// RFC3339 timestamp when this task is next due to run.
    pub next_run_at: String,
    /// RFC3339 timestamp of node creation.
    pub created_at: String,
    /// RFC3339 timestamp of last update.
    pub updated_at: String,
}
