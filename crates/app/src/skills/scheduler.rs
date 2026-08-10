//! Scheduler Skill — controls the autonomous self-improvement loop.
//!
//! 4 tools: `scheduler_control`, `run_scheduler_tick`, `manage_chain`, `manage_scheduled_task`
//! Scheduler status and chain listing are served by REST: GET /api/scheduler/status,
//! /api/scheduler/chains. Scheduled-task listing is deliberately NOT REST-only —
//! see `manage_scheduled_task(action=list)`.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::repository::Neo4jClient;
use crate::services::scheduler::SchedulerService;
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

pub struct SchedulerSkill {
    service: Arc<SchedulerService>,
    neo4j: Option<Neo4jClient>,
    /// Live tool names populated after skill registration — used by the audit action.
    live_tools: Arc<RwLock<Vec<String>>>,
}

impl SchedulerSkill {
    pub fn new(service: Arc<SchedulerService>, neo4j: Option<Neo4jClient>) -> Self {
        Self {
            service,
            neo4j,
            live_tools: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Inject a pre-populated live tool list (replaces the default empty vec).
    pub fn with_live_tools(mut self, live_tools: Arc<RwLock<Vec<String>>>) -> Self {
        self.live_tools = live_tools;
        self
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    // get_scheduler_status is served by GET /api/scheduler/status (REST API)

    fn scheduler_control_def() -> ToolDefinition {
        ToolDefinition {
            name: "scheduler_control".to_string(),
            description: "Start, stop, or reconfigure the autonomous scheduler loop. \
                action=start: enable the loop (optionally set interval/session). \
                action=stop: pause the loop (running jobs are not affected). \
                action=configure: update any combination of runtime settings."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["start", "stop", "configure"],
                        "description": "start: enable loop. stop: pause loop. configure: update settings."
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

    fn run_scheduler_tick_def() -> ToolDefinition {
        ToolDefinition {
            name: "run_scheduler_tick".to_string(),
            description: "Execute a scheduler tick immediately, bypassing the timer. \
                          Lists pending tasks, builds job chains, and enqueues them. \
                          Returns the number of tasks found and dispatched."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
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
    // SchedulerChain + ScheduledTask tool definitions
    // =========================================================================

    // list_scheduler_chains is served by GET /api/scheduler/chains (REST API)
    // Listing scheduled tasks is `manage_scheduled_task(action=list)`, NOT just
    // GET /api/scheduled-tasks. It lived only in REST during the tool-reduction
    // refactor, which left the chat agent with no way to see its own recurring
    // work — it answered project-status questions from :Note alone and reported
    // running schedules as unbuilt plans. REST keeps its endpoint for the UI.

    fn manage_chain_def() -> ToolDefinition {
        ToolDefinition {
            name: "manage_chain".to_string(),
            description: "Define or remove a SchedulerChain routing rule in Neo4j. \
                action=define: store or update a chain — when a task goal matches `pattern` \
                or any entry in `patterns` (case-insensitive CONTAINS) the scheduler dispatches \
                `steps` instead of the built-in fallback. Use pattern=\"\" with priority=9999 for a \
                default chain. Template vars in steps: {{task_id}}, {{goal}}, {{date}}, {{file_slug}}. \
                action=remove: delete a chain by its `id`."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["define", "remove"],
                        "description": "define: store/update chain. remove: delete by id."
                    },
                    "name": {
                        "type": "string",
                        "description": "Unique chain name used as the MERGE key (defaults to pattern value)."
                    },
                    "pattern": {
                        "type": "string",
                        "description": "Primary goal substring to match (required for action=define). Use \"\" for a default chain."
                    },
                    "patterns": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Additional goal substrings (OR-matched with pattern)."
                    },
                    "steps": {
                        "type": "array",
                        "description": "ChainStep array (tool_name, arguments, priority?, provider_hint?, etc.). \
                                        Template vars: {{task_id}}, {{goal}}, {{date}}, {{file_slug}}. (required for define)"
                    },
                    "priority": {
                        "type": "integer",
                        "description": "Check order — lower = first (default 100). Use 9999 for default chain."
                    },
                    "description": {
                        "type": "string",
                        "description": "Human-readable description of this chain."
                    },
                    "no_evaluator": {
                        "type": "boolean",
                        "description": "When true, suppress the evaluator step even if task has success_criteria (default false)."
                    },
                    "no_adversarial": {
                        "type": "boolean",
                        "description": "When true, suppress the adversarial pre-flight step even if task has success_criteria (default false)."
                    },
                    "id": {
                        "type": "string",
                        "description": "SchedulerChain node id (required for action=remove)."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    fn manage_scheduled_task_def() -> ToolDefinition {
        ToolDefinition {
            name: "manage_scheduled_task".to_string(),
            description: "List, create/update, delete, or audit ScheduledTasks. \
                action=list: return every scheduled task with its name, description, enabled flag, \
                interval, last run and next run. USE THIS to answer any question about what the brain \
                is scheduled to do, what recurring work exists, or the status of an ongoing project — \
                a project's implementation usually lives in ScheduledTasks, not in notes. \
                action=upsert: if a task with `name` exists it is updated in-place, otherwise created. \
                Steps are ChainStep objects (tool_name, arguments, priority?, max_attempts?, provider_hint?). \
                Template vars: {{task_id}}, {{goal}}, {{date}}. Do NOT include update_task — appended automatically. \
                Ownership: tasks created here are managed_by='runtime' (the seeder never touches them). \
                Tasks managed_by='yaml' are force-synced from schedules/*.yaml on every startup — \
                runtime edits to them are overwritten unless you pass managed_by='runtime' to take ownership. \
                action=delete: permanently remove a task by `id` (use upsert with enabled=false to pause instead). \
                action=audit: scan all ScheduledTask steps against the live tool registry and return a report \
                of broken tool names, plus optionally disable tasks that reference only dead tools."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["list", "upsert", "delete", "audit"],
                        "description": "list: show all scheduled tasks and when they last/next run. upsert: create or update. delete: remove by id. audit: validate all task steps."
                    },
                    "enabled_only": {
                        "type": "boolean",
                        "description": "list only: return only enabled tasks (default false — disabled tasks are usually the interesting ones)."
                    },
                    "filter": {
                        "type": "string",
                        "description": "list only: case-insensitive substring matched against name and description. Use it when looking for a specific project's schedules — the full list can be truncated before it reaches you."
                    },
                    "verbose": {
                        "type": "boolean",
                        "description": "list only: include id, description, managed_by and step_count. Default false keeps the list compact enough to survive result truncation; set true only when narrowed by `filter`."
                    },
                    "disable_broken": {
                        "type": "boolean",
                        "description": "audit only: if true, disable tasks where ALL steps reference dead tools (default false)."
                    },
                    "name": {
                        "type": "string",
                        "description": "Task name — upsert key and goal text (required for upsert)."
                    },
                    "description": { "type": "string" },
                    "enabled": {
                        "type": "boolean",
                        "description": "Whether to dispatch when due (default true)."
                    },
                    "interval_seconds": {
                        "type": "integer",
                        "minimum": 60,
                        "description": "Recurrence period in seconds (required for upsert)."
                    },
                    "steps": {
                        "type": "array",
                        "description": "Job chain steps (required for upsert)."
                    },
                    "next_run_at": {
                        "type": "string",
                        "description": "ISO8601 datetime to force next run (omit to use now)."
                    },
                    "managed_by": {
                        "type": "string",
                        "enum": ["yaml", "runtime"],
                        "description": "Ownership marker. Omit to keep current (new tasks default to 'runtime'). Set 'runtime' to take ownership of a yaml-seeded task so the seeder stops overwriting it."
                    },
                    "id": {
                        "type": "string",
                        "description": "ScheduledTask id (required for action=delete)."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    // =========================================================================
    // SchedulerChain + ScheduledTask handlers
    // =========================================================================

    async fn handle_manage_chain(&self, args: Option<Value>) -> ToolCallResult {
        let args = args.unwrap_or_default();
        let action = match args["action"].as_str() {
            Some(a) => a.to_string(),
            None => return ToolCallResult::error("`action` is required".to_string()),
        };

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        match action.as_str() {
            "define" => {
                let pattern = match args["pattern"].as_str() {
                    Some(p) => p.to_string(),
                    None => {
                        return ToolCallResult::error(
                            "`pattern` required for action=define".to_string(),
                        );
                    }
                };
                if args["steps"].as_array().is_none() {
                    return ToolCallResult::error(
                        "`steps` array required for action=define".to_string(),
                    );
                }
                let steps_json = match serde_json::to_string(&args["steps"]) {
                    Ok(s) => s,
                    Err(e) => {
                        return ToolCallResult::error(format!("Failed to serialize steps: {e}"));
                    }
                };
                let priority = args["priority"].as_i64().unwrap_or(100);
                let description = args["description"].as_str().unwrap_or("").to_string();
                let name = args["name"].as_str().unwrap_or(&pattern).to_string();
                let patterns: Vec<String> = args["patterns"]
                    .as_array()
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(String::from))
                            .collect()
                    })
                    .unwrap_or_default();
                let no_evaluator = args["no_evaluator"].as_bool().unwrap_or(false);
                let no_adversarial = args["no_adversarial"].as_bool().unwrap_or(false);
                let id = uuid::Uuid::new_v4().to_string();

                let cypher = "MERGE (c:SchedulerChain {name: $name}) \
                              ON CREATE SET c.id = $id, c.created_at = datetime() \
                              SET c.pattern       = $pattern, \
                                  c.patterns      = $patterns, \
                                  c.steps         = $steps, \
                                  c.priority      = $priority, \
                                  c.description   = $description, \
                                  c.no_evaluator  = $no_evaluator, \
                                  c.no_adversarial = $no_adversarial, \
                                  c.updated_at    = datetime()";

                if let Err(e) = neo4j
                    .run(
                        neo4rs::query(cypher)
                            .param("name", name.as_str())
                            .param("pattern", pattern.clone())
                            .param("patterns", patterns)
                            .param("id", id)
                            .param("steps", steps_json)
                            .param("priority", priority)
                            .param("description", description.as_str())
                            .param("no_evaluator", no_evaluator)
                            .param("no_adversarial", no_adversarial),
                    )
                    .await
                {
                    return ToolCallResult::error(format!("Failed to store SchedulerChain: {e}"));
                }

                ToolCallResult::success_json(json!({
                    "stored": true,
                    "name": name,
                    "pattern": pattern,
                    "priority": priority,
                    "no_evaluator": no_evaluator,
                    "no_adversarial": no_adversarial,
                    "step_count": args["steps"].as_array().map(|a| a.len()).unwrap_or(0),
                }))
            }
            "remove" => {
                let id = match args["id"].as_str() {
                    Some(v) => v.to_string(),
                    None => {
                        return ToolCallResult::error(
                            "`id` required for action=remove".to_string(),
                        );
                    }
                };
                let cypher =
                    "MATCH (c:SchedulerChain {id: $id}) DELETE c RETURN count(c) AS deleted";
                match neo4j
                    .execute(neo4rs::query(cypher).param("id", id.clone()))
                    .await
                {
                    Ok(rows) => {
                        let deleted = rows
                            .first()
                            .and_then(|r| r.get::<i64>("deleted").ok())
                            .unwrap_or(0);
                        if deleted > 0 {
                            ToolCallResult::success_json(json!({ "deleted": true, "id": id }))
                        } else {
                            ToolCallResult::error(format!("No SchedulerChain found with id '{id}'"))
                        }
                    }
                    Err(e) => ToolCallResult::error(format!("Failed to remove chain: {e}")),
                }
            }
            other => {
                ToolCallResult::error(format!("Unknown action `{other}`. Use define or remove."))
            }
        }
    }

    async fn handle_manage_scheduled_task(&self, args: Option<Value>) -> ToolCallResult {
        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        let args = args.unwrap_or_default();
        let action = match args["action"].as_str() {
            Some(a) => a.to_string(),
            None => return ToolCallResult::error("`action` is required".to_string()),
        };

        match action.as_str() {
            // Listing was originally left to `GET /api/scheduled-tasks`, which the
            // chat agent cannot call. The result: asked "where are we on project X",
            // the brain searched :Note, found only the design discussion, and
            // reported live recurring work as an unbuilt plan. A project's actual
            // implementation lives in ScheduledTasks, so listing them must be a tool.
            "list" => {
                let enabled_only = args["enabled_only"].as_bool().unwrap_or(false);
                let filter = args["filter"].as_str().map(|s| s.to_lowercase());
                let verbose = args["verbose"].as_bool().unwrap_or(false);
                let all = match neo4j.list_scheduled_tasks(enabled_only).await {
                    Ok(t) => t,
                    Err(e) => {
                        return ToolCallResult::error(format!(
                            "Failed to list scheduled tasks: {e}"
                        ));
                    }
                };
                let total = all.len();
                let names: Vec<String> = all.iter().map(|t| t.name.clone()).collect();
                let tasks: Vec<_> = match filter {
                    Some(ref f) => all
                        .into_iter()
                        .filter(|t| {
                            t.name.to_lowercase().contains(f)
                                || t.description
                                    .as_deref()
                                    .is_some_and(|d| d.to_lowercase().contains(f))
                        })
                        .collect(),
                    None => all,
                };
                // The payload is kept deliberately lean. Chat truncates tool
                // results to as little as 2000 chars, and the repository orders
                // by next_run_at ASC — so long-cadence schedules (exactly the
                // strategic, project-level ones) sort last and are the first to
                // be cut. A verbose row here means those get silently dropped.
                let rows: Vec<Value> = tasks
                    .iter()
                    .map(|t| {
                        let mut row = json!({
                            "name":           t.name,
                            "enabled":        t.enabled,
                            "every":          humanize_interval(t.interval_seconds),
                            // Date only: the time-of-day of a weekly job is noise
                            // next to whether it ran at all.
                            "last_run":       t.last_run_at.as_deref()
                                                  .map(|d| &d[..10.min(d.len())]),
                            "next_run":       &t.next_run_at[..10.min(t.next_run_at.len())],
                        });
                        if verbose {
                            row["id"] = json!(t.id);
                            row["description"] = json!(t.description);
                            row["managed_by"] = json!(t.managed_by);
                            row["interval_seconds"] = json!(t.interval_seconds);
                            row["step_count"] = json!(
                                serde_json::from_str::<Vec<Value>>(&t.steps)
                                    .map(|s| s.len())
                                    .unwrap_or(0)
                            );
                        }
                        row
                    })
                    .collect();
                // A filter that matches nothing is the single most misleading
                // result this tool can return: asked about the "Niche
                // Intelligence Agency", the model filtered on that phrase, got
                // an empty list, and reported three live schedules as
                // nonexistent — they are filed under their internal name
                // ("hardware tripwire", "SLM benchmark watch"). So an empty
                // filtered result always carries every name back with it. The
                // model cannot conclude "none exist" while looking at the list.
                if filter.is_some() && rows.is_empty() && total > 0 {
                    return ToolCallResult::success_json(json!({
                        "count":           0,
                        "total_scheduled": total,
                        "filter":          filter,
                        "warning": format!(
                            "No scheduled task NAME or DESCRIPTION matched that filter. \
                             This does NOT mean none exist — {total} scheduled tasks are \
                             active. A project is usually filed under an internal/technical \
                             name rather than the name the user calls it. Every name is \
                             listed below: re-run with a filter drawn from these, or with \
                             no filter at all."
                        ),
                        "all_scheduled_task_names": names,
                    }));
                }
                let enabled_count = tasks.iter().filter(|t| t.enabled).count();
                ToolCallResult::success_json(json!({
                    "count":           rows.len(),
                    "total_scheduled": total,
                    "enabled_count":   enabled_count,
                    "disabled_count":  rows.len() - enabled_count,
                    "filter":          filter,
                    "scheduled_tasks": rows,
                }))
            }
            "upsert" => {
                let name = match args["name"].as_str() {
                    Some(v) => v.to_string(),
                    None => {
                        return ToolCallResult::error(
                            "`name` required for action=upsert".to_string(),
                        );
                    }
                };
                let interval_seconds = match args["interval_seconds"].as_i64() {
                    Some(v) if v >= 60 => v,
                    _ => {
                        return ToolCallResult::error(
                            "`interval_seconds` (>=60) required for action=upsert".to_string(),
                        );
                    }
                };
                let steps_json = match serde_json::to_string(&args["steps"]) {
                    Ok(s) if args["steps"].is_array() => s,
                    _ => {
                        return ToolCallResult::error(
                            "`steps` must be an array for action=upsert".to_string(),
                        );
                    }
                };
                use crate::services::queue::ChainStep;
                if let Err(e) = serde_json::from_str::<Vec<ChainStep>>(&steps_json) {
                    return ToolCallResult::error(format!("steps JSON invalid: {e}"));
                }

                let description = args["description"].as_str();
                let enabled = args["enabled"].as_bool().unwrap_or(true);
                let managed_by = match args["managed_by"].as_str() {
                    Some("yaml") => Some("yaml"),
                    Some("runtime") => Some("runtime"),
                    Some(other) => {
                        return ToolCallResult::error(format!(
                            "`managed_by` must be 'yaml' or 'runtime', got '{other}'"
                        ));
                    }
                    None => None,
                };

                let existing = neo4j
                    .execute(
                        neo4rs::query(
                            "MATCH (s:ScheduledTask {name: $name}) \
                             RETURN s.id AS id, s.managed_by AS managed_by",
                        )
                        .param("name", name.as_str()),
                    )
                    .await
                    .ok()
                    .and_then(|rows| rows.into_iter().next())
                    .map(|row| {
                        (
                            row.get::<String>("id").unwrap_or_default(),
                            row.get::<String>("managed_by").ok(),
                        )
                    });

                let mut yaml_owned_warning: Option<String> = None;
                let result = if let Some((id, current_managed_by)) = existing {
                    if current_managed_by.as_deref() == Some("yaml")
                        && managed_by.is_none_or(|m| m == "yaml")
                    {
                        yaml_owned_warning = Some(
                            "This task is yaml-managed: steps, description, and interval \
                             will be overwritten from schedules/*.yaml on the next startup. \
                             Pass managed_by='runtime' to take runtime ownership, or edit \
                             the YAML file instead."
                                .to_string(),
                        );
                    }
                    neo4j
                        .update_scheduled_task(
                            &id,
                            Some(&name),
                            Some(description),
                            Some(enabled),
                            Some(interval_seconds),
                            Some(&steps_json),
                            args["next_run_at"].as_str(),
                            managed_by,
                        )
                        .await
                        .map(|opt| opt.map(|t| (t, false)))
                        .map_err(|e| e.to_string())
                } else {
                    let next_run_at = args["next_run_at"]
                        .as_str()
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
                    neo4j
                        .create_scheduled_task(
                            &name,
                            description,
                            enabled,
                            interval_seconds,
                            &steps_json,
                            &next_run_at,
                            managed_by.unwrap_or("runtime"),
                        )
                        .await
                        .map(|t| Some((t, true)))
                        .map_err(|e| e.to_string())
                };

                match result {
                    Ok(Some((task, created))) => {
                        let mut response = json!({
                            "created": created,
                            "updated": !created,
                            "task": task,
                        });
                        if let Some(w) = yaml_owned_warning {
                            response["warning"] = json!(w);
                        }
                        ToolCallResult::success_json(response)
                    }
                    Ok(None) => ToolCallResult::error("Task not found after update".to_string()),
                    Err(e) => {
                        ToolCallResult::error(format!("Failed to upsert scheduled task: {e}"))
                    }
                }
            }
            "delete" => {
                let id = match args["id"].as_str() {
                    Some(v) => v.to_string(),
                    None => {
                        return ToolCallResult::error(
                            "`id` required for action=delete".to_string(),
                        );
                    }
                };
                match neo4j.delete_scheduled_task(&id).await {
                    Ok(true) => ToolCallResult::success_json(json!({ "deleted": true, "id": id })),
                    Ok(false) => {
                        ToolCallResult::error(format!("No ScheduledTask found with id '{id}'"))
                    }
                    Err(e) => {
                        ToolCallResult::error(format!("Failed to delete scheduled task: {e}"))
                    }
                }
            }
            "audit" => self.handle_audit_scheduled_tasks(&args).await,
            other => ToolCallResult::error(format!(
                "Unknown action `{other}`. Use upsert, delete, or audit."
            )),
        }
    }

    async fn handle_audit_scheduled_tasks(&self, args: &Value) -> ToolCallResult {
        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        // Fetch all ScheduledTask nodes.
        let rows = match neo4j
            .execute(neo4rs::query(
                "MATCH (s:ScheduledTask) RETURN s.id AS id, s.name AS name, \
                 s.enabled AS enabled, s.steps AS steps",
            ))
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return ToolCallResult::error(format!("Failed to fetch scheduled tasks: {e}"));
            }
        };

        let live_tools = self.live_tools.read().await;
        let disable_broken = args["disable_broken"].as_bool().unwrap_or(false);

        let mut healthy: Vec<Value> = Vec::new();
        let mut broken: Vec<Value> = Vec::new();
        let mut disabled_ids: Vec<String> = Vec::new();

        for row in rows {
            let id: String = row.get("id").unwrap_or_default();
            let name: String = row.get("name").unwrap_or_default();
            let enabled: bool = row.get("enabled").unwrap_or(true);
            let steps_json: String = row.get("steps").unwrap_or_default();

            let steps: Vec<Value> = match serde_json::from_str(&steps_json) {
                Ok(v) => v,
                Err(_) => {
                    broken.push(json!({
                        "id": id, "name": name,
                        "issue": "steps JSON could not be parsed",
                    }));
                    continue;
                }
            };

            let dead: Vec<String> = steps
                .iter()
                .filter_map(|s| s["tool_name"].as_str().map(|t| t.to_string()))
                .filter(|t| !live_tools.is_empty() && !live_tools.contains(t))
                .collect();

            if dead.is_empty() {
                healthy.push(json!({ "id": id, "name": name, "step_count": steps.len() }));
            } else {
                let all_dead = dead.len() == steps.len();
                broken.push(json!({
                    "id": id,
                    "name": name,
                    "enabled": enabled,
                    "dead_tools": dead,
                    "all_steps_dead": all_dead,
                }));

                if disable_broken && all_dead && enabled {
                    // Disable rather than delete so the user can inspect and fix.
                    let _ = neo4j
                        .run(
                            neo4rs::query(
                                "MATCH (s:ScheduledTask {id: $id}) SET s.enabled = false",
                            )
                            .param("id", id.clone()),
                        )
                        .await;
                    disabled_ids.push(id);
                }
            }
        }

        let live_tool_count = live_tools.len();
        drop(live_tools);

        ToolCallResult::success_json(json!({
            "live_tool_count": live_tool_count,
            "healthy_count": healthy.len(),
            "broken_count": broken.len(),
            "disabled_count": disabled_ids.len(),
            "healthy": healthy,
            "broken": broken,
            "disabled_ids": disabled_ids,
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
        let mut tools = vec![
            Self::scheduler_control_def(),
            Self::run_scheduler_tick_def(),
        ];
        if self.neo4j.is_some() {
            tools.push(Self::manage_chain_def());
            tools.push(Self::manage_scheduled_task_def());
        }
        tools
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        let result = match tool_name {
            "scheduler_control" => self.handle_scheduler_control(arguments).await,
            "run_scheduler_tick" => self.handle_run_scheduler_tick().await,
            "manage_chain" => self.handle_manage_chain(arguments).await,
            "manage_scheduled_task" => self.handle_manage_scheduled_task(arguments).await,
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
}
