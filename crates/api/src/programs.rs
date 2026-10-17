//! Programs for the HBI (PROGRAMS_PLAN.md): the tree, one program, and its
//! edits. The rules live in `services/programs.rs`, shared with
//! `manage_program`.
//!
//! - `GET /api/programs` — the tree in depth-first rows, each with its issues,
//!   plus the enabled workflows in no program.
//! - `GET /api/programs/{name}` — one program: definition, owner, children,
//!   its workflows' state, open todos, issues.
//! - `POST /api/programs` (create only), `PUT|DELETE /api/programs/{name}`,
//!   `POST /api/programs/{name}/reset` — edits, checked whole before anything
//!   is written; a status change pauses or resumes the program's workflows.

use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::Path,
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
