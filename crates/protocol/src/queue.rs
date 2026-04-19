//! Queue trait abstraction for job queue implementations.
//!
//! This module defines the core `JobQueue` trait that abstracts over different
//! queue backends (in-memory, Redis, NATS, etc.). The current Neo4j-backed
//! implementation lives in the `agent-brain` crate.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

/// Status of a background job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum JobStatus {
    /// Waiting to be picked up by a worker.
    Queued,
    /// Currently executing.
    Running,
    /// Finished successfully.
    Completed,
    /// Failed but within max_attempts — can be retried.
    Failed,
    /// Exhausted all retry attempts (not yet moved to DLQ).
    Dead,
    /// Manually paused — will not run until resumed.
    Parked,
    /// Permanently cancelled.
    Cancelled,
    /// Moved to dead letter queue.
    DeadLetter,
}

impl std::fmt::Display for JobStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            JobStatus::Queued => "queued",
            JobStatus::Running => "running",
            JobStatus::Completed => "completed",
            JobStatus::Failed => "failed",
            JobStatus::Dead => "dead",
            JobStatus::Parked => "parked",
            JobStatus::Cancelled => "cancelled",
            JobStatus::DeadLetter => "dead_letter",
        };
        write!(f, "{s}")
    }
}

impl std::str::FromStr for JobStatus {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, ()> {
        match s {
            "queued" => Ok(JobStatus::Queued),
            "running" => Ok(JobStatus::Running),
            "completed" => Ok(JobStatus::Completed),
            "failed" => Ok(JobStatus::Failed),
            "dead" => Ok(JobStatus::Dead),
            "parked" => Ok(JobStatus::Parked),
            "cancelled" => Ok(JobStatus::Cancelled),
            "dead_letter" => Ok(JobStatus::DeadLetter),
            _ => Err(()),
        }
    }
}

/// A specification for creating a new job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobSpec {
    /// The tool name to invoke.
    pub tool_name: String,
    /// JSON arguments passed to the tool.
    pub arguments: Option<Value>,
    /// Priority: 0 = lowest, 3 = critical. Higher values are processed first.
    pub priority: u8,
    /// Maximum allowed attempts before the job is marked Dead.
    pub max_attempts: u32,
    /// Optional session ID for grouping related jobs.
    pub session_id: Option<String>,
    /// Optional parent job ID for chained / sub-task jobs.
    pub parent_job_id: Option<String>,
    /// Optional hint for choosing a specific LLM provider.
    pub provider_hint: Option<String>,
    /// Optional context profile name for observability and routing hints.
    pub context_profile: Option<String>,
    /// Optional TTL in seconds. Job will be auto-cancelled if not completed by this time.
    pub ttl_secs: Option<u64>,
    /// Optional human-readable description of the job.
    pub description: Option<String>,
}

impl Default for JobSpec {
    fn default() -> Self {
        Self {
            tool_name: String::new(),
            arguments: None,
            priority: 1,
            max_attempts: 3,
            session_id: None,
            parent_job_id: None,
            provider_hint: None,
            context_profile: None,
            ttl_secs: None,
            description: None,
        }
    }
}

/// Progress update for a running job.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct JobProgress {
    /// Current progress value (0-100).
    pub percent: u8,
    /// Optional status message describing current phase.
    pub message: Option<String>,
    /// Optional metadata (e.g., items processed, bytes transferred).
    pub metadata: Option<Value>,
    /// When this progress was reported (RFC3339 timestamp).
    pub updated_at: String,
}

impl JobProgress {
    /// Create a new progress update.
    pub fn new(percent: u8, message: impl Into<String>) -> Self {
        Self {
            percent: percent.min(100),
            message: Some(message.into()),
            metadata: None,
            updated_at: chrono::Utc::now().to_rfc3339(),
        }
    }

    /// Create a progress update with metadata.
    pub fn with_metadata(mut self, metadata: Value) -> Self {
        self.metadata = Some(metadata);
        self
    }
}

/// A job record returned from the queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Job {
    /// Unique identifier.
    pub id: String,
    /// Tool name to invoke.
    pub tool_name: String,
    /// JSON arguments.
    pub arguments: Option<Value>,
    /// Priority level.
    pub priority: u8,
    /// Current status.
    pub status: JobStatus,
    /// Creation timestamp (RFC3339).
    pub created_at: String,
    /// Last update timestamp (RFC3339).
    pub updated_at: String,
    /// When execution started (RFC3339), if applicable.
    pub started_at: Option<String>,
    /// When execution completed (RFC3339), if applicable.
    pub completed_at: Option<String>,
    /// Tool output after successful completion.
    pub result: Option<Value>,
    /// Last error message on failure.
    pub error: Option<String>,
    /// How many times this job has been attempted.
    pub attempt_count: u32,
    /// Maximum allowed attempts.
    pub max_attempts: u32,
    /// Optional session ID.
    pub session_id: Option<String>,
    /// Optional parent job ID for chaining.
    pub parent_job_id: Option<String>,
    /// Optional provider hint.
    pub provider_hint: Option<String>,
    /// Optional context profile.
    pub context_profile: Option<String>,
    /// Plain-text result from preceding chain step.
    pub prev_result: Option<String>,
    /// Current progress (0-100) if job is running.
    pub progress_percent: Option<u8>,
    /// Current progress message if job is running.
    pub progress_message: Option<String>,
    /// When this job expires (RFC3339), if TTL was set.
    pub expires_at: Option<String>,
    /// When this job was moved to dead letter queue (RFC3339), if applicable.
    pub dead_lettered_at: Option<String>,
    /// Reason for dead lettering, if applicable.
    pub dead_letter_reason: Option<String>,
}

/// One step in a sequential job chain.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChainStep {
    pub tool_name: String,
    #[serde(default)]
    pub arguments: Option<Value>,
    /// 0=low … 3=critical. Defaults to 1 (normal).
    pub priority: Option<u8>,
    /// Max execution attempts. Defaults to 3.
    pub max_attempts: Option<u32>,
    pub provider_hint: Option<String>,
    pub context_profile: Option<String>,
    /// Optional TTL in seconds.
    pub ttl_secs: Option<u64>,
    /// Optional description.
    pub description: Option<String>,
}

/// Statistics for the queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueStats {
    /// Number of jobs in each status.
    pub counts_by_status: HashMap<String, u64>,
    /// Total jobs in the system.
    pub total_jobs: u64,
    /// Jobs currently in the in-memory heap (pending immediate execution).
    pub in_memory_pending: usize,
    /// Jobs currently executing.
    pub running: usize,
    /// Maximum concurrent executions allowed.
    pub max_concurrent: usize,
    /// Whether the queue is enabled.
    pub enabled: bool,
    /// Per-provider execution statistics.
    pub per_provider: HashMap<String, ProviderStats>,
    /// Additional metrics (histograms, latencies, etc.).
    #[serde(flatten)]
    pub metrics: HashMap<String, Value>,
}

/// Statistics for a specific provider.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderStats {
    /// Currently running jobs.
    pub running: usize,
    /// Maximum allowed concurrent jobs.
    pub max: usize,
    /// Total jobs completed since startup.
    pub completed: u64,
    /// Total jobs failed since startup.
    pub failed: u64,
    /// Average execution time in milliseconds (if available).
    pub avg_duration_ms: Option<u64>,
}

/// Dead letter queue entry.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeadLetterEntry {
    /// Original job ID.
    pub job_id: String,
    /// When the job was moved to DLQ (RFC3339).
    pub moved_at: String,
    /// Reason for dead lettering.
    pub reason: String,
    /// Number of attempts before dead lettering.
    pub attempt_count: u32,
    /// Last error message.
    pub last_error: Option<String>,
    /// Original job data.
    pub job: Job,
}

/// Configuration for the queue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueConfig {
    /// Global maximum concurrent jobs (informational).
    pub max_concurrent: usize,
    /// Concurrency limit for Ollama (local) jobs.
    pub max_concurrent_ollama: usize,
    /// Concurrency limit for Anthropic API jobs.
    pub max_concurrent_anthropic: usize,
    /// Concurrency limit for Gemini API jobs.
    pub max_concurrent_gemini: usize,
    /// Whether the queue is enabled.
    pub enabled: bool,
    /// How often to poll for missed jobs (seconds).
    pub poll_interval_secs: u64,
    /// Default TTL for jobs without explicit TTL (seconds). None means no TTL.
    pub default_ttl_secs: Option<u64>,
    /// Maximum age of dead jobs before cleanup (seconds).
    pub dead_job_retention_secs: u64,
    /// Maximum age of completed jobs before cleanup (seconds).
    pub completed_job_retention_secs: u64,
    /// Maximum number of entries in the dead letter queue.
    pub dlq_max_size: usize,
}

impl Default for QueueConfig {
    fn default() -> Self {
        Self {
            max_concurrent: 5,
            max_concurrent_ollama: 3,
            max_concurrent_anthropic: 2,
            max_concurrent_gemini: 5,
            enabled: true,
            poll_interval_secs: 30,
            default_ttl_secs: Some(3600),            // 1 hour default
            dead_job_retention_secs: 7 * 24 * 3600,  // 7 days
            completed_job_retention_secs: 24 * 3600, // 1 day
            dlq_max_size: 10000,
        }
    }
}

/// The core queue trait — abstracts over different queue implementations.
#[async_trait]
pub trait JobQueue: Send + Sync {
    /// Submit a single job to the queue.
    /// Returns the job ID on success.
    async fn enqueue(&self, spec: JobSpec) -> Result<String, QueueError>;

    /// Submit a sequential chain of jobs.
    /// The first step is enqueued immediately; subsequent steps are parked
    /// until their predecessor completes successfully.
    /// Returns the list of job IDs in chain order.
    async fn enqueue_chain(
        &self,
        steps: &[ChainStep],
        session_id: Option<&str>,
    ) -> Result<Vec<String>, QueueError>;

    /// Cancel a job by ID.
    /// Returns true if the job was found and cancelled.
    async fn cancel(&self, job_id: &str) -> Result<bool, QueueError>;

    /// Retry a failed, dead, or cancelled job.
    /// Returns true if the job was found and requeued.
    async fn retry(&self, job_id: &str) -> Result<bool, QueueError>;

    /// Drain all queued jobs (cancel them).
    /// Returns the number of jobs cancelled.
    async fn drain(&self) -> Result<usize, QueueError>;

    /// Get a single job by ID.
    async fn get_job(&self, id: &str) -> Result<Option<Job>, QueueError>;

    /// List jobs with optional status filter.
    async fn list_jobs(
        &self,
        status: Option<JobStatus>,
        limit: usize,
    ) -> Result<Vec<Job>, QueueError>;

    /// Update the runtime configuration.
    async fn update_config(&self, config: QueueConfig) -> Result<QueueConfig, QueueError>;

    /// Get current configuration.
    async fn get_config(&self) -> Result<QueueConfig, QueueError>;

    /// Get queue statistics.
    async fn stats(&self) -> Result<QueueStats, QueueError>;

    /// Update progress for a running job.
    async fn update_progress(&self, job_id: &str, progress: JobProgress) -> Result<(), QueueError>;

    /// Get the current progress for a job.
    async fn get_progress(&self, job_id: &str) -> Result<Option<JobProgress>, QueueError>;

    /// Move expired jobs to cancelled status.
    /// Returns the number of jobs expired.
    async fn expire_jobs(&self) -> Result<usize, QueueError>;

    /// List entries in the dead letter queue.
    async fn list_dead_letter(&self, limit: usize) -> Result<Vec<DeadLetterEntry>, QueueError>;

    /// Retry a job from the dead letter queue.
    async fn retry_dead_letter(&self, job_id: &str) -> Result<bool, QueueError>;

    /// Permanently delete a dead letter entry.
    async fn delete_dead_letter(&self, job_id: &str) -> Result<bool, QueueError>;

    /// Clean up old completed and dead jobs.
    /// Returns the number of jobs deleted.
    async fn cleanup_old_jobs(&self) -> Result<usize, QueueError>;
}

/// Error type for queue operations.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct QueueError {
    pub code: QueueErrorCode,
    pub message: String,
}

impl QueueError {
    pub fn new(code: QueueErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for QueueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:?}: {}", self.code, self.message)
    }
}

impl std::error::Error for QueueError {}

/// Error codes for queue operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum QueueErrorCode {
    /// Job not found.
    NotFound,
    /// Invalid job specification.
    InvalidSpec,
    /// Job is in wrong state for operation.
    InvalidState,
    /// Queue is at capacity.
    AtCapacity,
    /// Persistence error (database unavailable, etc.).
    PersistenceError,
    /// Timeout waiting for operation.
    Timeout,
    /// Unknown error.
    Unknown,
}
