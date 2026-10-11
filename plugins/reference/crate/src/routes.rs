//! The reference plugin's HTTP routes, served by the host under
//! `/api/plugins/reference/` (paths below are relative to it). Reference
//! links: which Wikidata item / Wikipedia article an entity is (HBI plan
//! Phase 18, `wiki.rs`); entity types and kinds (Phase 19, `kinds.rs`).
//!
//! - `GET  /entities/{id}/wiki` — its link, the article's first paragraph,
//!   the candidates the lookup saw, and what the user rejected.
//! - `POST /entities/{id}/wiki/lookup` — look it up now, on demand: one
//!   `link_entities` job with the entity-linking schedule's own step (and so
//!   its model pin), allowed for entities no claim mentions.
//! - `POST /entities/{id}/wiki/decide` `{qid, decision: confirm|reject}`.
//! - `POST /entities/{id}/wiki/url` `{url}` — a Wikipedia or Wikidata URL.
//! - `POST /wiki/describe` `{ids}` — one-line descriptions for review lists.
//! - `GET  /wiki/coverage?bucket=&limit=` — linked / none / waiting, drillable.
//! - `GET  /entities/types` — open type questions and the vocabulary (Phase 19).
//! - `PUT  /entities/{id}/type` `{entity_type, kind?}` / `DELETE` — set by hand / unlock.
//! - `POST /entities/{id}/type/answer` `{answer: keep|reference}`.

use std::sync::Arc;

use agent_brain_plugin_api::Workflows;
use agent_brain_plugin_api::axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde_json::{Value, json};

use crate::db::Db;
use crate::kinds as entity_kinds;
use crate::wiki;

/// What the handlers share.
pub struct RouteState {
    pub db: Db,
    pub workflows: Option<Arc<dyn Workflows>>,
}

type St = State<Arc<RouteState>>;

pub fn router(state: Arc<RouteState>) -> Router {
    Router::new()
        .route("/entities/{id}/wiki", get(handle_entity_wiki))
        .route(
            "/entities/{id}/wiki/lookup",
            post(handle_entity_wiki_lookup),
        )
        .route(
            "/entities/{id}/wiki/decide",
            post(handle_entity_wiki_decide),
        )
        .route("/entities/{id}/wiki/url", post(handle_entity_wiki_url))
        .route("/entities/types", get(handle_entity_types))
        .route(
            "/entities/{id}/type",
            put(handle_set_entity_type).delete(handle_unlock_entity_type),
        )
        .route(
            "/entities/{id}/type/answer",
            post(handle_answer_entity_type),
        )
        .route("/wiki/describe", post(handle_wiki_describe))
        .route("/wiki/coverage", get(handle_wiki_coverage))
        .with_state(state)
}

fn error(status: StatusCode, msg: impl std::fmt::Display) -> Response {
    (status, Json(json!({ "error": msg.to_string() }))).into_response()
}
fn internal(msg: impl std::fmt::Display) -> Response {
    error(StatusCode::INTERNAL_SERVER_ERROR, msg)
}
fn bad_request(msg: impl std::fmt::Display) -> Response {
    error(StatusCode::BAD_REQUEST, msg)
}
fn unavailable(msg: &str) -> Response {
    error(StatusCode::SERVICE_UNAVAILABLE, msg)
}
fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "Not found")
}

const LINK_TOOL: &str = "link_entities";

async fn handle_entity_wiki(State(st): St, Path(id): Path<String>) -> Response {
    let neo4j = &st.db;
    match wiki::entity_reference(neo4j, &id).await {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

async fn handle_entity_wiki_lookup(State(st): St, Path(id): Path<String>) -> Response {
    let neo4j = &st.db;
    if wiki::contact().is_none() {
        return bad_request(
            "WIKIMEDIA_CONTACT is not set: Wikimedia asks automated clients for a contact",
        );
    }
    let Some(workflows) = st.workflows.as_ref() else {
        return unavailable("Workflows not available");
    };
    match wiki::mark_pending(neo4j, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found(),
        Err(e) => return internal(e),
    }
    // The entity-linking workflow's own step, so one place pins the model.
    match workflows
        .enqueue_tool_step(
            LINK_TOOL,
            json!({ "entity_id": id, "on_demand": true }),
            "Look up one entity's reference link (on demand)",
        )
        .await
    {
        Ok(job_id) => Json(json!({ "job_id": job_id })).into_response(),
        Err(e) => internal(e),
    }
}

fn outcome<T: serde::Serialize>(r: anyhow::Result<Result<T, String>>, what: &str) -> Response {
    match r {
        Ok(Ok(v)) => Json(json!({ "ok": true, "result": v })).into_response(),
        Ok(Err(e)) => bad_request(format!("{what}: {e}")),
        Err(e) => internal(e),
    }
}

async fn handle_entity_wiki_decide(
    State(st): St,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let neo4j = &st.db;
    let (Some(qid), Some(decision)) = (body["qid"].as_str(), body["decision"].as_str()) else {
        return bad_request("`qid` and `decision` (confirm or reject) are required");
    };
    outcome(
        wiki::decide(neo4j, &id, qid, decision).await,
        "Reference not changed",
    )
}

async fn handle_entity_wiki_url(
    State(st): St,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let neo4j = &st.db;
    let Some(url) = body["url"].as_str().filter(|u| !u.trim().is_empty()) else {
        return bad_request("`url` is required");
    };
    if wiki::contact().is_none() {
        return bad_request(
            "WIKIMEDIA_CONTACT is not set: Wikimedia asks automated clients for a contact",
        );
    }
    outcome(wiki::set_url(neo4j, &id, url).await, "Reference not set")
}

async fn handle_wiki_describe(State(st): St, Json(body): Json<Value>) -> Response {
    let neo4j = &st.db;
    let ids: Vec<String> = body["ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().map(String::from))
        .take(500)
        .collect();
    match wiki::describe(neo4j, &ids).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(serde::Deserialize)]
pub struct CoverageQuery {
    bucket: Option<String>,
    limit: Option<i64>,
}

async fn handle_wiki_coverage(State(st): St, Query(q): Query<CoverageQuery>) -> Response {
    let neo4j = &st.db;
    match wiki::coverage(
        neo4j,
        q.bucket.as_deref(),
        q.limit.unwrap_or(100).clamp(1, 500),
    )
    .await
    {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => bad_request(e),
        Err(e) => internal(e),
    }
}

// ─── Entity types and kinds (HBI plan Phase 19, `services/entity_kinds.rs`) ──

#[derive(serde::Deserialize)]
pub struct TypesQuery {
    limit: Option<i64>,
}

/// `GET /entities/types` — open type questions, and the types and kinds
/// the HBI offers.
async fn handle_entity_types(State(st): St, Query(q): Query<TypesQuery>) -> Response {
    let neo4j = &st.db;
    match entity_kinds::questions(neo4j, q.limit.unwrap_or(100).clamp(1, 500)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

/// `PUT /entities/{id}/type` `{entity_type, kind?}` — set by hand (final).
/// `DELETE` — let the brain decide again.
async fn handle_set_entity_type(
    State(st): St,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let neo4j = &st.db;
    let Some(t) = body["entity_type"].as_str() else {
        return bad_request("`entity_type` is required");
    };
    outcome(
        entity_kinds::set_type(neo4j, &id, t, body["kind"].as_str()).await,
        "Type not set",
    )
}

async fn handle_unlock_entity_type(State(st): St, Path(id): Path<String>) -> Response {
    let neo4j = &st.db;
    outcome(entity_kinds::unlock(neo4j, &id).await, "Type not unlocked")
}

/// `POST /entities/{id}/type/answer` `{answer: keep|reference}`.
async fn handle_answer_entity_type(
    State(st): St,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let neo4j = &st.db;
    let Some(a) = body["answer"].as_str() else {
        return bad_request("`answer` (keep or reference) is required");
    };
    outcome(
        entity_kinds::answer(neo4j, &id, a).await,
        "Type question not answered",
    )
}
