//! The task and working-memory stores skills depend on, and their
//! implementations on Neo4jClient.

use serde_json::{Value, json};

use agent_brain_models::{Task, TaskStatus};
use agent_brain_repository::Neo4jClient;
use async_trait::async_trait;

// ============================================================================
// TaskStore
// ============================================================================

/// Methods on `Neo4jClient` (task repository) used by `TaskSkill`.
#[async_trait]
pub trait TaskStore: Send + Sync {
    /// Create a run of `workflow` (the default workflow when `None`) with
    /// `inputs`. Returns `(task_id, workflow name)`; `Err` says why it was refused.
    async fn start_run(
        &self,
        workflow: Option<&str>,
        inputs: &serde_json::Map<String, serde_json::Value>,
        goal: &str,
        context: Option<&str>,
        success_criteria: Option<&str>,
    ) -> anyhow::Result<(String, String)>;

    async fn update_task_status(&self, id: &str, status: TaskStatus) -> anyhow::Result<()>;

    async fn store_reflection_note(
        &self,
        content: &str,
        task_id: Option<&str>,
    ) -> anyhow::Result<String>;

    async fn store_outcome_note(
        &self,
        content: &str,
        task_id: Option<&str>,
    ) -> anyhow::Result<String>;

    /// If all subtasks of the parent are completed, auto-complete the parent too.
    /// Returns `Some(parent_id)` if the parent was auto-completed, `None` otherwise.
    async fn auto_complete_parent_if_done(&self, task_id: &str) -> anyhow::Result<Option<String>>;

    /// Return recent tasks for duplicate detection.
    ///
    /// Returns tasks created within `days_lookback` days with active or terminal
    /// statuses.  Similarity scoring is the caller's responsibility.
    async fn find_similar_tasks(&self, days_lookback: u32) -> anyhow::Result<Vec<Task>>;
}

// ============================================================================
// WorkingMemoryStore
// ============================================================================

/// Low-level storage operations for `WorkingMemorySkill`.
///
/// These map directly to the Cypher queries in the skill.
#[async_trait]
pub trait WorkingMemoryStore: Send + Sync {
    /// Insert a new working-memory entry and return the `turn_index` assigned.
    async fn push_entry(
        &self,
        id: &str,
        session_id: &str,
        content: &str,
        role: &str,
        ts: &str,
    ) -> anyhow::Result<i64>;

    /// Return entries for `session_id` ordered by turn, capped at `limit`.
    async fn get_entries(&self, session_id: &str, limit: usize) -> anyhow::Result<Vec<Value>>;

    /// Return session summaries (session_id, started_at, msg_count, title).
    async fn list_sessions(&self, limit: i64) -> anyhow::Result<Vec<Value>>;

    /// Mark a session archived (hidden from default list but preserved for training data).
    async fn archive_session(&self, session_id: &str) -> anyhow::Result<()>;
}

#[async_trait::async_trait]
impl WorkingMemoryStore for Neo4jClient {
    async fn push_entry(
        &self,
        id: &str,
        session_id: &str,
        content: &str,
        role: &str,
        ts: &str,
    ) -> anyhow::Result<i64> {
        let cypher = r#"
        OPTIONAL MATCH (w:WorkingMemory {session_id: $session_id})
        WITH COALESCE(max(w.turn_index), -1) + 1 AS next_turn
        CREATE (wm:WorkingMemory {
            id: $id,
            session_id: $session_id,
            content: $content,
            role: $role,
            turn_index: next_turn,
            created_at: datetime($ts)
        })
        RETURN wm.turn_index AS turn_index
        "#;

        let q = neo4rs::query(cypher)
            .param("id", id)
            .param("session_id", session_id)
            .param("content", content)
            .param("role", role)
            .param("ts", ts);

        let rows = self
            .execute(q)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        let turn_index = rows
            .first()
            .and_then(|r| r.get::<i64>("turn_index").ok())
            .unwrap_or(0);
        Ok(turn_index)
    }

    async fn get_entries(&self, session_id: &str, limit: usize) -> anyhow::Result<Vec<Value>> {
        let cypher = r#"
        MATCH (w:WorkingMemory {session_id: $session_id})
        RETURN w.turn_index AS turn, w.role AS role, w.content AS content,
               w.feedback_score AS feedback
        ORDER BY w.turn_index ASC LIMIT $limit
        "#;

        let q = neo4rs::query(cypher)
            .param("session_id", session_id)
            .param("limit", limit as i64);

        let rows = self
            .execute(q)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(rows
            .iter()
            .map(|row| {
                json!({
                    "turn": row.get::<i64>("turn").unwrap_or(0),
                    "role": row.get::<String>("role").unwrap_or_default(),
                    "content": row.get::<String>("content").unwrap_or_default(),
                    "feedback": row.get::<i64>("feedback").ok(),
                })
            })
            .collect())
    }

    async fn list_sessions(&self, limit: i64) -> anyhow::Result<Vec<Value>> {
        // `task_goal` is what makes an agent-created session identifiable in the
        // History list. Chains name their scratch sessions `<kind>-{{task_id}}`
        // (`gap-`, `video-`, `offgrid-`, `slmwatch-`), so the id after the first
        // `-` is an indexed Task lookup that yields the actual topic — "fill
        // knowledge gap: AI Governance and Biosecurity Protocols" instead of a
        // uuid. Date-scoped sessions (`news-`, `todos-`, `claims-`) match no
        // Task and return null; the UI labels those from the kind plus the date.
        //
        // LIMIT lands before the OPTIONAL MATCHes so the Task lookups run for
        // the returned page only, not for every session in the graph.
        let cypher = r#"
        MATCH (w:WorkingMemory)
        WHERE w.archived IS NULL OR w.archived = false
        WITH w.session_id AS sid,
             toString(min(w.created_at)) AS started_at,
             count(w) AS msg_count
        ORDER BY started_at DESC
        LIMIT $limit
        OPTIONAL MATCH (first:WorkingMemory {session_id: sid, turn_index: 0})
        WHERE first.archived IS NULL OR first.archived = false
        WITH sid, started_at, msg_count, first,
             substring(sid, size(split(sid, '-')[0]) + 1) AS task_id
        OPTIONAL MATCH (t:Task {id: task_id})
        RETURN sid AS session_id, started_at, msg_count,
               COALESCE(first.content, sid) AS title,
               t.goal AS task_goal
        ORDER BY started_at DESC
        "#;

        let q = neo4rs::query(cypher).param("limit", limit);
        let rows = self
            .execute(q)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(rows
            .iter()
            .map(|row| {
                json!({
                    "session_id": row.get::<String>("session_id").unwrap_or_default(),
                    "started_at": row.get::<String>("started_at").unwrap_or_default(),
                    "msg_count":  row.get::<i64>("msg_count").unwrap_or(0),
                    "title":      row.get::<String>("title").unwrap_or_default(),
                    // Null for every session whose id is not `<kind>-<task_id>`.
                    "task_goal":  row.get::<String>("task_goal").ok()
                })
            })
            .collect())
    }

    async fn archive_session(&self, session_id: &str) -> anyhow::Result<()> {
        let q = neo4rs::query(
            "MATCH (w:WorkingMemory {session_id: $session_id}) SET w.archived = true",
        )
        .param("session_id", session_id);
        self.run(q).await.map_err(|e| anyhow::anyhow!("{}", e))
    }
}

// ============================================================================
// TaskStore
// ============================================================================

#[async_trait::async_trait]
impl TaskStore for Neo4jClient {
    async fn start_run(
        &self,
        workflow: Option<&str>,
        inputs: &serde_json::Map<String, serde_json::Value>,
        goal: &str,
        context: Option<&str>,
        success_criteria: Option<&str>,
    ) -> anyhow::Result<(String, String)> {
        crate::workflows::start_run(
            self,
            workflow,
            inputs,
            Some(goal),
            context,
            success_criteria,
        )
        .await
        .map(|r| (r.task_id, r.workflow))
        .map_err(|e| anyhow::anyhow!("{e}"))
    }

    async fn update_task_status(&self, id: &str, status: TaskStatus) -> anyhow::Result<()> {
        Neo4jClient::update_task_status(self, id, status)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn store_reflection_note(
        &self,
        content: &str,
        task_id: Option<&str>,
    ) -> anyhow::Result<String> {
        Neo4jClient::store_reflection_note(self, content, task_id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn store_outcome_note(
        &self,
        content: &str,
        task_id: Option<&str>,
    ) -> anyhow::Result<String> {
        Neo4jClient::store_outcome_note(self, content, task_id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn auto_complete_parent_if_done(&self, task_id: &str) -> anyhow::Result<Option<String>> {
        // Find parent task; if all subtasks are completed, complete the parent too.
        let cypher = r#"
        MATCH (child:Task {id: $task_id})<-[:SUBTASK_OF]-(parent:Task)
        WHERE parent.status <> 'completed'
        WITH parent
        MATCH (sibling:Task)-[:SUBTASK_OF]->(parent)
        WITH parent, collect(sibling.status) AS statuses
        WHERE ALL(s IN statuses WHERE s = 'completed')
        SET parent.status = 'completed'
        RETURN parent.id AS parent_id
        "#;

        let q = neo4rs::query(cypher).param("task_id", task_id);
        let rows = self
            .execute(q)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(rows.first().and_then(|r| r.get::<String>("parent_id").ok()))
    }

    async fn find_similar_tasks(&self, days_lookback: u32) -> anyhow::Result<Vec<Task>> {
        Neo4jClient::find_similar_tasks(self, days_lookback)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}
