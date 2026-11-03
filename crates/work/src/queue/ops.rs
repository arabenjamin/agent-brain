//! Operator actions on the queue: cancel, retry, drain, stats, expiry, dead letters and cleanup.

use super::QueueService;
use agent_brain_models::{AgentJob, AgentJobStatus, PrioritizedJob};
use std::sync::atomic::Ordering;

impl QueueService {
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

    /// Cancel all queued jobs. Returns the number cancelled.
    ///
    /// Drains the in-memory heap *and* cancels any `queued` job in Neo4j that was never
    /// loaded into the heap (e.g. added directly to the DB) — otherwise the next periodic
    /// reload would resurrect it right after a drain.
    pub async fn drain(&self) -> Result<usize, String> {
        // 1. Drain the heap and tombstone those ids so any in-flight pop skips them.
        let jobs: Vec<AgentJob> = {
            let mut heap = self.heap.lock().await;
            heap.drain().map(|pj| pj.job).collect()
        };
        {
            let mut set = self.cancelled_ids.lock().await;
            for job in &jobs {
                set.insert(job.id.clone());
            }
        }

        // 2. Cancel every queued job in Neo4j (covers heap jobs and any queued job missing
        //    from the heap), collecting ids so their parked chain children can be cancelled.
        let now = chrono::Utc::now().to_rfc3339();
        let cypher = "MATCH (j:AgentJob {status: 'queued'}) \
                      SET j.status = 'cancelled', j.updated_at = datetime($now) \
                      RETURN collect(j.id) AS ids";
        let cancelled_ids: Vec<String> = match self
            .neo4j
            .execute(neo4rs::query(cypher).param("now", now))
            .await
        {
            Ok(rows) => rows
                .first()
                .and_then(|r| r.get::<Vec<String>>("ids").ok())
                .unwrap_or_default(),
            Err(e) => return Err(e.to_string()),
        };

        // 3. Cascade-cancel parked children of every cancelled job.
        for id in &cancelled_ids {
            let _ = self.neo4j.cancel_parked_children(id).await;
        }

        Ok(cancelled_ids.len())
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

        let avail_local = self.semaphore_local.read().await.available_permits();
        let avail_anthropic = self.semaphore_anthropic.read().await.available_permits();
        let avail_gemini = self.semaphore_gemini.read().await.available_permits();
        let running_local = cfg.max_concurrent_local.saturating_sub(avail_local);
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
            "deferred_for_gpu_lease": self.gpu_deferred.load(Ordering::Relaxed),
            "running_now": running_local + running_anthropic + running_gemini,
            "max_concurrent": cfg.max_concurrent,
            "enabled": cfg.enabled,
            "poll_interval_secs": cfg.poll_interval_secs,
            "per_provider": {
                "local": { "running": running_local, "max": cfg.max_concurrent_local },
                "anthropic": { "running": running_anthropic, "max": cfg.max_concurrent_anthropic },
                "gemini": { "running": running_gemini, "max": cfg.max_concurrent_gemini },
            },
            "by_status": db_stats,
            "provider_stats": provider_stats,
        })
    }

    /// Expire jobs that have exceeded their TTL.
    pub async fn expire_jobs(&self) -> Result<usize, String> {
        self.neo4j.expire_jobs().await.map_err(|e| e.to_string())
    }

    /// List jobs in the dead letter queue.
    pub async fn list_dead_letter(
        &self,
        limit: usize,
    ) -> Result<Vec<agent_brain_models::AgentJob>, String> {
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

    /// Clean up old completed, failed, dead, and dead-letter jobs.
    pub async fn cleanup_old_jobs(&self) -> Result<usize, String> {
        // Default: keep completed for 1 day; failed/dead for 7 days.
        let mut total = self
            .neo4j
            .cleanup_old_jobs(24 * 3600, 7 * 24 * 3600)
            .await
            .map_err(|e| e.to_string())?;
        // Dead-letter entries are kept longer (30 days) since they carry
        // failure forensics, but must not accumulate forever either.
        total += self
            .neo4j
            .cleanup_old_dead_letter(30 * 24 * 3600)
            .await
            .map_err(|e| e.to_string())?;
        Ok(total)
    }
}
