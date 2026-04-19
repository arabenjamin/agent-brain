use chrono::Utc;
use neo4rs::{Node, query};
use tracing::info;
use uuid::Uuid;

use crate::{Neo4jClient, RepositoryError};
use agent_brain_models::{AgentJob, AgentJobStatus};

impl Neo4jClient {
    /// Create a new AgentJob node in Neo4j and return its ID.
    #[allow(clippy::too_many_arguments)]
    pub async fn create_agent_job(
        &self,
        tool_name: &str,
        arguments: Option<&serde_json::Value>,
        priority: u8,
        max_attempts: u32,
        session_id: Option<&str>,
        parent_job_id: Option<&str>,
        provider_hint: Option<&str>,
        context_profile: Option<&str>,
        description: Option<&str>,
        ttl_secs: Option<u64>,
    ) -> Result<String, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let args_json = arguments.map(|a| a.to_string()).unwrap_or_default();

        // Calculate expiration time if TTL is provided
        let expires_at = ttl_secs
            .map(|secs| (chrono::Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339());

        let q = query(
            "CREATE (j:AgentJob {
                id: $id,
                tool_name: $tool_name,
                args_json: $args_json,
                priority: $priority,
                status: 'queued',
                created_at: $now,
                updated_at: $now,
                attempt_count: 0,
                max_attempts: $max_attempts,
                session_id: $session_id,
                parent_job_id: $parent_job_id,
                provider_hint: $provider_hint,
                context_profile: $context_profile,
                description: $description,
                ttl_secs: $ttl_secs,
                expires_at: $expires_at,
                progress_percent: null,
                progress_message: null,
                duration_ms: null
            })",
        )
        .param("id", id.clone())
        .param("tool_name", tool_name)
        .param("args_json", args_json)
        .param("priority", priority as i64)
        .param("now", now.clone())
        .param("max_attempts", max_attempts as i64)
        .param("session_id", session_id.unwrap_or(""))
        .param("parent_job_id", parent_job_id.unwrap_or(""))
        .param("provider_hint", provider_hint.unwrap_or(""))
        .param("context_profile", context_profile.unwrap_or(""))
        .param("description", description.unwrap_or(""))
        .param("ttl_secs", ttl_secs.map(|v| v as i64).unwrap_or(0i64))
        .param("expires_at", expires_at.unwrap_or_default());

        self.run(q).await?;
        info!(id = %id, tool = %tool_name, "Created AgentJob");
        Ok(id)
    }

    /// Fetch a single AgentJob by ID.
    pub async fn get_agent_job(&self, id: &str) -> Result<Option<AgentJob>, RepositoryError> {
        let q = query("MATCH (j:AgentJob {id: $id}) RETURN j").param("id", id);
        let rows = self.execute(q).await?;
        if let Some(row) = rows.into_iter().next() {
            let node: Node = row
                .get("j")
                .map_err(|e| RepositoryError::InvalidData(e.to_string()))?;
            Ok(Some(node_to_agent_job(&node)))
        } else {
            Ok(None)
        }
    }

    /// List all queued (and parked) jobs ordered by priority desc then created_at asc.
    /// Used at startup to reload the in-memory heap.
    pub async fn list_queued_agent_jobs(&self) -> Result<Vec<AgentJob>, RepositoryError> {
        // Only load explicitly queued jobs — parked jobs must not run until their
        // parent explicitly unparks them via unpark_children().  Including 'parked'
        // here caused children to execute before their parent completed, breaking
        // the chain ordering and making {{_prev}} substitution impossible.
        let q = query(
            "MATCH (j:AgentJob) WHERE j.status = 'queued' \
             RETURN j ORDER BY j.priority DESC, j.created_at ASC",
        );
        let rows = self.execute(q).await?;
        rows.into_iter()
            .map(|row| {
                let node: Node = row
                    .get("j")
                    .map_err(|e| RepositoryError::InvalidData(e.to_string()))?;
                Ok(node_to_agent_job(&node))
            })
            .collect()
    }

    /// List jobs with optional status filter and limit.
    pub async fn list_agent_jobs(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<AgentJob>, RepositoryError> {
        let rows = if let Some(s) = status {
            let q = query(
                "MATCH (j:AgentJob {status: $status}) RETURN j \
                 ORDER BY j.created_at DESC LIMIT $limit",
            )
            .param("status", s)
            .param("limit", limit as i64);
            self.execute(q).await?
        } else {
            let q = query("MATCH (j:AgentJob) RETURN j ORDER BY j.created_at DESC LIMIT $limit")
                .param("limit", limit as i64);
            self.execute(q).await?
        };

        rows.into_iter()
            .map(|row| {
                let node: Node = row
                    .get("j")
                    .map_err(|e| RepositoryError::InvalidData(e.to_string()))?;
                Ok(node_to_agent_job(&node))
            })
            .collect()
    }

    /// Reset any jobs that were `running` when the process died back to `queued`.
    /// Returns the number of jobs reset.
    pub async fn reset_running_agent_jobs(&self) -> Result<usize, RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {status: 'running'}) \
             SET j.status = 'queued', j.updated_at = $now \
             RETURN count(j) AS n",
        )
        .param("now", now);
        let rows = self.execute(q).await?;
        let count = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0) as usize;
        Ok(count)
    }

    /// Update a job's status to a new value.
    pub async fn update_agent_job_status(
        &self,
        id: &str,
        status: AgentJobStatus,
    ) -> Result<(), RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query("MATCH (j:AgentJob {id: $id}) SET j.status = $status, j.updated_at = $now")
            .param("id", id)
            .param("status", status.to_string())
            .param("now", now);
        self.run(q).await
    }

    /// Mark a job as running and increment attempt_count.
    pub async fn set_job_started(&self, id: &str) -> Result<(), RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {id: $id}) \
             SET j.status = 'running', \
                 j.started_at = $now, \
                 j.updated_at = $now, \
                 j.attempt_count = j.attempt_count + 1",
        )
        .param("id", id)
        .param("now", now);
        self.run(q).await
    }

    /// Mark a job as completed and store the result JSON.
    pub async fn set_job_completed(
        &self,
        id: &str,
        result_json: &str,
    ) -> Result<(), RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {id: $id}) \
             SET j.status = 'completed', \
                 j.completed_at = $now, \
                 j.updated_at = $now, \
                 j.result_json = $result, \
                 j.duration_ms = duration.between(datetime(j.started_at), datetime($now)).milliseconds",
        )
        .param("id", id)
        .param("now", now)
        .param("result", result_json);
        self.run(q).await
    }

    /// Re-queue a failed job for automatic retry (attempt_count already incremented).
    /// Sets status back to 'queued' so the coordinator picks it up again.
    pub async fn requeue_for_retry(&self, id: &str, error: &str) -> Result<(), RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {id: $id}) \
             SET j.status = 'queued', \
                 j.updated_at = $now, \
                 j.error = $error",
        )
        .param("id", id)
        .param("now", now)
        .param("error", error);
        self.run(q).await
    }

    /// Mark a job as failed (can still be retried manually).
    pub async fn set_job_failed(&self, id: &str, error: &str) -> Result<(), RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {id: $id}) \
             SET j.status = 'failed', \
                 j.completed_at = $now, \
                 j.updated_at = $now, \
                 j.error = $error",
        )
        .param("id", id)
        .param("now", now)
        .param("error", error);
        self.run(q).await
    }

    /// Mark a job as dead (exhausted all retries) and move to dead letter queue.
    pub async fn set_job_dead(&self, id: &str, last_error: &str) -> Result<(), RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {id: $id}) \
             SET j.status = 'dead_letter', \
                 j.completed_at = $now, \
                 j.updated_at = $now, \
                 j.error = $error, \
                 j.dead_lettered_at = $now, \
                 j.dead_letter_reason = $error",
        )
        .param("id", id)
        .param("now", now)
        .param("error", last_error);
        self.run(q).await
    }

    /// Reset a failed/dead/cancelled job back to queued so it can be retried.
    pub async fn retry_agent_job(&self, id: &str) -> Result<(), RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {id: $id}) \
             SET j.status = 'queued', \
                 j.updated_at = $now, \
                 j.error = null, \
                 j.attempt_count = 0",
        )
        .param("id", id)
        .param("now", now);
        self.run(q).await
    }

    /// Create a new AgentJob in Neo4j with status `parked` (waiting for parent to complete).
    #[allow(clippy::too_many_arguments)]
    pub async fn create_agent_job_parked(
        &self,
        tool_name: &str,
        arguments: Option<&serde_json::Value>,
        priority: u8,
        max_attempts: u32,
        session_id: Option<&str>,
        parent_job_id: &str,
        provider_hint: Option<&str>,
        context_profile: Option<&str>,
        description: Option<&str>,
        ttl_secs: Option<u64>,
    ) -> Result<String, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let args_json = arguments.map(|a| a.to_string()).unwrap_or_default();

        // Calculate expiration time if TTL is provided
        let expires_at = ttl_secs
            .map(|secs| (chrono::Utc::now() + chrono::Duration::seconds(secs as i64)).to_rfc3339());

        let q = query(
            "CREATE (j:AgentJob {
                id: $id,
                tool_name: $tool_name,
                args_json: $args_json,
                priority: $priority,
                status: 'parked',
                created_at: $now,
                updated_at: $now,
                attempt_count: 0,
                max_attempts: $max_attempts,
                session_id: $session_id,
                parent_job_id: $parent_job_id,
                provider_hint: $provider_hint,
                context_profile: $context_profile,
                description: $description,
                ttl_secs: $ttl_secs,
                expires_at: $expires_at,
                progress_percent: null,
                progress_message: null,
                duration_ms: null
            })",
        )
        .param("id", id.clone())
        .param("tool_name", tool_name)
        .param("args_json", args_json)
        .param("priority", priority as i64)
        .param("now", now.clone())
        .param("max_attempts", max_attempts as i64)
        .param("session_id", session_id.unwrap_or(""))
        .param("parent_job_id", parent_job_id)
        .param("provider_hint", provider_hint.unwrap_or(""))
        .param("context_profile", context_profile.unwrap_or(""))
        .param("description", description.unwrap_or(""))
        .param("ttl_secs", ttl_secs.map(|v| v as i64).unwrap_or(0i64))
        .param("expires_at", expires_at.unwrap_or_default());

        self.run(q).await?;
        info!(id = %id, tool = %tool_name, parent = %parent_job_id, "Created parked AgentJob");
        Ok(id)
    }

    /// Promote all parked children of a completed job to `queued`.
    /// Stamps `parent_result_text` onto each child as `prev_result_json` so the
    /// coordinator can substitute `{{_prev}}` in the child's arguments at execution time.
    /// Returns the newly-queued jobs so the coordinator can push them onto the heap.
    pub async fn unpark_children(
        &self,
        parent_job_id: &str,
        parent_result_text: &str,
    ) -> Result<Vec<AgentJob>, RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {parent_job_id: $parent_id, status: 'parked'})
             SET j.status = 'queued', j.updated_at = $now, j.prev_result_json = $prev
             RETURN j",
        )
        .param("parent_id", parent_job_id)
        .param("now", now)
        .param("prev", parent_result_text);

        let rows = self.execute(q).await?;
        let jobs = rows
            .into_iter()
            .map(|row| {
                let node: Node = row
                    .get("j")
                    .map_err(|e| RepositoryError::InvalidData(e.to_string()))?;
                Ok(node_to_agent_job(&node))
            })
            .collect::<Result<Vec<_>, RepositoryError>>()?;

        if !jobs.is_empty() {
            info!(count = jobs.len(), parent = %parent_job_id, "Unparked chained jobs");
        }
        Ok(jobs)
    }

    /// Cancel all parked jobs whose parent is in a terminal state or no longer exists.
    ///
    /// Called at startup recovery and after bulk drain to clear orphaned chain steps
    /// that accumulated due to crashes or explicit cancellations.
    /// Returns the number of jobs cancelled.
    pub async fn cancel_orphaned_parked_jobs(&self) -> Result<usize, RepositoryError> {
        let now = Utc::now().to_rfc3339();
        // Two cases: parent exists but is terminal, or parent was deleted entirely.
        let q = query(
            "MATCH (child:AgentJob {status: 'parked'}) \
             WHERE child.parent_job_id IS NOT NULL AND child.parent_job_id <> '' \
             OPTIONAL MATCH (parent:AgentJob {id: child.parent_job_id}) \
             WITH child, parent \
             WHERE parent IS NULL \
                OR parent.status IN ['cancelled', 'dead', 'dead_letter'] \
             SET child.status = 'cancelled', child.updated_at = $now \
             RETURN count(child) AS n",
        )
        .param("now", now);

        let rows = self.execute(q).await?;
        let count = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0) as usize;

        if count > 0 {
            info!(count, "Cancelled orphaned parked jobs");
        }
        Ok(count)
    }

    /// Cancel all parked children of a failed/dead job.
    /// Returns the number of jobs cancelled.
    pub async fn cancel_parked_children(
        &self,
        parent_job_id: &str,
    ) -> Result<usize, RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {parent_job_id: $parent_id, status: 'parked'})
             SET j.status = 'cancelled', j.updated_at = $now
             RETURN count(j) AS n",
        )
        .param("parent_id", parent_job_id)
        .param("now", now);

        let rows = self.execute(q).await?;
        let count = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0) as usize;

        if count > 0 {
            info!(count, parent = %parent_job_id, "Cancelled parked chain jobs (parent failed)");
        }
        Ok(count)
    }

    /// Return a per-status count map plus a total.
    pub async fn get_queue_stats(&self) -> Result<serde_json::Value, RepositoryError> {
        let q = query("MATCH (j:AgentJob) RETURN j.status AS status, count(j) AS n");
        let rows = self.execute(q).await?;
        let mut map = serde_json::Map::new();
        let mut total: i64 = 0;
        for row in &rows {
            let status = row.get::<String>("status").unwrap_or_default();
            let n = row.get::<i64>("n").unwrap_or(0);
            map.insert(status, serde_json::json!(n));
            total += n;
        }
        map.insert("total".to_string(), serde_json::json!(total));
        Ok(serde_json::Value::Object(map))
    }

    // =========================================================================
    // Progress tracking
    // =========================================================================

    /// Update progress for a running job.
    pub async fn update_job_progress(
        &self,
        id: &str,
        percent: u8,
        message: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> Result<(), RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let percent_i64 = percent.min(100) as i64;
        let metadata_str = metadata.map(|v| v.to_string()).unwrap_or_default();

        let q = query(
            "MATCH (j:AgentJob {id: $id}) \
             SET j.progress_percent = $percent, \
                 j.progress_message = $message, \
                 j.progress_metadata = $metadata, \
                 j.progress_updated_at = $now, \
                 j.updated_at = $now",
        )
        .param("id", id)
        .param("percent", percent_i64)
        .param("message", message.unwrap_or(""))
        .param("metadata", metadata_str)
        .param("now", now);

        self.run(q).await
    }

    /// Get the current progress for a job.
    pub async fn get_job_progress(
        &self,
        id: &str,
    ) -> Result<Option<(u8, Option<String>, Option<String>)>, RepositoryError> {
        let q = query(
            "MATCH (j:AgentJob {id: $id}) \
             RETURN j.progress_percent AS percent, \
                    j.progress_message AS message, \
                    j.progress_updated_at AS updated_at",
        )
        .param("id", id);

        let rows = self.execute(q).await?;
        if let Some(row) = rows.into_iter().next() {
            let percent: Option<i64> = row.get("percent").unwrap_or(None);
            let message: Option<String> = row.get("message").unwrap_or(None);
            let updated_at: Option<String> = row.get("updated_at").unwrap_or(None);

            let msg = if message.as_deref().unwrap_or("").is_empty() {
                None
            } else {
                message
            };

            let pct = percent.map(|v| v as u8);

            if let Some(p) = pct {
                Ok(Some((p, msg, updated_at)))
            } else {
                Ok(None)
            }
        } else {
            Ok(None)
        }
    }

    // =========================================================================
    // TTL and expiration
    // =========================================================================

    /// Find and cancel expired jobs. Returns the number of jobs expired.
    pub async fn expire_jobs(&self) -> Result<usize, RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob) \
             WHERE j.expires_at IS NOT NULL \
               AND j.expires_at <> '' \
               AND datetime(j.expires_at) <= datetime($now) \
               AND j.status IN ['queued', 'running', 'parked'] \
             SET j.status = 'cancelled', \
                 j.updated_at = $now, \
                 j.error = 'Job expired: TTL reached' \
             RETURN count(j) AS n",
        )
        .param("now", now);

        let rows = self.execute(q).await?;
        let count = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0) as usize;

        if count > 0 {
            info!(count, "Expired jobs due to TTL");
        }
        Ok(count)
    }

    // =========================================================================
    // Dead Letter Queue operations
    // =========================================================================

    /// Move a job to the dead letter queue.
    pub async fn move_to_dead_letter(&self, id: &str, reason: &str) -> Result<(), RepositoryError> {
        let now = Utc::now().to_rfc3339();

        // First, mark the job as dead_letter status
        let q = query(
            "MATCH (j:AgentJob {id: $id}) \
             SET j.status = 'dead_letter', \
                 j.dead_lettered_at = $now, \
                 j.dead_letter_reason = $reason, \
                 j.updated_at = $now",
        )
        .param("id", id)
        .param("now", now.clone())
        .param("reason", reason);

        self.run(q).await
    }

    /// List jobs in the dead letter queue.
    pub async fn list_dead_letter(&self, limit: usize) -> Result<Vec<AgentJob>, RepositoryError> {
        let q = query(
            "MATCH (j:AgentJob {status: 'dead_letter'}) \
             RETURN j \
             ORDER BY j.dead_lettered_at DESC \
             LIMIT $limit",
        )
        .param("limit", limit as i64);

        let rows = self.execute(q).await?;
        rows.into_iter()
            .map(|row| {
                let node: Node = row
                    .get("j")
                    .map_err(|e| RepositoryError::InvalidData(e.to_string()))?;
                Ok(node_to_agent_job(&node))
            })
            .collect()
    }

    /// Retry a job from the dead letter queue.
    pub async fn retry_dead_letter(&self, id: &str) -> Result<bool, RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {id: $id, status: 'dead_letter'}) \
             SET j.status = 'queued', \
                 j.updated_at = $now, \
                 j.error = null, \
                 j.attempt_count = 0, \
                 j.dead_lettered_at = null, \
                 j.dead_letter_reason = null \
             RETURN count(j) AS n",
        )
        .param("id", id)
        .param("now", now);

        let rows = self.execute(q).await?;
        let count = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0);

        Ok(count > 0)
    }

    /// Permanently delete a dead letter entry.
    pub async fn delete_dead_letter(&self, id: &str) -> Result<bool, RepositoryError> {
        let q = query(
            "MATCH (j:AgentJob {id: $id, status: 'dead_letter'}) \
             DELETE j \
             RETURN count(j) AS n",
        )
        .param("id", id);

        let rows = self.execute(q).await?;
        let count = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0);

        Ok(count > 0)
    }

    /// Get dead letter queue statistics.
    pub async fn get_dead_letter_stats(&self) -> Result<serde_json::Value, RepositoryError> {
        let q = query(
            "MATCH (j:AgentJob {status: 'dead_letter'}) \
             RETURN j.dead_letter_reason AS reason, count(j) AS n",
        );

        let rows = self.execute(q).await?;
        let mut map = serde_json::Map::new();
        let mut total: i64 = 0;
        for row in &rows {
            let reason = row
                .get::<String>("reason")
                .unwrap_or_else(|_| "unknown".to_string());
            let n = row.get::<i64>("n").unwrap_or(0);
            map.insert(reason, serde_json::json!(n));
            total += n;
        }
        map.insert("total".to_string(), serde_json::json!(total));
        Ok(serde_json::Value::Object(map))
    }

    // =========================================================================
    // Cleanup operations
    // =========================================================================

    /// Clean up old completed and dead jobs.
    /// Returns the number of jobs deleted.
    pub async fn cleanup_old_jobs(
        &self,
        completed_retention_secs: u64,
        dead_retention_secs: u64,
    ) -> Result<usize, RepositoryError> {
        // Delete old completed jobs
        let completed_cutoff = (chrono::Utc::now()
            - chrono::Duration::seconds(completed_retention_secs as i64))
        .to_rfc3339();
        let q_completed = query(
            "MATCH (j:AgentJob {status: 'completed'}) \
             WHERE datetime(j.completed_at) <= datetime($cutoff) \
             DELETE j \
             RETURN count(j) AS n",
        )
        .param("cutoff", completed_cutoff.clone());

        let rows = self.execute(q_completed).await?;
        let completed_count = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0);

        // Delete old dead jobs
        let dead_cutoff = (chrono::Utc::now()
            - chrono::Duration::seconds(dead_retention_secs as i64))
        .to_rfc3339();
        let q_dead = query(
            "MATCH (j:AgentJob {status: 'dead'}) \
             WHERE datetime(j.completed_at) <= datetime($cutoff) \
             DELETE j \
             RETURN count(j) AS n",
        )
        .param("cutoff", dead_cutoff);

        let rows = self.execute(q_dead).await?;
        let dead_count = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0);

        // Delete old cancelled jobs (same retention window as completed).
        let q_cancelled = query(
            "MATCH (j:AgentJob {status: 'cancelled'}) \
             WHERE datetime(j.updated_at) <= datetime($cutoff) \
             DELETE j \
             RETURN count(j) AS n",
        )
        .param("cutoff", completed_cutoff);

        let rows = self.execute(q_cancelled).await?;
        let cancelled_count = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0);

        let total = (completed_count + dead_count + cancelled_count) as usize;
        if total > 0 {
            info!(
                completed = completed_count,
                dead = dead_count,
                cancelled = cancelled_count,
                total,
                "Cleaned up old jobs"
            );
        }
        Ok(total)
    }

    /// Clean up old dead letter entries (acknowledged and old).
    pub async fn cleanup_old_dead_letter(
        &self,
        retention_secs: u64,
    ) -> Result<usize, RepositoryError> {
        let cutoff =
            (chrono::Utc::now() - chrono::Duration::seconds(retention_secs as i64)).to_rfc3339();

        let q = query(
            "MATCH (j:AgentJob {status: 'dead_letter'}) \
             WHERE datetime(j.dead_lettered_at) <= datetime($cutoff) \
             DELETE j \
             RETURN count(j) AS n",
        )
        .param("cutoff", cutoff);

        let rows = self.execute(q).await?;
        let count = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0) as usize;

        if count > 0 {
            info!(count, "Cleaned up old dead letter entries");
        }
        Ok(count)
    }

    /// Get provider execution statistics.
    pub async fn get_provider_stats(&self) -> Result<serde_json::Value, RepositoryError> {
        let q = query(
            "MATCH (j:AgentJob) \
             WHERE j.status = 'completed' AND j.duration_ms IS NOT NULL \
             RETURN j.provider_hint AS provider, \
                    count(j) AS total, \
                    avg(j.duration_ms) AS avg_duration, \
                    min(j.duration_ms) AS min_duration, \
                    max(j.duration_ms) AS max_duration",
        );

        let rows = self.execute(q).await?;
        let mut providers = serde_json::Map::new();
        for row in &rows {
            let provider: String = row.get("provider").unwrap_or_default();
            if !provider.is_empty() {
                let total: i64 = row.get("total").unwrap_or(0);
                let avg: f64 = row.get("avg_duration").unwrap_or(0.0);
                let min: i64 = row.get("min_duration").unwrap_or(0);
                let max: i64 = row.get("max_duration").unwrap_or(0);

                let stats = serde_json::json!({
                    "total_completed": total,
                    "avg_duration_ms": avg,
                    "min_duration_ms": min,
                    "max_duration_ms": max,
                });
                providers.insert(provider, stats);
            }
        }
        Ok(serde_json::Value::Object(providers))
    }
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn node_to_agent_job(node: &Node) -> AgentJob {
    let id: String = node.get("id").unwrap_or_default();
    let tool_name: String = node.get("tool_name").unwrap_or_default();
    let args_json: String = node.get("args_json").unwrap_or_default();
    let arguments = if args_json.is_empty() {
        None
    } else {
        serde_json::from_str(&args_json).ok()
    };
    let priority: i64 = node.get("priority").unwrap_or(1);
    let status_str: String = node.get("status").unwrap_or("queued".to_string());
    let status = status_str
        .parse::<AgentJobStatus>()
        .unwrap_or(AgentJobStatus::Queued);
    let created_at: String = node.get("created_at").unwrap_or_default();
    let updated_at: String = node.get("updated_at").unwrap_or_default();
    let started_at: Option<String> = node.get("started_at").unwrap_or(None);
    let completed_at: Option<String> = node.get("completed_at").unwrap_or(None);
    let result_json: String = node.get("result_json").unwrap_or_default();
    let result = if result_json.is_empty() {
        None
    } else {
        serde_json::from_str(&result_json).ok()
    };
    let error: Option<String> = node.get("error").unwrap_or(None);
    let attempt_count: i64 = node.get("attempt_count").unwrap_or(0);
    let max_attempts: i64 = node.get("max_attempts").unwrap_or(3);
    let session_id: String = node.get("session_id").unwrap_or_default();
    let parent_job_id: String = node.get("parent_job_id").unwrap_or_default();
    let provider_hint: String = node.get("provider_hint").unwrap_or_default();
    let context_profile: String = node.get("context_profile").unwrap_or_default();
    let prev_result: String = node.get("prev_result_json").unwrap_or_default();

    // Progress tracking fields
    let progress_percent: Option<i64> = node.get("progress_percent").unwrap_or(None);
    let progress_message: Option<String> = node.get("progress_message").unwrap_or(None);
    let progress_metadata: String = node.get("progress_metadata").unwrap_or_default();
    let progress_updated_at: Option<String> = node.get("progress_updated_at").unwrap_or(None);

    // TTL and expiration
    let expires_at: Option<String> = node.get("expires_at").unwrap_or(None);
    let ttl_secs: i64 = node.get("ttl_secs").unwrap_or(0);

    // Dead letter queue
    let dead_lettered_at: Option<String> = node.get("dead_lettered_at").unwrap_or(None);
    let dead_letter_reason: String = node.get("dead_letter_reason").unwrap_or_default();

    // Description and observability
    let description: String = node.get("description").unwrap_or_default();
    let duration_ms: Option<i64> = node.get("duration_ms").unwrap_or(None);

    AgentJob {
        id,
        tool_name,
        arguments,
        priority: priority as u8,
        status,
        created_at,
        updated_at,
        started_at,
        completed_at,
        result,
        error,
        attempt_count: attempt_count as u32,
        max_attempts: max_attempts as u32,
        session_id: if session_id.is_empty() {
            None
        } else {
            Some(session_id)
        },
        parent_job_id: if parent_job_id.is_empty() {
            None
        } else {
            Some(parent_job_id)
        },
        provider_hint: if provider_hint.is_empty() {
            None
        } else {
            Some(provider_hint)
        },
        context_profile: if context_profile.is_empty() {
            None
        } else {
            Some(context_profile)
        },
        prev_result: if prev_result.is_empty() {
            None
        } else {
            Some(prev_result)
        },
        // Progress tracking
        progress_percent: progress_percent.map(|v| v as u8),
        progress_message,
        progress_metadata: if progress_metadata.is_empty() {
            None
        } else {
            serde_json::from_str(&progress_metadata).ok()
        },
        progress_updated_at,
        // TTL
        expires_at,
        ttl_secs: if ttl_secs == 0 {
            None
        } else {
            Some(ttl_secs as u64)
        },
        // Dead letter queue
        dead_lettered_at,
        dead_letter_reason: if dead_letter_reason.is_empty() {
            None
        } else {
            Some(dead_letter_reason)
        },
        // Description
        description: if description.is_empty() {
            None
        } else {
            Some(description)
        },
        duration_ms: duration_ms.map(|v| v as u64),
    }
}
