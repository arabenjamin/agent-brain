//! Work lineage for the HBI's Work workspace: where a task, job, schedule or
//! todo came from, what it ran, and what it produced.
//!
//! Nothing stores this chain as edges, so it is reconstructed from what is
//! recorded:
//! - a schedule run is a Task whose `context` names the ScheduledTask
//!   ([`run_record_context`](crate::services::scheduler::run_record_context));
//! - every job of a task-owned chain carries `__owner_task_id` in its args;
//! - chain steps are linked by `parent_job_id`;
//! - a job's notes are the ids its result reports, kept only if a `:Note`
//!   with that id exists;
//! - a todo turned into a task is `(:Todo)-[:SPAWNED]->(:Task)`.
//!
//! Job records are pruned (completed/cancelled/failed/dead past their
//! retention), so an old task can have no jobs left; the response says so.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{Extension, Json, extract::Path, response::IntoResponse};
use serde_json::{Value, json};

use super::rest::{RestState, bad_request, internal, not_found, unavailable};
use crate::repository::{Neo4jClient, node_ts};
use crate::services::scheduler::{parse_run_record, run_record_marker};

/// Result text shown inline in a chain; the job view returns more.
const RESULT_PREVIEW_CHARS: usize = 1_500;
const RESULT_FULL_CHARS: usize = 20_000;
const NOTE_PREVIEW_CHARS: usize = 240;
const SCHEDULE_RUNS_SHOWN: i64 = 20;

// ── Pure helpers (unit-tested) ───────────────────────────────────────────────

/// The text a tool returned, from a stored `ToolCallResult` JSON.
pub(crate) fn result_text(result_json: &str) -> Option<String> {
    let v: Value = serde_json::from_str(result_json).ok()?;
    let parts: Vec<&str> = v
        .get("content")?
        .as_array()?
        .iter()
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .collect();
    (!parts.is_empty()).then(|| parts.join("\n"))
}

/// Ids a tool result reports (`id`, `note_id`, `note_ids`), from its text when
/// that is JSON. Candidates only: the caller keeps those that are notes.
pub(crate) fn reported_ids(text: &str) -> Vec<String> {
    let Ok(v) = serde_json::from_str::<Value>(text) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for key in ["id", "note_id", "note_ids"] {
        match v.get(key) {
            Some(Value::String(s)) => out.push(s.clone()),
            Some(Value::Array(a)) => {
                out.extend(a.iter().filter_map(|x| x.as_str().map(String::from)))
            }
            _ => {}
        }
    }
    out
}

/// Cut `s` to at most `max` chars; the flag says whether anything was cut.
pub(crate) fn clip(s: &str, max: usize) -> (String, bool) {
    match s.char_indices().nth(max) {
        Some((i, _)) => (s[..i].to_string(), true),
        None => (s.to_string(), false),
    }
}

/// Chain order for jobs given as `(id, parent_job_id)`, already sorted by
/// creation time: each root (no parent in the set) followed by its
/// descendants. Jobs in a cycle or otherwise unreached keep their creation
/// order at the end, so nothing is dropped.
pub(crate) fn chain_order(jobs: &[(String, Option<String>)]) -> Vec<usize> {
    let ids: HashSet<&str> = jobs.iter().map(|(id, _)| id.as_str()).collect();
    let mut children: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut roots = Vec::new();
    for (i, (_, parent)) in jobs.iter().enumerate() {
        match parent.as_deref().filter(|p| ids.contains(p)) {
            Some(p) => children.entry(p).or_default().push(i),
            None => roots.push(i),
        }
    }
    let mut order = Vec::with_capacity(jobs.len());
    let mut seen = vec![false; jobs.len()];
    let mut stack: Vec<usize> = roots.into_iter().rev().collect();
    while let Some(i) = stack.pop() {
        if std::mem::replace(&mut seen[i], true) {
            continue;
        }
        order.push(i);
        if let Some(kids) = children.get(jobs[i].0.as_str()) {
            stack.extend(kids.iter().rev());
        }
    }
    order.extend((0..jobs.len()).filter(|&i| !seen[i]));
    order
}

/// Step tool names from a ScheduledTask's stored `steps` JSON, as the runner
/// sees them: `ChainStep` reads `tool_name` only. A step stored in the old
/// `tool`/`args` shape runs as an empty tool name and fails ("Unknown tool"),
/// so it is counted in the second value rather than shown under the name it
/// will never run as.
pub(crate) fn step_tools(steps_json: &str) -> (Vec<String>, usize) {
    let steps = serde_json::from_str::<Vec<Value>>(steps_json).unwrap_or_default();
    let mut tools = Vec::with_capacity(steps.len());
    let mut legacy = 0;
    for s in &steps {
        match s
            .get("tool_name")
            .and_then(Value::as_str)
            .filter(|t| !t.is_empty())
        {
            Some(t) => tools.push(t.to_string()),
            None => {
                if s.get("tool").is_some() {
                    legacy += 1;
                }
                tools.push(String::new());
            }
        }
    }
    (tools, legacy)
}

// ── Shared shapes ────────────────────────────────────────────────────────────

/// A job as the Work views show it. `preview` bounds the result text.
fn job_json(node: &neo4rs::Node, preview: usize) -> (Value, Vec<String>) {
    let args: Option<Value> = node
        .get::<String>("args_json")
        .ok()
        .filter(|s| !s.is_empty())
        .and_then(|s| serde_json::from_str(&s).ok());
    // `__`-prefixed keys are queue metadata, not the tool's arguments.
    let owner = args
        .as_ref()
        .and_then(|a| a.get("__owner_task_id"))
        .and_then(Value::as_str)
        .map(String::from);
    let visible_args = args.map(|a| match a {
        Value::Object(m) => Value::Object(
            m.into_iter()
                .filter(|(k, _)| !k.starts_with("__"))
                .collect(),
        ),
        other => other,
    });

    let text = node
        .get::<String>("result_json")
        .ok()
        .and_then(|r| result_text(&r));
    let candidates = text.as_deref().map(reported_ids).unwrap_or_default();
    let (result, truncated, chars) = match &text {
        Some(t) => {
            let (c, cut) = clip(t, preview);
            (Some(c), cut, t.chars().count())
        }
        None => (None, false, 0),
    };

    let v = json!({
        "id":                 node.get::<String>("id").unwrap_or_default(),
        "tool_name":          node.get::<String>("tool_name").unwrap_or_default(),
        "status":             node.get::<String>("status").unwrap_or_default(),
        "attempt_count":      node.get::<i64>("attempt_count").unwrap_or(0),
        "max_attempts":       node.get::<i64>("max_attempts").unwrap_or(3),
        "provider_hint":      node.get::<String>("provider_hint").ok().filter(|s| !s.is_empty()),
        "parent_job_id":      node.get::<String>("parent_job_id").ok().filter(|s| !s.is_empty()),
        "owner_task_id":      owner,
        "error":              node.get::<String>("error").ok().filter(|s| !s.is_empty()),
        "dead_letter_reason": node.get::<String>("dead_letter_reason").ok().filter(|s| !s.is_empty()),
        "created_at":         node_ts(node, "created_at"),
        "started_at":         node_ts(node, "started_at"),
        "completed_at":       node_ts(node, "completed_at"),
        "duration_ms":        node.get::<i64>("duration_ms").ok(),
        "args":               visible_args,
        "result":             result,
        "result_truncated":   truncated,
        "result_chars":       chars,
    });
    (v, candidates)
}

/// The candidates that are notes, with a short preview of each.
async fn existing_notes(neo4j: &Neo4jClient, ids: &[String]) -> HashMap<String, Value> {
    if ids.is_empty() {
        return HashMap::new();
    }
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note) WHERE n.id IN $ids \
                 RETURN n.id AS id, n.note_type AS note_type, n.content AS content, \
                        toString(n.created_at) AS created_at",
            )
            .param("ids", ids.to_vec()),
        )
        .await
        .unwrap_or_default();
    rows.iter()
        .map(|r| {
            let id = r.get::<String>("id").unwrap_or_default();
            let (preview, _) = clip(
                &r.get::<String>("content").unwrap_or_default(),
                NOTE_PREVIEW_CHARS,
            );
            let v = json!({
                "id": id,
                "note_type": r.get::<String>("note_type").ok(),
                "preview": preview,
                "created_at": r.get::<String>("created_at").ok(),
            });
            (id, v)
        })
        .collect()
}

/// The candidates that are Tasks (a `create_task` step reports the task it
/// made), except `exclude` — `update_task` echoes the owner's own id.
async fn existing_tasks(
    neo4j: &Neo4jClient,
    ids: &[String],
    exclude: &str,
) -> HashMap<String, Value> {
    let ids: Vec<String> = ids
        .iter()
        .filter(|i| i.as_str() != exclude)
        .cloned()
        .collect();
    if ids.is_empty() {
        return HashMap::new();
    }
    neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Task) WHERE t.id IN $ids \
                 RETURN t.id AS id, t.goal AS goal, t.status AS status",
            )
            .param("ids", ids),
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let id = r.get::<String>("id").unwrap_or_default();
            let v = json!({ "id": id, "goal": r.get::<String>("goal").ok(), "status": r.get::<String>("status").ok() });
            (id, v)
        })
        .collect()
}

/// What a task's chain produced: notes stored and tasks it created.
struct Produced {
    notes: Vec<Value>,
    tasks: Vec<Value>,
}

/// Jobs owned by a task, in chain order, with the notes and tasks each one produced.
async fn owned_jobs(
    neo4j: &Neo4jClient,
    task_id: &str,
    preview: usize,
) -> Result<(Vec<Value>, Produced), String> {
    let needle = format!("\"__owner_task_id\":\"{task_id}\"");
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (j:AgentJob) WHERE j.args_json CONTAINS $needle \
                 RETURN j ORDER BY j.created_at ASC",
            )
            .param("needle", needle),
        )
        .await
        .map_err(|e| e.to_string())?;

    let mut jobs: Vec<(Value, Vec<String>)> = rows
        .iter()
        .filter_map(|r| r.get::<neo4rs::Node>("j").ok())
        .map(|n| job_json(&n, preview))
        .collect();

    let all_candidates: Vec<String> = jobs.iter().flat_map(|(_, c)| c.clone()).collect();
    let notes = existing_notes(neo4j, &all_candidates).await;
    let tasks = existing_tasks(neo4j, &all_candidates, task_id).await;

    let mut produced = Produced {
        notes: Vec::new(),
        tasks: Vec::new(),
    };
    for (job, candidates) in &mut jobs {
        let my_notes: Vec<Value> = candidates
            .iter()
            .filter_map(|id| notes.get(id).cloned())
            .collect();
        let my_tasks: Vec<Value> = candidates
            .iter()
            .filter_map(|id| tasks.get(id).cloned())
            .collect();
        for (list, out) in [
            (&my_notes, &mut produced.notes),
            (&my_tasks, &mut produced.tasks),
        ] {
            for item in list {
                let mut item = item.clone();
                item["via_job"] = job["id"].clone();
                out.push(item);
            }
        }
        job["notes"] = json!(my_notes);
        job["tasks"] = json!(my_tasks);
    }

    let keys: Vec<(String, Option<String>)> = jobs
        .iter()
        .map(|(j, _)| {
            (
                j["id"].as_str().unwrap_or_default().to_string(),
                j["parent_job_id"].as_str().map(String::from),
            )
        })
        .collect();
    let ordered = chain_order(&keys)
        .into_iter()
        .map(|i| jobs[i].0.clone())
        .collect();
    Ok((ordered, produced))
}

/// When a task has no jobs left: why, in words.
async fn missing_jobs_reason(
    neo4j: &Neo4jClient,
    task_created: Option<&str>,
    status: &str,
) -> String {
    if status == "created" {
        return "Not dispatched yet: the scheduler turns a created task into a job chain on its next tick.".into();
    }
    let oldest = neo4j
        .execute(neo4rs::query(
            "MATCH (j:AgentJob) RETURN toString(min(j.created_at)) AS t",
        ))
        .await
        .ok()
        .and_then(|rows| rows.first().and_then(|r| r.get::<String>("t").ok()));
    match (oldest, task_created) {
        (Some(o), Some(c)) if c < o.as_str() => format!(
            "Job records are pruned after their retention window; the oldest kept is from {o}, and this task is older."
        ),
        _ => "No job carries this task as its owner. It may have been run by a path that does not stamp ownership (chat, bedtime chain).".into(),
    }
}

// ── GET /api/work/tasks/{id} ─────────────────────────────────────────────────

/// A task's lineage: where it came from, its job chain, what it produced.
pub async fn handle_task_lineage(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let rows = match neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Task {id: $id}) \
                 OPTIONAL MATCH (t)-[:SUBTASK_OF]->(p:Task) \
                 OPTIONAL MATCH (td:Todo)-[:SPAWNED]->(t) \
                 RETURN t.id AS id, t.goal AS goal, t.status AS status, t.context AS context, \
                        t.success_criteria AS success_criteria, \
                        toString(t.created_at) AS created_at, toString(t.updated_at) AS updated_at, \
                        p.id AS parent_id, p.goal AS parent_goal, p.status AS parent_status, \
                        td.id AS todo_id, td.title AS todo_title",
            )
            .param("id", id.as_str()),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let Some(row) = rows.first() else {
        return not_found();
    };
    let s = |k: &str| row.get::<String>(k).ok().filter(|v| !v.is_empty());
    let status = s("status").unwrap_or_default();
    let context = s("context");
    let created_at = s("created_at");

    // Where it came from: a schedule run, a todo, or a parent task.
    let mut origin = Value::Null;
    if let Some(sid) = context.as_deref().and_then(parse_run_record) {
        let name = neo4j
            .get_scheduled_task(sid)
            .await
            .ok()
            .flatten()
            .map(|st| st.name);
        origin = json!({ "kind": "schedule", "id": sid, "name": name, "exists": name.is_some() });
    } else if let Some(tid) = s("todo_id") {
        origin = json!({ "kind": "todo", "id": tid, "title": s("todo_title") });
    } else if let Some(pid) = s("parent_id") {
        origin = json!({ "kind": "task", "id": pid, "goal": s("parent_goal"), "status": s("parent_status") });
    }

    let subtasks: Vec<Value> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (c:Task)-[:SUBTASK_OF]->(:Task {id: $id}) \
                 RETURN c.id AS id, c.goal AS goal, c.status AS status ORDER BY c.created_at",
            )
            .param("id", id.as_str()),
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| json!({ "id": r.get::<String>("id").ok(), "goal": r.get::<String>("goal").ok(), "status": r.get::<String>("status").ok() }))
        .collect();

    let (jobs, produced, jobs_error) = match owned_jobs(neo4j, &id, RESULT_PREVIEW_CHARS).await {
        Ok((j, p)) => (j, p, None),
        Err(e) => (
            Vec::new(),
            Produced {
                notes: Vec::new(),
                tasks: Vec::new(),
            },
            Some(e),
        ),
    };
    let Produced {
        notes: mut produced_notes,
        tasks: produced_tasks,
    } = produced;

    // Reviews written about this task (adversarial critic, reflections).
    let reflections = neo4j
        .execute(
            neo4rs::query("MATCH (n:Note)-[:REFLECTS_ON]->(:Task {id: $id}) RETURN n.id AS id")
                .param("id", id.as_str()),
        )
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.get::<String>("id").ok())
        .collect::<Vec<_>>();
    for (_, mut n) in existing_notes(neo4j, &reflections).await {
        n["via"] = json!("reflects_on");
        produced_notes.push(n);
    }

    let jobs_note = if jobs.is_empty() && jobs_error.is_none() {
        Some(missing_jobs_reason(neo4j, created_at.as_deref(), &status).await)
    } else {
        None
    };

    Json(json!({
        "task": {
            "id": id,
            "goal": s("goal"),
            "status": status,
            "context": context,
            "success_criteria": s("success_criteria"),
            "created_at": created_at,
            "updated_at": s("updated_at"),
        },
        "origin": origin,
        "subtasks": subtasks,
        "jobs": jobs,
        "jobs_note": jobs_note,
        "jobs_error": jobs_error,
        "notes": produced_notes,
        "created_tasks": produced_tasks,
    }))
    .into_response()
}

// ── GET /api/work/jobs/{id} ──────────────────────────────────────────────────

/// One job in full, with its owning task and its place in the chain.
pub async fn handle_job_detail(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let rows = match neo4j
        .execute(neo4rs::query("MATCH (j:AgentJob {id: $id}) RETURN j").param("id", id.as_str()))
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let Some(node) = rows.first().and_then(|r| r.get::<neo4rs::Node>("j").ok()) else {
        return not_found();
    };
    let (mut job, candidates) = job_json(&node, RESULT_FULL_CHARS);
    let notes = existing_notes(neo4j, &candidates).await;
    job["notes"] = json!(notes.into_values().collect::<Vec<_>>());

    let mut owner = Value::Null;
    let mut chain: Vec<Value> = Vec::new();
    if let Some(tid) = job["owner_task_id"].as_str().map(String::from) {
        let t = neo4j
            .execute(
                neo4rs::query("MATCH (t:Task {id: $id}) RETURN t.goal AS goal, t.status AS status")
                    .param("id", tid.as_str()),
            )
            .await
            .unwrap_or_default();
        owner = json!({
            "id": tid,
            "goal": t.first().and_then(|r| r.get::<String>("goal").ok()),
            "status": t.first().and_then(|r| r.get::<String>("status").ok()),
            "exists": !t.is_empty(),
        });
        if let Ok((jobs, _)) = owned_jobs(neo4j, &tid, 0).await {
            chain = jobs
                .into_iter()
                .map(|j| json!({ "id": j["id"], "tool_name": j["tool_name"], "status": j["status"] }))
                .collect();
        }
    }

    Json(json!({ "job": job, "owner_task": owner, "chain": chain })).into_response()
}

// ── Schedules ────────────────────────────────────────────────────────────────

/// Run-record tasks grouped by the ScheduledTask they ran, newest first.
async fn runs_by_schedule(neo4j: &Neo4jClient) -> Result<HashMap<String, Vec<Value>>, String> {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (t:Task) WHERE t.context STARTS WITH 'Run of ScheduledTask ' \
             RETURN t.id AS id, t.status AS status, t.context AS context, \
                    toString(t.created_at) AS created_at, toString(t.updated_at) AS updated_at \
             ORDER BY t.created_at DESC",
        ))
        .await
        .map_err(|e| e.to_string())?;
    let mut out: HashMap<String, Vec<Value>> = HashMap::new();
    for r in &rows {
        let ctx = r.get::<String>("context").unwrap_or_default();
        if let Some(sid) = parse_run_record(&ctx) {
            out.entry(sid.to_string()).or_default().push(json!({
                "task_id": r.get::<String>("id").ok(),
                "status": r.get::<String>("status").ok(),
                "created_at": r.get::<String>("created_at").ok(),
                "updated_at": r.get::<String>("updated_at").ok(),
            }));
        }
    }
    Ok(out)
}

fn schedule_json(st: &crate::repository::ScheduledTask) -> Value {
    let mut v = serde_json::to_value(st).unwrap_or(Value::Null);
    let (tools, legacy) = step_tools(&st.steps);
    v["step_tools"] = json!(tools);
    v["legacy_steps"] = json!(legacy);
    v
}

/// GET /api/work/schedules — every schedule with its last run's outcome.
pub async fn handle_list_schedules(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let schedules = match neo4j.list_scheduled_tasks(false).await {
        Ok(s) => s,
        Err(e) => return internal(e),
    };
    let (runs, runs_error) = match runs_by_schedule(neo4j).await {
        Ok(r) => (r, None),
        Err(e) => (HashMap::new(), Some(e)),
    };
    let list: Vec<Value> = schedules
        .iter()
        .map(|st| {
            let mine = runs.get(&st.id);
            let mut v = schedule_json(st);
            v["last_run"] = mine.and_then(|r| r.first().cloned()).unwrap_or(Value::Null);
            v["runs_recorded"] = json!(mine.map_or(0, Vec::len));
            v
        })
        .collect();
    Json(json!({ "count": list.len(), "schedules": list, "runs_error": runs_error }))
        .into_response()
}

/// GET /api/work/schedules/{id} — one schedule, its steps and recent runs.
pub async fn handle_schedule_detail(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let st = match neo4j.get_scheduled_task(&id).await {
        Ok(Some(st)) => st,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Task) WHERE t.context CONTAINS $marker \
                 WITH t ORDER BY t.created_at DESC \
                 WITH collect(t) AS rs \
                 RETURN size(rs) AS total, \
                        [t IN rs[0..$n] | {task_id: t.id, status: t.status, \
                          created_at: toString(t.created_at), updated_at: toString(t.updated_at)}] AS runs",
            )
            .param("marker", run_record_marker(&id))
            .param("n", SCHEDULE_RUNS_SHOWN),
        )
        .await;
    let (total, runs) = match rows {
        Ok(r) => (
            r.first().and_then(|x| x.get::<i64>("total").ok()).unwrap_or(0),
            r.first()
                .and_then(|x| x.get::<Vec<neo4rs::BoltMap>>("runs").ok())
                .unwrap_or_default()
                .into_iter()
                .map(|m| {
                    let g = |k: &str| m.get::<String>(k).ok();
                    json!({ "task_id": g("task_id"), "status": g("status"), "created_at": g("created_at"), "updated_at": g("updated_at") })
                })
                .collect::<Vec<_>>(),
        ),
        Err(e) => return internal(e),
    };
    let steps: Value = serde_json::from_str(&st.steps).unwrap_or(Value::Null);
    let mut v = schedule_json(&st);
    v["steps"] = steps;
    Json(json!({ "schedule": v, "runs": runs, "runs_total": total, "runs_shown": runs.len() }))
        .into_response()
}

/// POST /api/work/schedules/{id}/run — dispatch one run now. Returns the
/// run-record task id, which the UI opens.
pub async fn handle_run_schedule_now(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };
    match svc.run_scheduled_task_now(&id).await {
        Ok(task_id) => Json(json!({ "task_id": task_id })).into_response(),
        Err(e) if e.starts_with("No ScheduledTask") => not_found(),
        Err(e) => internal(e),
    }
}

// ── Todos ────────────────────────────────────────────────────────────────────

async fn spawned_tasks(neo4j: &Neo4jClient, todo_id: &str) -> Vec<Value> {
    neo4j
        .execute(
            neo4rs::query(
                "MATCH (:Todo {id: $id})-[:SPAWNED]->(t:Task) \
                 RETURN t.id AS id, t.goal AS goal, t.status AS status, toString(t.created_at) AS created_at \
                 ORDER BY t.created_at DESC",
            )
            .param("id", todo_id),
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String>("id").ok(),
                "goal": r.get::<String>("goal").ok(),
                "status": r.get::<String>("status").ok(),
                "created_at": r.get::<String>("created_at").ok(),
            })
        })
        .collect()
}

/// GET /api/work/todos/{id} — a todo and the tasks made from it.
pub async fn handle_todo_detail(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    match neo4j.get_todo(&id).await {
        Ok(Some(todo)) => {
            Json(json!({ "todo": todo, "tasks": spawned_tasks(neo4j, &id).await })).into_response()
        }
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// POST /api/work/todos/{id}/task — hand a todo to the brain as a Task.
///
/// The task's goal is the todo's title and its context carries the
/// description, so the scheduler plans it like any created task on its next
/// tick. Linked `(:Todo)-[:SPAWNED]->(:Task)`; the todo itself is unchanged.
pub async fn handle_todo_to_task(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let todo = match neo4j.get_todo(&id).await {
        Ok(Some(t)) => t,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    if todo.title.trim().is_empty() {
        return bad_request("This todo has no title to use as a goal");
    }
    let mut context = format!("From todo '{}' (todo: {})", todo.title, todo.id);
    if let Some(d) = todo.description.as_deref().filter(|d| !d.trim().is_empty()) {
        context.push_str("\n\n");
        context.push_str(d);
    }
    let task_id = match neo4j.create_task(&todo.title, Some(&context), None).await {
        Ok(t) => t,
        Err(e) => return internal(e),
    };
    if let Err(e) = neo4j
        .run(
            neo4rs::query(
                "MATCH (td:Todo {id: $todo}), (t:Task {id: $task}) \
                 MERGE (td)-[r:SPAWNED]->(t) SET r.created_at = datetime()",
            )
            .param("todo", id.as_str())
            .param("task", task_id.as_str()),
        )
        .await
    {
        // The task exists either way; say the link is missing rather than hide it.
        return Json(json!({ "task_id": task_id, "warning": format!("Task created but not linked to the todo: {e}") }))
            .into_response();
    }
    Json(json!({ "task_id": task_id })).into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn result_text_joins_text_parts() {
        let r = r#"{"content":[{"type":"text","text":"a"},{"type":"image"},{"type":"text","text":"b"}]}"#;
        assert_eq!(result_text(r).as_deref(), Some("a\nb"));
        assert_eq!(result_text("not json"), None);
        assert_eq!(result_text(r#"{"content":[]}"#), None);
    }

    #[test]
    fn reported_ids_reads_the_id_keys_only() {
        let t =
            r#"{"answer":"x","id":"n1","links_created":9,"note_ids":["n2","n3"],"task_id":"t9"}"#;
        assert_eq!(reported_ids(t), vec!["n1", "n2", "n3"]);
        assert!(reported_ids("plain prose").is_empty());
    }

    #[test]
    fn clip_counts_chars_not_bytes() {
        assert_eq!(clip("héllo", 3), ("hél".to_string(), true));
        assert_eq!(clip("hi", 3), ("hi".to_string(), false));
    }

    fn j(id: &str, parent: Option<&str>) -> (String, Option<String>) {
        (id.to_string(), parent.map(String::from))
    }

    #[test]
    fn chain_order_follows_parents() {
        // Created out of order: c's parent is b, b's parent is a.
        let jobs = vec![j("a", None), j("c", Some("b")), j("b", Some("a"))];
        assert_eq!(chain_order(&jobs), vec![0, 2, 1]);
    }

    #[test]
    fn chain_order_keeps_a_parent_outside_the_set_as_a_root_and_drops_nothing() {
        let jobs = vec![
            j("x", Some("gone")),
            j("y", Some("x")),
            j("p", Some("q")),
            j("q", Some("p")),
        ];
        let order = chain_order(&jobs);
        assert_eq!(&order[..2], &[0, 1]);
        let mut all = order.clone();
        all.sort();
        assert_eq!(all, vec![0, 1, 2, 3], "a cycle is appended, not lost");
    }

    #[test]
    fn step_tools_reports_what_the_runner_reads() {
        // The runner reads `tool_name`; an old `tool` key runs as "" and fails.
        let s = r#"[{"tool_name":"search_web"},{"tool":"store_note"},{"args":{}}]"#;
        assert_eq!(
            step_tools(s),
            (vec!["search_web".into(), String::new(), String::new()], 1)
        );
        assert_eq!(step_tools("garbage"), (Vec::new(), 0));
    }
}
