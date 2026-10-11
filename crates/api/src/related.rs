//! How entities relate today: through the notes that mention both
//! (HBI plan §10.4, Phase 16). The graph holds no entity→entity edges yet
//! (typed relations are Phase 17), so every pair here is computed from
//! `(:Note)-[:MENTIONS]->(:Entity)` and drills to the notes behind it.
//!
//! - `GET /api/entities/{id}/related?topic=&limit=` — the entities it shares
//!   notes with, ranked by [`strength`], not by raw count: raw counts put a
//!   hub first everywhere (C-SPAN, in 184 notes, topped 30 of 300 lists it
//!   appears in; by strength, 1).
//! - `GET /api/entities/pair?a=&b=&limit=` — the notes two entities share.
//! - `GET /api/entities/{id}/neighbourhood?topic=&limit=` — an ego graph for
//!   the map: the entity, its strongest neighbours, and the strength of every
//!   pair among them.
//!
//! With `topic`, only that topic's notes count, and entities the user keeps
//! out of it (`NOT_IN_TOPIC`) are left out.
//!
//! Typed relationships (`RELATES`, Phase 17) are the epistemics plugin's:
//! its routes under `/api/plugins/epistemics/` serve them, and the HBI adds
//! them to the pair view and the map (`relations/between`, `relations/among`).

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::rest::{RestState, bad_request, graph, internal, not_found, or_500};
use super::work::clip;

/// Added to both mention counts: two entities that share their only note
/// would otherwise score a perfect 1.0 on one sentence of evidence.
pub const SHRINK: f64 = 3.0;
const DEFAULT_LIMIT: i64 = 15;
const MAX_LIMIT: i64 = 50;
const NEIGHBOURS: i64 = 12;

/// How strongly two entities go together: the notes they share, over the
/// geometric mean of how often each is mentioned (each shrunk by [`SHRINK`]).
/// 1.0 would be two entities only ever mentioned together, often.
pub fn strength(shared: i64, mentions_a: i64, mentions_b: i64) -> f64 {
    shared as f64 / (((mentions_a as f64) + SHRINK) * ((mentions_b as f64) + SHRINK)).sqrt()
}

/// The Cypher computing `strength` from `shared`, `ma`, `mo` (kept beside
/// [`strength`] so the two cannot drift; the test checks them together).
const STRENGTH_CYPHER: &str = "shared / sqrt((ma + $k) * (mo + $k))";

/// Notes of `$topic` only, when it is set.
const IN_TOPIC: &str = "($topic = '' OR EXISTS { (n)-[:IN_TOPIC]->(:Topic {name: $topic}) })";
/// Entity `o` is not kept out of `$topic` by the user.
const NOT_HIDDEN: &str =
    "($topic = '' OR NOT EXISTS { (o)-[:NOT_IN_TOPIC]->(:Topic {name: $topic}) })";

fn limit(params: &HashMap<String, String>, default: i64) -> i64 {
    params
        .get("limit")
        .and_then(|l| l.parse::<i64>().ok())
        .unwrap_or(default)
        .clamp(1, MAX_LIMIT)
}

fn topic(params: &HashMap<String, String>) -> String {
    params.get("topic").cloned().unwrap_or_default()
}

/// The entities `id` shares notes with, strongest first, and how many there
/// are in all.
async fn related_rows(
    neo4j: &agent_brain_repository::Neo4jClient,
    id: &str,
    topic: &str,
    limit: i64,
) -> anyhow::Result<Option<(i64, Vec<Value>)>> {
    let rows = neo4j
        .execute(
            neo4rs::query(&format!(
                "MATCH (a:Entity {{id: $id}}) \
                 OPTIONAL CALL (a) {{ \
                   MATCH (a)<-[:MENTIONS]-(n:Note)-[:MENTIONS]->(o:Entity) \
                   WHERE o <> a AND NOT o.entity_type IN ['url', 'date'] AND {IN_TOPIC} AND {NOT_HIDDEN} \
                   WITH o, count(DISTINCT n) AS shared \
                   WITH o, shared, COUNT {{ (:Note)-[:MENTIONS]->(a) }} AS ma, \
                        COUNT {{ (:Note)-[:MENTIONS]->(o) }} AS mo \
                   RETURN o, shared, mo, {STRENGTH_CYPHER} AS strength \
                 }} \
                 WITH a, o, shared, mo, strength ORDER BY strength DESC, shared DESC, o.name \
                 WITH a, collect(CASE WHEN o IS NULL THEN NULL ELSE {{id: o.id, name: o.name, \
                      entity_type: o.entity_type, shared: shared, mentions: mo, strength: strength}} END) AS rows \
                 RETURN size(rows) AS total, rows[0..$limit] AS rows"
            ))
            .param("id", id)
            .param("topic", topic)
            .param("k", SHRINK)
            .param("limit", limit),
        )
        .await?;
    let Some(r) = rows.first() else {
        return Ok(None);
    };
    let list: Vec<Value> = r
        .get::<Vec<neo4rs::BoltMap>>("rows")
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            json!({
                "id": m.get::<String>("id").ok(),
                "name": m.get::<String>("name").ok(),
                "entity_type": m.get::<String>("entity_type").ok(),
                "shared": m.get::<i64>("shared").unwrap_or(0),
                "mentions": m.get::<i64>("mentions").unwrap_or(0),
                "strength": (m.get::<f64>("strength").unwrap_or(0.0) * 1000.0).round() / 1000.0,
            })
        })
        .collect();
    Ok(Some((r.get::<i64>("total").unwrap_or(0), list)))
}

pub async fn handle_related(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let neo4j = graph!(state);
    let topic = topic(&params);
    match related_rows(neo4j, &id, &topic, limit(&params, DEFAULT_LIMIT)).await {
        Ok(Some((total, related))) => Json(json!({
            "entity": id,
            "topic": if topic.is_empty() { Value::Null } else { json!(topic) },
            "total": total,
            "returned": related.len(),
            "related": related,
            "method": format!("shared notes / sqrt((mentions A + {SHRINK}) × (mentions B + {SHRINK}))"),
        }))
        .into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// `GET /api/entities/pair?a=&b=` — the notes that mention both, newest first.
pub async fn handle_pair(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let neo4j = graph!(state);
    let (Some(a), Some(b)) = (params.get("a"), params.get("b")) else {
        return bad_request("`a` and `b` (entity ids) are required");
    };
    let topic = topic(&params);
    let rows = or_500!(
        neo4j
            .execute(
                neo4rs::query(&format!(
                    "MATCH (ea:Entity {{id: $a}}), (eb:Entity {{id: $b}}) \
                 OPTIONAL CALL (ea, eb) {{ \
                   MATCH (ea)<-[:MENTIONS]-(n:Note)-[:MENTIONS]->(eb) WHERE {IN_TOPIC} \
                   RETURN n ORDER BY n.created_at DESC \
                 }} \
                 WITH ea, eb, collect(n) AS ns \
                 RETURN ea.name AS a_name, eb.name AS b_name, size(ns) AS shared, \
                        [n IN ns[0..$limit] | {{id: n.id, note_type: n.note_type, \
                          source_context: n.source_context, content: n.content, \
                          created_at: toString(n.created_at)}}] AS notes"
                ))
                .param("a", a.as_str())
                .param("b", b.as_str())
                .param("topic", topic.as_str())
                .param("limit", limit(&params, 20)),
            )
            .await
    );
    let Some(r) = rows.first() else {
        return bad_request(format!("No entity '{a}' or '{b}'"));
    };
    let notes: Vec<Value> = r
        .get::<Vec<neo4rs::BoltMap>>("notes")
        .unwrap_or_default()
        .into_iter()
        .map(|m| {
            let (preview, truncated) = clip(&m.get::<String>("content").unwrap_or_default(), 240);
            json!({
                "id": m.get::<String>("id").ok(),
                "note_type": m.get::<String>("note_type").ok(),
                "source_context": m.get::<String>("source_context").ok(),
                "created_at": m.get::<String>("created_at").ok(),
                "preview": preview,
                "preview_truncated": truncated,
            })
        })
        .collect();
    let shared = r.get::<i64>("shared").unwrap_or(0);
    Json(json!({
        "a": { "id": a, "name": r.get::<String>("a_name").ok() },
        "b": { "id": b, "name": r.get::<String>("b_name").ok() },
        "topic": if topic.is_empty() { Value::Null } else { json!(topic) },
        "shared": shared,
        "returned": notes.len(),
        "notes": notes,
    }))
    .into_response()
}

/// `GET /api/entities/{id}/neighbourhood` — the entity, its strongest
/// neighbours, and every pair among them that shares a note (for the map).
pub async fn handle_neighbourhood(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    let neo4j = graph!(state);
    let topic = topic(&params);
    let (total, related) = match related_rows(neo4j, &id, &topic, limit(&params, NEIGHBOURS)).await
    {
        Ok(Some(r)) => r,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    let mut ids: Vec<String> = related
        .iter()
        .filter_map(|r| r["id"].as_str().map(String::from))
        .collect();
    ids.push(id.clone());
    let rows = or_500!(neo4j
        .execute(
            neo4rs::query(
                "MATCH (x:Entity) WHERE x.id IN $ids \
                 RETURN x.id AS id, x.name AS name, x.entity_type AS entity_type, \
                        COUNT { (:Note)-[:MENTIONS]->(x) } AS mentions, \
                        [(x)-[:NOT_IN_TOPIC]->(t:Topic) | t.name] AS hidden_from, \
                        COLLECT { MATCH (x)<-[:MENTIONS]-(:Note)-[r:IN_TOPIC]->(t:Topic) WHERE r.rank = 1 \
                                  RETURN t.name ORDER BY t.name } AS topics",
            )
            .param("ids", ids.clone()),
        )
        .await);
    let mentions: HashMap<String, i64> = rows
        .iter()
        .filter_map(|r| {
            Some((
                r.get::<String>("id").ok()?,
                r.get::<i64>("mentions").unwrap_or(0),
            ))
        })
        .collect();
    let nodes: Vec<Value> = rows
        .iter()
        .map(|r| {
            // The topic most of its notes are mainly in: its colour on the map.
            let topics = r.get::<Vec<String>>("topics").unwrap_or_default();
            let mut counts: HashMap<&str, usize> = HashMap::new();
            for t in &topics {
                *counts.entry(t.as_str()).or_default() += 1;
            }
            let main = counts
                .into_iter()
                .max_by(|a, b| a.1.cmp(&b.1).then(b.0.cmp(a.0)))
                .map(|(t, _)| t.to_string());
            json!({
                "id": r.get::<String>("id").ok(),
                "name": r.get::<String>("name").ok(),
                "entity_type": r.get::<String>("entity_type").ok(),
                "mentions": r.get::<i64>("mentions").unwrap_or(0),
                "topic": main,
                "hidden_from": r.get::<Vec<String>>("hidden_from").unwrap_or_default(),
            })
        })
        .collect();
    let pairs = or_500!(
        neo4j
            .execute(
                neo4rs::query(&format!(
                    "MATCH (x:Entity)<-[:MENTIONS]-(n:Note)-[:MENTIONS]->(y:Entity) \
                 WHERE x.id IN $ids AND y.id IN $ids AND x.id < y.id AND {IN_TOPIC} \
                 RETURN x.id AS a, y.id AS b, count(DISTINCT n) AS shared"
                ))
                .param("ids", ids)
                .param("topic", topic.as_str()),
            )
            .await
    );
    let links: Vec<Value> = pairs
        .iter()
        .filter_map(|r| {
            let (a, b) = (r.get::<String>("a").ok()?, r.get::<String>("b").ok()?);
            let shared = r.get::<i64>("shared").unwrap_or(0);
            let s = strength(
                shared,
                *mentions.get(&a).unwrap_or(&0),
                *mentions.get(&b).unwrap_or(&0),
            );
            Some(json!({
                "source": a, "target": b, "shared": shared,
                "strength": (s * 1000.0).round() / 1000.0,
            }))
        })
        .collect();
    Json(json!({
        "center": id,
        "topic": if topic.is_empty() { Value::Null } else { json!(topic) },
        "total_related": total,
        "nodes": nodes,
        "links": links,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strength_damps_one_note_pairs_and_hubs() {
        // Two entities that share their only note: strong, but not certain.
        let lone = strength(1, 1, 1);
        assert!((lone - 0.25).abs() < 1e-9, "{lone}");
        // Prince Andrew / Amanda Thursk (2 shared, 2 and 4 mentions) beat
        // NewsNation, a hub in 185 notes that shares one.
        assert!(strength(2, 2, 4) > 5.0 * strength(1, 2, 185));
        // More shared evidence between the same two is always stronger.
        assert!(strength(5, 10, 10) > strength(4, 10, 10));
        assert_eq!(strength(0, 10, 10), 0.0);
    }

    #[test]
    fn the_cypher_and_the_rust_compute_the_same_thing() {
        assert_eq!(STRENGTH_CYPHER, "shared / sqrt((ma + $k) * (mo + $k))");
        let (shared, ma, mo, k) = (3.0_f64, 7.0_f64, 20.0_f64, SHRINK);
        assert!((strength(3, 7, 20) - shared / ((ma + k) * (mo + k)).sqrt()).abs() < 1e-12);
    }
}
