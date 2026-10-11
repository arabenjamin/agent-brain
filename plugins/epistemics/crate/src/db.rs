//! The graph as this plugin uses it: the host's graph through plugin-api's
//! neo4rs-shaped shim, so code ported from the core keeps its
//! `neo4j.execute(query(..).param(..))` shape.

use std::sync::Arc;

use agent_brain_plugin_api::Graph;
use anyhow::Result;

pub use agent_brain_plugin_api::graph::{Query, Row, query};

#[derive(Clone)]
pub struct Db {
    graph: Arc<dyn Graph>,
}

impl Db {
    pub fn new(graph: Arc<dyn Graph>) -> Self {
        Self { graph }
    }

    pub async fn execute(&self, q: Query) -> Result<Vec<Row>> {
        self.graph.execute(q).await
    }

    pub async fn run(&self, q: Query) -> Result<()> {
        self.graph.run(q).await
    }

    /// An episodic note, written as the core's repository writes one
    /// (`store_episodic_note`): the verifier's record of the evidence it read.
    pub async fn store_episodic_note(
        &self,
        content: &str,
        source_context: Option<&str>,
    ) -> Result<String> {
        let id = uuid::Uuid::new_v4().to_string();
        let now = chrono::Utc::now().to_rfc3339();
        self.run(
            query(
                "CREATE (n:Note {id: $id, content: $content, note_type: 'episodic', \
                 source_context: $src, provenance: 'system_record', \
                 created_at: datetime($ts), last_accessed_at: datetime($ts), \
                 access_count: 0, \
                 next_review_at: datetime($ts) + duration({days: 1}), \
                 review_interval_days: 1})",
            )
            .param("id", &id)
            .param("content", content)
            .param("src", source_context.unwrap_or(""))
            .param("ts", &now),
        )
        .await?;
        Ok(id)
    }
}
