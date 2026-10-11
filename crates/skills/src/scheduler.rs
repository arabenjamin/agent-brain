//! Scheduler Skill — controls the autonomous self-improvement loop.
//!
//! 2 tools: `scheduler_control` (incl. `action: tick`) and `manage_workflow`.
//! Scheduler status is served by REST (GET /api/scheduler/status). Workflow
//! listing is deliberately NOT REST-only — see `manage_workflow(action=list)`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};
use agent_brain_repository::{Neo4jClient, Workflow};
use agent_brain_scheduler::scheduler::SchedulerService;
use agent_brain_work::queue::ChainStep;
use agent_brain_work::validation;
use agent_brain_work::workflows;

pub struct SchedulerSkill {
    service: Arc<SchedulerService>,
    neo4j: Option<Neo4jClient>,
    /// Live tool names populated after skill registration — used by the audit action.
    live_tools: Arc<RwLock<Vec<String>>>,
    /// Registered tool `input_schema`s, keyed by tool name. Populated from the
    /// registry at the same snapshot point as `live_tools`. Empty until then,
    /// which safely disables schema validation (fail-open on our own wiring).
    tool_schemas: Arc<RwLock<HashMap<String, Value>>>,
    /// Object-envelope JSON Schemas from the `schemas/` seed dir, keyed by file
    /// stem (`workflow`). Loaded once at startup. Empty
    /// = envelope validation disabled (fail-open); the per-step arg gate still runs.
    envelope_schemas: Arc<HashMap<String, Value>>,
    /// The model catalog a step's `model` is checked against. `None` disables
    /// that check (fail-open, like the schema layers).
    telemetry: Option<agent_brain_repository::TelemetryClient>,
}

impl SchedulerSkill {
    pub fn new(service: Arc<SchedulerService>, neo4j: Option<Neo4jClient>) -> Self {
        Self {
            service,
            neo4j,
            live_tools: Arc::new(RwLock::new(Vec::new())),
            tool_schemas: Arc::new(RwLock::new(HashMap::new())),
            envelope_schemas: Arc::new(HashMap::new()),
            telemetry: None,
        }
    }

    /// Inject the telemetry client whose catalog step models are checked against.
    pub fn with_telemetry(
        mut self,
        telemetry: Option<agent_brain_repository::TelemetryClient>,
    ) -> Self {
        self.telemetry = telemetry;
        self
    }

    /// Inject a pre-populated live tool list (replaces the default empty vec).
    pub fn with_live_tools(mut self, live_tools: Arc<RwLock<Vec<String>>>) -> Self {
        self.live_tools = live_tools;
        self
    }

    /// Inject the registered tool `input_schema` map used to validate each
    /// step's `arguments` against the tool it calls.
    pub fn with_tool_schemas(mut self, tool_schemas: Arc<RwLock<HashMap<String, Value>>>) -> Self {
        self.tool_schemas = tool_schemas;
        self
    }

    /// Inject the object-envelope schemas loaded from the `schemas/` seed dir.
    pub fn with_envelope_schemas(mut self, envelope_schemas: Arc<HashMap<String, Value>>) -> Self {
        self.envelope_schemas = envelope_schemas;
        self
    }

    /// A soft warning for each step whose pinned model the routing policy
    /// would refuse right now for that step's data class. Soft, because a
    /// cooldown or a spend cap passes; the step dies with the same reason if
    /// it still holds when the step runs.
    fn pinned_model_warnings(&self, steps: &[ChainStep]) -> Vec<String> {
        use agent_brain_llm::routing::{Purpose, RouteContext};
        let Some(tel) = self.telemetry.as_ref() else {
            return Vec::new();
        };
        let rows = agent_brain_llm::model_router::catalog_models(tel);
        steps
            .iter()
            .enumerate()
            .filter_map(|(i, step)| {
                let model = step
                    .model
                    .as_deref()
                    .map(str::trim)
                    .filter(|m| !m.is_empty())?;
                agent_brain_llm::model_router::find_model(&rows, model)?;
                let ctx =
                    RouteContext::new(Purpose::Background, step.data_class.unwrap_or_default());
                agent_brain_llm::model_router::pinned_config(
                    &rows,
                    model,
                    &agent_brain_llm::LlmConfig::default(),
                    ctx,
                )
                .err()
                .map(|reason| {
                    format!(
                        "step {} (`{}`): {reason}. The step will fail with this reason unless \
                         it has changed by the time it runs.",
                        i + 1,
                        step.tool_name
                    )
                })
            })
            .collect()
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    // get_scheduler_status is served by GET /api/scheduler/status (REST API)

    fn scheduler_control_def() -> ToolDefinition {
        ToolDefinition {
            name: "scheduler_control".to_string(),
            description: "Start, stop, tick, or reconfigure the autonomous scheduler loop. \
                action=start: enable the loop (optionally set interval/session). \
                action=stop: pause the loop (running jobs are not affected). \
                action=tick: run one tick now, bypassing the timer; returns tasks found/dispatched. \
                action=configure: update any combination of runtime settings."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["start", "stop", "tick", "configure"],
                        "description": "start: enable loop. stop: pause loop. tick: run one tick now. configure: update settings."
                    },
                    "interval_secs": {
                        "type": "integer",
                        "minimum": 10,
                        "description": "Polling interval in seconds (default 300)."
                    },
                    "enabled": {
                        "type": "boolean",
                        "description": "Enable or disable the loop (configure mode)."
                    },
                    "max_tasks_per_run": {
                        "type": "integer",
                        "minimum": 1,
                        "maximum": 20,
                        "description": "Max tasks dispatched per tick (default 3)."
                    },
                    "error_budget": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Consecutive error limit before auto-pause (default 5)."
                    },
                    "session_id": {
                        "type": "string",
                        "description": "Session ID attached to enqueued jobs (null to clear)."
                    },
                    "idle_sleep_after_ticks": {
                        "type": "integer",
                        "minimum": 1,
                        "description": "Idle ticks before sleep mode (default 3)."
                    },
                    "sleep_interval_secs": {
                        "type": "integer",
                        "minimum": 60,
                        "description": "Tick interval in sleep mode in seconds (default 1800)."
                    },
                    "local_model": {
                        "type": "string",
                        "description": "Ollama model for background jobs (default: gemma4:latest)."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_scheduler_control(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            action: String,
            interval_secs: Option<u64>,
            enabled: Option<bool>,
            max_tasks_per_run: Option<usize>,
            error_budget: Option<u32>,
            #[serde(default, deserialize_with = "deserialize_optional_session")]
            session_id: Option<Option<String>>,
            idle_sleep_after_ticks: Option<u32>,
            sleep_interval_secs: Option<u64>,
            local_model: Option<String>,
        }

        let input: Input = match args.and_then(|v| serde_json::from_value(v).ok()) {
            Some(i) => i,
            None => return ToolCallResult::error("Invalid args: `action` is required".to_string()),
        };

        if input.action == "tick" {
            return self.handle_run_scheduler_tick().await;
        }
        let cfg = match input.action.as_str() {
            "start" => {
                self.service
                    .update_config(
                        input.interval_secs,
                        Some(true),
                        None,
                        None,
                        input.session_id,
                        None,
                        None,
                        None,
                    )
                    .await
            }
            "stop" => {
                self.service
                    .update_config(None, Some(false), None, None, None, None, None, None)
                    .await
            }
            "configure" => {
                self.service
                    .update_config(
                        input.interval_secs,
                        input.enabled,
                        input.max_tasks_per_run,
                        input.error_budget,
                        input.session_id,
                        input.idle_sleep_after_ticks,
                        input.sleep_interval_secs,
                        input.local_model,
                    )
                    .await
            }
            other => {
                return ToolCallResult::error(format!(
                    "Unknown action `{other}`. Use start, stop, or configure."
                ));
            }
        };

        ToolCallResult::success_json(json!({
            "action": input.action,
            "config": {
                "interval_secs":        cfg.interval_secs,
                "enabled":              cfg.enabled,
                "max_tasks_per_run":    cfg.max_tasks_per_run,
                "error_budget":         cfg.error_budget,
                "session_id":           cfg.session_id,
                "idle_sleep_after_ticks": cfg.idle_sleep_after_ticks,
                "sleep_interval_secs":  cfg.sleep_interval_secs,
                "local_model":          cfg.local_model,
            }
        }))
    }

    async fn handle_run_scheduler_tick(&self) -> ToolCallResult {
        match self.service.run_tick().await {
            Ok(result) => ToolCallResult::success_text(
                json!({
                    "success": true,
                    "tasks_found": result.tasks_found,
                    "tasks_dispatched": result.tasks_dispatched,
                    "skipped": result.skipped,
                    "new_tasks_created": result.new_tasks_created,
                })
                .to_string(),
            ),
            Err(e) => ToolCallResult::error(format!("Scheduler tick failed: {e}")),
        }
    }

    // =========================================================================
    // manage_workflow definition
    // =========================================================================

    // Listing is a tool, not only GET /workflows: when it lived only in REST the
    // chat agent could not see its own recurring work, answered project-status
    // questions from :Note alone and reported running schedules as unbuilt plans.

    fn manage_workflow_def() -> ToolDefinition {
        ToolDefinition {
            name: "manage_workflow".to_string(),
            description: "Workflows are the brain's fixed job chains: some run on a schedule \
                (`every`), all can be run by name with inputs. \
                action=list: every workflow with its schedule, inputs, last and next run. USE THIS \
                to answer what the brain does on its own or how a project is going — a project's \
                implementation usually lives in workflows, not in notes. \
                action=get: one workflow in full (`name` or `id`). \
                action=run: start a run of `name` with `inputs` (dispatched on the next scheduler \
                tick); the run is a Task. \
                action=upsert: create, or update the workflow named `name`; keys you leave out keep \
                their values. Steps are ChainStep objects (tool_name, arguments, provider_hint, id, \
                optional, …). Template vars: {{task_id}}, {{goal}}, {{date}}, {{now}}, {{weekday}}, \
                {{inputs.<name>}}, {{_prev}} (the step before), {{steps.<id>}} / {{steps.<id>.<path>}} \
                (an earlier named step). A trailing update_task is added at run time. Ownership: \
                created here = managed_by 'runtime'; a 'yaml' workflow is overwritten from \
                workflows/<name>.yaml on startup unless you pass managed_by='runtime'. Upsert \
                REJECTS a structural error and returns `step_warnings` for steps that run but \
                discard their inputs — FIX AND RE-UPSERT before reporting it as working: a `reason` \
                step needs `context` naming an earlier output, and a step that saves the result \
                must reference it. \
                action=delete: remove by `id` or `name` (upsert enabled=false to pause instead). \
                action=audit: check every stored workflow against the live tools; optionally \
                disable those whose every step names a dead tool."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": { "type": "string", "enum": ["list", "get", "run", "upsert", "delete", "audit"] },
                    "name": { "type": "string", "description": "The workflow's slug (get/run/upsert/delete)." },
                    "id": { "type": "string", "description": "get/delete by id instead of name." },
                    "inputs": {
                        "description": "run: the input values, e.g. {\"topic\": \"…\"}. upsert: the declared inputs, [{\"name\": \"topic\", \"description\": \"…\"}].",
                        "type": ["object", "array"]
                    },
                    "goal": { "type": "string", "description": "run: what the run is for, as a person reads it in the task list (default: name and inputs)." },
                    "context": { "type": "string", "description": "run: extra context stored on the run Task." },
                    "enabled_only": { "type": "boolean", "description": "list: only enabled workflows (default false)." },
                    "scheduled_only": { "type": "boolean", "description": "list: only workflows with a schedule (default false)." },
                    "filter": { "type": "string", "description": "list: case-insensitive substring of name or description." },
                    "verbose": { "type": "boolean", "description": "list: add id, description, ownership, steps, checks. Keep false unless narrowed by `filter`." },
                    "description": { "type": "string" },
                    "every": { "type": ["string", "null"], "description": "upsert: schedule period (30m, 6h, 1d, 1w); null removes the schedule." },
                    "steps": { "type": "array", "description": "upsert: the job chain." },
                    "checks": { "type": "array", "items": { "type": "string", "enum": ["evaluator", "critic"] }, "description": "upsert: opt-in checks around a run." },
                    "evaluation_rubric": { "type": ["string", "null"] },
                    "default": { "type": "boolean", "description": "upsert: make this the workflow free-text tasks run." },
                    "enabled": { "type": "boolean" },
                    "paused_reason": { "type": ["string", "null"], "description": "upsert: why it is disabled, when that is deliberate." },
                    "topic": { "type": ["string", "null"], "description": "upsert: the topics/*.yaml topic, or \"\"/null for one that spans topics." },
                    "kind": { "type": ["string", "null"], "enum": ["monitor", "brief", "learning", "maintenance", "dataset", "", null] },
                    "data_class": { "type": "string", "enum": ["private", "public"], "description": "upsert: stamped onto steps that set none." },
                    "next_run_at": { "type": "string", "description": "upsert: RFC3339 instant of the next scheduled run." },
                    "managed_by": { "type": "string", "enum": ["yaml", "runtime"] },
                    "disable_broken": { "type": "boolean", "description": "audit: disable workflows whose every step names a dead tool." }
                },
                "required": ["action"]
            }),
        }
    }

    // =========================================================================
    // manage_workflow
    // =========================================================================

    async fn handle_manage_workflow(&self, args: Option<Value>) -> ToolCallResult {
        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };
        let args = args.unwrap_or_default();
        let s = |k: &str| args[k].as_str().map(str::trim).filter(|v| !v.is_empty());
        match s("action").unwrap_or("") {
            "list" => self.list_workflows(neo4j, &args).await,
            "get" => match self.find(neo4j, s("id"), s("name")).await {
                Ok(wf) => {
                    let mut body = workflows::to_body(&wf);
                    body["id"] = json!(wf.id);
                    body["last_run_at"] = json!(wf.last_run_at);
                    body["next_run_at"] = json!(wf.next_run_at);
                    ToolCallResult::success_json(body)
                }
                Err(e) => ToolCallResult::error(e),
            },
            "run" => {
                let Some(name) = s("name") else {
                    return ToolCallResult::error("`name` is required for action=run".to_string());
                };
                let inputs = args["inputs"].as_object().cloned().unwrap_or_default();
                match workflows::start_run(
                    neo4j,
                    Some(name),
                    &inputs,
                    s("goal"),
                    s("context"),
                    None,
                )
                .await
                {
                    Ok(run) => ToolCallResult::success_json(json!({
                        "task_id": run.task_id,
                        "workflow": run.workflow,
                        "status": "created",
                        "note": "Dispatched on the next scheduler tick; follow it by its task_id."
                    })),
                    Err(e) => ToolCallResult::error(e),
                }
            }
            "upsert" => self.upsert_workflow(neo4j, &args).await,
            "delete" => {
                let wf = match self.find(neo4j, s("id"), s("name")).await {
                    Ok(wf) => wf,
                    Err(e) => return ToolCallResult::error(e),
                };
                match neo4j.delete_workflow(&wf.id).await {
                    Ok(_) => ToolCallResult::success_json(
                        json!({ "deleted": true, "name": wf.name, "id": wf.id }),
                    ),
                    Err(e) => ToolCallResult::error(format!("Failed to delete workflow: {e}")),
                }
            }
            "audit" => self.audit_workflows(neo4j, &args).await,
            other => ToolCallResult::error(format!(
                "Unknown action '{other}': use list, get, run, upsert, delete or audit."
            )),
        }
    }

    /// A workflow by id or name; `Err` lists the names that do exist.
    async fn find(
        &self,
        neo4j: &Neo4jClient,
        id: Option<&str>,
        name: Option<&str>,
    ) -> Result<Workflow, String> {
        let found = match (id, name) {
            (Some(id), _) => neo4j.get_workflow(id).await,
            (None, Some(n)) => neo4j.get_workflow_by_name(n).await,
            (None, None) => return Err("`name` or `id` is required".to_string()),
        }
        .map_err(|e| e.to_string())?;
        match found {
            Some(wf) => Ok(wf),
            None => {
                let names: Vec<String> = neo4j
                    .list_workflows(false)
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .map(|w| w.name)
                    .collect();
                Err(format!(
                    "No workflow '{}'. Workflows: {}",
                    id.or(name).unwrap_or(""),
                    names.join(", ")
                ))
            }
        }
    }

    // Listing is a tool, not REST-only: asked "where are we on project X", the
    // brain searched :Note, found the design discussion, and reported live
    // recurring work as an unbuilt plan. The payload is lean because chat may
    // cut a tool result to 2000 chars.
    async fn list_workflows(&self, neo4j: &Neo4jClient, args: &Value) -> ToolCallResult {
        let enabled_only = args["enabled_only"].as_bool().unwrap_or(false);
        let scheduled_only = args["scheduled_only"].as_bool().unwrap_or(false);
        let filter = args["filter"].as_str().map(|s| s.to_lowercase());
        let verbose = args["verbose"].as_bool().unwrap_or(false);
        let all = match neo4j.list_workflows(enabled_only).await {
            Ok(t) => t,
            Err(e) => return ToolCallResult::error(format!("Failed to list workflows: {e}")),
        };
        let total = all.len();
        let names: Vec<String> = all.iter().map(|t| t.name.clone()).collect();
        let shown: Vec<_> = all
            .into_iter()
            .filter(|t| !scheduled_only || t.interval_seconds.is_some())
            .filter(|t| match filter {
                Some(ref f) => {
                    t.name.to_lowercase().contains(f)
                        || t.description
                            .as_deref()
                            .is_some_and(|d| d.to_lowercase().contains(f))
                }
                None => true,
            })
            .collect();
        let day = |d: &Option<String>| d.as_deref().map(|d| d[..10.min(d.len())].to_string());
        let rows: Vec<Value> = shown
            .iter()
            .map(|t| {
                let mut row = json!({
                    "name": t.name,
                    "enabled": t.enabled,
                    "every": t.interval_seconds.map(humanize_interval).unwrap_or_else(|| "by name".into()),
                    "last_run": day(&t.last_run_at),
                });
                if t.interval_seconds.is_some() {
                    row["next_run"] = json!(day(&t.next_run_at));
                }
                if !t.inputs.is_empty() {
                    row["inputs"] = json!(t.inputs.iter().map(|i| i.name.as_str()).collect::<Vec<_>>());
                }
                if t.is_default {
                    row["default"] = json!(true);
                }
                // "Disabled" alone invites a guess at why; a deliberate pause
                // read as a breakage gets "fixed" by re-enabling it.
                if !t.enabled && let Some(ref why) = t.paused_reason {
                    row["paused_reason"] = json!(why);
                }
                if verbose {
                    row["id"] = json!(t.id);
                    row["description"] = json!(t.description);
                    row["managed_by"] = json!(t.managed_by);
                    row["checks"] = json!(t.checks);
                    row["topic"] = json!(t.topic);
                    row["kind"] = json!(t.kind);
                    row["step_count"] = json!(
                        serde_json::from_str::<Vec<Value>>(&t.steps).map(|s| s.len()).unwrap_or(0)
                    );
                }
                row
            })
            .collect();
        // An empty filtered result reported as "none exist" is the most
        // misleading answer this tool can give (a project is filed under its
        // internal name), so it always carries every name back.
        if filter.is_some() && rows.is_empty() && total > 0 {
            return ToolCallResult::success_json(json!({
                "count": 0,
                "total": total,
                "filter": filter,
                "warning": format!(
                    "No workflow NAME or DESCRIPTION matched that filter. This does NOT mean none \
                     exist — there are {total}. A project is usually filed under an internal name. \
                     Every name is below: re-run with a filter drawn from these, or none."
                ),
                "all_workflow_names": names,
            }));
        }
        let enabled = shown.iter().filter(|t| t.enabled).count();
        ToolCallResult::success_json(json!({
            "count": rows.len(),
            "total": total,
            "enabled_count": enabled,
            "disabled_count": rows.len() - enabled,
            "filter": filter,
            "workflows": rows,
        }))
    }

    async fn upsert_workflow(&self, neo4j: &Neo4jClient, args: &Value) -> ToolCallResult {
        let live = self.live_tools.read().await.clone();
        let schemas = self.tool_schemas.read().await.clone();
        let catalog = self
            .telemetry
            .as_ref()
            .map(agent_brain_llm::model_router::catalog_models)
            .unwrap_or_default();
        let checks = workflows::SaveChecks {
            live_tools: &live,
            tool_schemas: &schemas,
            catalog: &catalog,
            envelope: self.envelope_schemas.get("workflow"),
        };
        let saved = match workflows::save(neo4j, None, false, args, &checks).await {
            Ok(s) => s,
            Err(e) => return ToolCallResult::error(e.to_string()),
        };
        let steps: Vec<ChainStep> = serde_json::from_str(&saved.workflow.steps).unwrap_or_default();
        let mut step_warnings = lint_chain_steps(&steps, &live);
        step_warnings.extend(self.pinned_model_warnings(&steps));
        let mut response = json!({
            "created": saved.created,
            "updated": !saved.created,
            "workflow": workflows::to_body(&saved.workflow),
            "id": saved.workflow.id,
        });
        if !saved.warnings.is_empty() {
            response["warning"] = json!(saved.warnings.join(" "));
        }
        // Loud, and phrased as unfinished work: the workflow IS saved, and
        // silently these read as "done" and get reported as working.
        if !step_warnings.is_empty() {
            response["step_warnings"] = json!(step_warnings);
            response["needs_fix"] = json!(true);
            response["action_required"] = json!(
                "This workflow is SAVED BUT LIKELY BROKEN. These defects will not surface as \
                 errors; they surface as runs that produce nothing. Re-upsert with the fixes \
                 before telling the user it is set up, and tell them what you corrected."
            );
        }
        ToolCallResult::success_json(response)
    }

    async fn audit_workflows(&self, neo4j: &Neo4jClient, args: &Value) -> ToolCallResult {
        let all = match neo4j.list_workflows(false).await {
            Ok(w) => w,
            Err(e) => return ToolCallResult::error(format!("Failed to fetch workflows: {e}")),
        };
        let live_tools = self.live_tools.read().await.clone();
        let tool_schemas = self.tool_schemas.read().await.clone();
        let catalog = self
            .telemetry
            .as_ref()
            .map(agent_brain_llm::model_router::catalog_models)
            .unwrap_or_default();
        let disable_broken = args["disable_broken"].as_bool().unwrap_or(false);

        let (mut healthy, mut degraded, mut broken, mut disabled) =
            (vec![], vec![], vec![], vec![]);
        for wf in all {
            let Ok(steps) = serde_json::from_str::<Vec<Value>>(&wf.steps) else {
                broken.push(json!({ "name": wf.name, "issue": "steps JSON could not be parsed" }));
                continue;
            };
            let dead: Vec<String> = steps
                .iter()
                .filter_map(|s| s["tool_name"].as_str().map(String::from))
                .filter(|t| !live_tools.is_empty() && !live_tools.contains(t))
                .collect();
            // The same structural checks a save applies, over what is stored
            // (including yaml-seeded workflows that never went through a save).
            let declared: Vec<String> = wf.inputs.iter().map(|i| i.name.clone()).collect();
            let structural: Vec<String> =
                validation::validate_step_array(&steps, &[], &tool_schemas)
                    .iter()
                    .chain(validation::validate_step_models(&steps, &catalog).iter())
                    .chain(validation::validate_input_refs(&steps, &declared).iter())
                    .map(validation::Violation::render)
                    .collect();
            let lint = serde_json::from_value::<Vec<ChainStep>>(Value::Array(steps.clone()))
                .map(|cs| lint_chain_steps(&cs, &[]))
                .unwrap_or_default();
            if dead.is_empty() && structural.is_empty() {
                if lint.is_empty() {
                    healthy.push(json!({ "name": wf.name, "step_count": steps.len() }));
                } else {
                    degraded
                        .push(json!({ "name": wf.name, "enabled": wf.enabled, "warnings": lint }));
                }
                continue;
            }
            // Auto-disable only when every step names a dead tool: a structural
            // error is a real defect but not grounds to switch off a committed workflow.
            let all_dead = !dead.is_empty() && dead.len() == steps.len();
            broken.push(json!({
                "name": wf.name, "enabled": wf.enabled, "dead_tools": dead,
                "structural_errors": structural, "all_steps_dead": all_dead, "warnings": lint,
            }));
            if disable_broken && all_dead && wf.enabled {
                let patch = agent_brain_repository::WorkflowPatch {
                    enabled: Some(false),
                    paused_reason: Some(Some("audit: every step names a dead tool".into())),
                    ..Default::default()
                };
                if neo4j.update_workflow(&wf.id, &patch).await.is_ok() {
                    disabled.push(wf.name);
                }
            }
        }
        ToolCallResult::success_json(json!({
            "live_tool_count": live_tools.len(),
            "healthy_count": healthy.len(),
            "degraded_count": degraded.len(),
            "broken_count": broken.len(),
            "disabled_count": disabled.len(),
            "healthy": healthy,
            "degraded": degraded,
            "broken": broken,
            "disabled": disabled,
        }))
    }
}

// ---------------------------------------------------------------------------
// Custom deserializer: handles absent, null, and string values for session_id
// ---------------------------------------------------------------------------

fn deserialize_optional_session<'de, D>(deserializer: D) -> Result<Option<Option<String>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let v: Option<Value> = Option::deserialize(deserializer)?;
    match v {
        None => Ok(None),                            // field absent
        Some(Value::Null) => Ok(Some(None)),         // explicit null → clear
        Some(Value::String(s)) => Ok(Some(Some(s))), // string → set
        _ => Ok(None),
    }
}

/// Tools whose entire purpose is to persist or hand on the material they are
/// given. Mid-chain, one of these with no `{{_prev}}` anywhere in its arguments
/// stores a fixed literal and silently drops the upstream step's output.
const PERSISTING_TOOLS: &[&str] = &[
    "store_note",
    "push_context",
    "write_workspace_file",
    "create_task",
    "notify_user",
];

/// `note_type` values retrieval and the epistemics layer actually understand.
/// `store_note` takes a free-form string, so an invented type (a chat-authored
/// schedule stored an `analysis` note on 2026-09-08) is persisted silently and
/// then never labelled or selected correctly — `label_claims`, consolidation,
/// and `recall_recent` all key off these names. `news_raw` is the pre-classified
/// news feed type; kept here so the lint doesn't flag a legitimate one.
const VALID_NOTE_TYPES: &[&str] = &[
    "semantic",
    "episodic",
    "reflection",
    "consolidated",
    "outcome",
    "inference",
    "claim",
    "source_record",
    "news",
    "news_raw",
    "unsourced_synthesis",
    // Written by the queue's meta-learning chain (services/queue.rs) and
    // excluded from consolidation source selection by name in
    // services/knowledge/. It is a real type; it was simply never added here,
    // so the lint warned "not a recognized type" about the brain's own output.
    "meta_learning_result",
];

/// True when a step's arguments reference the previous step's output in any
/// form — `{{_prev}}`, a dotted `{{_prev.id}}` path, the `{{result…}}` alias,
/// or a named step's `{{steps.<id>}}`.
fn references_prev(args: Option<&Value>) -> bool {
    args.is_some_and(|v| {
        let s = v.to_string();
        s.contains("{{_prev") || s.contains("{{result") || s.contains("{{steps.")
    })
}

/// Lint a chain for steps that are structurally valid but discard their own
/// inputs. Returns human-readable warnings, most severe first; empty means clean.
///
/// This exists because a workflow's steps are enqueued verbatim — no checks
/// unless the workflow opts in. Deserializing
/// as `Vec<ChainStep>` proves the shape is legal and nothing more, so a chain
/// authored from chat could be accepted, dispatched, and run weekly while doing
/// nothing at all. `Off-Grid Networking Monitor` (created 2026-08-12) did
/// exactly that: a `reason` step with no `context` answered "there is no
/// information" seconds after its own `search_web` returned five results, and a
/// `store_note` whose content was a fixed sentence stored 55 characters a week,
/// forever. Every rule below is one of those three defects.
///
/// Warnings, never rejections. A first step may legitimately store a literal,
/// and a `reason` step may legitimately stand alone — the caller knows which.
/// A false positive that blocks a valid schedule is worse than a warning the
/// author reads and dismisses.
fn lint_chain_steps(steps: &[ChainStep], live_tools: &[String]) -> Vec<String> {
    let mut warnings = Vec::new();

    let has_search = steps.iter().any(|s| s.tool_name == "search_web");

    for (i, step) in steps.iter().enumerate() {
        let n = i + 1;
        let tool = step.tool_name.as_str();
        let args = step.arguments.as_ref();

        if !live_tools.is_empty() && !live_tools.contains(&step.tool_name) {
            warnings.push(format!(
                "step {n} (`{tool}`): no such tool is registered — this step will fail every run."
            ));
        }

        // `reason` reads its retrieval context from `context`. Given only a
        // `question`, it runs RAG over the graph and never sees the upstream
        // step, however carefully that step was written.
        if tool == "reason"
            && i > 0
            && args
                .and_then(|a| a.get("context"))
                .and_then(|c| c.as_str())
                .is_none_or(str::is_empty)
        {
            let prev = &steps[i - 1].tool_name;
            warnings.push(format!(
                "step {n} (`reason`): no `context` argument, so it will reason over the \
                 knowledge graph and never see step {} (`{prev}`) output. Pass \
                 `context: \"{{{{_prev}}}}\"` to hand the upstream result in.",
                i
            ));
        }

        // Routing by omission. `use_local` in queue.rs is
        // `provider_hint == Some("ollama")`, so leaving the field out is not
        // "default to local" — it falls through to the active config, which is
        // a cloud model. A chat-authored schedule almost never sets the field,
        // so its every step lands on cloud: six near-duplicate supply-chain
        // schedules created on 2026-08-25 put four searches and a store_note
        // each on a cloud model that had no reason to be involved, and the
        // clobbered hardware tripwire did the same for two weeks.
        //
        // Only *omission* is flagged. An explicit `ollama-cloud` is a decision
        // someone made and is left alone — the failure being caught is the
        // author who never knew the field existed. A step that pins a `model`
        // needs no hint: it runs on that model, and the hint is derived from it.
        if step.provider_hint.is_none() && step.model.as_deref().is_none_or(|m| m.trim().is_empty())
        {
            warnings.push(format!(
                "step {n} (`{tool}`): no `provider_hint`, so it routes to the active \
                 model (a cloud model) rather than local. Set `provider_hint: \"ollama\"` \
                 unless this step genuinely needs the cloud, in which case set it \
                 explicitly to `\"ollama-cloud\"`."
            ));
        }

        // `run_agent` runs a whole sub-agent for the step. Its `agent` and
        // `task` must be present and non-empty: a blank `agent` cannot name a
        // spawnable worker, and a blank `task` gives the agent nothing to do —
        // both fail at runtime. The spawnable-membership check is enforced live
        // by `run_agent` itself (it returns the roster for a bad name); lint has
        // no roster, so it verifies the fields are populated at all.
        if tool == "run_agent" {
            if args
                .and_then(|a| a.get("agent"))
                .and_then(|c| c.as_str())
                .is_none_or(str::is_empty)
            {
                warnings.push(format!(
                    "step {n} (`run_agent`): no `agent` argument, so it cannot name a spawnable \
                     worker and will fail every run. Set `agent` to a spawnable agent name."
                ));
            }
            if args
                .and_then(|a| a.get("task"))
                .and_then(|c| c.as_str())
                .is_none_or(str::is_empty)
            {
                warnings.push(format!(
                    "step {n} (`run_agent`): no `task` argument, so the agent has nothing to do. \
                     Set `task` to a complete, self-contained instruction (use `{{{{_prev}}}}` to \
                     hand in the upstream result)."
                ));
            }
        }

        if PERSISTING_TOOLS.contains(&tool) && i > 0 && !references_prev(args) {
            warnings.push(format!(
                "step {n} (`{tool}`): arguments contain no `{{{{_prev}}}}`, so it will \
                 persist a fixed literal and discard step {} output.",
                i
            ));
        }

        // A note with no retrieval key cannot be found by the next run, which
        // is what makes a watch schedule a watch rather than a weekly restatement.
        if tool == "store_note" {
            if args
                .and_then(|a| a.get("source_context"))
                .and_then(|c| c.as_str())
                .is_none_or(str::is_empty)
            {
                warnings.push(format!(
                    "step {n} (`store_note`): no `source_context`, so a later run has no key \
                     to retrieve this note by and cannot build on it."
                ));
            }
            // An invented note_type is stored verbatim and then never labelled,
            // consolidated, or recalled correctly — retrieval and the epistemics
            // layer only understand the canonical set.
            if let Some(nt) = args
                .and_then(|a| a.get("note_type"))
                .and_then(|c| c.as_str())
                .filter(|t| !t.is_empty())
                && !VALID_NOTE_TYPES.contains(&nt)
            {
                warnings.push(format!(
                    "step {n} (`store_note`): `note_type: \"{nt}\"` is not a recognized type, so \
                     the note is stored but never labelled or recalled correctly. Use one of: {}.",
                    VALID_NOTE_TYPES.join(", ")
                ));
            }
            // Web-derived material is what a source asserted, not what the brain
            // established. Typed `semantic` it comes back out of retrieval
            // unlabelled and gets relayed as settled fact.
            if has_search
                && args
                    .and_then(|a| a.get("note_type"))
                    .and_then(|c| c.as_str())
                    .is_none_or(|t| t == "semantic")
            {
                warnings.push(format!(
                    "step {n} (`store_note`): this chain searches the web, so its output is \
                     what sources asserted, not established knowledge. Use \
                     `note_type: \"source_record\"` so retrieval labels it as unverified."
                ));
            }
        }
    }

    // A chain whose FINAL step only computes has nowhere to put its result.
    //
    // Deliberately narrow. The first version of this rule asked whether any step
    // was in PERSISTING_TOOLS, and flagged five of six live schedules: three
    // persist through their own side effects (`poll_media_sources` creates
    // Tasks, `discover_media_sources` creates MediaSource nodes,
    // `write_codebase_doc` writes a file) and one ends in `reason` with
    // `store_inference: true`. An enumerate-the-persisters rule cannot see any
    // of that, and a lint that is wrong five times out of six is one the next
    // author learns to skip past. So: only the two terminal-compute tools that
    // actually occur, and `reason` only when it is not storing.
    if let Some(last) = steps.last() {
        let computes_only = match last.tool_name.as_str() {
            "search_web" => true,
            "reason" => {
                last.arguments
                    .as_ref()
                    .and_then(|a| a.get("store_inference"))
                    .and_then(|v| v.as_bool())
                    != Some(true)
            }
            _ => false,
        };
        if computes_only {
            warnings.push(format!(
                "final step (`{}`) only computes — the chain's result is not stored, \
                 handed on, or notified anywhere, so every run throws its output away. \
                 Add a `store_note`/`notify_user` step, or set `store_inference: true`.",
                last.tool_name
            ));
        }
    }

    warnings
}

/// Render a recurrence period as a human phrase ("weekly", "every 6h").
///
/// The raw `interval_seconds` is what the scheduler needs; `604800` is not what
/// a reader needs in order to judge whether a project is ticking over.
fn humanize_interval(secs: i64) -> String {
    const HOUR: i64 = 3600;
    const DAY: i64 = 86_400;
    match secs {
        s if s <= 0 => "invalid".to_string(),
        s if s % (30 * DAY) == 0 && s / (30 * DAY) == 1 => "monthly".to_string(),
        s if s % (7 * DAY) == 0 && s / (7 * DAY) == 1 => "weekly".to_string(),
        s if s % (7 * DAY) == 0 => format!("every {} weeks", s / (7 * DAY)),
        s if s == DAY => "daily".to_string(),
        s if s % DAY == 0 => format!("every {} days", s / DAY),
        s if s % HOUR == 0 => format!("every {}h", s / HOUR),
        s if s % 60 == 0 => format!("every {}m", s / 60),
        s => format!("every {s}s"),
    }
}

// =========================================================================
// Skill implementation
// =========================================================================

#[async_trait]
impl Skill for SchedulerSkill {
    fn name(&self) -> &str {
        "scheduler"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        let mut tools = vec![Self::scheduler_control_def()];
        if self.neo4j.is_some() {
            tools.push(Self::manage_workflow_def());
        }
        tools
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        let result = match tool_name {
            "scheduler_control" => self.handle_scheduler_control(arguments).await,
            "manage_workflow" => self.handle_manage_workflow(arguments).await,
            _ => return None,
        };
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn humanize_interval_names_the_common_cadences() {
        assert_eq!(humanize_interval(86_400), "daily");
        assert_eq!(humanize_interval(604_800), "weekly");
        assert_eq!(humanize_interval(2_592_000), "monthly");
    }

    #[test]
    fn humanize_interval_handles_multiples_and_sub_day_periods() {
        // The three "Critical Tech Dependency Mapping" schedules use exactly
        // these cadences; "every 2 weeks" is what makes a bi-weekly job legible.
        assert_eq!(humanize_interval(1_209_600), "every 2 weeks");
        assert_eq!(humanize_interval(21_600), "every 6h");
        assert_eq!(humanize_interval(300), "every 5m");
        assert_eq!(humanize_interval(172_800), "every 2 days");
    }

    #[test]
    fn humanize_interval_rejects_a_nonsense_period() {
        assert_eq!(humanize_interval(0), "invalid");
        assert_eq!(humanize_interval(-1), "invalid");
    }

    /// A step pinned local, which is what a schedule step should almost always
    /// be. Set explicitly so these tests exercise the rule they are named for
    /// rather than all tripping the missing-`provider_hint` warning; see
    /// [`step_without_hint`] for that one.
    fn step(tool: &str, args: Value) -> ChainStep {
        serde_json::from_value(
            json!({ "tool_name": tool, "arguments": args, "provider_hint": "ollama" }),
        )
        .unwrap()
    }

    /// A step as a chat session typically authors one — no `provider_hint` at
    /// all, which routes to the active cloud model rather than to local.
    fn step_without_hint(tool: &str, args: Value) -> ChainStep {
        serde_json::from_value(json!({ "tool_name": tool, "arguments": args })).unwrap()
    }

    /// The exact chain `Off-Grid Networking Monitor` was created with from a
    /// chat session. It deserialized cleanly, was accepted, dispatched, and ran
    /// — producing a 55-character placeholder note. All three defects must be
    /// named, because "it saved fine" is what the author reported to the user.
    #[test]
    fn lint_catches_the_off_grid_monitor_as_authored() {
        let steps = vec![
            step(
                "search_web",
                json!({ "query": "Meshtastic Reticulum LoRa news" }),
            ),
            step(
                "reason",
                json!({ "question": "Based on the latest search results, what is new?" }),
            ),
            step(
                "store_note",
                json!({ "content": "Updates on off-grid networking research for {{date}}." }),
            ),
        ];

        let w = lint_chain_steps(&steps, &[]);
        assert!(
            w.iter()
                .any(|s| s.contains("step 2") && s.contains("no `context`")),
            "reason-without-context not caught: {w:?}"
        );
        assert!(
            w.iter()
                .any(|s| s.contains("step 3") && s.contains("fixed literal")),
            "store_note-without-prev not caught: {w:?}"
        );
        assert!(
            w.iter().any(|s| s.contains("source_context")),
            "missing retrieval key not caught: {w:?}"
        );
        assert!(
            w.iter().any(|s| s.contains("source_record")),
            "web-derived note typed semantic not caught: {w:?}"
        );
    }

    /// The rewritten chain must come back clean, or the lint is noise the next
    /// author learns to ignore.
    #[test]
    fn lint_passes_the_repaired_chain() {
        let steps = vec![
            step(
                "search_web",
                json!({ "query": "Meshtastic firmware release" }),
            ),
            step(
                "push_context",
                json!({ "session_id": "offgrid-{{task_id}}", "content": "## SWEEP\n{{_prev}}" }),
            ),
            step(
                "neo4j_query",
                json!({ "cypher": "MATCH (w:WorkingMemory) RETURN w.content AS content" }),
            ),
            step(
                "reason",
                json!({ "question": "Write the watch report.", "context": "{{_prev}}" }),
            ),
            step(
                "store_note",
                json!({
                    "content": "# Off-grid networking watch\n\n{{_prev}}",
                    "note_type": "source_record",
                    "source_context": "off_grid_networking_watch"
                }),
            ),
        ];

        assert!(
            lint_chain_steps(&steps, &[]).is_empty(),
            "repaired chain should lint clean: {:?}",
            lint_chain_steps(&steps, &[])
        );
    }

    /// An invented note_type (`analysis`, stored by a chat-authored schedule on
    /// 2026-09-08) must be flagged — it persists but is never labelled or recalled.
    #[test]
    fn lint_flags_an_unknown_note_type() {
        let steps = vec![step(
            "store_note",
            json!({
                "content": "{{_prev}}",
                "note_type": "analysis",
                "source_context": "intelligence_proxy_analysis"
            }),
        )];
        let w = lint_chain_steps(&steps, &[]);
        assert!(
            w.iter().any(|s| s.contains("not a recognized type")),
            "unknown note_type not caught: {w:?}"
        );
    }

    /// A canonical note_type must not trip the unknown-type warning.
    #[test]
    fn lint_accepts_a_known_note_type() {
        let steps = vec![step(
            "store_note",
            json!({
                "content": "{{_prev}}",
                "note_type": "unsourced_synthesis",
                "source_context": "x"
            }),
        )];
        let w = lint_chain_steps(&steps, &[]);
        assert!(
            !w.iter().any(|s| s.contains("not a recognized type")),
            "known note_type wrongly flagged: {w:?}"
        );
    }

    /// `{{_prev.id}}` is how a claim step links provenance back to the note it
    /// came from. It is a reference to the previous step like any other, and
    /// flagging it would push authors to delete the thing that makes the edge.
    #[test]
    fn lint_accepts_a_dotted_prev_path_as_a_reference() {
        let steps = vec![
            step(
                "store_note",
                json!({ "content": "report", "source_context": "x" }),
            ),
            step(
                "create_task",
                json!({ "goal": "follow up", "context": "{{_prev.id}}" }),
            ),
        ];
        let w = lint_chain_steps(&steps, &[]);
        assert!(
            !w.iter().any(|s| s.contains("step 2")),
            "dotted path should count as referencing prev: {w:?}"
        );
    }

    /// A first step has nothing upstream to discard, so a literal is correct
    /// there — this is why the rules are indexed from step 2.
    #[test]
    fn lint_does_not_fault_a_literal_first_step() {
        let steps = vec![
            step(
                "push_context",
                json!({ "session_id": "s", "content": "## BASELINE" }),
            ),
            step(
                "store_note",
                json!({ "content": "{{_prev}}", "source_context": "x" }),
            ),
        ];
        let w = lint_chain_steps(&steps, &[]);
        assert!(
            !w.iter().any(|s| s.contains("step 1")),
            "first step should not be faulted: {w:?}"
        );
    }

    #[test]
    fn lint_flags_a_run_agent_step_missing_agent_or_task() {
        let steps = vec![step("run_agent", json!({ "task": "summarise X" }))];
        let w = lint_chain_steps(&steps, &["run_agent".to_string()]);
        assert!(
            w.iter().any(|s| s.contains("no `agent` argument")),
            "missing agent should be flagged: {w:?}"
        );

        let steps = vec![step("run_agent", json!({ "agent": "researcher" }))];
        let w = lint_chain_steps(&steps, &["run_agent".to_string()]);
        assert!(
            w.iter().any(|s| s.contains("no `task` argument")),
            "missing task should be flagged: {w:?}"
        );
    }

    #[test]
    fn lint_passes_a_well_formed_run_agent_step() {
        let steps = vec![step(
            "run_agent",
            json!({ "agent": "researcher", "task": "the latest on X: {{_prev}}" }),
        )];
        let w = lint_chain_steps(&steps, &["run_agent".to_string()]);
        assert!(
            !w.iter().any(|s| s.contains("run_agent")),
            "a populated run_agent step should not be faulted: {w:?}"
        );
    }

    #[test]
    fn lint_flags_dead_tool_names() {
        let steps = vec![
            step("search_web", json!({ "query": "x" })),
            step("frobnicate", json!({})),
        ];
        let w = lint_chain_steps(&steps, &["search_web".to_string()]);
        assert!(w.iter().any(|s| s.contains("no such tool")), "{w:?}");
    }

    #[test]
    fn lint_flags_a_chain_that_ends_by_computing_and_discarding() {
        let steps = vec![
            step("neo4j_query", json!({ "cypher": "MATCH (n) RETURN n" })),
            step(
                "reason",
                json!({ "question": "how healthy?", "context": "{{_prev}}" }),
            ),
        ];
        let w = lint_chain_steps(&steps, &[]);
        assert!(
            w.iter().any(|s| s.contains("throws its output away")),
            "{w:?}"
        );
    }

    /// Every one of these was a false positive from the first version of the
    /// rule, taken from live `schedules/*.yaml`. A tool that persists through
    /// its own side effects is invisible to any enumerate-the-persisters check,
    /// and `reason` with `store_inference: true` does store its output.
    #[test]
    fn lint_does_not_fault_chains_that_persist_via_side_effects() {
        let side_effect_endings = [
            step("poll_media_sources", json!({})),
            step("discover_media_sources", json!({ "max": 3 })),
            step("write_codebase_doc", json!({ "content": "{{_prev}}" })),
            step(
                "reason",
                json!({ "question": "q", "context": "{{_prev}}", "store_inference": true }),
            ),
        ];

        for last in side_effect_endings {
            let tool = last.tool_name.clone();
            let steps = vec![
                step("neo4j_query", json!({ "cypher": "MATCH (n) RETURN n" })),
                last,
            ];
            let w = lint_chain_steps(&steps, &[]);
            assert!(
                !w.iter().any(|s| s.contains("throws its output away")),
                "`{tool}` ending should not be faulted: {w:?}"
            );
        }
    }

    /// The definition that overwrote `workflows/hardware-tripwire.yaml` from a
    /// chat session on 2026-08-25, reduced to its routing. Not one step named a
    /// `provider_hint`, so all six ran on the cloud model — including two
    /// `search_web` calls and a `store_note`, none of which needs an LLM of any
    /// size. The same omission put six near-duplicate supply-chain schedules on
    /// cloud the same afternoon. Nothing rejected any of it.
    #[test]
    fn lint_flags_every_step_that_routes_to_cloud_by_omission() {
        let steps = vec![
            step_without_hint("search_web", json!({ "question": "primary signals" })),
            step_without_hint("reason", json!({ "question": "q", "context": "{{_prev}}" })),
            step_without_hint(
                "store_note",
                json!({ "content": "{{_prev}}", "source_context": "hardware_tripwire" }),
            ),
        ];

        let w = lint_chain_steps(&steps, &[]);
        for n in 1..=3 {
            assert!(
                w.iter()
                    .any(|s| s.starts_with(&format!("step {n} ")) && s.contains("provider_hint")),
                "step {n} routing-by-omission not caught: {w:?}"
            );
        }
    }

    /// A pinned model decides where a step runs, so its missing hint is not
    /// routing by omission (the queue derives the hint from the model).
    #[test]
    fn lint_does_not_ask_a_pinned_step_for_a_hint() {
        let pinned: ChainStep = serde_json::from_value(json!({
            "tool_name": "reason",
            "arguments": { "question": "q" },
            "model": "gpt-oss:20b-cloud",
        }))
        .unwrap();
        let w = lint_chain_steps(&[pinned], &[]);
        assert!(!w.iter().any(|s| s.contains("provider_hint")), "{w:?}");
    }

    /// Only omission is a defect. A step that names the cloud on purpose — the
    /// one synthesis step these watch schedules are built around — must pass,
    /// or the warning trains authors to delete the very field it asks for.
    #[test]
    fn lint_does_not_fault_a_deliberate_cloud_step() {
        let deliberate: ChainStep = serde_json::from_value(json!({
            "tool_name": "reason",
            "arguments": { "question": "synthesise", "context": "{{_prev}}" },
            "provider_hint": "ollama-cloud",
            "required_capabilities": ["reasoning"],
        }))
        .unwrap();

        let steps = vec![
            step("search_web", json!({ "query": "signals" })),
            deliberate,
            step(
                "store_note",
                json!({ "content": "{{_prev}}", "source_context": "hardware_tripwire" }),
            ),
        ];

        let w = lint_chain_steps(&steps, &[]);
        assert!(
            !w.iter().any(|s| s.contains("provider_hint")),
            "an explicit cloud step must not be faulted: {w:?}"
        );
    }
}
