use std::path::Path;

use serde::Deserialize;
use tracing::{info, warn};
use uuid::Uuid;

use crate::repository::Neo4jClient;

#[derive(Debug, Deserialize)]
struct ChainFile {
    name: String,
    pattern: String,
    #[serde(default)]
    patterns: Vec<String>,
    #[serde(default = "default_priority")]
    priority: i64,
    #[serde(default)]
    description: String,
    #[serde(default)]
    evaluation_rubric: Option<String>,
    #[serde(default)]
    no_evaluator: bool,
    steps: Vec<serde_json::Value>,
}

fn default_priority() -> i64 {
    100
}

/// Read every `*.yaml` file from `dir`, parse each as a chain definition, and
/// MERGE a `SchedulerChain` node for each one.
///
/// Idempotent: re-running updates pattern, steps, priority, and description.
/// Non-fatal: parse or upsert errors for a single file are logged and skipped.
/// Returns the number of chains successfully upserted.
pub async fn seed_chains_from_dir(neo4j: &Neo4jClient, dir: &Path) -> anyhow::Result<usize> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("Cannot read chains directory {}: {}", dir.display(), e))?;

    let mut seeded = 0usize;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_chains: cannot read file");
                continue;
            }
        };

        let chain: ChainFile = match serde_yaml::from_str(&text) {
            Ok(c) => c,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_chains: YAML parse error");
                continue;
            }
        };

        let steps_json = match serde_json::to_string(&chain.steps) {
            Ok(s) => s,
            Err(e) => {
                warn!(name = %chain.name, error = %e, "seed_chains: steps serialization error");
                continue;
            }
        };

        let rubric = chain.evaluation_rubric.unwrap_or_default();

        let cypher = "MERGE (c:SchedulerChain {name: $name}) \
                      ON CREATE SET c.id = $id, c.created_at = datetime() \
                      SET c.pattern           = $pattern, \
                          c.patterns          = $patterns, \
                          c.priority          = $priority, \
                          c.description       = $description, \
                          c.evaluation_rubric = $rubric, \
                          c.no_evaluator      = $no_evaluator, \
                          c.steps             = $steps, \
                          c.updated_at        = datetime()";

        if let Err(e) = neo4j
            .run(
                neo4rs::query(cypher)
                    .param("name", chain.name.as_str())
                    .param("id", Uuid::new_v4().to_string())
                    .param("pattern", chain.pattern.as_str())
                    .param("patterns", chain.patterns.clone())
                    .param("priority", chain.priority)
                    .param("description", chain.description.as_str())
                    .param("rubric", rubric.as_str())
                    .param("no_evaluator", chain.no_evaluator)
                    .param("steps", steps_json.as_str()),
            )
            .await
        {
            warn!(name = %chain.name, error = %e, "seed_chains: Neo4j upsert failed");
            continue;
        }

        info!(name = %chain.name, "Seeded SchedulerChain");
        seeded += 1;
    }

    Ok(seeded)
}
