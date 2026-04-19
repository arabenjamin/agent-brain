use chrono::{Duration, Utc};
use neo4rs::{BoltNull, BoltType, query};
use tracing::info;
use uuid::Uuid;

use crate::{Neo4jClient, RepositoryError};
use agent_brain_models::ScheduledTask;

// ---------------------------------------------------------------------------
// Row → ScheduledTask helper
// ---------------------------------------------------------------------------

fn row_to_scheduled_task(row: &neo4rs::Row) -> Result<ScheduledTask, RepositoryError> {
    let node: neo4rs::Node = row
        .get("s")
        .map_err(|e| RepositoryError::InvalidData(e.to_string()))?;

    let description: Option<String> = node.get("description").ok();

    let last_run_at: Option<String> = node.get("last_run_at").ok();

    Ok(ScheduledTask {
        id: node
            .get("id")
            .map_err(|e| RepositoryError::InvalidData(e.to_string()))?,
        name: node
            .get("name")
            .map_err(|e| RepositoryError::InvalidData(e.to_string()))?,
        description,
        enabled: node
            .get("enabled")
            .map_err(|e| RepositoryError::InvalidData(e.to_string()))?,
        interval_seconds: node
            .get("interval_seconds")
            .map_err(|e| RepositoryError::InvalidData(e.to_string()))?,
        steps: node
            .get("steps")
            .map_err(|e| RepositoryError::InvalidData(e.to_string()))?,
        last_run_at,
        next_run_at: node
            .get("next_run_at")
            .map_err(|e| RepositoryError::InvalidData(e.to_string()))?,
        created_at: node
            .get("created_at")
            .map_err(|e| RepositoryError::InvalidData(e.to_string()))?,
        updated_at: node
            .get("updated_at")
            .map_err(|e| RepositoryError::InvalidData(e.to_string()))?,
    })
}

// ---------------------------------------------------------------------------
// ScheduledTask CRUD
// ---------------------------------------------------------------------------

impl Neo4jClient {
    /// Create a new `ScheduledTask` node. Returns the created record.
    pub async fn create_scheduled_task(
        &self,
        name: &str,
        description: Option<&str>,
        enabled: bool,
        interval_seconds: i64,
        steps: &str,
        next_run_at: &str,
    ) -> Result<ScheduledTask, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();

        let mut q = query(
            "CREATE (s:ScheduledTask { \
               id: $id, name: $name, description: $description, \
               enabled: $enabled, interval_seconds: $interval_seconds, \
               steps: $steps, next_run_at: $next_run_at, \
               created_at: $created_at, updated_at: $updated_at \
             }) \
             RETURN s",
        )
        .param("id", id.clone())
        .param("name", name)
        .param("enabled", enabled)
        .param("interval_seconds", interval_seconds)
        .param("steps", steps)
        .param("next_run_at", next_run_at)
        .param("created_at", now.clone())
        .param("updated_at", now);

        if let Some(d) = description {
            q = q.param("description", d);
        } else {
            q = q.param("description", BoltType::Null(BoltNull));
        }

        let rows = self.execute(q).await?;
        let row = rows
            .first()
            .ok_or_else(|| RepositoryError::InvalidData("No row returned from CREATE".into()))?;
        let task = row_to_scheduled_task(row)?;
        info!(id = %id, name = %name, "Created ScheduledTask");
        Ok(task)
    }

    /// Fetch a single `ScheduledTask` by id. Returns `None` if not found.
    pub async fn get_scheduled_task(
        &self,
        id: &str,
    ) -> Result<Option<ScheduledTask>, RepositoryError> {
        let rows = self
            .execute(query("MATCH (s:ScheduledTask {id: $id}) RETURN s").param("id", id))
            .await?;
        rows.first().map(row_to_scheduled_task).transpose()
    }

    /// List all `ScheduledTask` nodes ordered by `next_run_at` ascending.
    /// When `enabled_only` is true only enabled tasks are returned.
    pub async fn list_scheduled_tasks(
        &self,
        enabled_only: bool,
    ) -> Result<Vec<ScheduledTask>, RepositoryError> {
        let cypher = if enabled_only {
            "MATCH (s:ScheduledTask {enabled: true}) RETURN s ORDER BY s.next_run_at ASC"
        } else {
            "MATCH (s:ScheduledTask) RETURN s ORDER BY s.next_run_at ASC"
        };
        self.execute(query(cypher))
            .await?
            .iter()
            .map(row_to_scheduled_task)
            .collect()
    }

    /// Return all enabled `ScheduledTask` nodes where `next_run_at <= now()`.
    pub async fn get_due_scheduled_tasks(&self) -> Result<Vec<ScheduledTask>, RepositoryError> {
        let rows = self
            .execute(query(
                "MATCH (s:ScheduledTask {enabled: true}) \
                 WHERE s.next_run_at <= toString(datetime()) \
                 RETURN s ORDER BY s.next_run_at ASC",
            ))
            .await?;
        rows.iter().map(row_to_scheduled_task).collect()
    }

    /// Partial update — `None` means leave unchanged.
    /// Returns `None` if the node does not exist.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_scheduled_task(
        &self,
        id: &str,
        name: Option<&str>,
        description: Option<Option<&str>>,
        enabled: Option<bool>,
        interval_seconds: Option<i64>,
        steps: Option<&str>,
        next_run_at: Option<&str>,
    ) -> Result<Option<ScheduledTask>, RepositoryError> {
        // Check existence first.
        if self.get_scheduled_task(id).await?.is_none() {
            return Ok(None);
        }

        let now = Utc::now().to_rfc3339();
        // Build SET clause dynamically.
        let mut sets: Vec<&str> = vec!["s.updated_at = $now"];
        if name.is_some() {
            sets.push("s.name = $name");
        }
        if enabled.is_some() {
            sets.push("s.enabled = $enabled");
        }
        if interval_seconds.is_some() {
            sets.push("s.interval_seconds = $interval_seconds");
        }
        if steps.is_some() {
            sets.push("s.steps = $steps");
        }
        if next_run_at.is_some() {
            sets.push("s.next_run_at = $next_run_at");
        }
        if description.is_some() {
            sets.push("s.description = $description");
        }

        let cypher = format!(
            "MATCH (s:ScheduledTask {{id: $id}}) SET {} RETURN s",
            sets.join(", ")
        );
        let mut q = query(&cypher).param("id", id).param("now", now.as_str());

        if let Some(v) = name {
            q = q.param("name", v);
        }
        if let Some(v) = enabled {
            q = q.param("enabled", v);
        }
        if let Some(v) = interval_seconds {
            q = q.param("interval_seconds", v);
        }
        if let Some(v) = steps {
            q = q.param("steps", v);
        }
        if let Some(v) = next_run_at {
            q = q.param("next_run_at", v);
        }
        if let Some(opt) = description {
            if let Some(v) = opt {
                q = q.param("description", v);
            } else {
                q = q.param("description", BoltType::Null(BoltNull));
            }
        }

        let rows = self.execute(q).await?;
        rows.first().map(row_to_scheduled_task).transpose()
    }

    /// Set `last_run_at = now` and `next_run_at = now + interval_seconds`
    /// after a successful dispatch.
    pub async fn record_scheduled_task_run(
        &self,
        id: &str,
        now: &str,
        next_run_at: &str,
    ) -> Result<(), RepositoryError> {
        self.run(
            query(
                "MATCH (s:ScheduledTask {id: $id}) \
                 SET s.last_run_at = $now, s.next_run_at = $next_run_at, s.updated_at = $now",
            )
            .param("id", id)
            .param("now", now)
            .param("next_run_at", next_run_at),
        )
        .await
    }

    /// Delete a `ScheduledTask` by id. Returns `true` if a node was deleted.
    pub async fn delete_scheduled_task(&self, id: &str) -> Result<bool, RepositoryError> {
        let rows = self
            .execute(
                query(
                    "MATCH (s:ScheduledTask {id: $id}) \
                     WITH s, s.id AS deleted_id \
                     DELETE s \
                     RETURN deleted_id",
                )
                .param("id", id),
            )
            .await?;
        Ok(!rows.is_empty())
    }

    /// Create a `ScheduledTask` only if no node with `name` already exists.
    /// Returns `(id, was_created)`.  Used for seeding built-in tasks at startup.
    /// `next_run_at` is set to `now` so the first run happens on the next tick.
    pub async fn seed_scheduled_task_if_absent(
        &self,
        name: &str,
        description: Option<&str>,
        interval_seconds: i64,
        steps: &str,
    ) -> Result<(String, bool), RepositoryError> {
        // Check if it already exists.
        let existing = self
            .execute(
                query("MATCH (s:ScheduledTask {name: $name}) RETURN s.id AS id")
                    .param("name", name),
            )
            .await?;

        if let Some(row) = existing.first() {
            let id: String = row
                .get("id")
                .map_err(|e| RepositoryError::InvalidData(e.to_string()))?;
            return Ok((id, false));
        }

        let now = Utc::now().to_rfc3339();
        // Schedule first run immediately.
        let task = self
            .create_scheduled_task(name, description, true, interval_seconds, steps, &now)
            .await?;
        Ok((task.id, true))
    }

    /// Compute `next_run_at` as `now + interval_seconds`.
    pub fn compute_next_run_at(interval_seconds: i64) -> String {
        (Utc::now() + Duration::seconds(interval_seconds)).to_rfc3339()
    }
}
