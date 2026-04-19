use chrono::Utc;
use neo4rs::query;
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use crate::{Neo4jClient, RepositoryError};

/// A single todo item stored in Neo4j.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    /// `pending` | `in_progress` | `done`
    pub status: String,
    /// 0 = urgent, 1 = high, 2 = normal, 3 = low
    pub priority: i64,
    /// Tags stored as a JSON array string internally.
    pub tags: Vec<String>,
    pub due_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

impl Neo4jClient {
    /// Create a new Todo node and return it.
    pub async fn create_todo(
        &self,
        title: &str,
        description: Option<&str>,
        status: Option<&str>,
        priority: Option<i64>,
        tags: Option<&[String]>,
        due_at: Option<&str>,
    ) -> Result<Todo, RepositoryError> {
        let id = Uuid::new_v4().to_string();
        let now = Utc::now().to_rfc3339();
        let status = status.unwrap_or("pending").to_string();
        let priority = priority.unwrap_or(2);
        let tags_json =
            serde_json::to_string(tags.unwrap_or(&[])).unwrap_or_else(|_| "[]".to_string());

        let q = query(
            "CREATE (t:Todo {
                id: $id,
                title: $title,
                description: $description,
                status: $status,
                priority: $priority,
                tags: $tags,
                due_at: $due_at,
                created_at: $created_at,
                updated_at: $updated_at
            }) RETURN t.id AS id",
        )
        .param("id", id.clone())
        .param("title", title)
        .param("description", description.unwrap_or(""))
        .param("status", status.clone())
        .param("priority", priority)
        .param("tags", tags_json.clone())
        .param("due_at", due_at.unwrap_or(""))
        .param("created_at", now.clone())
        .param("updated_at", now.clone());

        self.execute(q).await?;

        info!(id = %id, "Created todo in Neo4j");

        Ok(Todo {
            id,
            title: title.to_string(),
            description: description.map(str::to_string),
            status,
            priority,
            tags: tags.map(|t| t.to_vec()).unwrap_or_default(),
            due_at: due_at.map(str::to_string),
            created_at: now.clone(),
            updated_at: now,
        })
    }

    /// Fetch a single Todo by id.
    pub async fn get_todo(&self, id: &str) -> Result<Option<Todo>, RepositoryError> {
        let q = query(
            "MATCH (t:Todo {id: $id})
             RETURN t.id AS id, t.title AS title, t.description AS description,
                    t.status AS status, t.priority AS priority, t.tags AS tags,
                    t.due_at AS due_at, t.created_at AS created_at, t.updated_at AS updated_at",
        )
        .param("id", id);

        let rows = self.execute(q).await?;
        Ok(rows.into_iter().next().and_then(|r| todo_from_row(&r)))
    }

    /// List todos, optionally filtered by status, ordered by priority asc then created_at desc.
    pub async fn list_todos(
        &self,
        status_filter: Option<&str>,
    ) -> Result<Vec<Todo>, RepositoryError> {
        let rows = if let Some(s) = status_filter {
            let q = query(
                "MATCH (t:Todo) WHERE t.status = $status
                 RETURN t.id AS id, t.title AS title, t.description AS description,
                        t.status AS status, t.priority AS priority, t.tags AS tags,
                        t.due_at AS due_at, t.created_at AS created_at, t.updated_at AS updated_at
                 ORDER BY t.priority ASC, t.created_at DESC",
            )
            .param("status", s);
            self.execute(q).await?
        } else {
            let q = query(
                "MATCH (t:Todo)
                 RETURN t.id AS id, t.title AS title, t.description AS description,
                        t.status AS status, t.priority AS priority, t.tags AS tags,
                        t.due_at AS due_at, t.created_at AS created_at, t.updated_at AS updated_at
                 ORDER BY t.priority ASC, t.created_at DESC",
            );
            self.execute(q).await?
        };

        Ok(rows.iter().filter_map(todo_from_row).collect())
    }

    /// Update a Todo's fields. Only `Some` values are applied; `None` means leave unchanged.
    /// For nullable fields (`description`, `due_at`) use `Some(None)` to clear the value.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_todo(
        &self,
        id: &str,
        title: Option<&str>,
        description: Option<Option<&str>>,
        status: Option<&str>,
        priority: Option<i64>,
        tags: Option<&[String]>,
        due_at: Option<Option<&str>>,
    ) -> Result<Option<Todo>, RepositoryError> {
        let current = match self.get_todo(id).await? {
            Some(t) => t,
            None => return Ok(None),
        };

        let new_title = title.unwrap_or(&current.title).to_string();
        let new_description: String = match description {
            Some(Some(s)) => s.to_string(),
            Some(None) => String::new(),
            None => current.description.clone().unwrap_or_default(),
        };
        let new_status = status.unwrap_or(&current.status).to_string();
        let new_priority = priority.unwrap_or(current.priority);
        let new_tags: Vec<String> = tags.map(|t| t.to_vec()).unwrap_or(current.tags.clone());
        let new_tags_json = serde_json::to_string(&new_tags).unwrap_or_else(|_| "[]".to_string());
        let new_due_at: String = match due_at {
            Some(Some(s)) => s.to_string(),
            Some(None) => String::new(),
            None => current.due_at.clone().unwrap_or_default(),
        };
        let now = Utc::now().to_rfc3339();

        let q = query(
            "MATCH (t:Todo {id: $id})
             SET t.title = $title,
                 t.description = $description,
                 t.status = $status,
                 t.priority = $priority,
                 t.tags = $tags,
                 t.due_at = $due_at,
                 t.updated_at = $updated_at",
        )
        .param("id", id)
        .param("title", new_title)
        .param("description", new_description)
        .param("status", new_status)
        .param("priority", new_priority)
        .param("tags", new_tags_json)
        .param("due_at", new_due_at)
        .param("updated_at", now);

        self.execute(q).await?;

        self.get_todo(id).await
    }

    /// Delete a Todo by id. Returns true if a node was removed.
    pub async fn delete_todo(&self, id: &str) -> Result<bool, RepositoryError> {
        let q =
            query("MATCH (t:Todo {id: $id}) DETACH DELETE t RETURN count(t) AS n").param("id", id);

        let rows = self.execute(q).await?;
        let n = rows
            .first()
            .and_then(|r| r.get::<i64>("n").ok())
            .unwrap_or(0);
        Ok(n > 0)
    }
}

// ────────────────────────────────────────────────────────────────────────────
// Helpers
// ────────────────────────────────────────────────────────────────────────────

fn todo_from_row(row: &neo4rs::Row) -> Option<Todo> {
    let id: String = row.get("id").ok()?;
    let title: String = row.get("title").unwrap_or_default();
    let description: Option<String> = row
        .get::<String>("description")
        .ok()
        .and_then(|s| if s.is_empty() { None } else { Some(s) });
    let status: String = row.get("status").unwrap_or_else(|_| "pending".to_string());
    let priority: i64 = row.get("priority").unwrap_or(2);
    let tags_str: String = row.get("tags").unwrap_or_else(|_| "[]".to_string());
    let tags: Vec<String> = serde_json::from_str(&tags_str).unwrap_or_default();
    let due_at: Option<String> = row
        .get::<String>("due_at")
        .ok()
        .and_then(|s| if s.is_empty() { None } else { Some(s) });
    let created_at: String = row.get("created_at").unwrap_or_default();
    let updated_at: String = row.get("updated_at").unwrap_or_default();

    Some(Todo {
        id,
        title,
        description,
        status,
        priority,
        tags,
        due_at,
        created_at,
        updated_at,
    })
}
