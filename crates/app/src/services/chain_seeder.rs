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
    #[serde(default)]
    no_adversarial: bool,
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
                          c.no_adversarial    = $no_adversarial, \
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
                    .param("no_adversarial", chain.no_adversarial)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn chains_dir() -> std::path::PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../chains")
    }

    /// Every shipped chain must deserialize as a `ChainFile`. A missing required
    /// field (e.g. the singular `pattern`) is otherwise only caught at runtime as
    /// a skipped-with-a-warning seed, leaving the chain silently absent.
    #[test]
    fn all_shipped_chains_parse() {
        let dir = chains_dir();
        let mut count = 0;
        for entry in std::fs::read_dir(&dir)
            .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
            .flatten()
        {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let text = std::fs::read_to_string(&path).unwrap();
            let parsed: Result<ChainFile, _> = serde_yaml::from_str(&text);
            assert!(
                parsed.is_ok(),
                "chain {} failed to parse: {:?}",
                path.display(),
                parsed.err()
            );
            count += 1;
        }
        assert!(count > 0, "no chains found at {}", dir.display());
    }

    /// The run-agent front-door chain declares both critic-skipping flags — and
    /// the seeder must now carry `no_adversarial` (it silently dropped it before).
    #[test]
    fn run_agent_chain_skips_the_extra_critics() {
        let text = std::fs::read_to_string(chains_dir().join("run-agent.yaml")).unwrap();
        let chain: ChainFile = serde_yaml::from_str(&text).expect("run-agent.yaml parses");
        assert!(chain.no_evaluator, "run-agent should skip the evaluator");
        assert!(
            chain.no_adversarial,
            "run-agent should skip the adversarial critic"
        );
        assert_eq!(chain.pattern, "run agent:");
    }
}
