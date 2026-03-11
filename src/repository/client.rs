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
            "CREATE CONSTRAINT resource_id IF NOT EXISTS FOR (r:Resource) REQUIRE r.id IS UNIQUE",
            "CREATE CONSTRAINT endpoint_id IF NOT EXISTS FOR (e:Endpoint) REQUIRE e.id IS UNIQUE",
            "CREATE CONSTRAINT schema_id IF NOT EXISTS FOR (s:Schema) REQUIRE s.id IS UNIQUE",
            "CREATE CONSTRAINT parameter_id IF NOT EXISTS FOR (p:Parameter) REQUIRE p.id IS UNIQUE",
            "CREATE CONSTRAINT healing_event_id IF NOT EXISTS FOR (h:HealingEvent) REQUIRE h.id IS UNIQUE",
            "CREATE CONSTRAINT api_credential_api_name IF NOT EXISTS FOR (c:ApiCredential) REQUIRE c.api_name IS UNIQUE",
            "CREATE CONSTRAINT procedure_id IF NOT EXISTS FOR (p:Procedure) REQUIRE p.id IS UNIQUE",
            "CREATE CONSTRAINT working_memory_id IF NOT EXISTS FOR (w:WorkingMemory) REQUIRE w.id IS UNIQUE",
            "CREATE CONSTRAINT entity_name IF NOT EXISTS FOR (e:Entity) REQUIRE e.name IS UNIQUE",
            "CREATE CONSTRAINT dynamic_tool_name IF NOT EXISTS FOR (d:DynamicTool) REQUIRE d.name IS UNIQUE",
            "CREATE CONSTRAINT agent_job_id IF NOT EXISTS FOR (j:AgentJob) REQUIRE j.id IS UNIQUE",
            "CREATE CONSTRAINT model_spec_name IF NOT EXISTS FOR (m:ModelSpec) REQUIRE m.name IS UNIQUE",
        ];

        let indexes = [
            "CREATE INDEX endpoint_path IF NOT EXISTS FOR (e:Endpoint) ON (e.path)",
            "CREATE INDEX endpoint_method IF NOT EXISTS FOR (e:Endpoint) ON (e.method)",
            "CREATE INDEX schema_name IF NOT EXISTS FOR (s:Schema) ON (s.name)",
            "CREATE INDEX parameter_name IF NOT EXISTS FOR (p:Parameter) ON (p.name)",
            // Vector index for RAG (bge-m3 = 1024 dim)
            "CREATE VECTOR INDEX note_embeddings IF NOT EXISTS FOR (n:Note) ON (n.embedding) OPTIONS {indexConfig: {`vector.dimensions`: 1024, `vector.similarity_function`: 'cosine'}}",
            // Vector index for Endpoints
            "CREATE VECTOR INDEX endpoint_embeddings IF NOT EXISTS FOR (e:Endpoint) ON (e.embedding) OPTIONS {indexConfig: {`vector.dimensions`: 1024, `vector.similarity_function`: 'cosine'}}",
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
            "CREATE INDEX model_spec_provider IF NOT EXISTS FOR (m:ModelSpec) ON (m.provider)",
        ];

        for constraint in constraints {
            self.graph.run(neo4rs::query(constraint)).await?;
        }

        for index in indexes {
            self.graph.run(neo4rs::query(index)).await?;
        }

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
}
