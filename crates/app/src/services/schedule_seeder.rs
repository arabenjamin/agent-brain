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
    /// Step fields every step gets unless it sets its own.
    #[serde(default)]
    defaults: serde_json::Map<String, serde_json::Value>,
    /// Initial enabled state, honored ONLY when the task is first created
    /// (`enabled` is graph-owned and never force-synced afterward). Absent ⇒
    /// `true`, so existing schedules are unaffected; set `false` to seed a task
    /// OFF for human review before it ever runs.
    #[serde(default)]
    enabled: Option<bool>,
    /// Default data class for every step that does not set its own
    /// (`services/routing.rs`). Absent ⇒ steps stay private.
    #[serde(default)]
    data_class: Option<crate::services::routing::DataClass>,
    /// The `topics/*.yaml` topic this schedule works on; absent = spans topics.
    #[serde(default)]
    topic: Option<String>,
    /// One of `SCHEDULE_KINDS`.
    #[serde(default)]
    kind: Option<String>,
}

/// A schedule's declared `topic` and `kind`, checked: the topic must be one
/// `topics/*.yaml` declares and the kind one of `SCHEDULE_KINDS`. Blank means
/// none. `Err` names every value that is wrong and what is allowed.
pub fn check_labels(
    topic: Option<&str>,
    kind: Option<&str>,
    topic_names: &[String],
) -> Result<(Option<String>, Option<String>), String> {
    let topic = topic.map(str::trim).filter(|t| !t.is_empty());
    let kind = kind.map(str::trim).filter(|k| !k.is_empty());
    let mut errors = Vec::new();
    if let Some(t) = topic
        && !topic_names.iter().any(|n| n == t)
    {
        errors.push(format!(
            "topic '{t}' is not declared in topics/*.yaml (declared: {})",
            topic_names.join(", ")
        ));
    }
    if let Some(k) = kind
        && !agent_brain_models::SCHEDULE_KINDS.contains(&k)
    {
        errors.push(format!(
            "kind '{k}' is not one of {}",
            agent_brain_models::SCHEDULE_KINDS.join(", ")
        ));
    }
    if errors.is_empty() {
        Ok((topic.map(String::from), kind.map(String::from)))
    } else {
        Err(errors.join("; "))
    }
}

/// The topic names `topics/*.yaml` declares.
pub fn declared_topics() -> Vec<String> {
    crate::services::topics::Topics::global()
        .defs
        .iter()
        .map(|d| d.name.clone())
        .collect()
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
    let topic_names = declared_topics();

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

        let mut steps = schedule.steps.clone();
        crate::services::chain_seeder::apply_step_defaults(&mut steps, &schedule.defaults);
        if let Some(class) = schedule.data_class {
            crate::services::routing::stamp_data_class(&mut steps, class);
        }
        let steps_json = match serde_json::to_string(&steps) {
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
                schedule.enabled.unwrap_or(true),
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

        // The file's topic and kind, for a schedule the file owns. A wrong
        // value is dropped with a warning, never stored.
        let (topic, kind) = match check_labels(
            schedule.topic.as_deref(),
            schedule.kind.as_deref(),
            &topic_names,
        ) {
            Ok(labels) => labels,
            Err(e) => {
                warn!(name = %schedule.name, error = %e, "seed_schedules: topic/kind rejected — stored as none");
                let topic = check_labels(schedule.topic.as_deref(), None, &topic_names)
                    .ok()
                    .and_then(|(t, _)| t);
                let kind = check_labels(None, schedule.kind.as_deref(), &topic_names)
                    .ok()
                    .and_then(|(_, k)| k);
                (topic, kind)
            }
        };
        if let Err(e) = neo4j
            .set_scheduled_task_labels(
                (&schedule.name, false),
                topic.as_deref(),
                kind.as_deref(),
                true,
            )
            .await
        {
            warn!(name = %schedule.name, error = %e, "seed_schedules: could not set topic/kind");
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every seed file's step references name an earlier step once its
    /// `defaults:` are applied — the check the lint runs on the live graph.
    #[test]
    fn seed_files_step_references_resolve() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        let mut checked = 0;
        for dir in ["schedules", "chains"] {
            for entry in std::fs::read_dir(root.join(dir)).unwrap().flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                    continue;
                }
                let v: serde_json::Value =
                    serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
                let mut steps = v["steps"].as_array().cloned().unwrap_or_default();
                let defaults = v["defaults"].as_object().cloned().unwrap_or_default();
                crate::services::chain_seeder::apply_step_defaults(&mut steps, &defaults);
                let hard: Vec<String> = crate::services::validation::validate_step_array(
                    &steps,
                    &[],
                    &Default::default(),
                )
                .iter()
                .map(|v| v.render())
                .collect();
                assert!(hard.is_empty(), "{}: {hard:?}", path.display());
                checked += 1;
            }
        }
        assert!(checked > 10);
    }

    #[test]
    fn defaults_fill_only_what_a_step_leaves_unset() {
        let mut steps = vec![
            serde_json::json!({"tool_name": "a"}),
            serde_json::json!({"tool_name": "b", "provider_hint": "ollama-cloud"}),
        ];
        let d = serde_json::json!({"provider_hint": "ollama", "priority": 1});
        crate::services::chain_seeder::apply_step_defaults(&mut steps, d.as_object().unwrap());
        assert_eq!(steps[0]["provider_hint"], "ollama");
        assert_eq!(steps[1]["provider_hint"], "ollama-cloud");
        assert_eq!(steps[1]["priority"], 1);
    }

    #[test]
    fn labels_are_checked_against_the_declared_topics_and_kinds() {
        let names = vec!["ai-ml".to_string(), "michigan".to_string()];
        assert_eq!(
            check_labels(Some("ai-ml"), Some("monitor"), &names),
            Ok((Some("ai-ml".into()), Some("monitor".into())))
        );
        assert_eq!(check_labels(Some(" "), None, &names), Ok((None, None)));
        let err = check_labels(Some("ai"), Some("watch"), &names).unwrap_err();
        assert!(
            err.contains("topic 'ai'") && err.contains("kind 'watch'"),
            "{err}"
        );
        assert!(err.contains("ai-ml, michigan"), "{err}");
    }

    #[test]
    fn every_schedule_file_declares_valid_labels() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../schedules");
        let topics = crate::services::topics::Topics::load_dir(std::path::Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../topics"
        )));
        let names: Vec<String> = topics.defs.iter().map(|d| d.name.clone()).collect();
        for entry in std::fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let file: ScheduleFile =
                serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
            check_labels(file.topic.as_deref(), file.kind.as_deref(), &names)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert!(
                file.kind.is_some(),
                "{}: every schedule declares a kind",
                path.display()
            );
        }
    }
}
