use serde::{Deserialize, Serialize};

/// A named list of steps the queue runs as one job chain (CORE_AND_PLUGINS_PLAN
/// Phase 2). Replaced both `SchedulerChain` (steps picked by regex on a goal)
/// and `ScheduledTask` (steps on a timer): a workflow runs on its schedule when
/// it has one, and by name, with inputs, whenever something asks for it.
///
/// Each run is a `:Task` carrying `workflow` = this name and its inputs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Workflow {
    /// UUID string. A migrated ScheduledTask kept its id.
    pub id: String,
    /// Unique slug (the `workflows/*.yaml` file stem); the name callers run it by.
    pub name: String,
    pub description: Option<String>,
    /// A disabled workflow neither runs on its schedule nor by name.
    pub enabled: bool,
    /// Schedule period in seconds; `None` = runs only when called by name.
    pub interval_seconds: Option<i64>,
    /// JSON-serialised `Vec<ChainStep>`.
    pub steps: String,
    /// The inputs a run supplies; steps read them as `{{inputs.<name>}}`.
    #[serde(default)]
    pub inputs: Vec<WorkflowInput>,
    /// Opt-in checks around a run: `evaluator` (score the result, re-queue
    /// below threshold) and `critic` (adversarial review before it starts).
    #[serde(default)]
    pub checks: Vec<String>,
    /// What the evaluator scores against; the run's `success_criteria` otherwise.
    #[serde(default)]
    pub evaluation_rubric: Option<String>,
    /// The one workflow a Task with no named workflow runs (free-text goals).
    #[serde(default)]
    pub is_default: bool,
    /// RFC3339; `None` = never run.
    pub last_run_at: Option<String>,
    /// RFC3339; `None` when there is no schedule.
    pub next_run_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// `"yaml"` = owned by `workflows/<name>.yaml` and force-synced on startup;
    /// `"runtime"` = created or taken over at runtime, never touched by the seeder.
    #[serde(default)]
    pub managed_by: Option<String>,
    /// Why it is disabled, when that was deliberate.
    #[serde(default)]
    pub paused_reason: Option<String>,
    /// The `topics/*.yaml` topic it works on, or `None` when it spans topics.
    #[serde(default)]
    pub topic: Option<String>,
    /// One of [`WORKFLOW_KINDS`].
    #[serde(default)]
    pub kind: Option<String>,
}

/// One declared input of a workflow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WorkflowInput {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// A run that lacks a required input is refused when it is created.
    #[serde(default = "yes")]
    pub required: bool,
}

fn yes() -> bool {
    true
}

impl Workflow {
    /// The input names that `inputs` lacks (required ones only).
    pub fn missing_inputs(&self, inputs: &serde_json::Map<String, serde_json::Value>) -> Vec<&str> {
        self.inputs
            .iter()
            .filter(|i| i.required)
            .filter(|i| {
                inputs
                    .get(&i.name)
                    .map(|v| v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty()))
                    .unwrap_or(true)
            })
            .map(|i| i.name.as_str())
            .collect()
    }
}

/// The kinds a workflow may declare. A closed set, like a status: the HBI
/// groups by it and the seeder and `manage_workflow` refuse anything else.
pub const WORKFLOW_KINDS: &[&str] = &["monitor", "brief", "learning", "maintenance", "dataset"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_inputs_names_only_required_absent_or_blank_ones() {
        let wf: Workflow = serde_json::from_value(serde_json::json!({
            "id": "1", "name": "w", "description": null, "enabled": true,
            "interval_seconds": null, "steps": "[]", "last_run_at": null,
            "next_run_at": null, "created_at": "", "updated_at": "",
            "inputs": [{"name": "url"}, {"name": "topic"}, {"name": "note", "required": false}]
        }))
        .unwrap();
        let given = serde_json::json!({"url": "https://x", "topic": "  "});
        assert_eq!(wf.missing_inputs(given.as_object().unwrap()), vec!["topic"]);
    }
}
