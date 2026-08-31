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
    /// Ownership marker. `"yaml"` = definition is owned by a `schedules/*.yaml`
    /// file and force-synced on every startup (runtime edits to steps,
    /// description, and interval are overwritten). `"runtime"` = created at
    /// runtime via tools or REST; the seeder never touches it. `None` on
    /// legacy nodes — resolved at seed time (claimed by a matching YAML name,
    /// otherwise backfilled to `"runtime"`).
    #[serde(default)]
    pub managed_by: Option<String>,
    /// Why this task is disabled, when it was disabled deliberately rather than
    /// never enabled. Free text, set alongside `enabled = false`.
    ///
    /// A disabled schedule and a *paused* one look identical from the outside,
    /// and the difference is the whole question a human asks: is this off
    /// because it is broken, because it was superseded, or because someone
    /// turned it off on purpose and means to turn it back on? Without a reason
    /// the honest answer months later is a guess, and the observed failure mode
    /// in this codebase is that a guess gets reported as a fact. Carried in the
    /// `list` payload for the same reason.
    #[serde(default)]
    pub paused_reason: Option<String>,
}
