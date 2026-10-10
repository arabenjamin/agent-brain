//! Reference links: which Wikidata item / Wikipedia article an entity is
//! (HBI plan Phase 18, `services/wiki.rs`).
//!
//! - `GET  /api/entities/{id}/wiki` — its link, the article's first paragraph,
//!   the candidates the lookup saw, and what the user rejected.
//! - `POST /api/entities/{id}/wiki/lookup` — look it up now, on demand: one
//!   `link_entities` job with the entity-linking schedule's own step (and so
//!   its model pin), allowed for entities no claim mentions.
//! - `POST /api/entities/{id}/wiki/decide` `{qid, decision: confirm|reject}`.
//! - `POST /api/entities/{id}/wiki/url` `{url}` — a Wikipedia or Wikidata URL.
//! - `POST /api/wiki/describe` `{ids}` — one-line descriptions for review lists.
//! - `GET  /api/wiki/coverage?bucket=&limit=` — linked / none / waiting, drillable.
//! - `GET  /api/entities/types` — open type questions and the vocabulary (Phase 19).
//! - `PUT  /api/entities/{id}/type` `{entity_type, kind?}` / `DELETE` — set by hand / unlock.
//! - `POST /api/entities/{id}/type/answer` `{answer: keep|reference}`.

use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::rest::{RestState, bad_request, internal, not_found, unavailable};
use crate::services::queue::ChainStep;
use crate::services::{entity_kinds, wiki};

const LINK_TOOL: &str = "link_entities";

pub async fn handle_entity_wiki(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    match wiki::entity_reference(neo4j, &id).await {
        Ok(Some(v)) => Json(v).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// The step that runs lookups: the entity-linking schedule's, so one place
/// pins the model. Found by its tool, whatever the schedule is called.
async fn link_step(neo4j: &crate::repository::Neo4jClient) -> Option<ChainStep> {
    // Disabled too: the button works while the background schedule is paused.
    let tasks = neo4j.list_scheduled_tasks(false).await.ok()?;
    tasks.iter().find_map(|t| {
        serde_json::from_str::<Vec<ChainStep>>(&t.steps)
            .ok()?
            .into_iter()
            .find(|s| s.tool_name == LINK_TOOL)
    })
}

pub async fn handle_entity_wiki_lookup(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    if wiki::contact().is_none() {
        return bad_request(
            "WIKIMEDIA_CONTACT is not set: Wikimedia asks automated clients for a contact",
        );
    }
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let queue = match handle.read().await.as_ref() {
        Some(s) => s.queue(),
        None => return unavailable("Scheduler not yet initialised"),
    };
    let Some(mut step) = link_step(neo4j).await else {
        return internal(
            "no schedule runs link_entities (schedules/entity-linking.yaml), so there is no model pinned for lookups",
        );
    };
    match wiki::mark_pending(neo4j, &id).await {
        Ok(true) => {}
        Ok(false) => return not_found(),
        Err(e) => return internal(e),
    }
    step.arguments = Some(json!({ "entity_id": id, "on_demand": true }));
    step.description = Some("Look up one entity's reference link (on demand)".into());
    match queue.enqueue_chain(&[step], None).await {
        Ok(ids) => Json(json!({ "job_id": ids.first() })).into_response(),
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

pub async fn handle_entity_wiki_decide(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let (Some(qid), Some(decision)) = (body["qid"].as_str(), body["decision"].as_str()) else {
        return bad_request("`qid` and `decision` (confirm or reject) are required");
    };
    outcome(
        wiki::decide(neo4j, &id, qid, decision).await,
        "Reference not changed",
    )
}

pub async fn handle_entity_wiki_url(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
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

pub async fn handle_wiki_describe(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
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

pub async fn handle_wiki_coverage(
    Extension(state): Extension<Arc<RestState>>,
    Query(q): Query<CoverageQuery>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
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

/// `GET /api/entities/types` — open type questions, and the types and kinds
/// the HBI offers.
pub async fn handle_entity_types(
    Extension(state): Extension<Arc<RestState>>,
    Query(q): Query<TypesQuery>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    match entity_kinds::questions(neo4j, q.limit.unwrap_or(100).clamp(1, 500)).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

/// `PUT /api/entities/{id}/type` `{entity_type, kind?}` — set by hand (final).
/// `DELETE` — let the brain decide again.
pub async fn handle_set_entity_type(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let Some(t) = body["entity_type"].as_str() else {
        return bad_request("`entity_type` is required");
    };
    outcome(
        entity_kinds::set_type(neo4j, &id, t, body["kind"].as_str()).await,
        "Type not set",
    )
}

pub async fn handle_unlock_entity_type(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    outcome(entity_kinds::unlock(neo4j, &id).await, "Type not unlocked")
}

/// `POST /api/entities/{id}/type/answer` `{answer: keep|reference}`.
pub async fn handle_answer_entity_type(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let Some(a) = body["answer"].as_str() else {
        return bad_request("`answer` (keep or reference) is required");
    };
    outcome(
        entity_kinds::answer(neo4j, &id, a).await,
        "Type question not answered",
    )
}
