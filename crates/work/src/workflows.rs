//! Starting and rendering workflow runs (CORE_AND_PLUGINS_PLAN Phase 2).
//!
//! A run is a `:Task` with `workflow` and `inputs_json`. Anything that wants
//! work done (a tool, REST, media polling, gap detection, the perception scan)
//! calls [`start_run`] with a workflow name and inputs; the scheduler's next
//! tick dispatches it. A schedule firing is dispatched at once by the scheduler.

use serde_json::{Map, Value};

use crate::queue::{ChainStep, substitute_template_vars};
use agent_brain_repository::{Neo4jClient, Workflow};

/// A run that was created.
#[derive(Debug, Clone)]
pub struct StartedRun {
    pub task_id: String,
    pub workflow: String,
}

/// Resolve `name` (or the default workflow when `None`) and refuse a run it
/// cannot do: unknown, disabled, unavailable, or missing a required input. `Err` says why
/// and names what does exist.
pub async fn resolve(
    neo4j: &Neo4jClient,
    name: Option<&str>,
    inputs: &Map<String, Value>,
) -> Result<Workflow, String> {
    let wf = match name.map(str::trim).filter(|n| !n.is_empty()) {
        Some(n) => match neo4j
            .get_workflow_by_name(n)
            .await
            .map_err(|e| e.to_string())?
        {
            Some(wf) => wf,
            None => {
                let names: Vec<String> = neo4j
                    .list_workflows(true)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|w| w.name)
                    .collect();
                return Err(format!(
                    "No workflow named '{n}'. Enabled workflows: {}.",
                    names.join(", ")
                ));
            }
        },
        None => neo4j
            .get_default_workflow()
            .await
            .map_err(|e| e.to_string())?
            .ok_or("No default workflow is set (a workflow file with `default: true`).")?,
    };
    if !wf.enabled {
        return Err(format!(
            "Workflow '{}' is disabled{}.",
            wf.name,
            wf.paused_reason
                .as_deref()
                .map(|r| format!(" ({r})"))
                .unwrap_or_default()
        ));
    }
    if let Some(why) = wf.unavailable.as_deref() {
        return Err(format!("Workflow '{}' cannot run: {why}.", wf.name));
    }
    let missing = wf.missing_inputs(inputs);
    if !missing.is_empty() {
        let declared: Vec<&str> = wf.inputs.iter().map(|i| i.name.as_str()).collect();
        return Err(format!(
            "Workflow '{}' needs input {} (declares: {}).",
            wf.name,
            missing.join(", "),
            declared.join(", ")
        ));
    }
    Ok(wf)
}

/// Create a run for the next scheduler tick. `goal` is what a person reads in
/// the task list; it defaults to the workflow name and its inputs.
pub async fn start_run(
    neo4j: &Neo4jClient,
    name: Option<&str>,
    inputs: &Map<String, Value>,
    goal: Option<&str>,
    context: Option<&str>,
    success_criteria: Option<&str>,
) -> Result<StartedRun, String> {
    let wf = resolve(neo4j, name, inputs).await?;
    let inputs = &with_chat_session(&wf, inputs, agent_brain_llm::ctx::current_chat_session());
    let goal = match goal.map(str::trim).filter(|g| !g.is_empty()) {
        Some(g) => g.to_string(),
        None => default_goal(&wf.name, inputs),
    };
    let inputs_v = Value::Object(inputs.clone());
    let task_id = neo4j
        .create_run(
            &goal,
            context,
            success_criteria,
            Some(&wf.name),
            Some(&inputs_v),
            "created",
        )
        .await
        .map_err(|e| e.to_string())?;
    Ok(StartedRun {
        task_id,
        workflow: wf.name,
    })
}

/// A workflow name: lowercase letters, digits and dashes, starting with a letter or digit.
pub fn is_slug(name: &str) -> bool {
    name.chars()
        .next()
        .is_some_and(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

/// `name: value, …` — the goal of a run nobody described.
pub fn default_goal(name: &str, inputs: &Map<String, Value>) -> String {
    let vals: Vec<String> = inputs.values().map(input_text).collect();
    if vals.is_empty() {
        name.to_string()
    } else {
        format!("{name}: {}", vals.join(", "))
    }
}

fn input_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The steps of one run, template variables filled: `{{task_id}}`, `{{goal}}`,
/// `{{date}}` / `{{now}}` / `{{weekday}}` (local), and `{{inputs.<name>}}`.
/// `{{_prev}}` and `{{steps.<id>}}` are left for the queue.
pub fn render_steps(
    wf: &Workflow,
    task_id: &str,
    goal: &str,
    inputs: &Map<String, Value>,
) -> Result<Vec<ChainStep>, String> {
    let raw: Value = serde_json::from_str(&wf.steps)
        .map_err(|e| format!("Workflow '{}' steps JSON invalid: {e}", wf.name))?;
    let date = agent_brain_graph::clock::today();
    let now = agent_brain_graph::clock::now_stamp();
    let weekday = agent_brain_graph::clock::weekday();
    // A declared input the run was not given renders empty, so an optional
    // one ("what the person said") never leaves `{{inputs.note}}` in a prompt.
    let input_vals: Vec<(String, String)> = inputs
        .iter()
        .map(|(k, v)| (format!("inputs.{k}"), input_text(v)))
        .chain(
            wf.inputs
                .iter()
                .filter(|i| !inputs.contains_key(&i.name))
                .map(|i| (format!("inputs.{}", i.name), String::new())),
        )
        .collect();
    let mut vars: Vec<(&str, &str)> = vec![
        ("task_id", task_id),
        ("goal", goal),
        ("date", &date),
        ("now", &now),
        ("weekday", &weekday),
    ];
    vars.extend(input_vals.iter().map(|(k, v)| (k.as_str(), v.as_str())));
    let substituted = substitute_template_vars(&raw, &vars);
    let steps: Vec<ChainStep> = serde_json::from_value(substituted)
        .map_err(|e| format!("Workflow '{}' steps do not parse: {e}", wf.name))?;
    Ok(steps.into_iter().filter(step_applies).collect())
}

/// A run started from a chat turn reports back there: a workflow that
/// declares a `session` input, started without one, is given the chat's
/// session. The model is never asked to copy an id, and a run started
/// anywhere else (the media watcher, a schedule) has no chat and gets none.
pub fn with_chat_session(
    wf: &Workflow,
    inputs: &Map<String, Value>,
    chat: Option<String>,
) -> Map<String, Value> {
    let mut out = inputs.clone();
    let given = out
        .get(CHAT_SESSION_INPUT)
        .and_then(Value::as_str)
        .is_some_and(|s| !s.trim().is_empty());
    if let Some(sid) = chat.filter(|s| !s.trim().is_empty())
        && !given
        && wf.inputs.iter().any(|i| i.name == CHAT_SESSION_INPUT)
    {
        out.insert(CHAT_SESSION_INPUT.to_string(), Value::String(sid));
    }
    out
}

/// The input name a workflow declares to receive the chat that started it.
pub const CHAT_SESSION_INPUT: &str = "session";

/// Whether a rendered step stays in its run: no `when`, or a `when` that
/// rendered to something. An input the run was not given is left as its
/// `{{inputs.x}}` renders empty; anything still holding `{{` counts as empty.
fn step_applies(step: &ChainStep) -> bool {
    step.when
        .as_deref()
        .is_none_or(|w| !w.trim().is_empty() && !w.contains("{{"))
}

// ── Saving (manage_workflow upsert, REST create/update, the HBI editor) ──

/// What a save is checked against. Empty lists disable that check (fail-open
/// on our own wiring); the envelope is the `schemas/workflow.json` schema.
pub struct SaveChecks<'a> {
    pub live_tools: &'a [String],
    pub tool_schemas: &'a std::collections::HashMap<String, Value>,
    pub catalog: &'a [agent_brain_llm::model_router::CatalogModel],
    pub envelope: Option<&'a Value>,
}

/// Why a save did not happen.
#[derive(Debug)]
pub enum SaveError {
    /// The request is wrong; nothing was saved.
    Invalid(String),
    NotFound(String),
    Storage(String),
}

impl std::fmt::Display for SaveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SaveError::Invalid(e) => write!(f, "Workflow rejected — nothing was saved: {e}"),
            SaveError::NotFound(e) | SaveError::Storage(e) => write!(f, "{e}"),
        }
    }
}

/// A saved workflow, whether it was created, and what the caller should know.
pub struct Saved {
    pub workflow: Workflow,
    pub created: bool,
    pub warnings: Vec<String>,
    /// `validation::lint_chain_steps`: steps that save but run badly.
    pub step_warnings: Vec<String>,
}

/// What `check` found: the save's warnings and the step lint, the save
/// itself refused only on a hard error.
pub struct Checked {
    pub warnings: Vec<String>,
    pub step_warnings: Vec<String>,
}

/// The fields a save body carries, from a stored workflow: what an edit is
/// laid over before the whole result is checked.
pub fn to_body(wf: &Workflow) -> Value {
    let mut b = serde_json::json!({
        "name": wf.name,
        "every": wf.interval_seconds.map(crate::workflow_seeder::format_every),
        "inputs": wf.inputs,
        "checks": wf.checks,
        "default": wf.is_default,
        "enabled": wf.enabled,
        "steps": serde_json::from_str::<Value>(&wf.steps).unwrap_or(Value::Array(vec![])),
    });
    for (k, v) in [
        ("description", &wf.description),
        ("evaluation_rubric", &wf.evaluation_rubric),
        ("paused_reason", &wf.paused_reason),
        ("topic", &wf.topic),
        ("kind", &wf.kind),
        ("managed_by", &wf.managed_by),
    ] {
        if let Some(v) = v {
            b[k] = Value::String(v.clone());
        }
    }
    b
}

/// A save body checked and resolved, not yet written: what `save` writes and
/// `check` reports on.
struct Prepared {
    current: Option<Workflow>,
    def: agent_brain_repository::WorkflowDef,
    enabled: bool,
    managed_by: Option<String>,
    paused_reason: Option<String>,
    warnings: Vec<String>,
    step_warnings: Vec<String>,
}

/// Every check `save` runs, with nothing written: the warnings and step lint
/// the save would return, or the error it would refuse with. The HBI checks an agent's draft
/// with it, so a draft that cannot be saved is not shown as ready.
pub async fn check(
    neo4j: &Neo4jClient,
    id: Option<&str>,
    create_only: bool,
    body: &Value,
    checks: &SaveChecks<'_>,
) -> Result<Checked, SaveError> {
    prepare(neo4j, id, create_only, body, checks)
        .await
        .map(|p| Checked {
            warnings: p.warnings,
            step_warnings: p.step_warnings,
        })
}

/// Create or update a workflow from a save body. With `id`, that workflow is
/// edited; without, the one named `name` is, or a new one is created — unless
/// `create_only`, when an existing name is refused instead of edited. Keys the
/// body leaves out keep their stored values; the merged whole is checked
/// (envelope, every step against its tool's schema, pinned models, step and
/// input references, topic and kind) and REJECTED on any hard error.
pub async fn save(
    neo4j: &Neo4jClient,
    id: Option<&str>,
    create_only: bool,
    body: &Value,
    checks: &SaveChecks<'_>,
) -> Result<Saved, SaveError> {
    let storage = |e: agent_brain_repository::RepositoryError| SaveError::Storage(e.to_string());
    let Prepared {
        current,
        def,
        enabled,
        managed_by,
        paused_reason,
        warnings,
        step_warnings,
    } = prepare(neo4j, id, create_only, body, checks).await?;

    let Some(cur) = current else {
        let wf = neo4j
            .create_workflow(&def, enabled, managed_by.as_deref().unwrap_or("runtime"))
            .await
            .map_err(storage)?;
        return Ok(Saved {
            workflow: wf,
            created: true,
            warnings,
            step_warnings,
        });
    };

    // A schedule added is first due now; a schedule removed has no next run.
    let next_run_at = match (
        body.get("next_run_at").and_then(Value::as_str),
        def.interval_seconds,
    ) {
        (_, None) => Some(None),
        (Some(t), Some(_)) => Some(Some(t.to_string())),
        (None, Some(_)) if cur.next_run_at.is_none() => Some(Some(chrono::Utc::now().to_rfc3339())),
        (None, Some(_)) => None,
    };
    let name = def.name.clone();
    let patch = agent_brain_repository::WorkflowPatch {
        name: Some(def.name),
        description: Some(def.description),
        enabled: Some(enabled),
        interval_seconds: Some(def.interval_seconds),
        steps: Some(def.steps),
        inputs: Some(def.inputs),
        checks: Some(def.checks),
        evaluation_rubric: Some(def.evaluation_rubric),
        is_default: Some(def.is_default),
        next_run_at,
        managed_by,
        paused_reason: Some(paused_reason),
        topic: Some(def.topic),
        kind: Some(def.kind),
    };
    let wf = neo4j
        .update_workflow(&cur.id, &patch)
        .await
        .map_err(storage)?
        .ok_or_else(|| {
            SaveError::NotFound(format!("Workflow '{name}' vanished during the save"))
        })?;
    Ok(Saved {
        workflow: wf,
        created: false,
        warnings,
        step_warnings,
    })
}

/// The `(:ApiKind)` names (`apis/_kinds.yaml`); empty when not seeded or unreadable.
pub async fn api_kinds(neo4j: &Neo4jClient) -> Vec<String> {
    neo4j
        .execute(neo4rs::query(
            "MATCH (k:ApiKind) RETURN k.name AS name ORDER BY name",
        ))
        .await
        .map(|rows| {
            rows.iter()
                .filter_map(|r| r.get::<String>("name").ok())
                .collect()
        })
        .unwrap_or_default()
}

/// The checking half of `save`: the stored workflow with the body laid over
/// it, every hard check run, and the definition it would write.
async fn prepare(
    neo4j: &Neo4jClient,
    id: Option<&str>,
    create_only: bool,
    body: &Value,
    checks: &SaveChecks<'_>,
) -> Result<Prepared, SaveError> {
    use crate::{validation, workflow_seeder};
    let storage = |e: agent_brain_repository::RepositoryError| SaveError::Storage(e.to_string());

    let current = match id {
        Some(id) => Some(
            neo4j
                .get_workflow(id)
                .await
                .map_err(storage)?
                .ok_or_else(|| SaveError::NotFound(format!("No workflow with id '{id}'")))?,
        ),
        None => match body.get("name").and_then(Value::as_str) {
            Some(n) => match neo4j.get_workflow_by_name(n).await.map_err(storage)? {
                Some(_) if create_only => {
                    return Err(SaveError::Invalid(format!(
                        "a workflow named '{n}' already exists; pick another name or edit that one"
                    )));
                }
                found => found,
            },
            None => return Err(SaveError::Invalid("`name` is required".into())),
        },
    };

    // The candidate: the stored workflow with the body laid over it.
    let mut cand = current
        .as_ref()
        .map(to_body)
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(obj) = body.as_object() {
        for (k, v) in obj {
            if !matches!(k.as_str(), "action" | "id" | "from_item" | "data_class") {
                cand[k] = v.clone();
            }
        }
    }
    // A REST client may send steps as a JSON string.
    if let Some(s) = cand.get("steps").and_then(Value::as_str) {
        cand["steps"] =
            serde_json::from_str(s).map_err(|e| SaveError::Invalid(format!("steps: {e}")))?;
    }
    if let Some(class) = body
        .get("data_class")
        .and_then(|v| serde_json::from_value::<agent_brain_llm::routing::DataClass>(v.clone()).ok())
        && let Some(steps) = cand.get_mut("steps").and_then(Value::as_array_mut)
    {
        agent_brain_llm::routing::stamp_data_class(steps, class);
    }

    let mut hard = Vec::new();
    if let Some(schema) = checks.envelope {
        hard.extend(validation::validate_against_schema(
            "workflow", schema, &cand,
        ));
    }
    let steps: Vec<Value> = cand["steps"].as_array().cloned().unwrap_or_default();
    if steps.is_empty() {
        return Err(SaveError::Invalid(
            "`steps` must be a non-empty array".into(),
        ));
    }
    let inputs: Vec<agent_brain_models::WorkflowInput> =
        serde_json::from_value(cand.get("inputs").cloned().unwrap_or(Value::Array(vec![])))
            .map_err(|e| SaveError::Invalid(format!("inputs: {e}")))?;
    let declared: Vec<String> = inputs.iter().map(|i| i.name.clone()).collect();
    hard.extend(validation::validate_step_array(
        &steps,
        checks.live_tools,
        checks.tool_schemas,
    ));
    hard.extend(validation::validate_step_models(&steps, checks.catalog));
    hard.extend(validation::validate_input_refs(&steps, &declared));
    hard.extend(validation::validate_step_when(&steps));
    hard.extend(validation::validate_step_needs(
        &steps,
        &api_kinds(neo4j).await,
        &workflow_seeder::declared_topics(),
    ));
    if validation::model_driven_call() {
        let stored: Vec<Value> = current
            .as_ref()
            .map(to_body)
            .and_then(|b| b["steps"].as_array().cloned())
            .unwrap_or_default();
        hard.extend(validation::validate_raw_graph_writes(&steps, &stored));
    }
    if validation::has_hard(&hard) {
        return Err(SaveError::Invalid(validation::render_hard(&hard)));
    }
    let chain = serde_json::from_value::<Vec<ChainStep>>(Value::Array(steps.clone()))
        .map_err(|e| SaveError::Invalid(format!("steps: {e}")))?;
    let step_warnings = validation::lint_chain_steps(&chain, checks.live_tools);

    let str_of = |k: &str| {
        cand.get(k)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let name = str_of("name").ok_or_else(|| SaveError::Invalid("`name` is required".into()))?;
    if !is_slug(&name) {
        return Err(SaveError::Invalid(format!(
            "name '{name}' must be a slug: lowercase letters, digits and dashes (it is what the workflow is run by)"
        )));
    }
    let interval_seconds = str_of("every")
        .map(|e| workflow_seeder::parse_every(&e))
        .transpose()
        .map_err(SaveError::Invalid)?;
    let check_names: Vec<String> =
        serde_json::from_value(cand.get("checks").cloned().unwrap_or(Value::Array(vec![])))
            .map_err(|e| SaveError::Invalid(format!("checks: {e}")))?;
    workflow_seeder::check_checks(&check_names).map_err(SaveError::Invalid)?;
    let (topic, kind) = workflow_seeder::check_labels(
        str_of("topic").as_deref(),
        str_of("kind").as_deref(),
        &workflow_seeder::declared_topics(),
    )
    .map_err(SaveError::Invalid)?;
    let managed_by = match body.get("managed_by").and_then(Value::as_str) {
        None => None,
        Some(m @ ("yaml" | "runtime")) => Some(m.to_string()),
        Some(other) => {
            return Err(SaveError::Invalid(format!(
                "`managed_by` must be 'yaml' or 'runtime', got '{other}'"
            )));
        }
    };
    let def = agent_brain_repository::WorkflowDef {
        name: name.clone(),
        description: str_of("description"),
        interval_seconds,
        steps: Value::Array(steps).to_string(),
        inputs,
        checks: check_names,
        evaluation_rubric: str_of("evaluation_rubric"),
        is_default: cand
            .get("default")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        topic,
        kind,
        plugin: None,
    };
    let enabled = cand.get("enabled").and_then(Value::as_bool).unwrap_or(true);
    // A program's pause is lifted by the program, so the next reconcile does
    // not quietly undo this save.
    if let Some(cur) = &current
        && enabled
        && !cur.enabled
        && crate::programs::is_program_pause(cur.paused_reason.as_deref())
    {
        return Err(SaveError::Invalid(format!(
            "'{}' is off because {}. Resume the program (manage_program upsert status: active), or add the workflow to its keep_running.",
            cur.name,
            cur.paused_reason
                .as_deref()
                .and_then(|r| r.strip_prefix(crate::programs::PROGRAM_PAUSE))
                .map(|r| r.trim_start_matches(':').trim())
                .unwrap_or("its program is paused")
        )));
    }

    let mut warnings = Vec::new();
    if let Some(cur) = &current
        && cur.managed_by.as_deref() == Some("yaml")
        && managed_by.as_deref() != Some("runtime")
    {
        warnings.push(format!(
            "This workflow is yaml-managed: its definition is overwritten from \
             workflows/{}.yaml on the next startup. Edit the file, or pass \
             managed_by='runtime' to take ownership.",
            cur.name
        ));
    }
    Ok(Prepared {
        current,
        def,
        enabled,
        managed_by,
        paused_reason: str_of("paused_reason"),
        warnings,
        step_warnings,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn wf(steps: Value) -> Workflow {
        serde_json::from_value(json!({
            "id": "1", "name": "fill-knowledge-gap", "description": null, "enabled": true,
            "interval_seconds": null, "steps": steps.to_string(), "last_run_at": null,
            "next_run_at": null, "created_at": "", "updated_at": "",
            "inputs": [{"name": "topic"}, {"name": "session"}]
        }))
        .unwrap()
    }

    #[test]
    fn render_fills_inputs_and_builtins_and_leaves_prev() {
        let w = wf(json!([
            {"tool_name": "search_web", "arguments": {"query": "{{inputs.topic}}"}},
            {"tool_name": "store_note", "arguments": {"content": "{{goal}} {{task_id}}\n{{_prev}}"}}
        ]));
        let inputs = json!({"topic": "say \"x\""});
        let steps = render_steps(
            &w,
            "t1",
            "fill knowledge gap: x",
            inputs.as_object().unwrap(),
        )
        .unwrap();
        assert_eq!(steps[0].arguments.as_ref().unwrap()["query"], "say \"x\"");
        assert_eq!(
            steps[1].arguments.as_ref().unwrap()["content"],
            "fill knowledge gap: x t1\n{{_prev}}"
        );
    }

    #[test]
    fn a_when_step_runs_only_when_it_renders_to_something() {
        let w = wf(json!([
            {"tool_name": "search_web", "arguments": {"query": "{{inputs.topic}}"}},
            {"tool_name": "notify_user", "when": "{{inputs.session}}",
             "arguments": {"message": "{{_prev}}", "related_session_id": "{{inputs.session}}"}},
            {"tool_name": "store_note", "arguments": {"content": "{{_prev}}"}}
        ]));
        let given = json!({"topic": "x", "session": "chat-1"});
        let steps = render_steps(&w, "t", "g", given.as_object().unwrap()).unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(
            steps[1].arguments.as_ref().unwrap()["related_session_id"],
            "chat-1"
        );
        // Not given, or given empty: the step is left out and the chain closes up.
        for inputs in [json!({"topic": "x"}), json!({"topic": "x", "session": " "})] {
            let steps = render_steps(&w, "t", "g", inputs.as_object().unwrap()).unwrap();
            let tools: Vec<&str> = steps.iter().map(|s| s.tool_name.as_str()).collect();
            assert_eq!(tools, ["search_web", "store_note"]);
        }
    }

    #[test]
    fn a_chat_started_run_gets_its_session_only_if_it_declares_one() {
        let declares = wf(json!([]));
        let none = json!({"topic": "x"});
        let none = none.as_object().unwrap();
        let got = with_chat_session(&declares, none, Some("chat-9".into()));
        assert_eq!(got["session"], "chat-9");
        // Given explicitly: kept. No chat: nothing added.
        let given = json!({"session": "other"});
        let got = with_chat_session(&declares, given.as_object().unwrap(), Some("chat-9".into()));
        assert_eq!(got["session"], "other");
        assert!(
            with_chat_session(&declares, none, None)
                .get("session")
                .is_none()
        );
        // A workflow that does not declare it never gets it.
        let mut plain = wf(json!([]));
        plain.inputs.retain(|i| i.name != "session");
        assert!(
            with_chat_session(&plain, none, Some("chat-9".into()))
                .get("session")
                .is_none()
        );
    }

    #[test]
    fn names_are_slugs() {
        assert!(is_slug("daily-news") && is_slug("2fa-check"));
        assert!(!is_slug("Daily news") && !is_slug("-x") && !is_slug("") && !is_slug("a_b"));
    }

    #[test]
    fn default_goal_names_the_workflow_and_its_inputs() {
        let i = json!({"url": "https://x"});
        assert_eq!(
            default_goal("video-learning", i.as_object().unwrap()),
            "video-learning: https://x"
        );
        assert_eq!(default_goal("consolidate", &Map::new()), "consolidate");
    }
}
