//! The epistemics plugin's HTTP routes, served by the host under
//! `/api/plugins/epistemics/` (paths below are relative to it): typed
//! relationships between entities (HBI plan Phase 17, `relations.rs`), moved
//! from the core's `clients/related.rs` in Phase 4 pack 6.
//!
//! - `GET    /relations/types` — the vocabulary.
//! - `GET    /entities/{id}/relations` — its relationships, both directions,
//!   read from its side, with each one's evidence; rejected ones are counted.
//! - `GET    /relations/between?a=&b=` — between two entities, rejected
//!   included (the core's pair view shows them beside the shared notes).
//! - `GET    /relations/among?ids=a,b,…` — active and proposed among a set
//!   (the core's neighbourhood map draws them).
//! - `POST   /relations` `{from, to, type, note_id?}` — the user sets one.
//! - `POST   /relations/decide` `{from, to, type, accept}` — a proposal.
//! - `POST   /relations/relabel` `{from, to, type, new_type}`.
//! - `DELETE /relations?from=&to=&type=` — the user's own is deleted, an
//!   extracted one kept as rejected.
//! - `GET    /relations/proposed?limit=` — proposals to review, newest first.
//!
//! Reads fold the merge list's "related" answers in first (`fold_related_entity`),
//! so one given a moment ago shows up without waiting for a restart.

use std::collections::HashMap;
use std::sync::Arc;

use agent_brain_plugin_api::axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};

use crate::db::{Db, query};
use crate::relations::{self as rel, Vocabulary};

/// What the handlers share.
pub struct RouteState {
    pub db: Db,
}

type St = State<Arc<RouteState>>;

pub fn router(state: Arc<RouteState>) -> Router {
    Router::new()
        .route("/relations/types", get(handle_relation_types))
        .route("/entities/{id}/relations", get(handle_entity_relations))
        .route("/relations/between", get(handle_between))
        .route("/relations/among", get(handle_among))
        .route(
            "/relations",
            post(handle_set_relation).delete(handle_remove_relation),
        )
        .route("/relations/decide", post(handle_decide_relation))
        .route("/relations/relabel", post(handle_relabel_relation))
        .route("/relations/proposed", get(handle_proposed_relations))
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
fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

fn limit(params: &HashMap<String, String>, default: i64) -> i64 {
    params
        .get("limit")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(default)
        .clamp(1, 50)
}

async fn fold(db: &Db) {
    if let Err(e) = rel::fold_related_entity(db).await {
        tracing::warn!(error = %e, "relations: fold of RELATED_ENTITY answers failed");
    }
}

async fn handle_relation_types() -> Response {
    Json(json!({ "types": Vocabulary::global().types })).into_response()
}

/// A relationship row read from `side`'s point of view.
fn relation_json(m: &Value, side: &str) -> Value {
    let s = |k: &str| m.get(k).and_then(Value::as_str).map(String::from);
    let (from, to) = (s("from").unwrap_or_default(), s("to").unwrap_or_default());
    let t = s("type").unwrap_or_default();
    let vt = Vocabulary::global().get(&t);
    let outgoing = from == side;
    let reads = vt
        .map(|v| {
            if outgoing {
                v.label.clone()
            } else {
                v.inverse.clone()
            }
        })
        .unwrap_or_else(|| t.clone());
    let other = if outgoing { "to" } else { "from" };
    json!({
        "from": from, "to": to, "type": t, "reads": reads,
        "other": {
            "id": s(other),
            "name": s(&format!("{other}_name")),
            "entity_type": s(&format!("{other}_type")),
        },
        "method": s("method"), "status": s("status"),
        "evidence": m.get("evidence").cloned().unwrap_or_else(|| json!([])),
        "model": s("model"),
        "created_at": s("created_at"), "decided_at": s("decided_at"),
    })
}

const REL_ROW: &str = "{from: a.id, to: b.id, from_name: a.name, to_name: b.name, \
     from_type: a.entity_type, to_type: b.entity_type, type: r.type, method: r.method, \
     status: r.status, evidence: coalesce(r.evidence, []), model: r.model, \
     created_at: toString(r.created_at), decided_at: toString(r.decided_at)}";

async fn handle_entity_relations(State(st): St, Path(id): Path<String>) -> Response {
    fold(&st.db).await;
    let rows = match st
        .db
        .execute(
            query(&format!(
                "MATCH (x:Entity {{id: $id}}) \
                 OPTIONAL MATCH (a:Entity)-[r:RELATES]->(b:Entity) WHERE a = x OR b = x \
                 WITH x, collect(CASE WHEN r IS NULL THEN NULL ELSE {REL_ROW} END) AS rels \
                 RETURN [v IN rels WHERE v.status <> 'rejected'] AS rels, \
                        size([v IN rels WHERE v.status = 'rejected']) AS rejected"
            ))
            .param("id", id.as_str()),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let Some(r) = rows.first() else {
        return not_found();
    };
    let mut rels: Vec<Value> = r
        .get::<Vec<Value>>("rels")
        .unwrap_or_default()
        .iter()
        .map(|m| relation_json(m, &id))
        .collect();
    // Yours and accepted first, then proposals; by type within each.
    rels.sort_by_key(|v| {
        (
            v["status"] != "active",
            v["type"].as_str().unwrap_or("").to_string(),
        )
    });
    Json(json!({
        "entity": id,
        "relations": rels,
        "rejected": r.get::<i64>("rejected").unwrap_or(0),
    }))
    .into_response()
}

/// Relationships between two entities (either direction, rejected included,
/// so the pair view can say what was turned down), read from `a`'s side.
async fn handle_between(State(st): St, Query(params): Query<HashMap<String, String>>) -> Response {
    let p = |k: &str| {
        params
            .get(k)
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    };
    let (a, b) = (p("a"), p("b"));
    if a.is_empty() || b.is_empty() {
        return bad_request("`a` and `b` (entity ids) are required");
    }
    fold(&st.db).await;
    match st
        .db
        .execute(
            query(&format!(
                "MATCH (a:Entity)-[r:RELATES]->(b:Entity) \
                 WHERE (a.id = $x AND b.id = $y) OR (a.id = $y AND b.id = $x) \
                 RETURN {REL_ROW} AS v"
            ))
            .param("x", a.as_str())
            .param("y", b.as_str()),
        )
        .await
    {
        Ok(rows) => {
            let relations: Vec<Value> = rows
                .iter()
                .filter_map(|r| r.get::<Value>("v").ok())
                .map(|m| relation_json(&m, &a))
                .collect();
            Json(json!({ "a": a, "b": b, "relations": relations })).into_response()
        }
        Err(e) => internal(e),
    }
}

/// Active and proposed relationships among `ids` (comma-separated), for the
/// neighbourhood map.
async fn handle_among(State(st): St, Query(params): Query<HashMap<String, String>>) -> Response {
    let ids: Vec<String> = params
        .get("ids")
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .collect()
        })
        .unwrap_or_default();
    if ids.is_empty() {
        return Json(json!({ "relations": [] })).into_response();
    }
    fold(&st.db).await;
    match st
        .db
        .execute(
            query(
                "MATCH (a:Entity)-[r:RELATES]->(b:Entity) \
                 WHERE a.id IN $ids AND b.id IN $ids AND r.status IN ['active', 'proposed'] \
                 RETURN a.id AS a, b.id AS b, r.type AS type, r.status AS status, r.method AS method",
            )
            .param("ids", &ids),
        )
        .await
    {
        Ok(rows) => {
            let relations: Vec<Value> = rows
                .iter()
                .map(|r| {
                    let t = r.get::<String>("type").unwrap_or_default();
                    json!({
                        "source": r.get::<String>("a").ok(), "target": r.get::<String>("b").ok(),
                        "type": t,
                        "label": Vocabulary::global().get(&t).map(|v| v.label.clone()).unwrap_or(t.clone()),
                        "status": r.get::<String>("status").ok(), "method": r.get::<String>("method").ok(),
                    })
                })
                .collect();
            Json(json!({ "relations": relations })).into_response()
        }
        Err(e) => internal(e),
    }
}

fn text(body: &Value, k: &str) -> String {
    body[k].as_str().map(str::trim).unwrap_or("").to_string()
}

fn outcome(r: anyhow::Result<Result<Value, String>>, what: &str) -> Response {
    match r {
        Ok(Ok(v)) => Json(v).into_response(),
        Ok(Err(e)) => bad_request(format!("{what}: {e}")),
        Err(e) => internal(e),
    }
}

async fn handle_set_relation(State(st): St, Json(body): Json<Value>) -> Response {
    let note = text(&body, "note_id");
    outcome(
        rel::set_manual(
            &st.db,
            &text(&body, "from"),
            &text(&body, "to"),
            &text(&body, "type"),
            (!note.is_empty()).then_some(note.as_str()),
        )
        .await,
        "Relationship not set",
    )
}

async fn handle_decide_relation(State(st): St, Json(body): Json<Value>) -> Response {
    let Some(accept) = body["accept"].as_bool() else {
        return bad_request("`accept` (true or false) is required");
    };
    outcome(
        rel::decide(
            &st.db,
            &text(&body, "from"),
            &text(&body, "to"),
            &text(&body, "type"),
            accept,
        )
        .await,
        "Not decided",
    )
}

async fn handle_relabel_relation(State(st): St, Json(body): Json<Value>) -> Response {
    outcome(
        rel::relabel(
            &st.db,
            &text(&body, "from"),
            &text(&body, "to"),
            &text(&body, "type"),
            &text(&body, "new_type"),
        )
        .await,
        "Not relabelled",
    )
}

async fn handle_remove_relation(
    State(st): St,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let p = |k: &str| params.get(k).cloned().unwrap_or_default();
    outcome(
        rel::remove(&st.db, &p("from"), &p("to"), &p("type")).await,
        "Not removed",
    )
}

/// Proposals to review, newest first, each with its first evidence note.
async fn handle_proposed_relations(
    State(st): St,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let rows = match st
        .db
        .execute(
            query(&format!(
                "MATCH (a:Entity)-[r:RELATES {{status: 'proposed'}}]->(b:Entity) \
                 WITH a, r, b ORDER BY r.created_at DESC \
                 WITH collect({REL_ROW}) AS proposals \
                 UNWIND proposals[0..$limit] AS v \
                 OPTIONAL MATCH (n:Note {{id: head(v.evidence)}}) \
                 RETURN size(proposals) AS total, v, n.content AS evidence_text"
            ))
            .param("limit", limit(&params, 30)),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let total = rows
        .first()
        .and_then(|r| r.get::<i64>("total").ok())
        .unwrap_or(0);
    let list: Vec<Value> = rows
        .iter()
        .filter_map(|r| {
            let m = r.get::<Value>("v").ok()?;
            let from = m["from"].as_str().unwrap_or_default().to_string();
            let mut v = relation_json(&m, &from);
            v["from_name"] = m["from_name"].clone();
            v["evidence_text"] = json!(r.get::<String>("evidence_text").ok());
            Some(v)
        })
        .collect();
    Json(json!({ "total": total, "returned": list.len(), "proposed": list })).into_response()
}
