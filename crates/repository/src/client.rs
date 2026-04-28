use neo4rs::{ConfigBuilder, Graph, Query, Row};
use std::sync::Arc;

use super::error::Result;

/// Neo4j database client wrapper.
#[derive(Clone)]
pub struct Neo4jClient {
    graph: Arc<Graph>,
}

impl Neo4jClient {
    /// Create a new Neo4j client connection.
    pub async fn new(uri: &str, username: &str, password: &str) -> Result<Self> {
        let config = ConfigBuilder::default()
            .uri(uri)
            .user(username)
            .password(password)
            .build()?;

        let graph = Graph::connect(config).await?;

        Ok(Self {
            graph: Arc::new(graph),
        })
    }

    /// Get a reference to the underlying graph.
    pub fn graph(&self) -> &Graph {
        &self.graph
    }

    /// Initialize the database schema with constraints and indexes.
    pub async fn init_schema(&self) -> Result<()> {
        // ... (existing schema init)
        let constraints = [
            "CREATE CONSTRAINT scheduled_task_id IF NOT EXISTS FOR (s:ScheduledTask) REQUIRE s.id IS UNIQUE",
            "CREATE CONSTRAINT scheduled_task_name IF NOT EXISTS FOR (s:ScheduledTask) REQUIRE s.name IS UNIQUE",
            "CREATE CONSTRAINT procedure_id IF NOT EXISTS FOR (p:Procedure) REQUIRE p.id IS UNIQUE",
            "CREATE CONSTRAINT working_memory_id IF NOT EXISTS FOR (w:WorkingMemory) REQUIRE w.id IS UNIQUE",
            "CREATE CONSTRAINT entity_name IF NOT EXISTS FOR (e:Entity) REQUIRE e.name IS UNIQUE",
            "CREATE CONSTRAINT dynamic_tool_name IF NOT EXISTS FOR (d:DynamicTool) REQUIRE d.name IS UNIQUE",
            "CREATE CONSTRAINT agent_job_id IF NOT EXISTS FOR (j:AgentJob) REQUIRE j.id IS UNIQUE",
            "CREATE CONSTRAINT todo_id IF NOT EXISTS FOR (t:Todo) REQUIRE t.id IS UNIQUE",
            // Note: ModelSpec nodes removed — model registry now lives in DuckDB model_registry table
        ];

        let indexes = [
            // Vector index for RAG (bge-m3 = 1024 dim)
            "CREATE VECTOR INDEX note_embeddings IF NOT EXISTS FOR (n:Note) ON (n.embedding) OPTIONS {indexConfig: {`vector.dimensions`: 1024, `vector.similarity_function`: 'cosine'}}",
            // Spaced repetition index
            "CREATE INDEX note_next_review IF NOT EXISTS FOR (n:Note) ON (n.next_review_at)",
            // Full-text index for BM25 hybrid search
            "CREATE FULLTEXT INDEX note_content_fulltext IF NOT EXISTS FOR (n:Note) ON EACH [n.content]",
            // Working memory session index
            "CREATE INDEX working_memory_session IF NOT EXISTS FOR (w:WorkingMemory) ON (w.session_id)",
            // Entity type index
            "CREATE INDEX entity_type_idx IF NOT EXISTS FOR (e:Entity) ON (e.entity_type)",
            // Dynamic tool creation timestamp index
            "CREATE INDEX dynamic_tool_idx IF NOT EXISTS FOR (d:DynamicTool) ON (d.created_at)",
            // Agent job indexes
            "CREATE INDEX agent_job_status IF NOT EXISTS FOR (j:AgentJob) ON (j.status)",
            "CREATE INDEX agent_job_priority IF NOT EXISTS FOR (j:AgentJob) ON (j.priority)",
            "CREATE INDEX agent_job_created IF NOT EXISTS FOR (j:AgentJob) ON (j.created_at)",
            "CREATE INDEX todo_status IF NOT EXISTS FOR (t:Todo) ON (t.status)",
            "CREATE INDEX todo_priority IF NOT EXISTS FOR (t:Todo) ON (t.priority)",
            // AgentNotification index
            "CREATE INDEX agent_notification_read IF NOT EXISTS FOR (n:AgentNotification) ON (n.read)",
            "CREATE INDEX agent_notification_created IF NOT EXISTS FOR (n:AgentNotification) ON (n.created_at)",
            // Note: model_spec_provider index removed — model registry lives in DuckDB
        ];

        for statement in constraints.iter().chain(indexes.iter()) {
            if let Err(e) = self.graph.run(neo4rs::query(statement)).await {
                // Ignore "equivalent schema already exists" — happens when a
                // matching index/constraint was created with a different name.
                // IF NOT EXISTS only guards by name, not by schema definition.
                if !is_schema_already_exists_error(&e) {
                    return Err(e.into());
                }
            }
        }

        Ok(())
    }

    // ========================================================================
    // AgentNotification CRUD
    // ========================================================================

    /// Create a new notification that the brain wants to show the user.
    pub async fn create_notification(
        &self,
        id: &str,
        message: &str,
        context: Option<&str>,
        related_session_id: Option<&str>,
        created_at: &str,
    ) -> Result<()> {
        self.graph
            .run(
                neo4rs::query(
                    "CREATE (n:AgentNotification {
                    id: $id, message: $message, context: $context,
                    related_session_id: $session_id, created_at: $created_at, read: false
                })",
                )
                .param("id", id)
                .param("message", message)
                .param("context", context.unwrap_or(""))
                .param("session_id", related_session_id.unwrap_or(""))
                .param("created_at", created_at),
            )
            .await?;
        Ok(())
    }

    /// List notifications, optionally filtering to unread only.
    pub async fn list_notifications(&self, unread_only: bool) -> Result<Vec<serde_json::Value>> {
        let cypher = if unread_only {
            "MATCH (n:AgentNotification) WHERE n.read = false \
             RETURN n ORDER BY n.created_at DESC LIMIT 100"
        } else {
            "MATCH (n:AgentNotification) \
             RETURN n ORDER BY n.created_at DESC LIMIT 100"
        };
        let rows = self.execute(neo4rs::query(cypher)).await?;
        let mut out = Vec::new();
        for row in rows {
            if let Ok(node) = row.get::<neo4rs::Node>("n") {
                let mut obj = serde_json::Map::new();
                obj.insert(
                    "id".into(),
                    serde_json::Value::String(node.get::<String>("id").unwrap_or_default()),
                );
                obj.insert(
                    "message".into(),
                    serde_json::Value::String(node.get::<String>("message").unwrap_or_default()),
                );
                obj.insert(
                    "context".into(),
                    serde_json::Value::String(node.get::<String>("context").unwrap_or_default()),
                );
                obj.insert(
                    "related_session_id".into(),
                    serde_json::Value::String(
                        node.get::<String>("related_session_id").unwrap_or_default(),
                    ),
                );
                obj.insert(
                    "created_at".into(),
                    serde_json::Value::String(node.get::<String>("created_at").unwrap_or_default()),
                );
                obj.insert(
                    "read".into(),
                    serde_json::Value::Bool(node.get::<bool>("read").unwrap_or(false)),
                );
                out.push(serde_json::Value::Object(obj));
            }
        }
        Ok(out)
    }

    /// Mark a single notification as read.
    pub async fn mark_notification_read(&self, id: &str) -> Result<()> {
        self.graph
            .run(
                neo4rs::query(
                    "MATCH (n:AgentNotification {id: $id}) SET n.read = true, n.read_at = $ts",
                )
                .param("id", id)
                .param("ts", chrono::Utc::now().to_rfc3339().as_str()),
            )
            .await?;
        Ok(())
    }

    /// Mark all unread notifications as read.
    pub async fn mark_all_notifications_read(&self) -> Result<()> {
        self.graph
            .run(
                neo4rs::query(
                    "MATCH (n:AgentNotification {read: false}) \
                     SET n.read = true, n.read_at = $ts",
                )
                .param("ts", chrono::Utc::now().to_rfc3339().as_str()),
            )
            .await?;
        Ok(())
    }

    /// Execute a query that returns rows.
    pub async fn execute(&self, query: Query) -> Result<Vec<Row>> {
        let mut result = self.graph.execute(query).await?;
        let mut rows = Vec::new();
        while let Some(row) = result.next().await? {
            rows.push(row);
        }
        Ok(rows)
    }

    /// Execute a query that modifies the database (no return value).
    pub async fn run(&self, query: Query) -> Result<()> {
        self.graph.run(query).await?;
        Ok(())
    }

    /// Fetch the domain list for a named SourceList node.
    /// Returns an empty vec if the node doesn't exist yet.
    pub async fn get_source_list(&self, name: &str) -> Result<Vec<String>> {
        let rows = self
            .execute(
                neo4rs::query("MATCH (s:SourceList {name: $name}) RETURN s.domains AS domains")
                    .param("name", name),
            )
            .await?;
        let Some(row) = rows.into_iter().next() else {
            return Ok(vec![]);
        };
        let domains: Vec<String> = row.get::<Vec<String>>("domains").unwrap_or_default();
        Ok(domains)
    }

    /// Upsert a SourceList node (MERGE on name, SET domains + description).
    pub async fn upsert_source_list(
        &self,
        name: &str,
        domains: &[String],
        description: &str,
    ) -> Result<()> {
        let now = chrono::Utc::now().to_rfc3339();
        self.run(
            neo4rs::query(
                "MERGE (s:SourceList {name: $name}) \
                 ON CREATE SET s.created_at = $now \
                 SET s.domains = $domains, s.description = $description, s.updated_at = $now",
            )
            .param("name", name)
            .param("domains", domains.to_vec())
            .param("description", description)
            .param("now", now.as_str()),
        )
        .await
    }
}

/// Returns true if the neo4rs error is a "schema already exists" error.
/// Neo4j raises `EquivalentSchemaRuleAlreadyExists` when an index or constraint
/// with the same schema (label + property) exists under a *different* name,
/// even though the Cypher statement used `IF NOT EXISTS`.
fn is_schema_already_exists_error(e: &neo4rs::Error) -> bool {
    if let neo4rs::Error::Neo4j(neo4j_err) = e {
        return neo4j_err
            .code()
            .contains("EquivalentSchemaRuleAlreadyExists");
    }
    false
}
