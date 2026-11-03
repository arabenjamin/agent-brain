//! `/api/workflows`: the HBI editor. The chat agent uses `manage_workflow`.

use super::{RestState, bad_request, graph, internal, not_found};
use axum::{
    Extension, Json,
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// GET /workflows[?enabled_only=true]
pub async fn handle_list_workflows(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let neo4j = graph!(state, "Workflow storage not available");
    let enabled_only = params.get("enabled_only").is_some_and(|v| v == "true");
    match neo4j.list_workflows(enabled_only).await {
        Ok(w) => Json(serde_json::json!({ "workflows": w })).into_response(),
        Err(e) => internal(e),
    }
}

/// The same hard checks `manage_workflow` applies: each step's tool exists
/// and its arguments fit the tool's own `input_schema`, pinned models are in
/// the catalog, references resolve, and topic and kind are valid.
pub(crate) async fn save_workflow(
    state: &RestState,
    neo4j: &agent_brain_repository::Neo4jClient,
    id: Option<&str>,
    create_only: bool,
    body: &Value,
) -> Result<agent_brain_work::workflows::Saved, agent_brain_work::workflows::SaveError> {
    let (live, schemas, catalog) = check_inputs(state).await;
    // The same `workflow.json` envelope manage_workflow checks against; the
    // HBI save skipped it until 2026-10-16.
    let envelope = agent_brain_work::validation::envelope_schema("workflow");
    let checks = agent_brain_work::workflows::SaveChecks {
        live_tools: &live,
        tool_schemas: &schemas,
        catalog: &catalog,
        envelope: envelope.as_ref(),
    };
    agent_brain_work::workflows::save(neo4j, id, create_only, body, &checks).await
}

/// What a workflow save is checked against: the registered tools, their
/// input schemas, and the model catalog.
async fn check_inputs(
    state: &RestState,
) -> (
    Vec<String>,
    HashMap<String, Value>,
    Vec<agent_brain_llm::model_router::CatalogModel>,
) {
    let (live, schemas) = match state.tool_registry.as_ref() {
        Some(reg) => {
            let defs = reg.read().await.list();
            let live: Vec<String> = defs.iter().map(|d| d.name.clone()).collect();
            let schemas: HashMap<String, Value> =
                defs.into_iter().map(|d| (d.name, d.input_schema)).collect();
            (live, schemas)
        }
        None => (Vec::new(), HashMap::new()),
    };
    let catalog = state
        .telemetry
        .as_ref()
        .map(agent_brain_llm::model_router::catalog_models)
        .unwrap_or_default();
    (live, schemas, catalog)
}

pub(crate) fn save_error(e: agent_brain_work::workflows::SaveError) -> axum::response::Response {
    use agent_brain_work::workflows::SaveError;
    match e {
        SaveError::Invalid(_) => bad_request(e.to_string()),
        SaveError::NotFound(_) => not_found(),
        SaveError::Storage(m) => internal(m),
        SaveError::Refused(r) => refused(&r),
    }
}

/// A save the revision rules refused: 409 when the object moved since it was
/// read, 400 when the request lacks a reason or the version it read.
pub(crate) fn refused(r: &agent_brain_work::revisions::Refused) -> Response {
    let status = if r.is_conflict() {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    (
        status,
        Json(serde_json::json!({"error": r.to_string(), "current_version": r.current()})),
    )
        .into_response()
}

/// POST /workflows — create; a name that exists is refused (PUT edits).
pub async fn handle_create_workflow(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let neo4j = graph!(state, "Workflow storage not available");
    match save_workflow(&state, neo4j, None, true, &body).await {
        Ok(saved) => {
            // Drafted from an item ("make it a workflow"): link it back.
            if let Some(item) = body
                .get("from_item")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty())
                && let Err(e) =
                    agent_brain_scheduler::items::link_became(neo4j, item, &saved.workflow.id).await
            {
                return internal(format!(
                    "created, but not linked to the item it came from: {e}"
                ));
            }
            let status = if saved.created {
                StatusCode::CREATED
            } else {
                StatusCode::OK
            };
            let mut v = serde_json::json!(saved.workflow);
            if !saved.step_warnings.is_empty() {
                v["step_warnings"] = serde_json::json!(saved.step_warnings);
            }
            (status, Json(v)).into_response()
        }
        Err(e) => save_error(e),
    }
}

/// POST /workflows/check — every check a save runs, nothing written. The body
/// is a save body; with `id` it is checked as an edit of that workflow, without
/// as a new one. 200 `{ok, warnings, step_warnings}` (step lint: saves, but
/// runs badly), or 400 with the error a save would give.
pub async fn handle_check_workflow(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let neo4j = graph!(state, "Workflow storage not available");
    let id = body
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let (live, schemas, catalog) = check_inputs(&state).await;
    let envelope = agent_brain_work::validation::envelope_schema("workflow");
    let checks = agent_brain_work::workflows::SaveChecks {
        live_tools: &live,
        tool_schemas: &schemas,
        catalog: &catalog,
        envelope: envelope.as_ref(),
    };
    match agent_brain_work::workflows::check(neo4j, id, id.is_none(), &body, &checks).await {
        Ok(c) => Json(json!({
            "ok": true,
            "warnings": c.warnings,
            "step_warnings": c.step_warnings,
        }))
        .into_response(),
        Err(e) => save_error(e),
    }
}

/// GET /workflows/:id
pub async fn handle_get_workflow(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let neo4j = graph!(state, "Workflow storage not available");
    match neo4j.get_workflow(&id).await {
        Ok(Some(wf)) => Json(serde_json::json!(wf)).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// PUT /workflows/:id — keys the body leaves out keep their values.
pub async fn handle_update_workflow(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let neo4j = graph!(state, "Workflow storage not available");
    match save_workflow(&state, neo4j, Some(&id), false, &body).await {
        Ok(saved) => {
            // Turned on: an item it was drafted from is done ("became a workflow").
            let closed = if body.get("enabled").and_then(Value::as_bool) == Some(true) {
                agent_brain_scheduler::items::close_items_that_became(neo4j, &saved.workflow.id)
                    .await
            } else {
                Vec::new()
            };
            let mut v = serde_json::json!(saved.workflow);
            if !closed.is_empty() {
                v["closed_items"] = serde_json::json!(closed);
            }
            if !saved.warnings.is_empty() {
                v["warnings"] = serde_json::json!(saved.warnings);
            }
            if !saved.step_warnings.is_empty() {
                v["step_warnings"] = serde_json::json!(saved.step_warnings);
            }
            Json(v).into_response()
        }
        Err(e) => save_error(e),
    }
}

/// DELETE /workflows/:id
pub async fn handle_delete_workflow(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let neo4j = graph!(state, "Workflow storage not available");
    let wf = match neo4j.get_workflow(&id).await {
        Ok(Some(wf)) => wf,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    match agent_brain_work::workflows::delete(neo4j, &wf, None).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => save_error(e),
    }
}
