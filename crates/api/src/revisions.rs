//! Revisions for the HBI (OUTSIDE_REVIEW_PLAN Phase 2). The rules live in
//! `agent_brain_work::revisions`.
//!
//! - `GET /api/revisions/{kind}/{name}` — the history, newest first, and where
//!   the object stands against its file. `kind` is workflow, profile or program.
//! - `GET /api/revisions/{kind}/{name}/{version}` — one revision in full, with
//!   what it changed.
//! - `GET /api/revisions/{kind}/{name}/file` — what the bundle file would say to
//!   match what is live: for the owner to review and commit, never written here.
//! - `POST /api/revisions/{kind}/{name}/revert` `{version, reason, based_on}` —
//!   a new revision copying an old definition, through the kind's own save.

use std::sync::Arc;

use axum::{
    Json,
    extract::{Extension, Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Deserialize;
use serde_json::{Value, json};

use super::rest::{RestState, bad_request, graph, internal, not_found, refused, unavailable};
use agent_brain_repository::{Neo4jClient, RevisionKind, canonical, content_hash};
use agent_brain_work::{programs, revisions, workflows};

#[derive(Deserialize)]
pub struct Limit {
    limit: Option<i64>,
}

#[derive(Deserialize)]
pub struct Revert {
    version: i64,
    reason: Option<String>,
    based_on: Option<i64>,
}

fn kind_of(s: &str) -> Result<RevisionKind, String> {
    RevisionKind::parse(s)
        .ok_or_else(|| format!("kind must be workflow, profile or program, not '{s}'"))
}

/// What is live, as its revisions record it; `None` = no such object.
async fn live(
    state: &RestState,
    neo4j: &Neo4jClient,
    kind: RevisionKind,
    name: &str,
) -> anyhow::Result<Option<Value>> {
    Ok(match kind {
        RevisionKind::Workflow => neo4j
            .get_workflow_by_name(name)
            .await?
            .map(|w| workflows::live_definition(&w)),
        RevisionKind::Program => programs::load_all(neo4j)
            .await?
            .iter()
            .find(|s| s.def.name == name)
            .map(|s| programs::definition_of(&s.def)),
        RevisionKind::Profile => match &state.context_builder {
            Some(handle) => match handle.read().await.as_ref() {
                Some(cb) => cb
                    .get_profile(name)
                    .await
                    .map(|p| agent_brain_work::profiles::definition_of(&p)),
                None => None,
            },
            None => None,
        },
    })
}

pub async fn handle_history(
    Extension(state): Extension<Arc<RestState>>,
    Path((kind, name)): Path<(String, String)>,
    Query(q): Query<Limit>,
) -> Response {
    let neo4j = graph!(state);
    let kind = match kind_of(&kind) {
        Ok(k) => k,
        Err(e) => return bad_request(e),
    };
    match revisions::history(neo4j, kind, &name, q.limit).await {
        Ok(h) => {
            if h["exists"] == json!(false) && h["revisions"].as_array().is_some_and(Vec::is_empty) {
                return not_found();
            }
            Json(h).into_response()
        }
        Err(e) => internal(e),
    }
}

pub async fn handle_get(
    Extension(state): Extension<Arc<RestState>>,
    Path((kind, name, version)): Path<(String, String, i64)>,
) -> Response {
    let neo4j = graph!(state);
    let kind = match kind_of(&kind) {
        Ok(k) => k,
        Err(e) => return bad_request(e),
    };
    match revisions::revision_detail(neo4j, kind, &name, version).await {
        Ok(Some(r)) => Json(r).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// What the file says about `enabled`: a file sets it only for a fresh install,
/// so a render keeps the file's word. `None` when the file does not say.
fn file_enabled(source: &str) -> Option<bool> {
    let plugins = agent_brain_graph::bundles::Plugins::global();
    let path = match source.strip_prefix("plugins/") {
        Some(rest) => plugins.dir.join(rest),
        None => std::path::PathBuf::from(source),
    };
    let text = std::fs::read_to_string(path).ok()?;
    serde_yaml::from_str::<serde_yaml::Value>(&text)
        .ok()?
        .get("enabled")?
        .as_bool()
}

pub async fn handle_file(
    Extension(state): Extension<Arc<RestState>>,
    Path((kind_name, name)): Path<(String, String)>,
) -> Response {
    let neo4j = graph!(state);
    let kind = match kind_of(&kind_name) {
        Ok(k) => k,
        Err(e) => return bad_request(e),
    };
    let definition = match live(&state, neo4j, kind, &name).await {
        Ok(Some(d)) => d,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    let history = match revisions::history(neo4j, kind, &name, Some(1)).await {
        Ok(h) => h,
        Err(e) => return internal(e),
    };
    let source = history["seed_source"].as_str().map(String::from);
    let enabled = match (&source, kind) {
        (Some(s), RevisionKind::Workflow) => file_enabled(s),
        (None, RevisionKind::Workflow) => neo4j
            .get_workflow_by_name(&name)
            .await
            .ok()
            .flatten()
            .map(|w| w.enabled),
        _ => None,
    };
    let yaml = match revisions::file_text(kind, &definition, enabled) {
        Ok(y) => y,
        Err(e) => return internal(e),
    };
    Json(json!({
        "kind": kind.as_str(),
        "name": name,
        "version": history["version"],
        "new_file": source.is_none(),
        "path": source.unwrap_or_else(|| revisions::suggested_path(kind, &name)),
        "matches_file": history["drift"].is_null(),
        "live_hash": content_hash(&canonical(&definition)),
        "yaml": yaml,
    }))
    .into_response()
}

pub async fn handle_revert(
    Extension(state): Extension<Arc<RestState>>,
    Path((kind, name)): Path<(String, String)>,
    Json(b): Json<Revert>,
) -> Response {
    let neo4j = graph!(state);
    let kind = match kind_of(&kind) {
        Ok(k) => k,
        Err(e) => return bad_request(e),
    };
    let current = match live(&state, neo4j, kind, &name).await {
        Ok(c) => c,
        Err(e) => return internal(e),
    };
    let body = match revisions::revert_body(
        neo4j,
        kind,
        &name,
        b.version,
        b.reason.as_deref(),
        b.based_on,
        current.as_ref(),
    )
    .await
    {
        Ok(body) => body,
        Err(e) => return bad_request(e),
    };
    match kind {
        RevisionKind::Workflow => {
            match super::rest::save_workflow(&state, neo4j, None, false, &body).await {
                Ok(s) => (
                    StatusCode::OK,
                    Json(json!({"name": name, "version": s.workflow.version})),
                )
                    .into_response(),
                Err(e) => super::rest::save_error(e),
            }
        }
        RevisionKind::Program => match programs::save(neo4j, &body, false).await {
            Ok(s) => Json(json!({"name": s.program.name, "warnings": s.warnings})).into_response(),
            Err(programs::SaveError::Refused(r)) => refused(&r),
            Err(programs::SaveError::Storage(e)) => internal(e),
            Err(e) => bad_request(e),
        },
        RevisionKind::Profile => {
            let Some(handle) = state.context_builder.as_ref() else {
                return unavailable("Context builder not available");
            };
            let guard = handle.read().await;
            let Some(cb) = guard.as_ref() else {
                return unavailable("Context builder not yet initialised");
            };
            match cb.save_profile(&body).await {
                Ok((p, warnings)) => {
                    Json(json!({"name": p.name, "warnings": warnings})).into_response()
                }
                Err(e) => match e.downcast_ref::<revisions::Refused>() {
                    Some(r) => refused(r),
                    None => bad_request(format!("Profile not saved: {e}")),
                },
            }
        }
    }
}
