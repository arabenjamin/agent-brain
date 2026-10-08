//! Entity resolution for the HBI: merge proposals, a person's answers to
//! them, and undo. The logic lives in `services/entity_resolution.rs`; these
//! are the HTTP shapes.
//!
//! - `GET  /api/entities/candidates?entity=&limit=` — pairs that may be one thing
//! - `POST /api/entities/merge {from_id, into_id}` — fold one into the other
//! - `POST /api/entities/decide {a_id, b_id, verdict}` — `different` | `related`
//! - `POST /api/entities/aliases/{name}/unmerge` — undo a merge

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query},
    response::IntoResponse,
};
use serde::Deserialize;
use serde_json::json;

use super::rest::{RestState, bad_request, internal, unavailable};
use crate::services::entity_resolution as er;

const DEFAULT_LIMIT: usize = 25;
const MAX_LIMIT: usize = 200;

pub async fn handle_candidates(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let limit = params
        .get("limit")
        .and_then(|l| l.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    let entity = params
        .get("entity")
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    match er::candidates(neo4j, entity, limit).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => internal(e),
    }
}

#[derive(Deserialize)]
pub struct MergeBody {
    from_id: String,
    into_id: String,
}

pub async fn handle_merge(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<MergeBody>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    if body.from_id == body.into_id {
        return bad_request("from_id and into_id are the same entity");
    }
    let names = match neo4j
        .execute(
            neo4rs::query(
                "MATCH (f:Entity {id: $f}), (t:Entity {id: $t}) RETURN f.name AS f, t.name AS t",
            )
            .param("f", body.from_id.as_str())
            .param("t", body.into_id.as_str()),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let Some(row) = names.first() else {
        return bad_request(format!(
            "no entity with id '{}' or '{}'",
            body.from_id, body.into_id
        ));
    };
    let from: String = row.get("f").unwrap_or_default();
    let into: String = row.get("t").unwrap_or_default();
    match er::merge(neo4j, er::Rules::global(), &from, &into, "proposal", None).await {
        Ok(o) => Json(json!({ "merged": o, "into_id": body.into_id })).into_response(),
        Err(e) => internal(format!("{e:#}")),
    }
}

#[derive(Deserialize)]
pub struct DecideBody {
    a_id: String,
    b_id: String,
    verdict: String,
}

pub async fn handle_decide(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<DecideBody>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    match er::decide(neo4j, &body.a_id, &body.b_id, &body.verdict).await {
        Ok(()) => Json(json!({ "ok": true, "verdict": body.verdict })).into_response(),
        Err(e) => bad_request(format!("{e:#}")),
    }
}

pub async fn handle_unmerge(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    match er::unmerge(neo4j, &name).await {
        Ok(o) => {
            let seeded = er::Rules::global()
                .canonicals
                .iter()
                .any(|c| c.aliases.contains(&o.name));
            Json(json!({
                "unmerged": o,
                // A curated alias is not re-applied to this name (the restored
                // entity is distinct), but the YAML still lists it.
                "still_in_seed": seeded,
            }))
            .into_response()
        }
        Err(e) => bad_request(format!("{e:#}")),
    }
}
