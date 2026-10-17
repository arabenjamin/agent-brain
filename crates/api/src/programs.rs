//! Programs for the HBI (PROGRAMS_PLAN.md): the tree, one program, and its
//! edits. The rules live in `services/programs.rs`, shared with
//! `manage_program`.
//!
//! - `GET /api/programs` — the tree in depth-first rows, each with its issues,
//!   plus the enabled workflows in no program.
//! - `GET /api/programs/{name}` — one program: definition, owner, children,
//!   its workflows' state, open todos, issues.
//! - `GET /api/programs/scorecard?days=` — the `program_scorecard` view's rows
//!   (the workflow scorecard rolled up each program's subtree, with cost).
//! - `GET /api/programs/agents?program=&days=` — the `agent_scorecard` view.
//! - `GET /api/programs/schema` — `schemas/program.json`, what an agent drafting
//!   a program is given.
//! - `POST /api/programs/check` `{program, create}` — every check a save runs,
//!   nothing written: `{ok, warnings}` or 400 with the refusal.
//! - `POST /api/programs` (create only), `PUT|DELETE /api/programs/{name}`,
//!   `POST /api/programs/{name}/reset` — edits, checked whole before anything
//!   is written; a status change pauses or resumes the program's workflows.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::rest::{RestState, bad_request, graph, internal, not_found};
use agent_brain_work::programs::{self, SaveError};

fn saved_response(r: Result<programs::Saved, SaveError>) -> Response {
    match r {
        Ok(s) => Json(json!(s)).into_response(),
        Err(SaveError::Invalid(e)) => bad_request(format!("Program not saved: {e}")),
        Err(SaveError::NotFound(e)) => bad_request(e),
        Err(SaveError::Storage(e)) => internal(e),
    }
}

pub async fn handle_list(Extension(state): Extension<Arc<RestState>>) -> Response {
    let neo4j = graph!(state);
    match programs::tree(neo4j).await {
        Ok(t) => Json(t).into_response(),
        Err(e) => internal(e),
    }
}

/// Run a lookup view through the tool registry, so the HBI shows what the
/// brain reads.
async fn run_view(state: &RestState, args: Value) -> Response {
    let Some(registry) = state.tool_registry.as_ref() else {
        return internal("the tool registry is not available");
    };
    let result = registry.read().await.call("lookup", Some(args)).await;
    let Some(r) = result else {
        return internal("the lookup tool is not registered");
    };
    // Read through its JSON form: the api crate does not link the protocol.
    let text = serde_json::to_value(&r)
        .ok()
        .and_then(|v| v["content"][0]["text"].as_str().map(String::from))
        .unwrap_or_default();
    if r.is_error == Some(true) {
        return internal(text);
    }
    match serde_json::from_str::<Value>(&text) {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(format!("lookup: {e}")),
    }
}

fn days_of(params: &HashMap<String, String>) -> i64 {
    params
        .get("days")
        .and_then(|d| d.parse::<i64>().ok())
        .unwrap_or(30)
        .clamp(1, 365)
}

/// `GET /api/programs/scorecard?days=30` — the `program_scorecard` lookup
/// view, so the HBI and the brain read the same numbers.
pub async fn handle_scorecard(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let args = json!({"view": "program_scorecard", "days": days_of(&params)});
    run_view(&state, args).await
}

/// `GET /api/programs/agents?program=&days=30` — the `agent_scorecard` view:
/// the agents a program's work uses (all agents without `program`).
pub async fn handle_agents(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let mut args = json!({"view": "agent_scorecard", "days": days_of(&params)});
    if let Some(p) = params.get("program").filter(|p| !p.is_empty()) {
        args["program"] = json!(p);
    }
    run_view(&state, args).await
}

pub async fn handle_schema() -> Response {
    match agent_brain_work::validation::envelope_schema("program") {
        Some(s) => Json(s).into_response(),
        None => not_found(),
    }
}

/// `POST /api/programs/check` `{program: {...}, create: true}`: a dry run of
/// the save, so an agent's draft is checked before the form shows it ready.
pub async fn handle_check(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> Response {
    let neo4j = graph!(state);
    let create = body["create"].as_bool().unwrap_or(false);
    match programs::check_save(neo4j, &body["program"], create).await {
        Ok(warnings) => Json(json!({"ok": true, "warnings": warnings})).into_response(),
        Err(SaveError::Invalid(e)) | Err(SaveError::NotFound(e)) => bad_request(e),
        Err(SaveError::Storage(e)) => internal(e),
    }
}

pub async fn handle_get(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
) -> Response {
    let neo4j = graph!(state);
    match programs::detail(neo4j, &name).await {
        Ok(Some(p)) => Json(p).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// `POST /api/programs` — create; an existing name is refused.
pub async fn handle_create(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> Response {
    let neo4j = graph!(state);
    saved_response(programs::save(neo4j, &body, true).await)
}

/// `PUT /api/programs/{name}` — edit; keys left out keep their values, a key
/// set to null goes back to its default. The name does not change.
pub async fn handle_update(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
    Json(mut body): Json<Value>,
) -> Response {
    let neo4j = graph!(state);
    if body
        .get("name")
        .and_then(Value::as_str)
        .is_some_and(|n| n != name)
    {
        return bad_request(
            "A program's name can't change: create the new program, move its workflows, then delete this one",
        );
    }
    if !body.is_object() {
        return bad_request("the body is a JSON object of program fields");
    }
    body["name"] = json!(name);
    match programs::exists(neo4j, &name).await {
        Ok(true) => saved_response(programs::save(neo4j, &body, false).await),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

pub async fn handle_delete(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
) -> Response {
    let neo4j = graph!(state);
    match programs::delete(neo4j, &name).await {
        Ok(changes) => Json(json!({"deleted": name, "changes": changes})).into_response(),
        Err(SaveError::NotFound(_)) => not_found(),
        Err(SaveError::Invalid(e)) => bad_request(format!("Program not deleted: {e}")),
        Err(SaveError::Storage(e)) => internal(e),
    }
}

/// `POST /api/programs/{name}/reset` — back to its programs/*.yaml file.
pub async fn handle_reset(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
) -> Response {
    let neo4j = graph!(state);
    let plugins = agent_brain_graph::bundles::Plugins::global();
    saved_response(programs::reset(neo4j, &plugins, &name).await)
}
