//! The graph as this plugin uses it: the host's graph (through plugin-api's
//! neo4rs-shaped shim) and the core's entity merge candidates. Code ported
//! from the core keeps its `neo4j.execute(query(..).param(..))` shape.

use std::sync::Arc;

use agent_brain_plugin_api::{Entities, Graph};
use anyhow::Result;
use serde_json::Value;

pub use agent_brain_plugin_api::graph::{Query, Row, query};

#[derive(Clone)]
pub struct Db {
    graph: Arc<dyn Graph>,
    entities: Option<Arc<dyn Entities>>,
}

impl Db {
    pub fn new(graph: Arc<dyn Graph>, entities: Option<Arc<dyn Entities>>) -> Self {
        Self { graph, entities }
    }

    pub async fn execute(&self, q: Query) -> Result<Vec<Row>> {
        self.graph.execute(q).await
    }

    pub async fn run(&self, q: Query) -> Result<()> {
        self.graph.run(q).await
    }

    /// The core's merge candidates (`{candidates: [...]}`); empty when the
    /// host gave no entities.
    pub async fn merge_candidates(&self, entity: Option<&str>, limit: usize) -> Result<Value> {
        match &self.entities {
            Some(e) => e.merge_candidates(entity, limit).await,
            None => Ok(serde_json::json!({ "candidates": [] })),
        }
    }
}
