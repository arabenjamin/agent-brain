use neo4rs::{ConfigBuilder, Graph};
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
        let constraints = [
            "CREATE CONSTRAINT resource_id IF NOT EXISTS FOR (r:Resource) REQUIRE r.id IS UNIQUE",
            "CREATE CONSTRAINT endpoint_id IF NOT EXISTS FOR (e:Endpoint) REQUIRE e.id IS UNIQUE",
            "CREATE CONSTRAINT schema_id IF NOT EXISTS FOR (s:Schema) REQUIRE s.id IS UNIQUE",
            "CREATE CONSTRAINT parameter_id IF NOT EXISTS FOR (p:Parameter) REQUIRE p.id IS UNIQUE",
            "CREATE CONSTRAINT healing_event_id IF NOT EXISTS FOR (h:HealingEvent) REQUIRE h.id IS UNIQUE",
        ];

        let indexes = [
            "CREATE INDEX endpoint_path IF NOT EXISTS FOR (e:Endpoint) ON (e.path)",
            "CREATE INDEX endpoint_method IF NOT EXISTS FOR (e:Endpoint) ON (e.method)",
            "CREATE INDEX schema_name IF NOT EXISTS FOR (s:Schema) ON (s.name)",
            "CREATE INDEX parameter_name IF NOT EXISTS FOR (p:Parameter) ON (p.name)",
        ];

        for constraint in constraints {
            self.graph.run(neo4rs::query(constraint)).await?;
        }

        for index in indexes {
            self.graph.run(neo4rs::query(index)).await?;
        }

        Ok(())
    }
}
