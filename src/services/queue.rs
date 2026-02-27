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
use std::time::Duration;

use tokio::sync::{Mutex, Notify, RwLock, Semaphore};
use tracing::{debug, error, info, warn};

use crate::mcp::protocol::Content;
use crate::mcp::tools::ToolHandler;
use crate::models::{AgentJob, AgentJobStatus, PrioritizedJob};
use crate::repository::Neo4jClient;

const DEFAULT_MAX_CONCURRENT: usize = 5;

/// Runtime configuration for the queue coordinator.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Maximum number of jobs executing concurrently.
    pub max_concurrent: usize,
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
            enabled: true,
            poll_interval_secs: 30,
        }
    }
}

/// Priority job queue with Neo4j-backed persistence and Tokio worker coordination.
pub struct QueueService {
    neo4j: Neo4jClient,
    tool_handler: Arc<RwLock<Option<ToolHandler>>>,
    heap: Arc<Mutex<BinaryHeap<PrioritizedJob>>>,
    notify: Arc<Notify>,
    semaphore: Arc<Semaphore>,
    /// Publicly readable for `AgentSkill::handle_set_worker_config`.
    pub config: Arc<RwLock<WorkerConfig>>,
    /// Tombstone set — jobs cancelled while still in the heap (lazy deletion).
    cancelled_ids: Arc<Mutex<HashSet<String>>>,
}

impl QueueService {
    /// Create a new queue service.  Call `recover().await` and then
    /// `spawn_coordinator()` before enqueuing jobs.
    pub fn new(neo4j: Neo4jClient, tool_handler: Arc<RwLock<Option<ToolHandler>>>) -> Self {
        Self {
            neo4j,
            tool_handler,
            heap: Arc::new(Mutex::new(BinaryHeap::new())),
            notify: Arc::new(Notify::new()),
            semaphore: Arc::new(Semaphore::new(DEFAULT_MAX_CONCURRENT)),
            config: Arc::new(RwLock::new(WorkerConfig::default())),
            cancelled_ids: Arc::new(Mutex::new(HashSet::new())),
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
            .create_agent_job(tool_name, arguments, priority, max_attempts, session_id, parent_job_id, provider_hint)
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
        for job in jobs {
            let _ = self
                .neo4j
                .update_agent_job_status(&job.id, AgentJobStatus::Cancelled)
                .await;
        }
        Ok(count)
    }

    /// Fetch a single job record from Neo4j.
    pub async fn get_job(&self, id: &str) -> Option<AgentJob> {
        self.neo4j.get_agent_job(id).await.ok().flatten()
    }

    /// Update the runtime worker configuration.  Returns the new config.
    pub async fn update_config(
        &self,
        max_concurrent: Option<usize>,
        enabled: Option<bool>,
        poll_interval_secs: Option<u64>,
    ) -> WorkerConfig {
        let mut cfg = self.config.write().await;
        if let Some(v) = max_concurrent {
            cfg.max_concurrent = v;
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
        let heap_len = self.heap.lock().await.len();
        let cfg = self.config.read().await;
        let available = self.semaphore.available_permits();
        let running = cfg.max_concurrent.saturating_sub(available);

        serde_json::json!({
            "in_memory_pending": heap_len,
            "running_now": running,
            "max_concurrent": cfg.max_concurrent,
            "enabled": cfg.enabled,
            "poll_interval_secs": cfg.poll_interval_secs,
            "by_status": db_stats,
        })
    }

    // =========================================================================
    // Coordinator
    // =========================================================================

    /// Spawn the background coordinator task.
    /// Must be called **after** `tool_handler` has been populated (i.e. after
    /// `McpServerCore::build_skills()`).
    pub fn spawn_coordinator(queue: Arc<QueueService>) {
        tokio::spawn(async move {
            queue.run_coordinator().await;
        });
    }

    async fn run_coordinator(self: Arc<Self>) {
        info!("AgentJob coordinator started");
        loop {
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

                // Try to acquire a concurrency slot (non-blocking).
                let permit = match Arc::clone(&self.semaphore).try_acquire_owned() {
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

    async fn execute_job(self: Arc<Self>, job: AgentJob) {
        info!(job_id = %job.id, tool = %job.tool_name, priority = job.priority, "Executing AgentJob");

        if let Err(e) = self.neo4j.set_job_started(&job.id).await {
            error!(job_id = %job.id, "Failed to mark job running: {}", e);
            return;
        }

        let handler_guard = self.tool_handler.read().await;
        let Some(ref handler) = *handler_guard else {
            warn!(job_id = %job.id, "No tool handler — job cannot execute");
            let _ = self.neo4j.set_job_failed(&job.id, "Tool handler not available").await;
            return;
        };

        let result = handler.execute(&job.tool_name, job.arguments.clone()).await;
        // Drop the read lock before any awaits below.
        drop(handler_guard);

        let is_error = result.is_error.unwrap_or(false);
        let result_json = serde_json::to_string(&result).unwrap_or_default();

        if !is_error {
            if let Err(e) = self.neo4j.set_job_completed(&job.id, &result_json).await {
                error!(job_id = %job.id, "Failed to store completed result: {}", e);
            } else {
                info!(job_id = %job.id, "AgentJob completed");
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
            let (attempt, max) = if let Ok(Some(updated)) = self.neo4j.get_agent_job(&job.id).await {
                (updated.attempt_count, updated.max_attempts)
            } else {
                (job.attempt_count + 1, job.max_attempts)
            };

            if attempt >= max {
                let _ = self.neo4j.set_job_dead(&job.id, &error_text).await;
                warn!(job_id = %job.id, attempts = attempt, "AgentJob exhausted retries → dead");
            } else {
                let _ = self.neo4j.set_job_failed(&job.id, &error_text).await;
                warn!(job_id = %job.id, attempt = attempt, max = max, "AgentJob failed (retryable)");
            }
        }
    }
}
