//! Dead Letter Queue entry model.
//!
//! Stores jobs that have exhausted all retries or failed permanently.

use serde::{Deserialize, Serialize};

/// A dead letter queue entry for a failed job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetterEntry {
    /// Original job ID (also the primary key).
    pub job_id: String,
    /// When the job was moved to DLQ (RFC3339).
    pub moved_at: String,
    /// Reason for dead lettering.
    pub reason: DeadLetterReason,
    /// Number of attempts before dead lettering.
    pub attempt_count: u32,
    /// Last error message.
    pub last_error: Option<String>,
    /// Original job data as JSON.
    pub job_data: String,
    /// Whether this entry has been acknowledged/reviewed.
    pub acknowledged: bool,
    /// When this entry was acknowledged (RFC3339), if applicable.
    pub acknowledged_at: Option<String>,
}

/// Reason why a job was moved to the dead letter queue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeadLetterReason {
    /// Job exceeded maximum retry attempts.
    MaxRetriesExceeded,
    /// Job expired (TTL reached).
    Expired,
    /// Parent job in chain failed.
    ParentFailed,
    /// Manually moved to DLQ.
    Manual,
    /// Unknown error.
    Unknown,
}

impl std::fmt::Display for DeadLetterReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            DeadLetterReason::MaxRetriesExceeded => "max_retries_exceeded",
            DeadLetterReason::Expired => "expired",
            DeadLetterReason::ParentFailed => "parent_failed",
            DeadLetterReason::Manual => "manual",
            DeadLetterReason::Unknown => "unknown",
        };
        write!(f, "{s}")
    }
}

/// Statistics for the dead letter queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetterStats {
    /// Total entries in DLQ.
    pub total_entries: u64,
    /// Number of unacknowledged entries.
    pub unacknowledged: u64,
    /// Number of acknowledged entries.
    pub acknowledged: u64,
    /// Breakdown by reason.
    pub by_reason: std::collections::HashMap<String, u64>,
}
