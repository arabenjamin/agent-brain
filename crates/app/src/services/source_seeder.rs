use std::path::Path;

use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::repository::Neo4jClient;

#[derive(Debug, Deserialize)]
struct SourceListFile {
    name: String,
    description: String,
    domains: Vec<String>,
}

/// Read every `*.yaml` file from `dir`, parse each as a SourceList definition,
/// and seed it as a `(:SourceList)` node **if absent** (ON CREATE only).
///
/// Unlike schedules, the graph owns each list after first creation — runtime
/// edits via `neo4j_query` (`SET s.domains = [...]`) persist across restarts.
/// Delete a node to re-seed it from its YAML file.
///
/// Non-fatal throughout: a missing directory or a bad file is logged and
/// skipped — `search_web` degrades gracefully when a `source_list` name does
/// not resolve (the query simply runs unrestricted).
/// Returns the number of lists newly created.
pub async fn seed_sources_from_dir(neo4j: &Neo4jClient, dir: &Path) -> usize {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            warn!(dir = %dir.display(), error = %e, "seed_sources: cannot read sources directory");
            return 0;
        }
    };

    let mut created = 0usize;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_sources: cannot read file");
                continue;
            }
        };

        let list: SourceListFile = match serde_yaml::from_str(&text) {
            Ok(l) => l,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_sources: YAML parse error");
                continue;
            }
        };

        match neo4j
            .seed_source_list_if_absent(&list.name, &list.domains, &list.description)
            .await
        {
            Ok(true) => {
                info!(
                    name = %list.name,
                    domains = list.domains.len(),
                    "Seeded SourceList (graph-owned from now on)"
                );
                created += 1;
            }
            Ok(false) => {
                debug!(name = %list.name, "SourceList already exists — graph copy kept");
            }
            Err(e) => {
                warn!(name = %list.name, error = %e, "seed_sources: seed failed");
            }
        }
    }

    created
}
