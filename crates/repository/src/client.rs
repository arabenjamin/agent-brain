use neo4rs::{ConfigBuilder, Graph, Query, Row};
use std::sync::Arc;

use agent_brain_protocol::graph_schema;

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

    /// Initialize the database schema: core's constraints and indexes, from
    /// `plugins/core/schemas/graph/schema.cypher` (read at compile time). A
    /// plugin applies its own bundle's file in its `start`.
    pub async fn init_schema(&self) -> Result<()> {
        for statement in graph_schema::statements(agent_brain_protocol::graph_schema!(core)) {
            if let Err(e) = self.graph.run(neo4rs::query(&statement)).await {
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
                    related_session_id: $session_id, created_at: datetime($created_at), read: false
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
                    serde_json::Value::String(
                        crate::temporal::node_ts(&node, "created_at").unwrap_or_default(),
                    ),
                );
                obj.insert(
                    "read".into(),
                    serde_json::Value::Bool(node.get::<bool>("read").unwrap_or(false)),
                );
                obj.insert(
                    "needs_action".into(),
                    serde_json::Value::Bool(node.get::<bool>("needs_action").unwrap_or(false)),
                );
                obj.insert(
                    "item_id".into(),
                    node.get::<String>("item_id")
                        .ok()
                        .filter(|s| !s.is_empty())
                        .map_or(serde_json::Value::Null, serde_json::Value::String),
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
                    "MATCH (n:AgentNotification {id: $id}) SET n.read = true, n.read_at = datetime($ts)",
                )
                .param("id", id)
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

    /// Seed a SourceList node if absent (MERGE on name, ON CREATE SET only).
    /// The graph owns the node after creation — runtime edits to `domains`
    /// persist across restarts. Returns true when the node was created.
    ///
    /// `role` is absent for an approved list (search restriction, "established"
    /// tier); `platform` and `state_media` lists classify claim evidence instead.
    pub async fn seed_source_list_if_absent(
        &self,
        name: &str,
        domains: &[String],
        description: &str,
        role: Option<&str>,
    ) -> Result<bool> {
        let now = chrono::Utc::now().to_rfc3339();
        let rows = self
            .execute(
                neo4rs::query(
                    "MERGE (s:SourceList {name: $name}) \
                     ON CREATE SET s.domains = $domains, s.description = $description, \
                                   s.role = $role, \
                                   s.created_at = datetime($now), s.updated_at = datetime($now) \
                     RETURN s.created_at = datetime($now) AS created",
                )
                .param("name", name)
                .param("role", role)
                .param("domains", domains.to_vec())
                .param("description", description)
                .param("now", now.as_str()),
            )
            .await?;
        let created = rows
            .into_iter()
            .next()
            .and_then(|row| row.get::<bool>("created").ok())
            .unwrap_or(false);
        Ok(created)
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
