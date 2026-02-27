use chrono::Utc;
use neo4rs::{query, Node};
use tracing::info;
use uuid::Uuid;

use crate::models::{AgentJob, AgentJobStatus};
use crate::repository::{Neo4jClient, RepositoryError};

impl Neo4jClient {
    /// Create a new AgentJob node in Neo4j and return its ID.
    pub async fn create_agent_job(
        &self,
        tool_name: &str,
        arguments: Option<&serde_json::Value>,
        priority: u8,
        max_attempts: u32,
        session_id: Option<&str>,
        parent_job_id: Option<&str>,
        provider_hint: Option<&str>,
    ) -> Result<String, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let args_json = arguments.map(|a| a.to_string()).unwrap_or_default();

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
                provider_hint: $provider_hint
            })",
        )
        .param("id", id.clone())
        .param("tool_name", tool_name)
        .param("args_json", args_json)
        .param("priority", priority as i64)
        .param("now", now)
        .param("max_attempts", max_attempts as i64)
        .param("session_id", session_id.unwrap_or(""))
        .param("parent_job_id", parent_job_id.unwrap_or(""))
        .param("provider_hint", provider_hint.unwrap_or(""));

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
        let q = query(
            "MATCH (j:AgentJob) WHERE j.status IN ['queued', 'parked'] \
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
            let q = query(
                "MATCH (j:AgentJob) RETURN j ORDER BY j.created_at DESC LIMIT $limit",
            )
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
        let q = query(
            "MATCH (j:AgentJob {id: $id}) SET j.status = $status, j.updated_at = $now",
        )
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
    pub async fn set_job_completed(&self, id: &str, result_json: &str) -> Result<(), RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {id: $id}) \
             SET j.status = 'completed', \
                 j.completed_at = $now, \
                 j.updated_at = $now, \
                 j.result_json = $result",
        )
        .param("id", id)
        .param("now", now)
        .param("result", result_json);
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

    /// Mark a job as dead (exhausted all retries).
    pub async fn set_job_dead(&self, id: &str, last_error: &str) -> Result<(), RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {id: $id}) \
             SET j.status = 'dead', \
                 j.completed_at = $now, \
                 j.updated_at = $now, \
                 j.error = $error",
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
    pub async fn create_agent_job_parked(
        &self,
        tool_name: &str,
        arguments: Option<&serde_json::Value>,
        priority: u8,
        max_attempts: u32,
        session_id: Option<&str>,
        parent_job_id: &str,
        provider_hint: Option<&str>,
    ) -> Result<String, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let args_json = arguments.map(|a| a.to_string()).unwrap_or_default();

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
                provider_hint: $provider_hint
            })",
        )
        .param("id", id.clone())
        .param("tool_name", tool_name)
        .param("args_json", args_json)
        .param("priority", priority as i64)
        .param("now", now)
        .param("max_attempts", max_attempts as i64)
        .param("session_id", session_id.unwrap_or(""))
        .param("parent_job_id", parent_job_id)
        .param("provider_hint", provider_hint.unwrap_or(""));

        self.run(q).await?;
        info!(id = %id, tool = %tool_name, parent = %parent_job_id, "Created parked AgentJob");
        Ok(id)
    }

    /// Promote all parked children of a completed job to `queued`.
    /// Returns the newly-queued jobs so the coordinator can push them onto the heap.
    pub async fn unpark_children(
        &self,
        parent_job_id: &str,
    ) -> Result<Vec<AgentJob>, RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (j:AgentJob {parent_job_id: $parent_id, status: 'parked'})
             SET j.status = 'queued', j.updated_at = $now
             RETURN j",
        )
        .param("parent_id", parent_job_id)
        .param("now", now);

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
        session_id: if session_id.is_empty() { None } else { Some(session_id) },
        parent_job_id: if parent_job_id.is_empty() { None } else { Some(parent_job_id) },
        provider_hint: if provider_hint.is_empty() { None } else { Some(provider_hint) },
    }
}
