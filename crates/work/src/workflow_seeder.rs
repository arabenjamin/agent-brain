//! Seeds `(:Workflow)` nodes from the `workflows/*.yaml` of every loaded
//! plugin bundle (`services/plugins.rs`), recording the bundle on the node.
//!
//! A file owns the workflow named after it (`managed_by = "yaml"`): its
//! definition is force-synced on every startup. A runtime-owned workflow of the
//! same name is left alone. `enabled` is graph-owned; the file sets it only
//! when the workflow is first created.

use std::path::Path;

use serde::Deserialize;
use tracing::{info, warn};

use agent_brain_repository::{Neo4jClient, WorkflowDef, WorkflowInput, YamlSyncOutcome};

/// The checks a workflow may opt into.
pub const CHECKS: &[&str] = &["evaluator", "critic"];

/// One `workflows/*.yaml` file. Unknown keys are refused, so a leftover
/// chain or schedule field (`pattern`, `interval_seconds`) fails loudly.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowFile {
    /// Must equal the file stem.
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// Schedule period (`30m`, `6h`, `1d`, `1w`); absent = runs by name only.
    #[serde(default)]
    pub every: Option<String>,
    /// Initial enabled state, honoured only when the workflow is created.
    #[serde(default)]
    pub enabled: Option<bool>,
    /// The workflow a Task with no named workflow runs. Exactly one file sets it.
    #[serde(default)]
    pub default: bool,
    #[serde(default)]
    pub inputs: Vec<WorkflowInput>,
    #[serde(default)]
    pub checks: Vec<String>,
    #[serde(default)]
    pub evaluation_rubric: Option<String>,
    /// Default data class for every step that does not set its own.
    #[serde(default)]
    pub data_class: Option<agent_brain_llm::routing::DataClass>,
    /// The `topics/*.yaml` topic it works on; absent = spans topics.
    #[serde(default)]
    pub topic: Option<String>,
    /// One of `WORKFLOW_KINDS`; required when the workflow has a schedule.
    #[serde(default)]
    pub kind: Option<String>,
    /// Step fields every step gets unless it sets its own.
    #[serde(default)]
    pub defaults: serde_json::Map<String, serde_json::Value>,
    pub steps: Vec<serde_json::Value>,
}

impl WorkflowFile {
    /// The steps as stored: `defaults` merged in, `data_class` stamped.
    pub fn resolved_steps(&self) -> Vec<serde_json::Value> {
        let mut steps = self.steps.clone();
        apply_step_defaults(&mut steps, &self.defaults);
        if let Some(class) = self.data_class {
            agent_brain_llm::routing::stamp_data_class(&mut steps, class);
        }
        steps
    }
}

/// Merge a seed file's `defaults:` into each step; a field the step sets wins.
/// Lets a file say `provider_hint: llamacpp` once instead of on every step.
pub fn apply_step_defaults(
    steps: &mut [serde_json::Value],
    defaults: &serde_json::Map<String, serde_json::Value>,
) {
    for step in steps {
        if let serde_json::Value::Object(m) = step {
            for (k, v) in defaults {
                m.entry(k.clone()).or_insert_with(|| v.clone());
            }
        }
    }
}

/// Parse a schedule period: a whole number followed by `s`, `m`, `h`, `d` or
/// `w`. At least one minute.
pub fn parse_every(s: &str) -> Result<i64, String> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len()));
    let n: i64 = num
        .parse()
        .map_err(|_| format!("`every: {s}` must be a number and a unit, e.g. 6h or 1d"))?;
    let secs = match unit.trim() {
        "s" => n,
        "m" => n * 60,
        "h" => n * 3600,
        "d" => n * 86_400,
        "w" => n * 604_800,
        other => {
            return Err(format!(
                "`every: {s}`: unknown unit '{other}' (use s, m, h, d or w)"
            ));
        }
    };
    if secs < 60 {
        return Err(format!("`every: {s}` is under a minute"));
    }
    Ok(secs)
}

/// Write a period the way `parse_every` reads it, in the largest whole unit.
pub fn format_every(secs: i64) -> String {
    for (unit, size) in [("w", 604_800), ("d", 86_400), ("h", 3600), ("m", 60)] {
        if secs % size == 0 {
            return format!("{}{unit}", secs / size);
        }
    }
    format!("{secs}s")
}

/// A workflow's declared `topic` and `kind`, checked: the topic must be one
/// `topics/*.yaml` declares and the kind one of `WORKFLOW_KINDS`. Blank means
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
        && !agent_brain_models::WORKFLOW_KINDS.contains(&k)
    {
        errors.push(format!(
            "kind '{k}' is not one of {}",
            agent_brain_models::WORKFLOW_KINDS.join(", ")
        ));
    }
    if errors.is_empty() {
        Ok((topic.map(String::from), kind.map(String::from)))
    } else {
        Err(errors.join("; "))
    }
}

/// Every check name must be one of [`CHECKS`].
pub fn check_checks(checks: &[String]) -> Result<(), String> {
    match checks.iter().find(|c| !CHECKS.contains(&c.as_str())) {
        Some(c) => Err(format!("check '{c}' is not one of {}", CHECKS.join(", "))),
        None => Ok(()),
    }
}

/// The topic names `topics/*.yaml` declares.
pub fn declared_topics() -> Vec<String> {
    agent_brain_graph::topics::Topics::global()
        .defs
        .iter()
        .map(|d| d.name.clone())
        .collect()
}

/// Parse and check one file into what the graph stores. `Err` says what is wrong.
pub fn load_file(path: &Path, topic_names: &[String]) -> Result<(WorkflowDef, bool), String> {
    let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
    let file: WorkflowFile = serde_yaml::from_str(&text).map_err(|e| format!("YAML: {e}"))?;
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if file.name != stem {
        return Err(format!(
            "name '{}' must equal the file name '{stem}'",
            file.name
        ));
    }
    let interval_seconds = file.every.as_deref().map(parse_every).transpose()?;
    check_checks(&file.checks)?;
    // A wrong label is dropped with a warning, never stored.
    let (topic, kind) = check_labels(file.topic.as_deref(), file.kind.as_deref(), topic_names)
        .unwrap_or_else(|e| {
            warn!(name = %file.name, error = %e, "seed_workflows: topic/kind rejected — stored as none");
            (None, None)
        });
    let steps = serde_json::to_string(&file.resolved_steps()).map_err(|e| e.to_string())?;
    Ok((
        WorkflowDef {
            name: file.name,
            description: Some(file.description).filter(|d| !d.trim().is_empty()),
            interval_seconds,
            steps,
            inputs: file.inputs,
            checks: file.checks,
            evaluation_rubric: file.evaluation_rubric,
            is_default: file.default,
            topic,
            kind,
            plugin: None,
        },
        file.enabled.unwrap_or(true),
    ))
}

/// Seed every workflow file of the loaded bundles. A bad file is logged and
/// skipped; so is a name a bundle earlier in load order (core first) already
/// declared. Fails only when the core bundle has no `workflows/`. Returns the
/// number seeded.
pub async fn seed_workflows(
    neo4j: &Neo4jClient,
    plugins: &agent_brain_graph::bundles::Plugins,
) -> anyhow::Result<usize> {
    let core = plugins.core_dir("workflows");
    if !core.is_dir() {
        anyhow::bail!(
            "the core bundle has no workflows directory ({})",
            core.display()
        );
    }
    let topic_names = declared_topics();
    let kinds = crate::workflows::api_kinds(neo4j).await;
    let mut seen: std::collections::HashMap<String, String> = std::collections::HashMap::new();
    let mut seeded = 0usize;
    for (plugin, path) in plugins.files("workflows", "yaml") {
        let (mut def, enabled) = match load_file(&path, &topic_names) {
            Ok(d) => d,
            Err(e) => {
                warn!(path = %path.display(), error = %e, "seed_workflows: file skipped");
                continue;
            }
        };
        if let Some(first) = seen.get(&def.name) {
            warn!(name = %def.name, %plugin, first = %first, "seed_workflows: name already declared by an earlier plugin — file skipped");
            continue;
        }
        seen.insert(def.name.clone(), plugin.clone());
        def.plugin = Some(plugin);
        // A wrong `needs` is dropped with a warning, like a wrong topic: it only
        // labels the step, so the workflow still runs.
        if let Ok(mut steps) = serde_json::from_str::<Vec<serde_json::Value>>(&def.steps) {
            let bad = crate::validation::validate_step_needs(&steps, &kinds, &topic_names);
            if !bad.is_empty() {
                for v in &bad {
                    warn!(name = %def.name, at = %v.path, error = %v.message, "seed_workflows: step needs dropped");
                }
                for s in steps.iter_mut() {
                    let single = std::slice::from_ref(s);
                    if !crate::validation::validate_step_needs(single, &kinds, &topic_names)
                        .is_empty()
                        && let Some(m) = s.as_object_mut()
                    {
                        m.remove("needs");
                    }
                }
                def.steps = serde_json::Value::Array(steps).to_string();
            }
        }
        match neo4j.sync_yaml_workflow(&def, enabled).await {
            Ok(YamlSyncOutcome::Created) => info!(name = %def.name, "Seeded Workflow (yaml-owned)"),
            Ok(YamlSyncOutcome::Updated) => {}
            Ok(YamlSyncOutcome::RuntimeOwned) => warn!(
                name = %def.name,
                "Workflow is runtime-owned — file skipped (manage_workflow upsert managed_by='yaml' hands it back)"
            ),
            Err(e) => {
                warn!(name = %def.name, error = %e, "seed_workflows: upsert failed");
                continue;
            }
        }
        seeded += 1;
    }
    Ok(seeded)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workflows_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/core/workflows")
    }

    fn topic_names() -> Vec<String> {
        agent_brain_graph::topics::Topics::load_dir(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/core/topics"),
        )
        .defs
        .iter()
        .map(|d| d.name.clone())
        .collect()
    }

    /// Every shipped file loads, a scheduled one declares a valid kind, its
    /// step references name earlier steps, its `{{inputs.x}}` are declared,
    /// and exactly one file is the default.
    #[test]
    fn every_workflow_file_is_valid() {
        let names = topic_names();
        let mut defaults = vec![];
        let mut checked = 0;
        for entry in std::fs::read_dir(workflows_dir()).unwrap().flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) != Some("yaml") {
                continue;
            }
            let file: WorkflowFile = serde_yaml::from_str(&std::fs::read_to_string(&path).unwrap())
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            check_labels(file.topic.as_deref(), file.kind.as_deref(), &names)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            if file.every.is_some() {
                assert!(
                    file.kind.is_some(),
                    "{}: a scheduled workflow declares a kind",
                    path.display()
                );
            }
            let (def, _) =
                load_file(&path, &names).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            let steps: Vec<serde_json::Value> = serde_json::from_str(&def.steps).unwrap();
            let declared: Vec<String> = def.inputs.iter().map(|i| i.name.clone()).collect();
            let hard: Vec<String> =
                crate::validation::validate_step_array(&steps, &[], &Default::default())
                    .iter()
                    .chain(crate::validation::validate_input_refs(&steps, &declared).iter())
                    .chain(crate::validation::validate_step_when(&steps).iter())
                    .map(|v| v.render())
                    .collect();
            assert!(hard.is_empty(), "{}: {hard:?}", path.display());
            if def.is_default {
                defaults.push(def.name);
            }
            checked += 1;
        }
        assert!(checked >= 15, "only {checked} workflows found");
        assert_eq!(
            defaults.len(),
            1,
            "exactly one default workflow: {defaults:?}"
        );
    }

    #[test]
    fn defaults_fill_only_what_a_step_leaves_unset() {
        let mut steps = vec![
            serde_json::json!({"tool_name": "a"}),
            serde_json::json!({"tool_name": "b", "provider_hint": "ollama-cloud"}),
        ];
        let d = serde_json::json!({"provider_hint": "ollama", "priority": 1});
        apply_step_defaults(&mut steps, d.as_object().unwrap());
        assert_eq!(steps[0]["provider_hint"], "ollama");
        assert_eq!(steps[1]["provider_hint"], "ollama-cloud");
        assert_eq!(steps[1]["priority"], 1);
    }

    #[test]
    fn every_parses_units_and_round_trips() {
        assert_eq!(parse_every("1d"), Ok(86_400));
        assert_eq!(parse_every("6h"), Ok(21_600));
        assert_eq!(parse_every("2w"), Ok(1_209_600));
        assert!(parse_every("30s").is_err());
        assert!(parse_every("daily").is_err());
        assert!(parse_every("3x").is_err());
        for secs in [60, 3600, 21_600, 86_400, 172_800, 604_800, 90] {
            assert_eq!(
                parse_every(&format_every(secs)).ok(),
                Some(secs).filter(|s| *s >= 60)
            );
        }
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
        assert!(err.contains("ai-ml, michigan"), "{err}");
    }

    #[test]
    fn unknown_checks_are_refused() {
        assert!(check_checks(&["evaluator".into(), "critic".into()]).is_ok());
        assert!(check_checks(&["adversarial".into()]).is_err());
    }
}
