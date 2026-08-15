use chrono::Utc;
use neo4rs::{BoltNull, BoltType, query};
use tracing::info;
use uuid::Uuid;

use crate::{Neo4jClient, RepositoryError};
use agent_brain_models::{Task, TaskStatus};

/// Column projection for reading a `Task` into the model struct.
///
/// Timestamps are stored as native Neo4j `DATETIME` but `Task.created_at` /
/// `updated_at` are `String`, so they must come back through `toString()`.
/// This is why these reads project explicit columns instead of `RETURN t`:
/// `node.get::<String>("created_at")` on a datetime property fails, and the
/// call sites used `.unwrap_or_default()`, which turns that failure into an
/// empty string rather than an error.
const TASK_COLUMNS: &str = "t.id AS id, t.goal AS goal, t.status AS status, \
     t.context AS context, t.success_criteria AS success_criteria, \
     toString(t.created_at) AS created_at, toString(t.updated_at) AS updated_at";

/// Build a `Task` from a row projected with [`TASK_COLUMNS`].
fn task_from_row(row: &neo4rs::Row) -> Task {
    let status_str: String = row.get("status").unwrap_or_else(|_| "created".to_string());
    Task {
        id: row.get("id").unwrap_or_default(),
        goal: row.get("goal").unwrap_or_default(),
        context: row.get("context").unwrap_or(None),
        success_criteria: row.get("success_criteria").unwrap_or(None),
        status: match status_str.as_str() {
            "in_progress" => TaskStatus::InProgress,
            "completed" => TaskStatus::Completed,
            "failed" => TaskStatus::Failed,
            "blocked" => TaskStatus::Blocked,
            _ => TaskStatus::Created,
        },
        created_at: row.get("created_at").unwrap_or_default(),
        updated_at: row.get("updated_at").unwrap_or_default(),
    }
}

impl Neo4jClient {
    /// Create a new task in the database.
    pub async fn create_task(
        &self,
        goal: &str,
        context: Option<&str>,
        success_criteria: Option<&str>,
    ) -> Result<String, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        let mut q = query(
            "CREATE (t:Task {id: $id, goal: $goal, status: 'created', \
             created_at: datetime($created_at), updated_at: datetime($updated_at)}) \
             SET t.context = $context, t.success_criteria = $success_criteria \
             RETURN t.id",
        )
        .param("id", id.clone())
        .param("goal", goal)
        .param("created_at", now.clone())
        .param("updated_at", now);

        if let Some(ctx) = context {
            q = q.param("context", ctx);
        } else {
            q = q.param("context", BoltType::Null(BoltNull));
        }

        if let Some(sc) = success_criteria {
            q = q.param("success_criteria", sc);
        } else {
            q = q.param("success_criteria", BoltType::Null(BoltNull));
        }

        self.execute(q).await?;

        info!(id = %id, "Created task in Neo4j");
        Ok(id)
    }

    /// Reset tasks that have been stuck in `in_progress` for longer than `stale_hours` hours
    /// back to `created` so the scheduler will re-dispatch them.
    /// Returns the number of tasks reset.
    pub async fn reset_stale_in_progress_tasks(
        &self,
        stale_hours: u64,
    ) -> Result<usize, RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (t:Task {status: 'in_progress'}) \
             WHERE t.updated_at IS NOT NULL \
               AND t.updated_at < datetime() - duration({hours: $hours}) \
             SET t.status = 'failed', t.updated_at = datetime($now) \
             RETURN count(t) AS n",
        )
        .param("hours", stale_hours as i64)
        .param("now", now);
        let rows = self.execute(q).await?;
        let count = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0) as usize;
        if count > 0 {
            info!(
                count,
                stale_hours, "Reset stale in_progress tasks to failed"
            );
        }
        Ok(count)
    }

    /// Delete completed/cancelled tasks older than `days`, in batches, to keep
    /// the Task label from growing unboundedly (the scheduler scans it every tick).
    ///
    /// Tasks referenced by an AgentSpec (`PERFORMED` / `CONSTRUCTED_FOR`) are kept —
    /// they are the constructor's learning history. Returns total deleted.
    pub async fn delete_old_completed_tasks(&self, days: u32) -> Result<usize, RepositoryError> {
        if days == 0 {
            return Ok(0);
        }
        let mut total = 0usize;
        loop {
            let q = query(
                "MATCH (t:Task) \
                 WHERE t.status IN ['completed', 'cancelled'] \
                   AND coalesce(t.updated_at, t.created_at) \
                       < datetime() - duration({days: $days}) \
                   AND NOT EXISTS { MATCH (:AgentSpec)-[:PERFORMED|CONSTRUCTED_FOR]->(t) } \
                 WITH t LIMIT 1000 \
                 DETACH DELETE t \
                 RETURN count(*) AS n",
            )
            .param("days", days as i64);
            let rows = self.execute(q).await?;
            let n = rows
                .first()
                .and_then(|r| r.get::<i64>("n").ok())
                .unwrap_or(0) as usize;
            total += n;
            if n < 1000 {
                break;
            }
        }
        if total > 0 {
            info!(total, days, "Deleted old completed/cancelled tasks");
        }
        Ok(total)
    }

    /// Get a task by ID.
    pub async fn get_task(&self, id: &str) -> Result<Option<Task>, RepositoryError> {
        let q = query(&format!("MATCH (t:Task {{id: $id}}) RETURN {TASK_COLUMNS}")).param("id", id);

        let rows = self.execute(q).await?;

        Ok(rows.first().map(task_from_row))
    }

    /// Return recent tasks for duplicate detection by the caller.
    ///
    /// Fetches up to 30 tasks created within `days_lookback` days. Similarity
    /// scoring (Jaccard word-overlap) is performed by the caller so no text
    /// matching happens inside Neo4j.
    pub async fn find_similar_tasks(
        &self,
        days_lookback: u32,
    ) -> Result<Vec<Task>, RepositoryError> {
        let q = query(&format!(
            "MATCH (t:Task) \
             WHERE t.created_at >= datetime() - duration({{days: $days}}) \
               AND t.status IN ['created', 'in_progress', 'failed', 'completed'] \
             RETURN {TASK_COLUMNS} \
             ORDER BY t.created_at DESC LIMIT 30"
        ))
        .param("days", days_lookback as i64);

        let rows = self.execute(q).await?;
        Ok(rows.iter().map(task_from_row).collect())
    }

    /// Store a reflection note and optionally link it to a Task via REFLECTS_ON.
    /// Persist a lightweight episodic note (no embedding) directly in Neo4j.
    /// Use this from services that don't have access to `KnowledgeService` — it
    /// skips vector embedding but still participates in spaced-rep review.
    pub async fn store_episodic_note(
        &self,
        content: &str,
        source_context: Option<&str>,
    ) -> Result<String, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let src = source_context.unwrap_or("");

        let q = query(
            "CREATE (n:Note {id: $id, content: $content, note_type: 'episodic', \
             source_context: $src, provenance: 'user_input', \
             created_at: datetime($ts), last_accessed_at: datetime($ts), \
             access_count: 0, \
             next_review_at: datetime($ts) + duration({days: 1}), \
             review_interval_days: 1})",
        )
        .param("id", id.clone())
        .param("content", content)
        .param("src", src)
        .param("ts", now);

        self.run(q).await?;
        info!(note_id = %id, source_context = %src, "Stored episodic note");
        Ok(id)
    }

    pub async fn store_reflection_note(
        &self,
        content: &str,
        task_id: Option<&str>,
    ) -> Result<String, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        let create_q = query(
            "CREATE (n:Note {id: $id, content: $content, note_type: 'reflection', \
             provenance: 'synthesis_inference', \
             created_at: datetime($ts), last_accessed_at: datetime($ts), \
             access_count: 0, next_review_at: datetime($ts) + duration({days: 1}), \
             review_interval_days: 1})",
        )
        .param("id", id.clone())
        .param("content", content)
        .param("ts", now);

        self.run(create_q).await?;

        if let Some(tid) = task_id {
            let link_q = query(
                "MATCH (n:Note {id: $note_id}), (t:Task {id: $task_id}) \
                 MERGE (n)-[:REFLECTS_ON]->(t)",
            )
            .param("note_id", id.clone())
            .param("task_id", tid);
            // Log but don't fail if the task doesn't exist
            if let Err(e) = self.run(link_q).await {
                tracing::warn!("Could not link reflection note to task {}: {}", tid, e);
            }
        }

        info!(note_id = %id, "Stored reflection note");
        Ok(id)
    }

    /// Update task status.
    pub async fn update_task_status(
        &self,
        id: &str,
        status: TaskStatus,
    ) -> Result<(), RepositoryError> {
        let status_str = serde_json::to_string(&status)
            .unwrap_or_else(|_| "unknown".to_string())
            .trim_matches('"')
            .to_string();

        let now = Utc::now().to_rfc3339();

        let q = query(
            "MATCH (t:Task {id: $id}) SET t.status = $status, \
                 t.updated_at = datetime($updated_at)",
        )
        .param("id", id)
        .param("status", status_str)
        .param("updated_at", now);

        self.execute(q).await?;
        Ok(())
    }

    /// Mark a task `failed` and append a diagnostic reason to its `context`.
    ///
    /// Only transitions tasks still in a non-terminal state (`created` or
    /// `in_progress`), so a task that already completed (or failed via the
    /// evaluator/adversarial path) is left untouched — the guard makes this
    /// safe to call from the coordinator's dead-job path even if a sibling
    /// step already resolved the task. Returns `true` when a task was updated.
    ///
    /// This is what turns a stuck-`in_progress` scheduled run into a `failed`
    /// task with a real error, instead of leaving it for the 6-hour stale
    /// reaper to flip with no diagnosis attached.
    pub async fn fail_task_with_reason(
        &self,
        id: &str,
        reason: &str,
    ) -> Result<bool, RepositoryError> {
        let now = Utc::now().to_rfc3339();
        let q = query(
            "MATCH (t:Task {id: $id}) WHERE t.status IN ['created', 'in_progress'] \
             SET t.status = 'failed', t.updated_at = datetime($updated_at), \
                 t.context = coalesce(t.context, '') + '\n\n[FAILURE] ' + $reason \
             RETURN t.id AS id",
        )
        .param("id", id)
        .param("reason", reason)
        .param("updated_at", now);

        let rows = self.execute(q).await?;
        Ok(!rows.is_empty())
    }

    /// Link a child task as a subtask of a parent via SUBTASK_OF edge.
    pub async fn link_subtask(
        &self,
        parent_id: &str,
        child_id: &str,
    ) -> Result<(), RepositoryError> {
        let q = query(
            "MATCH (parent:Task {id: $parent_id}), (child:Task {id: $child_id}) \
             MERGE (child)-[:SUBTASK_OF]->(parent)",
        )
        .param("parent_id", parent_id)
        .param("child_id", child_id);

        self.run(q).await?;
        info!(parent_id = %parent_id, child_id = %child_id, "Linked subtask");
        Ok(())
    }

    /// List tasks with optional status filter and optional subtask parent info.
    pub async fn list_tasks(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<serde_json::Value>, RepositoryError> {
        let rows = if let Some(s) = status {
            let q = query(
                "MATCH (t:Task) WHERE t.status = $status \
                 OPTIONAL MATCH (t)-[:SUBTASK_OF]->(parent:Task) \
                 RETURN t.id AS id, t.goal AS goal, t.status AS status, \
                        t.context AS context, t.success_criteria AS success_criteria, \
                        toString(t.created_at) AS created_at, parent.id AS parent_id \
                 ORDER BY t.created_at DESC LIMIT $limit",
            )
            .param("status", s)
            .param("limit", limit as i64);
            self.execute(q).await?
        } else {
            let q = query(
                "MATCH (t:Task) \
                 OPTIONAL MATCH (t)-[:SUBTASK_OF]->(parent:Task) \
                 RETURN t.id AS id, t.goal AS goal, t.status AS status, \
                        t.context AS context, t.success_criteria AS success_criteria, \
                        toString(t.created_at) AS created_at, parent.id AS parent_id \
                 ORDER BY t.created_at DESC LIMIT $limit",
            )
            .param("limit", limit as i64);
            self.execute(q).await?
        };

        let mut tasks = Vec::new();
        for row in rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let goal = row.get::<String>("goal").unwrap_or_default();
            let status_val = row.get::<String>("status").unwrap_or_default();
            let context: Option<String> = row.get("context").unwrap_or(None);
            let success_criteria: Option<String> = row.get("success_criteria").unwrap_or(None);
            let created_at = row.get::<String>("created_at").unwrap_or_default();
            let parent_id: Option<String> = row.get("parent_id").unwrap_or(None);

            let deps = self.get_task_dependencies(&id).await.unwrap_or_default();

            tasks.push(serde_json::json!({
                "id": id,
                "goal": goal,
                "status": status_val,
                "context": context,
                "success_criteria": success_criteria,
                "created_at": created_at,
                "parent_id": parent_id,
                "depends_on": deps,
            }));
        }
        Ok(tasks)
    }

    /// Create a DEPENDS_ON edge: `from_id` cannot start until `to_id` completes.
    pub async fn link_task_dependency(
        &self,
        from_id: &str,
        to_id: &str,
    ) -> Result<(), RepositoryError> {
        let q = query(
            "MATCH (a:Task {id: $from_id}), (b:Task {id: $to_id}) \
             MERGE (a)-[:DEPENDS_ON]->(b)",
        )
        .param("from_id", from_id)
        .param("to_id", to_id);

        self.run(q).await?;
        info!(from_id = %from_id, to_id = %to_id, "Linked task dependency");
        Ok(())
    }

    /// If all sub-tasks of a parent are now completed, mark the parent completed too.
    ///
    /// Returns `Some(parent_id)` if a parent was auto-completed, `None` otherwise.
    pub async fn auto_complete_parent_if_done(
        &self,
        child_id: &str,
    ) -> Result<Option<String>, RepositoryError> {
        let now = Utc::now().to_rfc3339();
        // Find the parent and auto-complete it only when every sibling is completed.
        let q = query(
            "MATCH (child:Task {id: $child_id})-[:SUBTASK_OF]->(parent:Task) \
             WHERE parent.status <> 'completed' \
               AND NOT EXISTS { \
                   MATCH (other:Task)-[:SUBTASK_OF]->(parent) \
                   WHERE other.status <> 'completed' \
               } \
             SET parent.status = 'completed', parent.updated_at = datetime($now) \
             RETURN parent.id AS parent_id",
        )
        .param("child_id", child_id)
        .param("now", now);

        let rows = self.execute(q).await?;
        Ok(rows
            .into_iter()
            .next()
            .and_then(|r| r.get::<String>("parent_id").ok()))
    }

    /// Return task IDs that `task_id` directly depends on (i.e., must complete first).
    pub async fn get_task_dependencies(
        &self,
        task_id: &str,
    ) -> Result<Vec<String>, RepositoryError> {
        let q = query("MATCH (a:Task {id: $id})-[:DEPENDS_ON]->(b:Task) RETURN b.id AS dep_id")
            .param("id", task_id);

        let rows = self.execute(q).await?;
        Ok(rows
            .iter()
            .filter_map(|r| r.get::<String>("dep_id").ok())
            .collect())
    }

    /// Store an outcome note (note_type='outcome'), optionally linked to a task.
    pub async fn store_outcome_note(
        &self,
        content: &str,
        task_id: Option<&str>,
    ) -> Result<String, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        let create_q = query(
            "CREATE (n:Note {id: $id, content: $content, note_type: 'outcome', \
             provenance: 'synthesis_inference', \
             created_at: datetime($ts), last_accessed_at: datetime($ts), \
             access_count: 0, next_review_at: datetime($ts) + duration({days: 1}), \
             review_interval_days: 1})",
        )
        .param("id", id.clone())
        .param("content", content)
        .param("ts", now);

        self.run(create_q).await?;

        if let Some(tid) = task_id {
            let link_q = query(
                "MATCH (n:Note {id: $note_id}), (t:Task {id: $task_id}) \
                 MERGE (n)-[:REFLECTS_ON]->(t)",
            )
            .param("note_id", id.clone())
            .param("task_id", tid);
            if let Err(e) = self.run(link_q).await {
                tracing::warn!("Could not link outcome note to task {}: {}", tid, e);
            }
        }

        info!(note_id = %id, "Stored outcome note");
        Ok(id)
    }

    /// Record how a constructed AgentSpec performed on a task — the Phase 3
    /// learning edge. Called by the queue's evaluator hook after every graded
    /// run (pass AND fail: failures are the more valuable signal).
    /// Returns `true` when the task was linked to an AgentSpec.
    pub async fn record_agent_spec_performance(
        &self,
        task_id: &str,
        score: f64,
        passed: bool,
    ) -> Result<bool, RepositoryError> {
        let rows = self
            .execute(
                query(
                    "MATCH (a:AgentSpec)-[:CONSTRUCTED_FOR]->(t:Task {id: $task_id}) \
                     CREATE (a)-[:PERFORMED {score: $score, passed: $passed, \
                                             at: datetime($at)}]->(t) \
                     RETURN a.id AS id",
                )
                .param("task_id", task_id)
                .param("score", score)
                .param("passed", passed)
                .param("at", Utc::now().to_rfc3339()),
            )
            .await?;
        Ok(!rows.is_empty())
    }
}
