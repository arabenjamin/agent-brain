//! Topics for the HBI: the domains, how many notes each holds, the map, and
//! one topic's detail. Assignment itself is `services/topics.rs`.
//!
//! - `GET /api/topics` — every topic with counts, plus **coverage**: every
//!   top-level note is in a topic, assessed as fitting none, waiting, or failed.
//! - `GET /api/topics/map?per_topic=` — topic hubs, each topic's most-mentioned
//!   entities (no urls or dates), and per-entity size metrics: connections,
//!   certainty (claims), activity (last 7 days).
//! - `GET /api/topics/{name}` — one topic: definition, counts, top entities,
//!   newest notes, the schedules that watch it, and who owns its definition.
//! - `POST /api/topics`, `PUT|DELETE /api/topics/{name}`,
//!   `POST /api/topics/{name}/reset` — edit the topics themselves
//!   (`services/topics.rs` [`save`], [`delete`], [`reset`]): an edit makes the
//!   topic runtime-owned, and a changed definition re-classifies every note
//!   on idle time.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::rest::{RestState, bad_request, internal, not_found, unavailable};
use super::work::clip;
use crate::repository::Neo4jClient;
use crate::services::topics::{
    self as topic_service, MAX_FAILURES, SKIPPED_CYPHER, TopicDef, Topics,
};

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
    let topics = topics.as_ref();
    let v = topics.version.as_str();
    let rows = match neo4j
        .execute(neo4rs::query(
            "MATCH (t:Topic) \
             RETURN t.name AS name, t.label AS label, t.description AS description, \
                    COLLECT { MATCH (s:Workflow) WHERE s.topic = t.name RETURN s.name } AS schedules, t.source_lists AS source_lists, \
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
                   WHERE NOT e.entity_type IN ['url', 'date'] AND NOT (e)-[:NOT_IN_TOPIC]->(t) \
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

    // Entities the user keeps out of a topic: counted, so a hide is never silent.
    let hidden_by_user: Vec<Value> = neo4j
        .execute(neo4rs::query(
            "MATCH (e:Entity)-[:NOT_IN_TOPIC]->(t:Topic) \
             RETURN t.name AS topic, count(e) AS n, collect(e.name)[..20] AS names ORDER BY topic",
        ))
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "topic": r.get::<String>("topic").ok(),
                "count": r.get::<i64>("n").unwrap_or(0),
                "names": r.get::<Vec<String>>("names").unwrap_or_default(),
            })
        })
        .collect();

    Json(json!({
        "topics": topics,
        "entities": entities,
        "links": links,
        "per_topic": per_topic,
        "hidden_entity_types": ["url", "date"],
        "hidden_by_user": hidden_by_user,
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
                        t.source_lists AS source_lists, coalesce(t.managed_by, 'yaml') AS managed_by, \
                        toString(t.updated_at) AS updated_at, \
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

    let entities: Vec<Value> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Topic {name: $name})<-[:IN_TOPIC]-(n:Note)-[:MENTIONS]->(e:Entity) \
                 WHERE NOT e.entity_type IN ['url', 'date'] AND NOT (e)-[:NOT_IN_TOPIC]->(t) \
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

    // Entities the user keeps out of this topic, with what they would count.
    let hidden: Vec<Value> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (e:Entity)-[h:NOT_IN_TOPIC]->(t:Topic {name: $name}) \
                 RETURN e.id AS id, e.name AS name, e.entity_type AS entity_type, \
                        toString(h.at) AS at, \
                        COUNT { (t)<-[:IN_TOPIC]-(:Note)-[:MENTIONS]->(e) } AS notes \
                 ORDER BY notes DESC, name",
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
                "at": r.get::<String>("at").ok(),
            })
        })
        .collect();

    // The workflows that declare this topic (`topic:` on the workflow).
    let schedules: Vec<Value> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (s:Workflow {topic: $name}) \
                 RETURN s.name AS name, s.id AS id, coalesce(s.enabled, true) AS enabled, s.kind AS kind \
                 ORDER BY s.name",
            )
            .param("name", name.as_str()),
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "name": r.get::<String>("name").ok(),
                "id": r.get::<String>("id").ok(),
                "enabled": r.get::<bool>("enabled").unwrap_or(true),
                "kind": r.get::<String>("kind").ok(),
            })
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
            "managed_by": t.get::<String>("managed_by").ok(),
            // Who puts notes in it, and the rules when that is `rules`.
            "assign": Topics::global().get(&name).map(|d| d.assign).unwrap_or_default(),
            "members": Topics::global().get(&name).map(|d| d.members.clone()).unwrap_or_default(),
            "reference": Topics::global().get(&name).and_then(|d| d.reference.clone()),
            "updated_at": t.get::<String>("updated_at").ok(),
        },
        // The topics/*.yaml definition, when one exists: what "reset" restores,
        // and what a runtime-owned topic shadows.
        "seed": Topics::load_dir(&topic_service::topics_dir()).get(&name).cloned(),
        "reclassify": reclassify_status(neo4j).await,
        "schedules": schedules,
        "entities": entities,
        "hidden": hidden,
        "notes": notes,
        "notes_shown": notes.len(),
    }))
    .into_response()
}

/// How far the classifier is through the current topic version: the notes
/// still waiting (not yet classified under it, not given up on, not skipped).
async fn reclassify_status(neo4j: &Neo4jClient) -> Value {
    let topics = Topics::global();
    let waiting = count(
        neo4j,
        &format!(
            "MATCH (n:Note) WHERE NOT (n)-[:PART_OF]->() AND NOT {SKIPPED_CYPHER} \
               AND coalesce(n.topics_version, '') <> $v AND coalesce(n.topics_failed, 0) < $max \
             RETURN count(n) AS c"
        ),
        &topics,
    )
    .await;
    json!({ "version": topics.version, "waiting": waiting })
}

/// A topic definition from a request body; `name` comes from the path on update.
fn body_def(body: &Value, name: Option<&str>) -> Result<TopicDef, String> {
    let text = |k: &str| body[k].as_str().map(str::trim).unwrap_or("").to_string();
    let name = name.map(String::from).unwrap_or_else(|| text("name"));
    let current = Topics::global();
    let old = current.get(&name);
    let pick = |k: &str, prev: Option<&String>| -> String {
        if body.get(k).is_some() {
            text(k)
        } else {
            prev.cloned().unwrap_or_default()
        }
    };
    let source_lists = match body.get("source_lists") {
        None => old.map(|d| d.source_lists.clone()).unwrap_or_default(),
        Some(Value::Array(a)) => a
            .iter()
            .filter_map(|v| v.as_str().map(str::trim))
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
        Some(_) => return Err("source_lists must be an array of names".into()),
    };
    let assign = match body.get("assign") {
        None => old.map(|d| d.assign).unwrap_or_default(),
        Some(v) => v
            .as_str()
            .and_then(topic_service::Assign::parse)
            .ok_or("assign must be 'classifier' or 'rules'")?,
    };
    let members = match body.get("members") {
        None => old.map(|d| d.members.clone()).unwrap_or_default(),
        Some(v) => {
            serde_json::from_value::<Vec<topic_service::MemberRule>>(v.clone()).map_err(|e| {
                format!(
                    "members must be a list of {{note_type, source_context, content_contains}}: {e}"
                )
            })?
        }
    };
    let reference = match body.get("reference") {
        None => old.and_then(|d| d.reference.clone()),
        Some(Value::Null) => None,
        Some(Value::String(r)) if r.trim().is_empty() => None,
        Some(Value::String(r)) => Some(r.trim().to_string()),
        Some(_) => return Err("reference must be a URL or null".into()),
    };
    Ok(TopicDef {
        reference,
        label: pick("label", old.map(|d| &d.label)),
        description: pick("description", old.map(|d| &d.description)),
        source_lists,
        assign,
        members,
        name,
    })
}

/// `before` is the version before the write: only a changed definition
/// re-classifies (a label or source-list edit does not).
fn saved(topics: &Topics, name: &str, before: &str) -> Response {
    let note = if topics.version == before {
        "Saved. The definitions are unchanged, so no note is classified again."
    } else {
        "Saved. Notes are classified again under the new definitions while the brain is idle (about 4 hours for the whole graph; the map's coverage line shows progress)."
    };
    Json(json!({
        "saved": true,
        "topic": topics.get(name),
        "version": topics.version,
        "reclassify": topics.version != before,
        "note": note,
    }))
    .into_response()
}

/// `POST /api/topics` — create a topic (runtime-owned).
pub async fn handle_create(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let def = match body_def(&body, None) {
        Ok(d) => d,
        Err(e) => return bad_request(e),
    };
    let before = Topics::global().version.clone();
    match topic_service::save(neo4j, &def, true).await {
        Ok(Ok(t)) => saved(&t, &def.name, &before),
        Ok(Err(e)) => bad_request(format!("Topic not created: {e}")),
        Err(e) => internal(e),
    }
}

/// `PUT /api/topics/{name}` — change a topic's label, description or source
/// lists. The name is its identity and does not change.
pub async fn handle_update(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    if body
        .get("name")
        .and_then(|v| v.as_str())
        .is_some_and(|n| n != name)
    {
        return bad_request(
            "A topic's name can't change: create the new topic, move its schedules, then delete this one",
        );
    }
    let def = match body_def(&body, Some(&name)) {
        Ok(d) => d,
        Err(e) => return bad_request(e),
    };
    let before = Topics::global().version.clone();
    match topic_service::save(neo4j, &def, false).await {
        Ok(Ok(t)) => saved(&t, &name, &before),
        Ok(Err(e)) => bad_request(format!("Topic not saved: {e}")),
        Err(e) => internal(e),
    }
}

/// `DELETE /api/topics/{name}` — refused while a schedule declares it.
pub async fn handle_delete(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    match topic_service::delete(neo4j, &name).await {
        Ok(Ok(d)) => Json(json!({ "deleted": d })).into_response(),
        Ok(Err(e)) => bad_request(format!("Topic not deleted: {e}")),
        Err(e) => internal(e),
    }
}

/// `POST /api/topics/{name}/reset` — back to its topics/*.yaml definition.
pub async fn handle_reset(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let before = Topics::global().version.clone();
    match topic_service::reset(neo4j, &name).await {
        Ok(Ok(t)) => saved(&t, &name, &before),
        Ok(Err(e)) => bad_request(format!("Topic not reset: {e}")),
        Err(e) => internal(e),
    }
}

/// `PUT /api/notes/{id}/topics` `{topics: [main, second?]}` — set a note's
/// topics by hand and lock them (an empty list: fits no topic).
/// `DELETE /api/notes/{id}/topics/lock` — hand it back to the classifier.
pub async fn handle_set_note_topics(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let Some(list) = body["topics"].as_array() else {
        return bad_request(
            "`topics` must be an array of topic names, main first (empty: fits no topic)",
        );
    };
    let names: Vec<String> = list
        .iter()
        .filter_map(|v| v.as_str().map(String::from))
        .collect();
    match topic_service::set_note_topics(neo4j, &id, &names).await {
        Ok(Ok(t)) => Json(json!({ "topics": t, "locked": true })).into_response(),
        Ok(Err(e)) => bad_request(format!("Topics not set: {e}")),
        Err(e) => internal(e),
    }
}

pub async fn handle_unlock_note_topics(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    match topic_service::unlock_note(neo4j, &id).await {
        Ok(true) => Json(json!({
            "locked": false,
            "note": "The classifier decides this note's topics again, while the brain is idle.",
        }))
        .into_response(),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

/// `PUT|DELETE /api/entities/{id}/topics/{topic}/hidden` — keep an entity
/// out of a topic, or let it back in.
pub async fn handle_hide_entity(
    Extension(state): Extension<Arc<RestState>>,
    Path((id, topic)): Path<(String, String)>,
) -> Response {
    hide(state, id, topic, true).await
}

pub async fn handle_unhide_entity(
    Extension(state): Extension<Arc<RestState>>,
    Path((id, topic)): Path<(String, String)>,
) -> Response {
    hide(state, id, topic, false).await
}

async fn hide(state: Arc<RestState>, id: String, topic: String, on: bool) -> Response {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    match topic_service::hide_entity(neo4j, &id, &topic, on).await {
        Ok(true) => Json(json!({ "entity": id, "topic": topic, "hidden": on })).into_response(),
        Ok(false) => bad_request(format!("No entity '{id}' or no topic '{topic}'")),
        Err(e) => internal(e),
    }
}
