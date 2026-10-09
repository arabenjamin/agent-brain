use chrono::Utc;
use neo4rs::query;
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use crate::{Neo4jClient, RepositoryError};

/// The statuses an item may have (HBI plan §9.2). `awaiting_approval`: an
/// agent that writes is assigned and waits to be started; `awaiting_review`:
/// an agent finished and its result waits for the user.
pub const TODO_STATUSES: &[&str] = &[
    "pending",
    "in_progress",
    "awaiting_approval",
    "awaiting_review",
    "done",
];

/// Where an item came from (`origin_kind`): the user, a schedule's run, a
/// code proposal, or the brain asking (`notify_user` with `needs: action`).
pub const TODO_ORIGINS: &[&str] = &["user", "schedule", "proposal", "agent"];

/// A single todo item stored in Neo4j: the one record for work that wants
/// doing, by the user or by an agent (HBI plan §9.2).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Todo {
    pub id: String,
    pub title: String,
    pub description: Option<String>,
    /// One of [`TODO_STATUSES`].
    pub status: String,
    /// 0 = urgent, 1 = high, 2 = normal, 3 = low
    pub priority: i64,
    /// Tags stored as a JSON array string internally.
    pub tags: Vec<String>,
    pub due_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// `user`, `agent:<profile>` or `agent:auto`; `None` = unassigned (the user's own).
    #[serde(default)]
    pub assignee: Option<String>,
    /// One of [`TODO_ORIGINS`]; `None` on todos from before 2026-10-09 (the user's).
    #[serde(default)]
    pub origin_kind: Option<String>,
    /// What it links back to: a notification id, a proposal filename, a schedule id.
    #[serde(default)]
    pub origin_ref: Option<String>,
    /// What an agent is told; falls back to title + description.
    #[serde(default)]
    pub prompt: Option<String>,
    /// The item's thread: a chat session.
    #[serde(default)]
    pub session_id: Option<String>,
    /// Hidden from the Feed until then.
    #[serde(default)]
    pub snoozed_until: Option<String>,
    #[serde(default)]
    pub dispatched_at: Option<String>,
    #[serde(default)]
    pub dispatch_count: i64,
    #[serde(default)]
    pub result_note_id: Option<String>,
}

/// Item fields a write may set. `None` leaves a field as it is; `Some(None)`
/// clears it.
#[derive(Debug, Clone, Default)]
pub struct TodoItemPatch {
    pub assignee: Option<Option<String>>,
    pub origin_kind: Option<Option<String>>,
    pub origin_ref: Option<Option<String>>,
    pub prompt: Option<Option<String>>,
    pub session_id: Option<Option<String>>,
    pub snoozed_until: Option<Option<String>>,
    pub result_note_id: Option<Option<String>>,
}

impl TodoItemPatch {
    pub fn is_empty(&self) -> bool {
        self.assignee.is_none()
            && self.origin_kind.is_none()
            && self.origin_ref.is_none()
            && self.prompt.is_none()
            && self.session_id.is_none()
            && self.snoozed_until.is_none()
            && self.result_note_id.is_none()
    }
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
                due_at: CASE WHEN $due_at = '' THEN null ELSE datetime($due_at) END,
                created_at: datetime($created_at),
                updated_at: datetime($updated_at)
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
            assignee: None,
            origin_kind: None,
            origin_ref: None,
            prompt: None,
            session_id: None,
            snoozed_until: None,
            dispatched_at: None,
            dispatch_count: 0,
            result_note_id: None,
        })
    }

    /// Write the item fields of `patch` (see [`TodoItemPatch`]); validation is
    /// the caller's. Returns the updated item, `None` when it doesn't exist.
    pub async fn patch_todo_item(
        &self,
        id: &str,
        patch: &TodoItemPatch,
    ) -> Result<Option<Todo>, RepositoryError> {
        if patch.is_empty() {
            return self.get_todo(id).await;
        }
        let fields: [(&str, &Option<Option<String>>, bool); 7] = [
            ("assignee", &patch.assignee, false),
            ("origin_kind", &patch.origin_kind, false),
            ("origin_ref", &patch.origin_ref, false),
            ("prompt", &patch.prompt, false),
            ("session_id", &patch.session_id, false),
            ("snoozed_until", &patch.snoozed_until, true),
            ("result_note_id", &patch.result_note_id, false),
        ];
        let mut sets = vec!["t.updated_at = datetime()".to_string()];
        let mut q_params: Vec<(String, neo4rs::BoltType)> = Vec::new();
        for (key, value, is_time) in fields {
            let Some(value) = value else { continue };
            let param = format!("p_{key}");
            match value.as_deref().map(str::trim).filter(|v| !v.is_empty()) {
                Some(v) => {
                    sets.push(if is_time {
                        format!("t.{key} = datetime(${param})")
                    } else {
                        format!("t.{key} = ${param}")
                    });
                    q_params.push((param, neo4rs::BoltType::from(v)));
                }
                None => sets.push(format!("t.{key} = null")),
            }
        }
        let cypher = format!(
            "MATCH (t:Todo {{id: $id}}) SET {} RETURN t.id AS id",
            sets.join(", ")
        );
        let mut q = query(&cypher).param("id", id);
        for (k, v) in q_params {
            q = q.param(&k, v);
        }
        if self.execute(q).await?.is_empty() {
            return Ok(None);
        }
        self.get_todo(id).await
    }

    /// The item made for `origin_kind` + `origin_ref`, if one exists: how a
    /// proposal or a notification finds its item again.
    pub async fn find_todo_by_origin(
        &self,
        origin_kind: &str,
        origin_ref: &str,
    ) -> Result<Option<Todo>, RepositoryError> {
        let rows = self
            .execute(
                query(
                    "MATCH (t:Todo {origin_kind: $kind, origin_ref: $ref}) \
                     RETURN t ORDER BY t.created_at ASC LIMIT 1",
                )
                .param("kind", origin_kind)
                .param("ref", origin_ref),
            )
            .await?;
        Ok(rows.into_iter().next().and_then(|r| todo_from_row(&r)))
    }

    /// Fetch a single Todo by id.
    pub async fn get_todo(&self, id: &str) -> Result<Option<Todo>, RepositoryError> {
        let q = query("MATCH (t:Todo {id: $id}) RETURN t").param("id", id);

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
                 RETURN t ORDER BY t.priority ASC, t.created_at DESC",
            )
            .param("status", s);
            self.execute(q).await?
        } else {
            let q = query("MATCH (t:Todo) RETURN t ORDER BY t.priority ASC, t.created_at DESC");
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
                 t.due_at = CASE WHEN $due_at = '' THEN null ELSE datetime($due_at) END,
                 t.updated_at = datetime($updated_at)",
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
    let node: neo4rs::Node = row.get("t").ok()?;
    let text = |k: &str| node.get::<String>(k).ok().filter(|s| !s.is_empty());
    let id: String = node.get("id").ok()?;
    let tags_str: String = node.get("tags").unwrap_or_else(|_| "[]".to_string());
    Some(Todo {
        id,
        title: node.get("title").unwrap_or_default(),
        description: text("description"),
        status: node.get("status").unwrap_or_else(|_| "pending".to_string()),
        priority: node.get("priority").unwrap_or(2),
        tags: serde_json::from_str(&tags_str).unwrap_or_default(),
        due_at: crate::temporal::node_ts(&node, "due_at"),
        created_at: crate::temporal::node_ts(&node, "created_at").unwrap_or_default(),
        updated_at: crate::temporal::node_ts(&node, "updated_at").unwrap_or_default(),
        assignee: text("assignee"),
        origin_kind: text("origin_kind"),
        origin_ref: text("origin_ref"),
        prompt: text("prompt"),
        session_id: text("session_id"),
        snoozed_until: crate::temporal::node_ts(&node, "snoozed_until"),
        dispatched_at: crate::temporal::node_ts(&node, "dispatched_at"),
        dispatch_count: node.get("dispatch_count").unwrap_or(0),
        result_note_id: text("result_note_id"),
    })
}
