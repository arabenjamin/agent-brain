//! Topics for the HBI: the domains, how many notes each holds, the map, and
//! one topic's detail. Assignment itself is `services/topics.rs`.
//!
//! - `GET /api/topics` — every topic with counts, plus **coverage**: every
//!   top-level note is in a topic, assessed as fitting none, waiting, or failed.
//! - `GET /api/topics/map?per_topic=` — topic hubs, each topic's most-mentioned
//!   entities (no urls or dates), and per-entity size metrics: connections,
//!   certainty (claims), activity (last 7 days).
//! - `GET /api/topics/{name}` — one topic: definition, counts, top entities,
//!   newest notes, and the schedules that watch it.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query},
    response::IntoResponse,
};
use serde_json::{Value, json};

use super::rest::{RestState, internal, not_found, unavailable};
use super::work::clip;
use crate::repository::Neo4jClient;
use crate::services::topics::{MAX_FAILURES, SKIPPED_CYPHER, Topics};

const DEFAULT_PER_TOPIC: i64 = 25;
const MAX_PER_TOPIC: i64 = 60;

/// Per-claim certainty: refuted < unverified < unclassified-only < established < primary.
/// Mirrors the claim fields (`claim_status`, `corroboration_tier`).
const CLAIM_CERTAINTY_CYPHER: &str = "CASE \
     WHEN c.claim_status = 'refuted' THEN 0.0 \
     WHEN c.claim_status <> 'corroborated' THEN 0.25 \
     WHEN c.corroboration_tier STARTS WITH 'primary' THEN 1.0 \
     WHEN c.corroboration_tier STARTS WITH 'established' THEN 0.85 \
     ELSE 0.5 END";

async fn count(neo4j: &Neo4jClient, cypher: &str, topics: &Topics) -> i64 {
    neo4j
        .execute(
            neo4rs::query(cypher)
                .param("v", topics.version.as_str())
                .param("max", MAX_FAILURES)
                .param("skip", topics.skip_param()),
        )
        .await
        .ok()
        .and_then(|r| r.first().and_then(|r| r.get::<i64>("c").ok()))
        .unwrap_or(0)
}

pub async fn handle_list(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let topics = Topics::global();
    let v = topics.version.as_str();
    let rows = match neo4j
        .execute(neo4rs::query(
            "MATCH (t:Topic) \
             RETURN t.name AS name, t.label AS label, t.description AS description, \
                    t.schedules AS schedules, t.source_lists AS source_lists, \
                    COUNT { (:Note)-[:IN_TOPIC]->(t) } AS notes, \
                    COUNT { (:Note)-[r:IN_TOPIC]->(t) WHERE r.rank = 1 } AS main, \
                    COUNT { (n:Note)-[:IN_TOPIC]->(t) WHERE n.created_at >= datetime() - duration('P7D') } AS week \
             ORDER BY notes DESC",
        ))
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let list: Vec<Value> = rows
        .iter()
        .map(|r| {
            json!({
                "name": r.get::<String>("name").ok(),
                "label": r.get::<String>("label").ok(),
                "description": r.get::<String>("description").ok(),
                "schedules": r.get::<Vec<String>>("schedules").unwrap_or_default(),
                "source_lists": r.get::<Vec<String>>("source_lists").unwrap_or_default(),
                "notes": r.get::<i64>("notes").unwrap_or(0),
                "main": r.get::<i64>("main").unwrap_or(0),
                "week": r.get::<i64>("week").unwrap_or(0),
            })
        })
        .collect();

    // Coverage: every top-level note lands in exactly one of these.
    const TOP: &str = "MATCH (n:Note) WHERE NOT (n)-[:PART_OF]->() ";
    let total = count(neo4j, &format!("{TOP} RETURN count(n) AS c"), topics).await;
    let skipped = count(
        neo4j,
        &format!("{TOP} AND {SKIPPED_CYPHER} RETURN count(n) AS c"),
        topics,
    )
    .await;
    let scoped = format!("{TOP} AND NOT {SKIPPED_CYPHER}");
    let with_topic = count(
        neo4j,
        &format!("{scoped} AND n.topics_version = $v AND (n)-[:IN_TOPIC]->() RETURN count(n) AS c"),
        topics,
    )
    .await;
    let no_topic = count(
        neo4j,
        &format!(
            "{scoped} AND n.topics_version = $v AND NOT (n)-[:IN_TOPIC]->() RETURN count(n) AS c"
        ),
        topics,
    )
    .await;
    let failed = count(
        neo4j,
        &format!(
            "{scoped} AND coalesce(n.topics_version, '') <> $v AND coalesce(n.topics_failed, 0) >= $max RETURN count(n) AS c"
        ),
        topics,
    )
    .await;
    let waiting = (total - skipped - with_topic - no_topic - failed).max(0);

    // What the notes that fit no topic talk about: candidates for a new topic.
    let no_topic_entities: Vec<Value> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note)-[:MENTIONS]->(e:Entity) \
                 WHERE n.topics_version = $v AND NOT (n)-[:IN_TOPIC]->() \
                   AND NOT e.entity_type IN ['url', 'date'] \
                 RETURN e.id AS id, e.name AS name, count(DISTINCT n) AS notes \
                 ORDER BY notes DESC LIMIT 15",
            )
            .param("v", v),
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String>("id").ok(),
                "name": r.get::<String>("name").ok(),
                "notes": r.get::<i64>("notes").unwrap_or(0),
            })
        })
        .collect();

    Json(json!({
        "version": topics.version,
        "topics": list,
        "coverage": {
            "notes": total,
            "with_topic": with_topic,
            "no_topic": no_topic,
            "waiting": waiting,
            "failed": failed,
            "skipped": skipped,
        },
        "skip_rules": topics.scope.skip,
        "no_topic_entities": no_topic_entities,
        "method": "llm",
    }))
    .into_response()
}

pub async fn handle_map(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let per_topic = params
        .get("per_topic")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(DEFAULT_PER_TOPIC)
        .clamp(1, MAX_PER_TOPIC);

    let topic_rows = match neo4j
        .execute(neo4rs::query(
            "MATCH (t:Topic) \
             RETURN t.name AS name, t.label AS label, \
                    COUNT { (:Note)-[:IN_TOPIC]->(t) } AS notes, \
                    COUNT { (n:Note)-[:IN_TOPIC]->(t) WHERE n.created_at >= datetime() - duration('P7D') } AS week",
        ))
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let topics: Vec<Value> = topic_rows
        .iter()
        .map(|r| {
            json!({
                "name": r.get::<String>("name").ok(),
                "label": r.get::<String>("label").ok(),
                "notes": r.get::<i64>("notes").unwrap_or(0),
                "week": r.get::<i64>("week").unwrap_or(0),
            })
        })
        .collect();

    // Each topic's entities, ranked by how many of the topic's notes mention them.
    let link_rows = match neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Topic) \
                 CALL (t) { \
                   MATCH (t)<-[:IN_TOPIC]-(n:Note)-[:MENTIONS]->(e:Entity) \
                   WHERE NOT e.entity_type IN ['url', 'date'] \
                   RETURN e, count(DISTINCT n) AS w ORDER BY w DESC LIMIT $k \
                 } \
                 RETURN t.name AS topic, e.id AS id, w",
            )
            .param("k", per_topic),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let links: Vec<Value> = link_rows
        .iter()
        .map(|r| {
            json!({
                "topic": r.get::<String>("topic").ok(),
                "entity": r.get::<String>("id").ok(),
                "notes": r.get::<i64>("w").unwrap_or(0),
            })
        })
        .collect();
    let ids: Vec<String> = link_rows
        .iter()
        .filter_map(|r| r.get::<String>("id").ok())
        .collect::<HashSet<_>>()
        .into_iter()
        .collect();

    // Size metrics for every entity on the map.
    let ent_rows = neo4j
        .execute(
            neo4rs::query(&format!(
                "MATCH (e:Entity) WHERE e.id IN $ids \
                 OPTIONAL CALL (e) {{ \
                   MATCH (c:Note {{note_type: 'claim'}})-[:MENTIONS]->(e) \
                   RETURN count(c) AS claims, avg({CLAIM_CERTAINTY_CYPHER}) AS certainty \
                 }} \
                 RETURN e.id AS id, e.name AS name, e.entity_type AS entity_type, \
                        COUNT {{ (:Note)-[:MENTIONS]->(e) }} AS mentions, \
                        COUNT {{ (n:Note)-[:MENTIONS]->(e) WHERE n.created_at >= datetime() - duration('P7D') }} AS week, \
                        claims, certainty"
            ))
            .param("ids", ids),
        )
        .await;
    let ent_rows = match ent_rows {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let entities: Vec<Value> = ent_rows
        .iter()
        .map(|r| {
            let claims = r.get::<i64>("claims").unwrap_or(0);
            json!({
                "id": r.get::<String>("id").ok(),
                "name": r.get::<String>("name").ok(),
                "entity_type": r.get::<String>("entity_type").ok(),
                "mentions": r.get::<i64>("mentions").unwrap_or(0),
                "week": r.get::<i64>("week").unwrap_or(0),
                "claims": claims,
                "certainty": if claims > 0 { r.get::<f64>("certainty").ok() } else { None },
            })
        })
        .collect();

    Json(json!({
        "topics": topics,
        "entities": entities,
        "links": links,
        "per_topic": per_topic,
        "hidden_entity_types": ["url", "date"],
    }))
    .into_response()
}

pub async fn handle_topic(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let rows = match neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Topic {name: $name}) \
                 RETURN t.name AS name, t.label AS label, t.description AS description, \
                        t.schedules AS schedules, t.source_lists AS source_lists, \
                        COUNT { (:Note)-[:IN_TOPIC]->(t) } AS notes, \
                        COUNT { (:Note)-[r:IN_TOPIC]->(t) WHERE r.rank = 1 } AS main, \
                        COUNT { (n:Note)-[:IN_TOPIC]->(t) WHERE n.created_at >= datetime() - duration('P7D') } AS week",
            )
            .param("name", name.as_str()),
        )
        .await
    {
        Ok(r) => r,
        Err(e) => return internal(e),
    };
    let Some(t) = rows.first() else {
        return not_found();
    };
    let schedules = t.get::<Vec<String>>("schedules").unwrap_or_default();

    let entities: Vec<Value> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (:Topic {name: $name})<-[:IN_TOPIC]-(n:Note)-[:MENTIONS]->(e:Entity) \
                 WHERE NOT e.entity_type IN ['url', 'date'] \
                 RETURN e.id AS id, e.name AS name, e.entity_type AS entity_type, count(DISTINCT n) AS notes \
                 ORDER BY notes DESC LIMIT 20",
            )
            .param("name", name.as_str()),
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String>("id").ok(),
                "name": r.get::<String>("name").ok(),
                "entity_type": r.get::<String>("entity_type").ok(),
                "notes": r.get::<i64>("notes").unwrap_or(0),
            })
        })
        .collect();

    let notes: Vec<Value> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note)-[r:IN_TOPIC]->(:Topic {name: $name}) \
                 RETURN n.id AS id, n.note_type AS note_type, n.content AS content, \
                        toString(n.created_at) AS created_at, r.rank AS rank \
                 ORDER BY n.created_at DESC LIMIT 25",
            )
            .param("name", name.as_str()),
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let (preview, _) = clip(&r.get::<String>("content").unwrap_or_default(), 200);
            json!({
                "id": r.get::<String>("id").ok(),
                "note_type": r.get::<String>("note_type").ok(),
                "preview": preview,
                "created_at": r.get::<String>("created_at").ok(),
                "main": r.get::<i64>("rank").unwrap_or(1) == 1,
            })
        })
        .collect();

    // Schedules named in the YAML, resolved to ids the drawer can open. A name
    // that matches nothing is reported, not dropped.
    let found: HashMap<String, (String, bool)> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (s:ScheduledTask) WHERE s.name IN $names \
                 RETURN s.name AS name, s.id AS id, coalesce(s.enabled, true) AS enabled",
            )
            .param("names", schedules.clone()),
        )
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| {
            Some((
                r.get::<String>("name").ok()?,
                (
                    r.get::<String>("id").ok()?,
                    r.get::<bool>("enabled").unwrap_or(true),
                ),
            ))
        })
        .collect();
    let schedules: Vec<Value> = schedules
        .iter()
        .map(|n| match found.get(n) {
            Some((id, enabled)) => json!({ "name": n, "id": id, "enabled": enabled }),
            None => json!({ "name": n, "id": null, "missing": true }),
        })
        .collect();

    Json(json!({
        "topic": {
            "name": t.get::<String>("name").ok(),
            "label": t.get::<String>("label").ok(),
            "description": t.get::<String>("description").ok(),
            "source_lists": t.get::<Vec<String>>("source_lists").unwrap_or_default(),
            "notes": t.get::<i64>("notes").unwrap_or(0),
            "main": t.get::<i64>("main").unwrap_or(0),
            "week": t.get::<i64>("week").unwrap_or(0),
        },
        "schedules": schedules,
        "entities": entities,
        "notes": notes,
        "notes_shown": notes.len(),
    }))
    .into_response()
}
