use serde::{Deserialize, Serialize};
use std::cmp::Ordering;

/// Status of a background agent job.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentJobStatus {
    /// Waiting to be picked up by the coordinator.
    Queued,
    /// Currently executing.
    Running,
    /// Finished successfully.
    Completed,
    /// Failed but within max_attempts — can be retried.
    Failed,
    /// Exhausted all retry attempts.
    Dead,
    /// Manually paused — will not be picked up until resumed.
    Parked,
    /// Permanently cancelled.
    Cancelled,
}

impl std::fmt::Display for AgentJobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            AgentJobStatus::Queued => "queued",
            AgentJobStatus::Running => "running",
            AgentJobStatus::Completed => "completed",
            AgentJobStatus::Failed => "failed",
            AgentJobStatus::Dead => "dead",
            AgentJobStatus::Parked => "parked",
            AgentJobStatus::Cancelled => "cancelled",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for AgentJobStatus {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "queued" => Ok(AgentJobStatus::Queued),
            "running" => Ok(AgentJobStatus::Running),
            "completed" => Ok(AgentJobStatus::Completed),
            "failed" => Ok(AgentJobStatus::Failed),
            "dead" => Ok(AgentJobStatus::Dead),
            "parked" => Ok(AgentJobStatus::Parked),
            "cancelled" => Ok(AgentJobStatus::Cancelled),
            _ => Err(()),
        }
    }
}

/// A background job that executes an MCP tool asynchronously.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentJob {
    /// Unique identifier (UUID).
    pub id: String,
    /// MCP tool name to invoke.
    pub tool_name: String,
    /// JSON arguments passed to the tool.
    pub arguments: Option<serde_json::Value>,
    /// Priority: 0 = lowest, 3 = critical. Higher values are processed first.
    pub priority: u8,
    /// Current lifecycle status.
    pub status: AgentJobStatus,
    pub created_at: String,
    pub updated_at: String,
    pub started_at: Option<String>,
    pub completed_at: Option<String>,
    /// Tool output (serialised ToolCallResult) after successful completion.
    pub result: Option<serde_json::Value>,
    /// Last error message on failure.
    pub error: Option<String>,
    /// How many times this job has been attempted.
    pub attempt_count: u32,
    /// Maximum allowed attempts before the job is marked Dead.
    pub max_attempts: u32,
    /// Optional session ID for grouping related jobs.
    pub session_id: Option<String>,
    /// Optional parent job ID for chained / sub-task jobs.
    pub parent_job_id: Option<String>,
    /// Optional hint for choosing a specific LLM provider.
    pub provider_hint: Option<String>,
}

/// Wrapper for `BinaryHeap` ordering.
///
/// The heap is a **max-heap**, so higher priority wins.
/// Within the same priority, jobs are processed FIFO (earlier `created_at` first).
#[derive(Debug)]
pub struct PrioritizedJob {
    pub priority: u8,
    /// ISO-8601 timestamp — compared lexicographically (newer = larger string).
    pub created_at: String,
    pub job: AgentJob,
}

impl PartialEq for PrioritizedJob {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.created_at == other.created_at
    }
}

impl Eq for PrioritizedJob {}

impl PartialOrd for PrioritizedJob {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for PrioritizedJob {
    fn cmp(&self, other: &Self) -> Ordering {
        // Higher priority first.  Tie-break: earlier created_at first (FIFO within same priority).
        self.priority
            .cmp(&other.priority)
            .then_with(|| other.created_at.cmp(&self.created_at))
    }
}
