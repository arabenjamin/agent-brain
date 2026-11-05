//! Read-only operations views: scheduler, jobs, tasks, queue, activity, logs, tools, notifications.

use super::{
    RestState, bad_request, graph, internal, parse_since, parse_statuses, unavailable,
    where_clause, window_hours,
};
use agent_brain_repository::{Neo4jClient, node_ts};
use axum::{
    Extension, Json,
    extract::{Path, Query},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// GET /scheduler-config — return current scheduler config + state.
pub async fn handle_get_scheduler_config(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };
    Json(svc.status().await).into_response()
}

/// PUT /scheduler-config — update scheduler settings.
pub async fn handle_put_scheduler_config(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };

    let local_model = body["local_model"]
        .as_str()
        .map(std::string::ToString::to_string);
    let interval_secs = body["interval_secs"].as_u64();
    let enabled = body["enabled"].as_bool();

    let cfg = svc
        .update_config(
            interval_secs,
            enabled,
            None,
            None,
            None,
            None,
            None,
            local_model,
        )
        .await;

    Json(serde_json::json!({
        "updated": true,
        "config": {
            "interval_secs": cfg.interval_secs,
            "enabled": cfg.enabled,
            "local_model": cfg.local_model,
            "idle_sleep_after_ticks": cfg.idle_sleep_after_ticks,
            "sleep_interval_secs": cfg.sleep_interval_secs,
        }
    }))
    .into_response()
}

/// GET /api/scheduler/status — current scheduler config and runtime state.
pub async fn handle_get_scheduler_status(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };
    Json(svc.status().await).into_response()
}

/// GET /api/jobs?status=<filter>&limit=<n>
///
/// Returns agent jobs from Neo4j. The frontend uses this instead of an MCP
/// tool call so the agent's tool surface stays clean.
pub async fn handle_list_jobs(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let neo4j = graph!(state);

    let statuses = parse_statuses(&params);
    let limit: i64 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
        .min(500);
    // `since` filters on updated_at: a job "in the window" is one whose state
    // changed in it, which is what /api/activity counts.
    let since = match parse_since(&params) {
        Ok(s) => s,
        Err(e) => return bad_request(e),
    };

    let mut conds = Vec::new();
    if statuses.is_some() {
        conds.push("j.status IN $statuses");
    }
    if since.is_some() {
        conds.push("j.updated_at >= datetime($since)");
    }
    let cypher = format!(
        "MATCH (j:AgentJob) {}RETURN j ORDER BY j.created_at DESC LIMIT $limit",
        where_clause(&conds)
    );
    let mut q = neo4rs::query(&cypher).param("limit", limit);
    if let Some(ref s) = statuses {
        q = q.param("statuses", s.clone());
    }
    if let Some(ref s) = since {
        q = q.param("since", s.as_str());
    }

    match neo4j.execute(q).await {
        Ok(rows) => {
            let jobs: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let node: neo4rs::Node = match row.get("j") {
                        Ok(n) => n,
                        Err(_) => return json!(null),
                    };
                    let args_json: String = node.get("args_json").unwrap_or_default();
                    let args: Option<Value> = if args_json.is_empty() {
                        None
                    } else {
                        serde_json::from_str(&args_json).ok()
                    };
                    json!({
                        "id":            node.get::<String>("id").unwrap_or_default(),
                        "tool_name":     node.get::<String>("tool_name").unwrap_or_default(),
                        "status":        node.get::<String>("status").unwrap_or_default(),
                        "priority":      node.get::<i64>("priority").unwrap_or(1),
                        "attempt_count": node.get::<i64>("attempt_count").unwrap_or(0),
                        "max_attempts":  node.get::<i64>("max_attempts").unwrap_or(3),
                        "provider_hint": node.get::<String>("provider_hint").ok().filter(|s| !s.is_empty()),
                        "args":          args,
                        "error":         node.get::<String>("error").ok().filter(|s| !s.is_empty()),
                        "parent_job_id": node.get::<String>("parent_job_id").ok().filter(|s| !s.is_empty()),
                        "created_at":    node_ts(&node, "created_at").unwrap_or_default(),
                        "updated_at":    node_ts(&node, "updated_at").unwrap_or_default(),
                    })
                })
                .filter(|v| !v.is_null())
                .collect();
            Json(json!({ "count": jobs.len(), "jobs": jobs })).into_response()
        }
        Err(e) => internal(format!("Failed to list jobs: {e}")),
    }
}

/// GET /api/tasks?status=<filter>&limit=<n> — list tasks from Neo4j.
pub async fn handle_list_tasks(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let neo4j = graph!(state);

    let limit: i64 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
        .min(500);
    let statuses = parse_statuses(&params);
    let since = match parse_since(&params) {
        Ok(s) => s,
        Err(e) => return bad_request(e),
    };

    let mut conds = Vec::new();
    if statuses.is_some() {
        conds.push("t.status IN $statuses");
    }
    if since.is_some() {
        conds.push("t.created_at >= datetime($since)");
    }
    let cypher = format!(
        "MATCH (t:Task) {}\
         RETURN t.id AS id, t.goal AS goal, t.status AS status, \
                t.context AS context, toString(t.created_at) AS created_at, \
                [(t)-[:SUBTASK_OF]->(p) | p.id][0] AS parent_id \
         ORDER BY t.created_at DESC LIMIT $limit",
        where_clause(&conds)
    );
    let mut q = neo4rs::query(&cypher).param("limit", limit);
    if let Some(ref s) = statuses {
        q = q.param("statuses", s.clone());
    }
    if let Some(ref s) = since {
        q = q.param("since", s.as_str());
    }

    match neo4j.execute(q).await {
        Ok(rows) => {
            let tasks: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "id":         row.get::<String>("id").unwrap_or_default(),
                        "goal":       row.get::<String>("goal").unwrap_or_default(),
                        "status":     row.get::<String>("status").unwrap_or_default(),
                        "context":    row.get::<String>("context").ok().filter(|s| !s.is_empty()),
                        "created_at": row.get::<String>("created_at").unwrap_or_default(),
                        "parent_id":  row.get::<String>("parent_id").ok().filter(|s| !s.is_empty()),
                    })
                })
                .collect();
            Json(json!({ "count": tasks.len(), "tasks": tasks })).into_response()
        }
        Err(e) => internal(format!("Failed to list tasks: {e}")),
    }
}

/// GET /api/queue/status — current queue depth and per-provider utilization.
pub async fn handle_queue_status(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };
    Json(svc.queue_stats().await).into_response()
}

/// POST /api/queue/drain — cancel all queued (in-memory) jobs.
pub async fn handle_queue_drain(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };
    match svc.queue_drain().await {
        Ok(n) => Json(json!({ "drained": n })).into_response(),
        Err(e) => internal(e),
    }
}

/// One `RETURN <key> AS k, count(*) AS c` breakdown over a window: the total
/// and the per-key counts.
async fn count_by(neo4j: &Neo4jClient, cypher: &str, since: &str) -> Result<(i64, Value), String> {
    let rows = neo4j
        .execute(neo4rs::query(cypher).param("since", since))
        .await
        .map_err(|e| e.to_string())?;
    let mut total = 0;
    let mut by = serde_json::Map::new();
    for row in rows {
        let k = row
            .get::<String>("k")
            .unwrap_or_else(|_| "unknown".to_string());
        let c = row.get::<i64>("c").unwrap_or(0);
        total += c;
        by.insert(k, json!(c));
    }
    Ok((total, Value::Object(by)))
}

/// GET /api/activity?hours=24 — what the brain did in a look-back window.
///
/// Every count has rows behind it: `notes` → `/api/notes?since=…&note_type=…`,
/// `tasks` → `/api/tasks?since=…&status=…`, `jobs` (state changed in the
/// window) → `/api/jobs?since=…&status=…`, `claims_checked` →
/// `/api/notes?note_type=claim&verified_since=…`. A section whose query fails is
/// `null` and named in `errors` — never reported as zero.
pub async fn handle_get_activity(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let neo4j = graph!(state);
    let hours = window_hours(&params);
    let since = (chrono::Utc::now() - chrono::Duration::hours(hours)).to_rfc3339();

    let sections: [(&str, &str); 4] = [
        (
            "notes",
            "MATCH (n:Note) WHERE n.created_at >= datetime($since) \
             RETURN coalesce(n.note_type, 'untyped') AS k, count(*) AS c",
        ),
        (
            "tasks",
            "MATCH (t:Task) WHERE t.created_at >= datetime($since) \
             RETURN coalesce(t.status, 'unknown') AS k, count(*) AS c",
        ),
        (
            "jobs",
            "MATCH (j:AgentJob) WHERE j.updated_at >= datetime($since) \
             RETURN coalesce(j.status, 'unknown') AS k, count(*) AS c",
        ),
        (
            "claims_checked",
            "MATCH (n:Note {note_type: 'claim'}) WHERE n.verified_at >= datetime($since) \
             RETURN coalesce(n.claim_status, 'unknown') AS k, count(*) AS c",
        ),
    ];

    let mut out = serde_json::Map::new();
    let mut errors: Vec<String> = Vec::new();
    for (name, cypher) in sections {
        match count_by(neo4j, cypher, &since).await {
            Ok((total, by)) => {
                out.insert(name.to_string(), json!({ "total": total, "by": by }));
            }
            Err(e) => {
                out.insert(name.to_string(), Value::Null);
                errors.push(format!("{name}: {e}"));
            }
        }
    }

    let singles: [(&str, &str); 2] = [
        (
            "media_ingested",
            "MATCH (m:Media) WHERE m.ingested_at >= datetime($since) RETURN 'all' AS k, count(*) AS c",
        ),
        (
            "schedule_runs",
            "MATCH (w:Workflow) WHERE w.last_run_at >= datetime($since) \
             RETURN 'all' AS k, count(*) AS c",
        ),
    ];
    for (name, cypher) in singles {
        match count_by(neo4j, cypher, &since).await {
            Ok((total, _)) => {
                out.insert(name.to_string(), json!(total));
            }
            Err(e) => {
                out.insert(name.to_string(), Value::Null);
                errors.push(format!("{name}: {e}"));
            }
        }
    }

    out.insert("window_hours".to_string(), json!(hours));
    out.insert("since".to_string(), json!(since));
    out.insert("errors".to_string(), json!(errors));
    Json(Value::Object(out)).into_response()
}

/// GET /api/logs?limit=&level=&target=&q=&after=
/// Recent in-process log lines from the ring buffer, newest first.
/// `target` is a comma list of target prefixes to keep, `-prefix` to drop;
/// `quiet` a comma list of target prefixes shown only at WARN or above;
/// `q` is text in the message or target; `after` a timestamp for live tailing.
/// `matched`, `buffered` and `oldest` say how much there is and how far back
/// the buffer reaches.
pub async fn handle_get_logs(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
        .min(2000);

    let Some(ref buf) = state.log_buffer else {
        return Json(json!({ "count": 0, "entries": [], "matched": 0, "buffered": 0 }))
            .into_response();
    };
    let (include, exclude) = params
        .get("target")
        .map(|t| agent_brain_graph::logging::LogFilter::parse_targets(t))
        .unwrap_or_default();
    let filter = agent_brain_graph::logging::LogFilter {
        min_level: params.get("level").cloned(),
        include,
        exclude,
        quiet: params
            .get("quiet")
            .map(|q| {
                q.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        text: params
            .get("q")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        after: params.get("after").cloned().filter(|s| !s.is_empty()),
    };
    let page = buf.query(&filter, limit);
    Json(json!({
        "count": page.entries.len(),
        "entries": page.entries,
        "matched": page.matched,
        "buffered": page.buffered,
        "capacity": page.capacity,
        "oldest": page.oldest,
    }))
    .into_response()
}

/// GET /api/skills — live skill registry: each skill name with its tool list.
pub async fn handle_list_skills(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let Some(ref registry_arc) = state.tool_registry else {
        return Json(json!({ "skills": [] })).into_response();
    };
    let registry = registry_arc.read().await;
    let skills = registry.list_skills();
    Json(json!({ "skills": skills })).into_response()
}

/// GET /api/tools — the COMPLETE registered tool set.
///
/// Unlike MCP `tools/list`, this is NOT filtered by `MCP_TOOL_PROFILE`, so a UI
/// (the Scheduled Tasks step editor) can offer every tool a background step may
/// call — a scheduled step is dispatched through the skill registry regardless
/// of any chat profile. Returns a flat `tools` name list (for membership checks)
/// plus `groups` keyed by skill, each tool as `{name, description}`, and with
/// `?schemas=true` a `schemas` map of tool name → `input_schema`.
pub async fn handle_list_all_tools(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref registry_arc) = state.tool_registry else {
        return unavailable("Tool registry not available");
    };
    let registry = registry_arc.read().await;

    let groups: Vec<Value> = registry
        .skills_with_tools()
        .into_iter()
        .map(|(skill, tools)| {
            json!({
                "group": skill,
                "tools": tools
                    .iter()
                    .map(|t| json!({ "name": t.name, "description": t.description }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    let defs = registry.list();
    let names: Vec<String> = defs.iter().map(|t| t.name.clone()).collect();
    let mut body = json!({ "count": names.len(), "tools": names, "groups": groups });
    // `?schemas=true`: each tool's `input_schema`, so a step drafter (the HBI
    // editor's agent assist) can name real arguments instead of guessing.
    if params.get("schemas").is_some_and(|v| v == "true") {
        body["schemas"] = defs
            .into_iter()
            .map(|t| (t.name, t.input_schema))
            .collect::<serde_json::Map<_, _>>()
            .into();
    }
    Json(body).into_response()
}

/// GET /api/notifications?unread=true
pub async fn handle_list_notifications(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "No database"})),
        )
            .into_response();
    };
    let unread_only = params.get("unread").is_some_and(|v| v == "true");
    match neo4j.list_notifications(unread_only).await {
        Ok(items) => Json(json!({ "notifications": items })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/notifications/{id}/read
pub async fn handle_mark_notification_read(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "No database"})),
        )
            .into_response();
    };
    match neo4j.mark_notification_read(&id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}
