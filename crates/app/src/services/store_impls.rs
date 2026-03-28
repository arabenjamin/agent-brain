//! WorkingMemoryStore and ProcedureStore implementations on Neo4jClient.

use serde_json::{json, Value};

use crate::models::{Task, TaskStatus};
use crate::repository::Neo4jClient;
use crate::services::traits::{ProcedureStore, TaskStore, WorkingMemoryStore};

// ============================================================================
// WorkingMemoryStore
// ============================================================================

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

        let rows = self.execute(q).await.map_err(|e| anyhow::anyhow!("{}", e))?;
        let turn_index = rows
            .first()
            .and_then(|r| r.get::<i64>("turn_index").ok())
            .unwrap_or(0);
        Ok(turn_index)
    }

    async fn get_entries(&self, session_id: &str, limit: usize) -> anyhow::Result<Vec<Value>> {
        let cypher = r#"
        MATCH (w:WorkingMemory {session_id: $session_id})
        RETURN w.turn_index AS turn, w.role AS role, w.content AS content
        ORDER BY w.turn_index ASC LIMIT $limit
        "#;

        let q = neo4rs::query(cypher)
            .param("session_id", session_id)
            .param("limit", limit as i64);

        let rows = self.execute(q).await.map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(rows
            .iter()
            .map(|row| {
                json!({
                    "turn": row.get::<i64>("turn").unwrap_or(0),
                    "role": row.get::<String>("role").unwrap_or_default(),
                    "content": row.get::<String>("content").unwrap_or_default()
                })
            })
            .collect())
    }

    async fn list_sessions(&self, limit: i64) -> anyhow::Result<Vec<Value>> {
        let cypher = r#"
        MATCH (w:WorkingMemory)
        WITH w.session_id AS sid,
             toString(min(w.created_at)) AS started_at,
             count(w) AS msg_count
        OPTIONAL MATCH (first:WorkingMemory {session_id: sid, turn_index: 0})
        RETURN sid AS session_id, started_at, msg_count,
               COALESCE(first.content, sid) AS title
        ORDER BY started_at DESC
        LIMIT $limit
        "#;

        let q = neo4rs::query(cypher).param("limit", limit);
        let rows = self.execute(q).await.map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(rows
            .iter()
            .map(|row| {
                json!({
                    "session_id": row.get::<String>("session_id").unwrap_or_default(),
                    "started_at": row.get::<String>("started_at").unwrap_or_default(),
                    "msg_count":  row.get::<i64>("msg_count").unwrap_or(0),
                    "title":      row.get::<String>("title").unwrap_or_default()
                })
            })
            .collect())
    }

    async fn get_all_entries(&self, session_id: &str) -> anyhow::Result<Vec<Value>> {
        let cypher = r#"
        MATCH (w:WorkingMemory {session_id: $session_id})
        RETURN w.turn_index AS turn, w.role AS role, w.content AS content
        ORDER BY w.turn_index ASC
        "#;

        let q = neo4rs::query(cypher).param("session_id", session_id);
        let rows = self.execute(q).await.map_err(|e| anyhow::anyhow!("{}", e))?;
        Ok(rows
            .iter()
            .map(|row| {
                json!({
                    "turn": row.get::<i64>("turn").unwrap_or(0),
                    "role": row.get::<String>("role").unwrap_or_default(),
                    "content": row.get::<String>("content").unwrap_or_default()
                })
            })
            .collect())
    }

    async fn delete_session(&self, session_id: &str) -> anyhow::Result<()> {
        let q = neo4rs::query(
            "MATCH (w:WorkingMemory {session_id: $session_id}) DETACH DELETE w",
        )
        .param("session_id", session_id);
        self.run(q).await.map_err(|e| anyhow::anyhow!("{}", e))
    }
}

// ============================================================================
// ProcedureStore
// ============================================================================

#[async_trait::async_trait]
impl ProcedureStore for Neo4jClient {
    async fn store_procedure(
        &self,
        id: &str,
        name: &str,
        description: &str,
        steps_json: &str,
        timestamp: &str,
    ) -> anyhow::Result<()> {
        let cypher = r#"
        CREATE (p:Procedure {
            id: $id,
            name: $name,
            description: $description,
            steps: $steps,
            created_at: datetime($timestamp)
        })
        "#;

        let q = neo4rs::query(cypher)
            .param("id", id)
            .param("name", name)
            .param("description", description)
            .param("steps", steps_json)
            .param("timestamp", timestamp);

        self.run(q).await.map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn search_procedures(&self, query_str: &str, limit: usize) -> anyhow::Result<Vec<Value>> {
        let cypher = r#"
        MATCH (p:Procedure)
        WHERE toLower(p.name) CONTAINS toLower($query)
           OR toLower(p.description) CONTAINS toLower($query)
        RETURN p.id AS id, p.name AS name, p.description AS description, p.steps AS steps
        LIMIT $limit
        "#;

        let q = neo4rs::query(cypher)
            .param("query", query_str)
            .param("limit", limit as i64);

        let rows = self.execute(q).await.map_err(|e| anyhow::anyhow!("{}", e))?;
        let mut procedures = Vec::new();
        for row in rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let name = row.get::<String>("name").unwrap_or_default();
            let description = row.get::<String>("description").unwrap_or_default();
            let steps_str = row
                .get::<String>("steps")
                .unwrap_or_else(|_| "[]".to_string());
            let steps: Value = serde_json::from_str(&steps_str).unwrap_or(json!([]));
            procedures.push(json!({
                "id": id,
                "name": name,
                "description": description,
                "steps": steps
            }));
        }
        Ok(procedures)
    }
}

// ============================================================================
// TaskStore
// ============================================================================

#[async_trait::async_trait]
impl TaskStore for Neo4jClient {
    async fn create_task(&self, goal: &str, context: Option<&str>) -> anyhow::Result<String> {
        Neo4jClient::create_task(self, goal, context)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn get_task(&self, id: &str) -> anyhow::Result<Option<Task>> {
        Neo4jClient::get_task(self, id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn link_subtask(&self, parent_id: &str, child_id: &str) -> anyhow::Result<()> {
        Neo4jClient::link_subtask(self, parent_id, child_id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn link_task_dependency(&self, from_id: &str, to_id: &str) -> anyhow::Result<()> {
        Neo4jClient::link_task_dependency(self, from_id, to_id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn update_task_status(&self, id: &str, status: TaskStatus) -> anyhow::Result<()> {
        Neo4jClient::update_task_status(self, id, status)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn store_reflection_note(&self, content: &str, task_id: Option<&str>) -> anyhow::Result<String> {
        Neo4jClient::store_reflection_note(self, content, task_id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn store_outcome_note(&self, content: &str, task_id: Option<&str>) -> anyhow::Result<String> {
        Neo4jClient::store_outcome_note(self, content, task_id)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }

    async fn list_tasks(&self, status: Option<&str>, limit: usize) -> anyhow::Result<Vec<Value>> {
        Neo4jClient::list_tasks(self, status, limit)
            .await
            .map_err(|e| anyhow::anyhow!("{}", e))
    }
}
