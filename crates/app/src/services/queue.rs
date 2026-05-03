//! Agent job queue — priority-ordered background task executor.
//!
//! # Design
//!
//! - **Durability**: jobs are persisted to Neo4j so they survive server restarts.
//! - **Priority**: an in-memory `BinaryHeap` orders jobs by priority (0–3) then FIFO.
//! - **Concurrency**: a `tokio::sync::Semaphore` limits concurrent executions.
//! - **Wakeup**: a `Notify` wakes the coordinator immediately when a new job arrives.
//! - **Recovery**: on startup, `recover()` resets crashed `running` jobs to `queued`
//!   and reloads all `queued` jobs into the heap.
//!
//! # Resizing concurrency at runtime
//!
//! `update_config()` stores the new `max_concurrent` value but the underlying semaphore
//! is fixed at creation time.  To change effective concurrency, set `enabled = false`,
//! recreate the service, then re-enable.  Phase-2 multi-provider support will introduce
//! per-provider semaphores with dynamic resizing.

use std::collections::{BinaryHeap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

tokio::task_local! {
    /// Set to `true` inside a background job task when `provider_hint == "ollama"`.
    ///
    /// `SharedLlm` reads this flag to route generation calls to the local Ollama
    /// endpoint instead of the active (possibly cloud) model, preventing background
    /// maintenance jobs from consuming cloud quota.
    pub static USE_LOCAL_LLM: bool;
}

use serde::Deserialize;
use tokio::sync::{Mutex, Notify, RwLock, Semaphore, broadcast};
use tracing::{debug, error, info, warn};

use crate::brain_core::BrainEvent;
use crate::mcp::tools::ToolHandler;
use crate::models::{AgentJob, AgentJobStatus, PrioritizedJob, TaskStatus};
use crate::repository::Neo4jClient;
use agent_brain_protocol::{Content, ToolCallResult};

const DEFAULT_MAX_CONCURRENT: usize = 5;
const DEFAULT_MAX_CONCURRENT_OLLAMA: usize = 2;
const DEFAULT_MAX_CONCURRENT_ANTHROPIC: usize = 2;
const DEFAULT_MAX_CONCURRENT_GEMINI: usize = 5;

/// Progress tuple: (percent, message, updated_at).
pub type JobProgressTuple = (u8, Option<String>, Option<String>);

/// Runtime configuration for the queue coordinator.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Global maximum number of jobs executing concurrently (informational).
    pub max_concurrent: usize,
    /// Concurrency limit for Ollama (local) jobs.
    pub max_concurrent_ollama: usize,
    /// Concurrency limit for Anthropic API jobs.
    pub max_concurrent_anthropic: usize,
    /// Concurrency limit for Gemini API jobs.
    pub max_concurrent_gemini: usize,
    /// When `false`, the coordinator will not pick up new jobs.
    pub enabled: bool,
    /// How often (seconds) the coordinator polls Neo4j for jobs that might have
    /// been missed (e.g. added while the heap was empty).
    pub poll_interval_secs: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            max_concurrent_ollama: DEFAULT_MAX_CONCURRENT_OLLAMA,
            max_concurrent_anthropic: DEFAULT_MAX_CONCURRENT_ANTHROPIC,
            max_concurrent_gemini: DEFAULT_MAX_CONCURRENT_GEMINI,
            enabled: true,
            poll_interval_secs: 30,
        }
    }
}

/// One step in a sequential job chain submitted via `enqueue_chain`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ChainStep {
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
    pub priority: Option<u8>,
    pub max_attempts: Option<u32>,
    pub provider_hint: Option<String>,
    pub context_profile: Option<String>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    #[serde(default)]
    pub description: Option<String>,
    /// Minimum confidence score (0.0–1.0) required to execute this chain.
    /// When set on the first step the scheduler evaluates confidence before
    /// dispatching; if the score falls below the threshold the original chain
    /// is replaced with a lightweight diagnosis chain.
    #[serde(default)]
    pub confidence_threshold: Option<f32>,
    /// When `true`, this step is treated as an evaluator: after it completes the
    /// coordinator parses a 1–5 score from the output.  If the score is below
    /// `min_score` the original task is marked failed and re-created so the
    /// scheduler will dispatch a new attempt with the critique as context.
    #[serde(default)]
    pub is_evaluator: bool,
    /// Minimum acceptable score (1–5) from an evaluator step.  Defaults to 3.5.
    #[serde(default)]
    pub min_score: Option<f32>,
    /// Task ID of the parent goal being evaluated.  Stored as `__evaluator_task_id`
    /// in the job args so the coordinator can look up the original goal on re-queue.
    #[serde(default)]
    pub evaluator_task_id: Option<String>,
}

impl ChainStep {
    pub fn new(tool_name: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            ..Default::default()
        }
    }
}

/// Priority job queue with Neo4j-backed persistence and Tokio worker coordination.
pub struct QueueService {
    neo4j: Neo4jClient,
    tool_handler: Arc<RwLock<Option<ToolHandler>>>,
    heap: Arc<Mutex<BinaryHeap<PrioritizedJob>>>,
    notify: Arc<Notify>,
    semaphore_ollama: Arc<RwLock<Arc<Semaphore>>>,
    semaphore_anthropic: Arc<RwLock<Arc<Semaphore>>>,
    semaphore_gemini: Arc<RwLock<Arc<Semaphore>>>,
    pub config: Arc<RwLock<WorkerConfig>>,
    cancelled_ids: Arc<Mutex<HashSet<String>>>,
    /// Brain event bus — emits `JobCompleted / JobFailed / JobDead` events that
    /// transport adapters (e.g. HTTP SSE) can subscribe to and forward to clients.
    event_tx: Option<broadcast::Sender<BrainEvent>>,
    /// Set to `true` by `run_coordinator` on every heartbeat tick; proves the task is alive.
    coordinator_alive: Arc<AtomicBool>,
    /// Unix timestamp (seconds) of the last coordinator heartbeat, or -1 if never set.
    coordinator_last_heartbeat: Arc<AtomicI64>,
    /// Result of the last orphan-chain audit: count of orphaned parked jobs found (and cancelled).
    last_orphan_audit_count: Arc<AtomicI64>,
}

impl QueueService {
    pub fn new(
        neo4j: Neo4jClient,
        tool_handler: Arc<RwLock<Option<ToolHandler>>>,
        event_tx: Option<broadcast::Sender<BrainEvent>>,
    ) -> Self {
        Self {
            neo4j,
            tool_handler,
            heap: Arc::new(Mutex::new(BinaryHeap::new())),
            notify: Arc::new(Notify::new()),
            semaphore_ollama: Arc::new(RwLock::new(Arc::new(Semaphore::new(
                DEFAULT_MAX_CONCURRENT_OLLAMA,
            )))),
            semaphore_anthropic: Arc::new(RwLock::new(Arc::new(Semaphore::new(
                DEFAULT_MAX_CONCURRENT_ANTHROPIC,
            )))),
            semaphore_gemini: Arc::new(RwLock::new(Arc::new(Semaphore::new(
                DEFAULT_MAX_CONCURRENT_GEMINI,
            )))),
            config: Arc::new(RwLock::new(WorkerConfig::default())),
            cancelled_ids: Arc::new(Mutex::new(HashSet::new())),
            event_tx,
            coordinator_alive: Arc::new(AtomicBool::new(false)),
            coordinator_last_heartbeat: Arc::new(AtomicI64::new(-1)),
            last_orphan_audit_count: Arc::new(AtomicI64::new(-1)),
        }
    }

    // =========================================================================
    // Startup
    // =========================================================================

    /// Reset crashed jobs and reload the heap from Neo4j.
    pub async fn recover(&self) {
        match self.neo4j.reset_running_agent_jobs().await {
            Ok(n) if n > 0 => info!(count = n, "Reset crashed AgentJobs to queued"),
            Ok(_) => {}
            Err(e) => warn!("Failed to reset running jobs: {}", e),
        }

        // Cancel parked jobs whose parent is now terminal — these accumulated during
        // crashes or explicit cancellations and can never be unparked.
        match self.neo4j.cancel_orphaned_parked_jobs().await {
            Ok(n) if n > 0 => info!(count = n, "Cancelled orphaned parked AgentJobs on recovery"),
            Ok(_) => {}
            Err(e) => warn!("Failed to cancel orphaned parked jobs on recovery: {}", e),
        }

        match self.neo4j.list_queued_agent_jobs().await {
            Ok(jobs) => {
                let mut heap = self.heap.lock().await;
                for job in jobs {
                    heap.push(PrioritizedJob {
                        priority: job.priority,
                        created_at: job.created_at.clone(),
                        job,
                    });
                }
                let n = heap.len();
                if n > 0 {
                    info!(count = n, "Reloaded queued AgentJobs into heap");
                    self.notify.notify_one();
                }
            }
            Err(e) => warn!("Failed to load queued jobs on startup: {}", e),
        }
    }

    // =========================================================================
    // Public queue API
    // =========================================================================

    /// Submit a new job.  Persists to Neo4j, pushes to in-memory heap, and
    /// notifies the coordinator.  Returns the new job ID.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue(
        &self,
        tool_name: &str,
        arguments: Option<&serde_json::Value>,
        priority: u8,
        max_attempts: u32,
        session_id: Option<&str>,
        parent_job_id: Option<&str>,
        provider_hint: Option<&str>,
    ) -> Result<String, String> {
        let id = self
            .neo4j
            .create_agent_job(
                tool_name,
                arguments,
                priority,
                max_attempts,
                session_id,
                parent_job_id,
                provider_hint,
                None,
                None,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;

        // Reload full record so the heap entry has all fields.
        if let Ok(Some(job)) = self.neo4j.get_agent_job(&id).await {
            self.heap.lock().await.push(PrioritizedJob {
                priority: job.priority,
                created_at: job.created_at.clone(),
                job,
            });
            self.notify.notify_one();
        }

        Ok(id)
    }

    /// Submit a sequential chain of jobs.
    ///
    /// The **first** step is enqueued immediately (`queued`).
    /// Steps 2..N are stored as `parked`, each with `parent_job_id` pointing to the
    /// preceding step.  When a job completes the coordinator automatically promotes
    /// its parked children to `queued`.  If a job fails or is marked dead its parked
    /// children are cancelled.
    ///
    /// Returns the list of job IDs in chain order.
    pub async fn enqueue_chain(
        &self,
        steps: &[ChainStep],
        session_id: Option<&str>,
    ) -> Result<Vec<String>, String> {
        if steps.is_empty() {
            return Err("Chain must contain at least one step".to_string());
        }

        let mut ids: Vec<String> = Vec::with_capacity(steps.len());
        let mut prev_id: Option<String> = None;

        for (i, step) in steps.iter().enumerate() {
            let priority = step.priority.unwrap_or(1);
            let max_attempts = step.max_attempts.unwrap_or(3);

            // For evaluator steps, inject metadata fields into the args JSON so
            // execute_job can parse them without needing extra AgentJob columns.
            // The tool handler ignores unknown fields via serde default behaviour.
            let effective_args: Option<serde_json::Value> = if step.is_evaluator {
                let mut a = step.arguments.clone().unwrap_or(serde_json::json!({}));
                if let serde_json::Value::Object(ref mut m) = a {
                    m.insert(
                        "__evaluator_min_score".to_string(),
                        serde_json::json!(step.min_score.unwrap_or(3.5)),
                    );
                    if let Some(tid) = &step.evaluator_task_id {
                        m.insert("__evaluator_task_id".to_string(), serde_json::json!(tid));
                    }
                }
                Some(a)
            } else {
                step.arguments.clone()
            };

            let id = if i == 0 {
                self.neo4j
                    .create_agent_job(
                        &step.tool_name,
                        effective_args.as_ref(),
                        priority,
                        max_attempts,
                        session_id,
                        None,
                        step.provider_hint.as_deref(),
                        step.context_profile.as_deref(),
                        step.description.as_deref(),
                        step.ttl_secs,
                    )
                    .await
                    .map_err(|e| e.to_string())?
            } else {
                self.neo4j
                    .create_agent_job_parked(
                        &step.tool_name,
                        effective_args.as_ref(),
                        priority,
                        max_attempts,
                        session_id,
                        prev_id.as_deref().unwrap(),
                        step.provider_hint.as_deref(),
                        step.context_profile.as_deref(),
                        step.description.as_deref(),
                        step.ttl_secs,
                    )
                    .await
                    .map_err(|e| e.to_string())?
            };

            prev_id = Some(id.clone());
            ids.push(id);
        }

        // Push the first job to the in-memory heap.
        if let Ok(Some(job)) = self.neo4j.get_agent_job(&ids[0]).await {
            self.heap.lock().await.push(PrioritizedJob {
                priority: job.priority,
                created_at: job.created_at.clone(),
                job,
            });
            self.notify.notify_one();
        }

        info!(steps = ids.len(), "Enqueued job chain");
        Ok(ids)
    }

    /// Cancel a job by ID.  Returns `true` if the job was found and cancelled.
    pub async fn cancel(&self, job_id: &str) -> Result<bool, String> {
        let job = self
            .neo4j
            .get_agent_job(job_id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(job) = job else { return Ok(false) };

        if matches!(
            job.status,
            AgentJobStatus::Completed | AgentJobStatus::Dead | AgentJobStatus::Cancelled
        ) {
            return Ok(false);
        }

        self.neo4j
            .update_agent_job_status(job_id, AgentJobStatus::Cancelled)
            .await
            .map_err(|e| e.to_string())?;

        // Cancel any parked chain children — they can never run without this parent.
        let _ = self.neo4j.cancel_parked_children(job_id).await;

        // Lazy removal from heap via tombstone.
        self.cancelled_ids.lock().await.insert(job_id.to_string());
        Ok(true)
    }

    /// Retry a failed, dead, or cancelled job.
    pub async fn retry(&self, job_id: &str) -> Result<bool, String> {
        let job = self
            .neo4j
            .get_agent_job(job_id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(job) = job else { return Ok(false) };

        if !matches!(
            job.status,
            AgentJobStatus::Failed | AgentJobStatus::Dead | AgentJobStatus::Cancelled
        ) {
            return Ok(false);
        }

        self.neo4j
            .retry_agent_job(job_id)
            .await
            .map_err(|e| e.to_string())?;

        // Remove from tombstone set if it was there.
        self.cancelled_ids.lock().await.remove(job_id);

        if let Ok(Some(refreshed)) = self.neo4j.get_agent_job(job_id).await {
            self.heap.lock().await.push(PrioritizedJob {
                priority: refreshed.priority,
                created_at: refreshed.created_at.clone(),
                job: refreshed,
            });
            self.notify.notify_one();
        }
        Ok(true)
    }

    /// Cancel all queued (in-memory) jobs.  Returns the number cancelled.
    pub async fn drain(&self) -> Result<usize, String> {
        let jobs: Vec<AgentJob> = {
            let mut heap = self.heap.lock().await;
            heap.drain().map(|pj| pj.job).collect()
        };
        let count = jobs.len();
        for job in &jobs {
            let _ = self
                .neo4j
                .update_agent_job_status(&job.id, AgentJobStatus::Cancelled)
                .await;
            // Cancel parked children of each drained job.
            let _ = self.neo4j.cancel_parked_children(&job.id).await;
        }
        Ok(count)
    }

    /// Fetch a single job record from Neo4j.
    pub async fn get_job(&self, id: &str) -> Option<AgentJob> {
        self.neo4j.get_agent_job(id).await.ok().flatten()
    }

    /// Update the runtime worker configuration.  Returns the new config.
    ///
    /// Per-provider semaphore sizes are updated by swapping in a new semaphore with
    /// the requested capacity.  Jobs already holding a permit from the old semaphore
    /// continue unaffected; new jobs pick up the replacement.
    pub async fn update_config(
        &self,
        max_concurrent: Option<usize>,
        max_concurrent_ollama: Option<usize>,
        max_concurrent_anthropic: Option<usize>,
        max_concurrent_gemini: Option<usize>,
        enabled: Option<bool>,
        poll_interval_secs: Option<u64>,
    ) -> WorkerConfig {
        let mut cfg = self.config.write().await;
        if let Some(v) = max_concurrent {
            cfg.max_concurrent = v;
        }
        if let Some(v) = max_concurrent_ollama {
            cfg.max_concurrent_ollama = v;
            *self.semaphore_ollama.write().await = Arc::new(Semaphore::new(v));
        }
        if let Some(v) = max_concurrent_anthropic {
            cfg.max_concurrent_anthropic = v;
            *self.semaphore_anthropic.write().await = Arc::new(Semaphore::new(v));
        }
        if let Some(v) = max_concurrent_gemini {
            cfg.max_concurrent_gemini = v;
            *self.semaphore_gemini.write().await = Arc::new(Semaphore::new(v));
        }
        if let Some(v) = enabled {
            cfg.enabled = v;
            if v {
                // Re-enable: wake coordinator in case there are queued jobs.
                self.notify.notify_one();
            }
        }
        if let Some(v) = poll_interval_secs {
            cfg.poll_interval_secs = v;
        }
        cfg.clone()
    }

    /// Return queue statistics (in-memory + Neo4j).
    pub async fn stats(&self) -> serde_json::Value {
        let db_stats = self
            .neo4j
            .get_queue_stats()
            .await
            .unwrap_or(serde_json::json!({}));
        let provider_stats = self
            .neo4j
            .get_provider_stats()
            .await
            .unwrap_or(serde_json::json!({}));
        let heap_len = self.heap.lock().await.len();
        let cfg = self.config.read().await;

        let avail_ollama = self.semaphore_ollama.read().await.available_permits();
        let avail_anthropic = self.semaphore_anthropic.read().await.available_permits();
        let avail_gemini = self.semaphore_gemini.read().await.available_permits();
        let running_ollama = cfg.max_concurrent_ollama.saturating_sub(avail_ollama);
        let running_anthropic = cfg.max_concurrent_anthropic.saturating_sub(avail_anthropic);
        let running_gemini = cfg.max_concurrent_gemini.saturating_sub(avail_gemini);

        let coordinator_alive = self.coordinator_alive.load(Ordering::Relaxed);
        let hb_ts = self.coordinator_last_heartbeat.load(Ordering::Relaxed);
        let coordinator_last_heartbeat = if hb_ts >= 0 {
            chrono::DateTime::from_timestamp(hb_ts, 0)
                .map(|dt: chrono::DateTime<chrono::Utc>| dt.to_rfc3339())
        } else {
            None
        };
        let orphan_audit = self.last_orphan_audit_count.load(Ordering::Relaxed);

        serde_json::json!({
            "coordinator": {
                "alive": coordinator_alive,
                "last_heartbeat": coordinator_last_heartbeat,
            },
            "orphan_audit": {
                "last_cancelled": if orphan_audit >= 0 { serde_json::json!(orphan_audit) } else { serde_json::Value::Null },
            },
            "in_memory_pending": heap_len,
            "running_now": running_ollama + running_anthropic + running_gemini,
            "max_concurrent": cfg.max_concurrent,
            "enabled": cfg.enabled,
            "poll_interval_secs": cfg.poll_interval_secs,
            "per_provider": {
                "ollama": { "running": running_ollama, "max": cfg.max_concurrent_ollama },
                "anthropic": { "running": running_anthropic, "max": cfg.max_concurrent_anthropic },
                "gemini": { "running": running_gemini, "max": cfg.max_concurrent_gemini },
            },
            "by_status": db_stats,
            "provider_stats": provider_stats,
        })
    }

    /// List agent jobs from Neo4j, optionally filtered by status.
    /// Returns up to `limit` jobs ordered by created_at DESC.
    pub async fn list_jobs(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::models::AgentJob>, crate::repository::RepositoryError> {
        self.neo4j.list_agent_jobs(status, limit).await
    }

    // =========================================================================
    // Progress tracking
    // =========================================================================

    /// Update progress for a running job.
    pub async fn update_progress(
        &self,
        job_id: &str,
        percent: u8,
        message: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        self.neo4j
            .update_job_progress(job_id, percent, message, metadata)
            .await
            .map_err(|e| e.to_string())
    }

    /// Get progress for a job.
    pub async fn get_job_progress(&self, job_id: &str) -> Result<Option<JobProgressTuple>, String> {
        self.neo4j
            .get_job_progress(job_id)
            .await
            .map_err(|e| e.to_string())
    }

    // =========================================================================
    // TTL and expiration
    // =========================================================================

    /// Expire jobs that have exceeded their TTL.
    pub async fn expire_jobs(&self) -> Result<usize, String> {
        self.neo4j.expire_jobs().await.map_err(|e| e.to_string())
    }

    // =========================================================================
    // Dead Letter Queue
    // =========================================================================

    /// List jobs in the dead letter queue.
    pub async fn list_dead_letter(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::models::AgentJob>, String> {
        self.neo4j
            .list_dead_letter(limit)
            .await
            .map_err(|e| e.to_string())
    }

    /// Retry a job from the dead letter queue.
    pub async fn retry_dead_letter(&self, job_id: &str) -> Result<bool, String> {
        self.neo4j
            .retry_dead_letter(job_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// Permanently delete a dead letter entry.
    pub async fn delete_dead_letter(&self, job_id: &str) -> Result<bool, String> {
        self.neo4j
            .delete_dead_letter(job_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// Get dead letter queue statistics.
    pub async fn get_dead_letter_stats(&self) -> Result<serde_json::Value, String> {
        self.neo4j
            .get_dead_letter_stats()
            .await
            .map_err(|e| e.to_string())
    }

    // =========================================================================
    // Cleanup
    // =========================================================================

    /// Clean up old completed and dead jobs.
    pub async fn cleanup_old_jobs(&self) -> Result<usize, String> {
        // Default: keep completed for 1 day, dead for 7 days.
        self.neo4j
            .cleanup_old_jobs(24 * 3600, 7 * 24 * 3600)
            .await
            .map_err(|e| e.to_string())
    }

    // =========================================================================
    // Coordinator
    // =========================================================================

    /// Spawn the background coordinator task.
    /// Must be called **after** `tool_handler` has been populated (i.e. after
    /// `McpServerCore::build_skills()`).
    pub fn spawn_coordinator(queue: Arc<QueueService>) {
        let queue_coordinator = Arc::clone(&queue);
        tokio::spawn(async move {
            queue_coordinator.run_coordinator().await;
        });
        // Spawn periodic TTL expiration check (every 60 seconds).
        let queue_ttl = Arc::clone(&queue);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                if let Err(e) = queue_ttl.expire_jobs().await {
                    warn!("TTL expiration check failed: {}", e);
                }
            }
        });
        // Spawn periodic cleanup (every 5 minutes).
        let queue_cleanup = Arc::clone(&queue);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                if let Err(e) = queue_cleanup.cleanup_old_jobs().await {
                    warn!("Periodic cleanup failed: {}", e);
                }
            }
        });
        // Spawn periodic orphan-chain audit (every 5 minutes).
        // Cancels any parked jobs whose parent is terminal/missing, and records the count.
        let queue_orphan = Arc::clone(&queue);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                match queue_orphan.neo4j.cancel_orphaned_parked_jobs().await {
                    Ok(n) => {
                        queue_orphan
                            .last_orphan_audit_count
                            .store(n as i64, Ordering::Relaxed);
                        if n > 0 {
                            warn!(count = n, "Orphan-chain audit cancelled stuck parked jobs");
                        }
                    }
                    Err(e) => warn!("Orphan-chain audit failed: {}", e),
                }
            }
        });
    }

    async fn run_coordinator(self: Arc<Self>) {
        info!("AgentJob coordinator started");
        self.coordinator_alive.store(true, Ordering::Relaxed);
        loop {
            self.coordinator_last_heartbeat
                .store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
            let poll_secs = self.config.read().await.poll_interval_secs;

            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(poll_secs)) => {
                    // Periodic sync: pick up any jobs added directly to Neo4j.
                    self.reload_from_neo4j().await;
                }
            }

            // Drain the heap while capacity is available.
            loop {
                if !self.config.read().await.enabled {
                    break;
                }

                let pjob = { self.heap.lock().await.pop() };
                let Some(pjob) = pjob else { break };

                // Skip tombstoned (cancelled) jobs.
                {
                    let mut set = self.cancelled_ids.lock().await;
                    if set.remove(&pjob.job.id) {
                        continue;
                    }
                }

                // Pick the semaphore based on provider_hint.
                // Read the current inner Arc so that runtime config changes
                // (semaphore swaps) take effect on the next job dispatch.
                let semaphore: Arc<Semaphore> = {
                    let lock = match pjob.job.provider_hint.as_deref() {
                        Some("anthropic") => self.semaphore_anthropic.read().await,
                        Some("gemini") => self.semaphore_gemini.read().await,
                        _ => self.semaphore_ollama.read().await,
                    };
                    Arc::clone(&*lock)
                };

                // Try to acquire a concurrency slot (non-blocking).
                let permit = match semaphore.try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        // At capacity — put the job back and wait for the next wakeup.
                        self.heap.lock().await.push(pjob);
                        break;
                    }
                };

                let svc = Arc::clone(&self);
                tokio::spawn(async move {
                    let _permit = permit; // released when this task finishes
                    svc.execute_job(pjob.job).await;
                });
            }
        }
    }

    /// Reload queued jobs from Neo4j that are not already in the heap.
    async fn reload_from_neo4j(self: &Arc<Self>) {
        match self.neo4j.list_queued_agent_jobs().await {
            Ok(jobs) if !jobs.is_empty() => {
                let mut heap = self.heap.lock().await;
                let existing: HashSet<_> = heap.iter().map(|pj| pj.job.id.clone()).collect();
                let mut added = 0usize;
                for job in jobs {
                    if !existing.contains(&job.id) {
                        heap.push(PrioritizedJob {
                            priority: job.priority,
                            created_at: job.created_at.clone(),
                            job,
                        });
                        added += 1;
                    }
                }
                if added > 0 {
                    debug!(count = added, "Reloaded missed jobs from Neo4j");
                }
            }
            Ok(_) => {}
            Err(e) => warn!("Periodic Neo4j reload failed: {}", e),
        }
    }

    // =========================================================================
    // Job execution
    // =========================================================================

    /// Emit a brain event for a job status change.
    ///
    /// Ignores send errors — having no subscribers is fine.
    fn emit_job_event(&self, event: BrainEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
    }

    /// Promote any parked children of `parent_id` to queued and push them onto the heap.
    /// `prev_result_text` is the plain-text output of the completing job; it is stamped
    /// onto each child so `{{_prev}}` can be resolved when the child executes.
    async fn unpark_and_enqueue_children(
        self: &Arc<Self>,
        parent_id: &str,
        prev_result_text: &str,
    ) {
        match self
            .neo4j
            .unpark_children(parent_id, prev_result_text)
            .await
        {
            Ok(children) if !children.is_empty() => {
                let mut heap = self.heap.lock().await;
                for child in children {
                    heap.push(PrioritizedJob {
                        priority: child.priority,
                        created_at: child.created_at.clone(),
                        job: child,
                    });
                }
                self.notify.notify_one();
            }
            Ok(_) => {}
            Err(e) => warn!(parent = %parent_id, "Failed to unpark chain children: {}", e),
        }
    }

    async fn execute_job(self: Arc<Self>, job: AgentJob) {
        info!(job_id = %job.id, tool = %job.tool_name, priority = job.priority, "Executing AgentJob");

        if let Err(e) = self.neo4j.set_job_started(&job.id).await {
            error!(job_id = %job.id, "Failed to mark job running: {}", e);
            return;
        }

        let handler_guard = self.tool_handler.read().await;
        let Some(ref handler) = *handler_guard else {
            warn!(job_id = %job.id, "No tool handler — job cannot execute");
            let _ = self
                .neo4j
                .set_job_failed(&job.id, "Tool handler not available")
                .await;
            return;
        };

        // Resolve {{_prev}} / {{result}} in arguments if the job carries a prior step result.
        // {{result}} is treated as an alias for {{_prev}} — brain-generated chains often use
        // the more natural name.
        let resolved_args = match &job.prev_result {
            Some(prev_text)
                if job.arguments.as_ref().is_some_and(|a| {
                    let s = a.to_string();
                    s.contains("{{_prev}}") || s.contains("{{result}}")
                }) =>
            {
                job.arguments
                    .as_ref()
                    .map(|a| substitute_prev(a, prev_text))
            }
            _ => job.arguments.clone(),
        };

        // Run the tool call inside a task-local scope so `SharedLlm` can detect
        // background jobs and route to the local Ollama endpoint when appropriate.
        let use_local = job.provider_hint.as_deref() == Some("ollama");
        let result = USE_LOCAL_LLM
            .scope(use_local, handler.execute(&job.tool_name, resolved_args))
            .await;
        // Drop the read lock before any awaits below.
        drop(handler_guard);

        let is_error = result.is_error.unwrap_or(false);
        let result_json = serde_json::to_string(&result).unwrap_or_default();

        // Extract plain text from the result to pass to child steps via {{_prev}}.
        let result_text = extract_result_text(&result);

        if !is_error {
            if let Err(e) = self.neo4j.set_job_completed(&job.id, &result_json).await {
                error!(job_id = %job.id, "Failed to store completed result: {}", e);
            } else {
                info!(job_id = %job.id, "AgentJob completed");

                // Evaluator step: parse score and re-queue the parent task if below threshold.
                // When score fails, cancel parked children (e.g. update_task) so the task is
                // not prematurely marked completed — the retry task created by
                // handle_evaluator_requeue will drive the next attempt.
                let evaluator_blocked = if let Some(min_score) = job
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("__evaluator_min_score"))
                    .and_then(|v| v.as_f64())
                {
                    let score = parse_evaluator_score(&result_text);
                    let task_id = job
                        .arguments
                        .as_ref()
                        .and_then(|a| a.get("__evaluator_task_id"))
                        .and_then(|v| v.as_str())
                        .map(String::from);

                    if score < min_score as f32 {
                        warn!(
                            job_id = %job.id,
                            score = score,
                            min_score = min_score,
                            "Evaluator: score below threshold — cancelling downstream steps and re-queuing task"
                        );
                        let _ = self.neo4j.cancel_parked_children(&job.id).await;
                        if let Some(tid) = &task_id {
                            self.handle_evaluator_requeue(tid, score, &result_text)
                                .await;
                        }
                        true
                    } else {
                        info!(job_id = %job.id, score = score, "Evaluator: score passed");
                        false
                    }
                } else {
                    false
                };

                // Promote any chained children waiting on this job, unless the evaluator
                // already cancelled them due to a failed score.
                if !evaluator_blocked {
                    self.unpark_and_enqueue_children(&job.id, &result_text)
                        .await;
                }
                self.emit_job_event(BrainEvent::JobCompleted {
                    job_id: job.id.clone(),
                    tool_name: job.tool_name.clone(),
                    session_id: job.session_id.clone(),
                    result_preview: Some(result_text.chars().take(200).collect()),
                });
            }
        } else {
            let error_text = result
                .content
                .first()
                .and_then(|c| {
                    if let Content::Text { text } = c {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "Unknown error".to_string());

            // Re-fetch to get the updated attempt_count (set by set_job_started).
            let (attempt, max) = if let Ok(Some(updated)) = self.neo4j.get_agent_job(&job.id).await
            {
                (updated.attempt_count, updated.max_attempts)
            } else {
                (job.attempt_count + 1, job.max_attempts)
            };

            if attempt >= max {
                let _ = self.neo4j.set_job_dead(&job.id, &error_text).await;
                warn!(job_id = %job.id, attempts = attempt, "AgentJob exhausted retries → dead");
                // Parent chain is broken — cancel any waiting children.
                let _ = self.neo4j.cancel_parked_children(&job.id).await;
                // Store a reflection note so the brain can learn from this failure.
                let reflection_content = format!(
                    "Dead job: tool '{}' (job_id: {}) exhausted {}/{} attempts and was marked dead.\n\
                     Last error: {}\n\
                     This is an automated failure record. Investigate the tool definition, \
                     its input arguments, or any external dependencies to prevent recurrence.",
                    job.tool_name, job.id, attempt, max, error_text
                );
                if let Err(e) = self
                    .neo4j
                    .store_reflection_note(&reflection_content, None)
                    .await
                {
                    warn!(job_id = %job.id, error = %e, "Failed to store dead-job reflection note");
                } else {
                    debug!(job_id = %job.id, tool = %job.tool_name, "Stored dead-job reflection note");
                }

                // Enqueue a targeted meta-learning chain for non-infrastructure tools.
                // This triggers the Analyze→Hypothesize→Test→Integrate cycle immediately
                // rather than waiting for perception_scan to accumulate 3+ failures.
                if should_meta_learn(&job.tool_name) {
                    let search_query = format!("failure {} root cause error", job.tool_name);
                    let hypothesis_question = format!(
                        "You are a meta-learning system. A job running '{}' just died after {} \
                         attempts with error: {}\n\n\
                         Based on any related notes above:\n\
                         1. ANALYZE: What is causing this failure?\n\
                         2. HYPOTHESIZE: Form a specific, testable hypothesis.\n\
                         3. TEST: Propose a concrete test to confirm/refute the hypothesis.\n\
                         4. INTEGRATE: What single change would prevent this failure from recurring?",
                        job.tool_name, attempt, error_text
                    );
                    let meta_steps = vec![
                        crate::services::queue::ChainStep {
                            tool_name: "search_notes".to_string(),
                            arguments: Some(serde_json::json!({
                                "query": search_query,
                                "limit": 6
                            })),
                            priority: Some(0),
                            max_attempts: Some(2),
                            provider_hint: Some("ollama".to_string()),
                            description: Some(format!(
                                "Meta-learn: gather evidence for '{}' failure",
                                job.tool_name
                            )),
                            ..Default::default()
                        },
                        crate::services::queue::ChainStep {
                            tool_name: "reason".to_string(),
                            arguments: Some(serde_json::json!({
                                "question": hypothesis_question,
                                "store_inference": true
                            })),
                            priority: Some(0),
                            max_attempts: Some(2),
                            provider_hint: Some("ollama".to_string()),
                            description: Some(format!(
                                "Meta-learn: hypothesize root cause for '{}'",
                                job.tool_name
                            )),
                            ..Default::default()
                        },
                        crate::services::queue::ChainStep {
                            tool_name: "store_note".to_string(),
                            arguments: Some(serde_json::json!({
                                "content": "{{_prev}}",
                                "note_type": "meta_learning_result",
                                "source_context": format!("dead_job:{}", job.tool_name),
                                "provenance": "synthesis_inference"
                            })),
                            priority: Some(0),
                            max_attempts: Some(2),
                            provider_hint: Some("ollama".to_string()),
                            description: Some(format!(
                                "Meta-learn: store result for '{}'",
                                job.tool_name
                            )),
                            ..Default::default()
                        },
                    ];
                    if let Err(e) = self
                        .enqueue_chain(&meta_steps, job.session_id.as_deref())
                        .await
                    {
                        warn!(job_id = %job.id, error = %e, "Failed to enqueue meta-learning chain for dead job");
                    } else {
                        info!(job_id = %job.id, tool = %job.tool_name, "Enqueued meta-learning chain for dead job");
                    }
                }

                self.emit_job_event(BrainEvent::JobDead {
                    job_id: job.id.clone(),
                    tool_name: job.tool_name.clone(),
                    session_id: job.session_id.clone(),
                    error: error_text.clone(),
                });
            } else {
                // Re-queue for automatic retry: set status back to 'queued' so the
                // coordinator picks it up again.  Children remain parked and will be
                // unparked when the retry eventually succeeds.
                let _ = self.neo4j.requeue_for_retry(&job.id, &error_text).await;
                warn!(job_id = %job.id, attempt = attempt, max = max, "AgentJob failed — re-queued for retry");
                self.notify.notify_one(); // wake coordinator immediately
                self.emit_job_event(BrainEvent::JobFailed {
                    job_id: job.id.clone(),
                    tool_name: job.tool_name.clone(),
                    session_id: job.session_id.clone(),
                    error: error_text.clone(),
                });
            }
        }
    }

    // =========================================================================
    // Evaluator re-queue
    // =========================================================================

    /// Called when an evaluator step scores a task below its threshold.
    ///
    /// Marks the original task `failed` and creates a new `Task` node with the
    /// critique injected as context so the scheduler will re-dispatch it on the
    /// next tick.
    async fn handle_evaluator_requeue(&self, task_id: &str, score: f32, critique: &str) {
        let task = match self.neo4j.get_task(task_id).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                warn!(task_id = %task_id, "Evaluator re-queue: task not found");
                return;
            }
            Err(e) => {
                warn!(task_id = %task_id, error = %e, "Evaluator re-queue: failed to fetch task");
                return;
            }
        };

        // Mark the original task failed.
        let _ = self
            .neo4j
            .update_task_status(task_id, TaskStatus::Failed)
            .await;

        let retry_context = format!(
            "RETRY — previous attempt scored {:.1}/5.\n\nEvaluator critique:\n{}\n\nOriginal context: {}",
            score,
            critique.chars().take(800).collect::<String>(),
            task.context.as_deref().unwrap_or("none"),
        );

        match self
            .neo4j
            .create_task(
                &task.goal,
                Some(&retry_context),
                task.success_criteria.as_deref(),
            )
            .await
        {
            Ok(new_id) => info!(
                original_task_id = %task_id,
                new_task_id = %new_id,
                score,
                "Evaluator: created retry task"
            ),
            Err(e) => warn!(error = %e, "Evaluator: failed to create retry task"),
        }
    }
}

// ---------------------------------------------------------------------------
// Evaluator helpers
// ---------------------------------------------------------------------------

/// Parse a 1–5 score from the output of a `reflect_on_work` evaluator step.
///
/// Looks for an explicit `Score: N/5` line first; falls back to verdict keywords
/// ("FULLY MET" → 5, "PARTIALLY MET" → 3, "NOT MET" → 1).
fn parse_evaluator_score(text: &str) -> f32 {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Score:")
            && let Some(n_str) = rest.trim().split('/').next()
            && let Ok(n) = n_str.trim().parse::<f32>()
        {
            return n.clamp(1.0, 5.0);
        }
    }
    let lower = text.to_lowercase();
    if lower.contains("fully met") {
        5.0
    } else if lower.contains("partially met") {
        3.0
    } else if lower.contains("not met") {
        1.0
    } else {
        3.0
    }
}

// ---------------------------------------------------------------------------
// Meta-learning helpers
// ---------------------------------------------------------------------------

/// Returns `true` if exhausted retries for `tool_name` should trigger a
/// meta-learning chain rather than only a reflection note.
///
/// Maintenance and infrastructure tools are excluded to prevent the meta-
/// learning loop from generating endless self-referential failures.
fn should_meta_learn(tool_name: &str) -> bool {
    const EXCLUDED: &[&str] = &[
        "store_note",
        "update_task",
        "consolidate_memories",
        "synthesize_knowledge",
        "reason",
        "reflect_on_work",
        "prune_old_notes",
        "review_due_notes",
        "record_outcome",
        "get_task",
        "list_tasks",
    ];
    !EXCLUDED.contains(&tool_name)
}

// ---------------------------------------------------------------------------
// {{_prev}} template substitution helpers
// ---------------------------------------------------------------------------

/// Extract the plain-text content from a ToolCallResult for use as {{_prev}}.
/// Tries `content[0].text` first (standard ToolCallResult shape). When the text is
/// a JSON object with an `"answer"` field (the standard `reason` tool output shape),
/// returns just the answer string so downstream `store_note` steps receive clean
/// markdown instead of a JSON wrapper. Falls back to the full serialised result so
/// `{{_prev}}` is never left unreplaced when a tool returns structured data without
/// a human-readable answer field (e.g. `duckdb_query`, `search_web`).
fn extract_result_text(result: &ToolCallResult) -> String {
    let text = result
        .content
        .first()
        .and_then(|c| {
            if let Content::Text { text } = c {
                Some(text.as_str())
            } else {
                None
            }
        })
        .unwrap_or("");

    if !text.is_empty() {
        // When the text is a JSON object with an "answer" key (reason tool output),
        // return just the answer so {{_prev}} in store_note steps gets clean markdown.
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text)
            && let Some(answer) = parsed.get("answer").and_then(|a| a.as_str())
            && !answer.is_empty()
        {
            return answer.to_string();
        }
        text.to_string()
    } else {
        // Fallback: serialise the whole result so downstream steps always receive data.
        serde_json::to_string(result).unwrap_or_default()
    }
}

/// Recursively replace `{{_prev}}` and its alias `{{result}}` in all string values of a JSON
/// Value tree.  Operates at the Value level so there is no risk of JSON injection.
fn substitute_prev(val: &serde_json::Value, prev_text: &str) -> serde_json::Value {
    match val {
        serde_json::Value::String(s) => serde_json::Value::String(
            s.replace("{{_prev}}", prev_text)
                .replace("{{result}}", prev_text),
        ),
        serde_json::Value::Object(obj) => serde_json::Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), substitute_prev(v, prev_text)))
                .collect(),
        ),
        serde_json::Value::Array(arr) => {
            serde_json::Value::Array(arr.iter().map(|v| substitute_prev(v, prev_text)).collect())
        }
        other => other.clone(),
    }
}
