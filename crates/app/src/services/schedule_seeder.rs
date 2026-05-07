use std::path::Path;

use serde::Deserialize;
use tracing::{info, warn};

use crate::repository::Neo4jClient;

#[derive(Debug, Deserialize)]
struct ScheduleFile {
    name: String,
    description: String,
    interval_seconds: i64,
    steps: Vec<serde_json::Value>,
}

/// Read every `*.yaml` file from `dir`, parse each as a ScheduledTask definition,
/// and upsert the task into Neo4j (creating it if absent, force-updating steps otherwise).
///
/// Preserves the force-update behavior so step changes propagate to existing deployments.
/// Non-fatal: parse or upsert errors for a single file are logged and skipped.
/// Returns the number of tasks successfully processed.
pub async fn seed_schedules_from_dir(neo4j: &Neo4jClient, dir: &Path) -> anyhow::Result<usize> {
    let entries = std::fs::read_dir(dir)
        .map_err(|e| anyhow::anyhow!("Cannot read schedules directory {}: {}", dir.display(), e))?;

    let mut seeded = 0usize;

    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
            continue;
        }

        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_schedules: cannot read file");
                continue;
            }
        };

        let schedule: ScheduleFile = match serde_yaml::from_str(&text) {
            Ok(s) => s,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_schedules: YAML parse error");
                continue;
            }
        };

        let steps_json = match serde_json::to_string(&schedule.steps) {
            Ok(s) => s,
            Err(e) => {
                warn!(name = %schedule.name, error = %e, "seed_schedules: steps serialization error");
                continue;
            }
        };

        match neo4j
            .seed_scheduled_task_if_absent(
                &schedule.name,
                Some(schedule.description.as_str()),
                schedule.interval_seconds,
                &steps_json,
            )
            .await
        {
            Ok((id, true)) => {
                info!(name = %schedule.name, id = %id, "Seeded ScheduledTask");
            }
            Ok((_, false)) => {
                // Already exists — force-update steps so definition changes propagate.
                match neo4j
                    .update_scheduled_task_steps(&schedule.name, &steps_json)
                    .await
                {
                    Ok(true) => {
                        info!(name = %schedule.name, "Updated ScheduledTask steps");
                    }
                    Ok(false) => {
                        warn!(name = %schedule.name, "ScheduledTask not found during step update");
                    }
                    Err(e) => {
                        warn!(name = %schedule.name, error = %e, "Failed to update ScheduledTask steps");
                    }
                }
            }
            Err(e) => {
                warn!(name = %schedule.name, error = %e, "seed_schedules: upsert failed");
                continue;
            }
        }

        seeded += 1;
    }

    Ok(seeded)
}
