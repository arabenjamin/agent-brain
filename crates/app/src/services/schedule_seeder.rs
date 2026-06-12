use std::path::Path;

use serde::Deserialize;
use tracing::{info, warn};

use crate::repository::{Neo4jClient, YamlSyncOutcome};

#[derive(Debug, Deserialize)]
struct ScheduleFile {
    name: String,
    description: String,
    interval_seconds: i64,
    steps: Vec<serde_json::Value>,
}

/// Read every `*.yaml` file from `dir`, parse each as a ScheduledTask definition,
/// and upsert the task into Neo4j with `managed_by = "yaml"` ownership:
///
/// - Absent → created (yaml-owned).
/// - Existing yaml-owned (or legacy without `managed_by`) → steps, description,
///   and interval force-synced so file edits propagate on restart.
/// - Existing runtime-owned → left untouched (name collision is logged).
///
/// After seeding, any node still lacking `managed_by` is backfilled as
/// `"runtime"` — it has no YAML file, so the graph owns it.
///
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
                info!(name = %schedule.name, id = %id, "Seeded ScheduledTask (yaml-owned)");
            }
            Ok((_, false)) => {
                // Already exists — force-sync the definition if the file owns it.
                match neo4j
                    .sync_yaml_scheduled_task(
                        &schedule.name,
                        Some(schedule.description.as_str()),
                        schedule.interval_seconds,
                        &steps_json,
                    )
                    .await
                {
                    Ok(YamlSyncOutcome::Updated) => {
                        info!(name = %schedule.name, "Synced ScheduledTask from YAML");
                    }
                    Ok(YamlSyncOutcome::RuntimeOwned) => {
                        warn!(
                            name = %schedule.name,
                            "ScheduledTask is runtime-owned — YAML definition skipped \
                             (set managed_by='yaml' via manage_scheduled_task to hand it back)"
                        );
                    }
                    Ok(YamlSyncOutcome::NotFound) => {
                        warn!(name = %schedule.name, "ScheduledTask vanished during sync");
                    }
                    Err(e) => {
                        warn!(name = %schedule.name, error = %e, "Failed to sync ScheduledTask from YAML");
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

    // Whatever still lacks an owner has no YAML file backing it — the graph owns it.
    match neo4j.backfill_scheduled_task_managed_by().await {
        Ok(0) => {}
        Ok(n) => info!(
            count = n,
            "Backfilled managed_by='runtime' on ScheduledTasks"
        ),
        Err(e) => warn!(error = %e, "Failed to backfill ScheduledTask managed_by"),
    }

    Ok(seeded)
}
