use neo4rs::{query, Node, BoltType, BoltNull};
use chrono::Utc;
use uuid::Uuid;
use tracing::info;

use crate::models::{Task, TaskStatus};
use crate::repository::{Neo4jClient, RepositoryError};

impl Neo4jClient {
    /// Create a new task in the database.
    pub async fn create_task(
        &self,
        goal: &str,
        context: Option<&str>,
    ) -> Result<String, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        
        let mut q = query("CREATE (t:Task {id: $id, goal: $goal, status: 'created', created_at: $created_at, updated_at: $updated_at}) SET t.context = $context RETURN t.id")
            .param("id", id.clone())
            .param("goal", goal)
            .param("created_at", now.clone())
            .param("updated_at", now);

        if let Some(ctx) = context {
            q = q.param("context", ctx);
        } else {
            // Correct way to pass null in neo4rs
            q = q.param("context", BoltType::Null(BoltNull));
        }

        self.execute(q).await?;
        
        info!(id = %id, "Created task in Neo4j");
        Ok(id)
    }

    /// Get a task by ID.
    pub async fn get_task(&self, id: &str) -> Result<Option<Task>, RepositoryError> {
        let q = query("MATCH (t:Task {id: $id}) RETURN t").param("id", id);
        
        // Execute returns Vec<Row>
        let rows = self.execute(q).await?;
        
        if let Some(row) = rows.into_iter().next() {
            // Neo4rs DeError is basically a deserialization error, so we map it to our Serialization variant
            // We use a closure to construct the error properly as RepositoryError::Serialization expects serde_json::Error
            // But we can't easily convert DeError to serde_json::Error.
            // Let's use InvalidData for now or add a Neo4rs variant.
            // Actually RepositoryError::Neo4j wraps neo4rs::Error, but DeError is different?
            // neo4rs::Error contains DeError.
            // Let's map it to Neo4j error variant if possible, or stringify it.
            
            let node: Node = row.get("t").map_err(|e| RepositoryError::InvalidData(format!("Deserialization error: {}", e)))?;
            
            // Extract fields safely
            let id: String = node.get("id").unwrap_or_default();
            let goal: String = node.get("goal").unwrap_or_default();
            // node.get() for Option returns Result<Option<T>, DeError>, so we unwrap_or(None)
            let context: Option<String> = node.get("context").unwrap_or(None);
            let created_at: String = node.get("created_at").unwrap_or_default();
            let updated_at: String = node.get("updated_at").unwrap_or_default();
            
            let status_str: String = node.get("status").unwrap_or("created".to_string());
            let status = match status_str.as_str() {
                "in_progress" => TaskStatus::InProgress,
                "completed" => TaskStatus::Completed,
                "failed" => TaskStatus::Failed,
                "blocked" => TaskStatus::Blocked,
                _ => TaskStatus::Created,
            };

            Ok(Some(Task {
                id,
                goal,
                context,
                status,
                created_at,
                updated_at,
            }))
        } else {
            Ok(None)
        }
    }

    /// Store a reflection note and optionally link it to a Task via REFLECTS_ON.
    pub async fn store_reflection_note(
        &self,
        content: &str,
        task_id: Option<&str>,
    ) -> Result<String, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        let create_q = query(
            "CREATE (n:Note {id: $id, content: $content, note_type: 'reflection', \
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
    pub async fn update_task_status(&self, id: &str, status: TaskStatus) -> Result<(), RepositoryError> {
         let status_str = serde_json::to_string(&status)
            .unwrap_or_else(|_| "unknown".to_string())
            .trim_matches('"')
            .to_string();

        let now = Utc::now().to_rfc3339();

        let q = query("MATCH (t:Task {id: $id}) SET t.status = $status, t.updated_at = $updated_at")
            .param("id", id)
            .param("status", status_str)
            .param("updated_at", now);

        self.execute(q).await?;
        Ok(())
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
                        t.context AS context, t.created_at AS created_at, \
                        parent.id AS parent_id \
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
                        t.context AS context, t.created_at AS created_at, \
                        parent.id AS parent_id \
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
            let created_at = row.get::<String>("created_at").unwrap_or_default();
            let parent_id: Option<String> = row.get("parent_id").unwrap_or(None);

            // Fetch dependency IDs in a separate query to keep the list query simple.
            let deps = self.get_task_dependencies(&id).await.unwrap_or_default();

            tasks.push(serde_json::json!({
                "id": id,
                "goal": goal,
                "status": status_val,
                "context": context,
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
             SET parent.status = 'completed', parent.updated_at = $now \
             RETURN parent.id AS parent_id",
        )
        .param("child_id", child_id)
        .param("now", now);

        let rows = self.execute(q).await?;
        Ok(rows.into_iter().next().and_then(|r| r.get::<String>("parent_id").ok()))
    }

    /// Return task IDs that `task_id` directly depends on (i.e., must complete first).
    pub async fn get_task_dependencies(
        &self,
        task_id: &str,
    ) -> Result<Vec<String>, RepositoryError> {
        let q = query(
            "MATCH (a:Task {id: $id})-[:DEPENDS_ON]->(b:Task) RETURN b.id AS dep_id",
        )
        .param("id", task_id);

        let rows = self.execute(q).await?;
        Ok(rows.iter().filter_map(|r| r.get::<String>("dep_id").ok()).collect())
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
}
