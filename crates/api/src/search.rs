//! Search for the HBI's Knowledge workspace: one query, three ways in.
//!
//! - **semantic** — the brain's own hybrid retrieval (vector + BM25, RRF,
//!   freshness boost, chunk → parent) through the *non-perturbing* path, so a
//!   person browsing never moves the access/freshness signals retrieval ranks
//!   on. Each hit says why it matched ([`MatchTrace`]).
//! - **keyword** — BM25 alone over the fulltext index; a `"quoted phrase"`
//!   is a phrase query, bare words are all required.
//! - **id** — an exact id, across every kind the HBI can open.
//!
//! `auto` picks one from the query's shape. Entity names are matched in every
//! text mode, so "trump" also offers the Trump entities.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use axum::{
    Extension, Json,
    extract::{Path, Query},
    response::IntoResponse,
};
use serde_json::{Value, json};

use super::rest::{RestState, graph, not_found, or_500, unavailable};
use super::work::clip;
use agent_brain_knowledge::knowledge::{MatchTrace, TRACE_NO_VECTOR, sanitize_lucene_query};
use agent_brain_repository::Neo4jClient;

const DEFAULT_LIMIT: usize = 30;
const MAX_LIMIT: usize = 100;
const PREVIEW_CHARS: usize = 320;
const ENTITY_LIMIT: i64 = 12;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Semantic,
    Keyword,
    Id,
}

impl Mode {
    fn as_str(self) -> &'static str {
        match self {
            Mode::Semantic => "semantic",
            Mode::Keyword => "keyword",
            Mode::Id => "id",
        }
    }
}

fn looks_like_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    b.len() == 36
        && b.iter().enumerate().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => *c == b'-',
            _ => c.is_ascii_hexdigit(),
        })
}

/// The mode a query runs in. `auto`: a bare UUID is an id lookup, a query
/// wrapped in double quotes is a keyword phrase, anything else is semantic.
pub(crate) fn resolve_mode(query: &str, requested: Option<&str>) -> Mode {
    let q = query.trim();
    match requested.unwrap_or("auto") {
        "semantic" => Mode::Semantic,
        "keyword" => Mode::Keyword,
        "id" => Mode::Id,
        _ if looks_like_uuid(q) => Mode::Id,
        _ if q.len() >= 2 && q.starts_with('"') && q.ends_with('"') => Mode::Keyword,
        _ => Mode::Semantic,
    }
}

/// The Lucene query for keyword mode. A fully quoted query is one phrase;
/// otherwise every word is required (`+a +b`), since Lucene's default OR
/// made "sodium batteries" match every note that says "batteries".
/// Each part is escaped, so URLs and colons are literal.
pub(crate) fn keyword_query(query: &str) -> Option<String> {
    let q = query.trim();
    if q.len() >= 2 && q.starts_with('"') && q.ends_with('"') {
        let inner = q[1..q.len() - 1].trim();
        return (!inner.is_empty()).then(|| format!("\"{}\"", sanitize_lucene_query(inner)));
    }
    let parts: Vec<String> = q
        .split_whitespace()
        .map(|w| format!("+{}", sanitize_lucene_query(w)))
        .collect();
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// A note's topics, main topic first.
pub(crate) fn topic_names(maps: Vec<neo4rs::BoltMap>) -> Vec<String> {
    let mut v: Vec<(i64, String)> = maps
        .into_iter()
        .filter_map(|m| {
            let name: String = m.get("name").ok()?;
            Some((m.get::<i64>("rank").unwrap_or(9), name))
        })
        .collect();
    v.sort();
    v.into_iter().map(|(_, n)| n).collect()
}

/// Note ids in a topic, newest first.
async fn topic_note_ids(neo4j: &Neo4jClient, topic: &str, limit: usize) -> Vec<String> {
    neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note)-[:IN_TOPIC]->(:Topic {name: $t}) \
                 RETURN n.id AS id ORDER BY n.created_at DESC LIMIT $limit",
            )
            .param("t", topic)
            .param("limit", limit as i64),
        )
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.get::<String>("id").ok())
        .collect()
}

/// Which of `ids` are in `topic`.
async fn in_topic(neo4j: &Neo4jClient, topic: &str, ids: &[String]) -> HashSet<String> {
    neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note)-[:IN_TOPIC]->(:Topic {name: $t}) WHERE n.id IN $ids RETURN n.id AS id",
            )
            .param("t", topic)
            .param("ids", ids.to_vec()),
        )
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.get::<String>("id").ok())
        .collect()
}

/// Note fields shown in a result, keyed by id (content raw, not claim-labelled).
async fn note_meta(neo4j: &Neo4jClient, ids: &[String]) -> HashMap<String, Value> {
    if ids.is_empty() {
        return HashMap::new();
    }
    neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note) WHERE n.id IN $ids \
                 RETURN n.id AS id, n.note_type AS note_type, n.content AS content, \
                        toString(n.created_at) AS created_at, n.claim_status AS claim_status, \
                        n.source_context AS source_context, n.verification AS verification, \
                        [(n)-[r:IN_TOPIC]->(t:Topic) | {name: t.name, rank: r.rank}] AS topics",
            )
            .param("ids", ids.to_vec()),
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            let id = r.get::<String>("id").unwrap_or_default();
            let content = r.get::<String>("content").unwrap_or_default();
            let (preview, cut) = clip(&content, PREVIEW_CHARS);
            let v = json!({
                "id": id,
                "note_type": r.get::<String>("note_type").ok(),
                "preview": preview,
                "preview_truncated": cut,
                "created_at": r.get::<String>("created_at").ok(),
                "claim_status": r.get::<String>("claim_status").ok(),
                "source_context": r.get::<String>("source_context").ok(),
                "verification": r.get::<String>("verification").ok(),
                "topics": topic_names(r.get::<Vec<neo4rs::BoltMap>>("topics").unwrap_or_default()),
            });
            (id, v)
        })
        .collect()
}

/// Entities the query names: a name that contains the whole query, or one
/// that appears in it as whole words ("trump tariffs on china" → trump,
/// tariffs, china). Names under 4 characters only match the whole query, so
/// "us" and "ai" don't ride along on every search. Exact match first, then
/// longer (more specific) names, then the most mentioned.
async fn entity_matches(neo4j: &Neo4jClient, query: &str) -> Vec<Value> {
    let q = query.trim().trim_matches('"').trim().to_lowercase();
    if q.chars().count() < 2 {
        return Vec::new();
    }
    neo4j
        .execute(
            neo4rs::query(
                "MATCH (e:Entity) \
                 WHERE NOT e.entity_type IN ['url', 'date'] \
                   AND (e.name CONTAINS $q \
                        OR (size(e.name) >= 4 AND (' ' + $q + ' ') CONTAINS (' ' + e.name + ' ')) \
                        OR EXISTS { MATCH (:EntityAlias {name: $q})-[:ALIAS_OF]->(e) }) \
                 OPTIONAL MATCH (e)<-[m:MENTIONS]-() \
                 WITH e, count(m) AS mentions WHERE mentions > 0 \
                 RETURN e.id AS id, e.name AS name, e.entity_type AS entity_type, mentions \
                 ORDER BY (e.name = $q OR EXISTS { MATCH (:EntityAlias {name: $q})-[:ALIAS_OF]->(e) }) DESC, \
                          size(e.name) DESC, mentions DESC LIMIT $limit",
            )
            .param("q", q)
            .param("limit", ENTITY_LIMIT),
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "id": r.get::<String>("id").ok(),
                "name": r.get::<String>("name").ok(),
                "entity_type": r.get::<String>("entity_type").ok(),
                "mentions": r.get::<i64>("mentions").unwrap_or(0),
            })
        })
        .collect()
}

/// BM25 over the fulltext index, chunks resolved to their parent note
/// (keeping the best-scoring chunk), optionally filtered by note type.
async fn keyword_hits(
    neo4j: &Neo4jClient,
    lucene: &str,
    note_type: Option<&str>,
    limit: usize,
) -> Result<Vec<(String, MatchTrace)>, String> {
    let rows = neo4j
        .execute(
            neo4rs::query(
                "CALL db.index.fulltext.queryNodes('note_content_fulltext', $q, {limit: $fetch}) \
                 YIELD node, score \
                 OPTIONAL MATCH (node)-[:PART_OF]->(p:Note) \
                 WITH coalesce(p, node) AS n, node, score \
                 WHERE $nt = '' OR coalesce(n.note_type, 'semantic') = $nt \
                 RETURN n.id AS id, CASE WHEN n = node THEN null ELSE node.id END AS chunk, score \
                 ORDER BY score DESC",
            )
            .param("q", lucene)
            .param("fetch", (limit * 4) as i64)
            .param("nt", note_type.unwrap_or("")),
        )
        .await
        .map_err(|e| e.to_string())?;
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for r in rows {
        let Ok(id) = r.get::<String>("id") else {
            continue;
        };
        if !seen.insert(id.clone()) {
            continue; // a parent already credited by a better chunk
        }
        let rank = out.len() + 1;
        out.push((
            id,
            MatchTrace {
                rank,
                bm25_rank: Some(rank),
                bm25_score: r.get::<f64>("score").ok(),
                via_chunk: r.get::<String>("chunk").ok(),
                ..Default::default()
            },
        ));
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// What an id names, across every kind the HBI opens in its drawer.
async fn lookup_id(neo4j: &Neo4jClient, id: &str) -> Option<Value> {
    let rows = neo4j
        .execute(
            neo4rs::query(
                "CALL { \
                   MATCH (n:Note {id: $id}) RETURN 'note' AS kind, left(n.content, 120) AS label \
                   UNION MATCH (n:Entity {id: $id}) RETURN 'entity' AS kind, n.name AS label \
                   UNION MATCH (n:Task {id: $id}) RETURN 'task' AS kind, n.goal AS label \
                   UNION MATCH (n:AgentJob {id: $id}) RETURN 'job' AS kind, n.tool_name AS label \
                   UNION MATCH (n:Workflow {id: $id}) RETURN 'schedule' AS kind, n.name AS label \
                   UNION MATCH (n:Todo {id: $id}) RETURN 'todo' AS kind, n.title AS label \
                 } RETURN kind, label LIMIT 1",
            )
            .param("id", id),
        )
        .await
        .ok()?;
    rows.first().map(|r| {
        json!({ "kind": r.get::<String>("kind").ok(), "id": id, "label": r.get::<String>("label").ok() })
    })
}

/// GET /api/search?q=&mode=auto|semantic|keyword|id&type=&limit=&hops=
pub async fn handle_search(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let neo4j = graph!(state);
    let query = params
        .get("q")
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    let mode = resolve_mode(&query, params.get("mode").map(String::as_str));
    let note_type = params
        .get("type")
        .map(String::as_str)
        .filter(|s| !s.is_empty());
    let limit = params
        .get("limit")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, MAX_LIMIT);
    // Narrow to one topic. The search fetches more, then keeps the topic's
    // notes, so a topic filter doesn't silently shrink the result.
    let topic = params
        .get("topic")
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let fetch = if topic.is_some() {
        (limit * 4).min(MAX_LIMIT * 2)
    } else {
        limit
    };
    let hops = params
        .get("hops")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0)
        .min(2);

    let base = |extra: Value| {
        let mut v = json!({
            "query": query,
            "mode": mode.as_str(),
            "requested_mode": params.get("mode").cloned().unwrap_or_else(|| "auto".into()),
            "type": note_type,
            "topic": topic,
            "limit": limit,
        });
        if let (Value::Object(m), Value::Object(e)) = (&mut v, extra) {
            m.extend(e);
        }
        Json(v).into_response()
    };

    if query.is_empty() {
        // A topic alone lists its newest notes.
        if let Some(t) = topic.as_deref() {
            let ids = topic_note_ids(neo4j, t, limit).await;
            let meta = note_meta(neo4j, &ids).await;
            // Same shape as a search hit: rank is the order (newest first).
            let notes: Vec<Value> = ids
                .iter()
                .filter_map(|i| meta.get(i).cloned())
                .enumerate()
                .map(|(rank, mut v)| {
                    v["match"] = json!({ "rank": rank + 1 });
                    v
                })
                .collect();
            let truncated = notes.len() >= limit;
            return base(
                json!({ "mode": "topic", "notes": notes, "entities": [], "truncated": truncated }),
            );
        }
        return base(
            json!({ "notes": [], "entities": [], "error": "Type something to search for." }),
        );
    }

    if mode == Mode::Id {
        let found = lookup_id(neo4j, &query.to_lowercase()).await;
        return base(json!({ "notes": [], "entities": [], "id_match": found }));
    }

    let (hits, vector_available, error): (Vec<(String, MatchTrace)>, Option<bool>, Option<String>) =
        match mode {
            Mode::Semantic => {
                let Some(ks) = state.knowledge.as_ref() else {
                    return unavailable(
                        "Semantic search needs the knowledge service, which this brain was started without",
                    );
                };
                match ks
                    .search_notes_explained(&query, fetch, hops, note_type)
                    .await
                {
                    Ok((results, mut trace)) => {
                        let no_vector = trace.remove(TRACE_NO_VECTOR).is_some();
                        let hits = results
                            .iter()
                            .filter_map(|r| r["id"].as_str())
                            .map(|id| (id.to_string(), trace.remove(id).unwrap_or_default()))
                            .collect();
                        (hits, Some(!no_vector), None)
                    }
                    Err(e) => (Vec::new(), None, Some(e.to_string())),
                }
            }
            Mode::Keyword => match keyword_query(&query) {
                Some(lucene) => match keyword_hits(neo4j, &lucene, note_type, fetch).await {
                    Ok(h) => (h, None, None),
                    Err(e) => (
                        Vec::new(),
                        None,
                        Some(format!("Keyword search failed: {e}")),
                    ),
                },
                None => (
                    Vec::new(),
                    None,
                    Some("Nothing to search for inside the quotes.".into()),
                ),
            },
            Mode::Id => unreachable!(),
        };

    let mut hits = hits;
    let mut topic_filtered = Value::Null;
    if let Some(t) = topic.as_deref() {
        let ids: Vec<String> = hits.iter().map(|(id, _)| id.clone()).collect();
        let keep = in_topic(neo4j, t, &ids).await;
        let before = hits.len();
        hits.retain(|(id, _)| keep.contains(id));
        hits.truncate(limit);
        topic_filtered = json!({ "searched": before, "in_topic": hits.len() });
    }
    let ids: Vec<String> = hits.iter().map(|(id, _)| id.clone()).collect();
    let meta = note_meta(neo4j, &ids).await;
    let notes: Vec<Value> = hits
        .into_iter()
        .filter_map(|(id, trace)| {
            let mut v = meta.get(&id)?.clone();
            v["match"] = serde_json::to_value(trace).unwrap_or(Value::Null);
            Some(v)
        })
        .collect();
    let entities = entity_matches(neo4j, &query).await;
    let truncated = notes.len() >= limit;

    base(json!({
        "notes": notes,
        "entities": entities,
        "truncated": truncated,
        "vector_available": vector_available,
        "topic_filtered": topic_filtered,
        "error": error,
    }))
}

/// GET /api/lookup/{id} — what kind of thing an id names.
pub async fn handle_lookup(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let neo4j = graph!(state);
    match lookup_id(neo4j, &id.trim().to_lowercase()).await {
        Some(v) => Json(v).into_response(),
        None => not_found(),
    }
}

/// Most ids one `POST /api/lookup` resolves.
const LOOKUP_MANY_MAX: usize = 60;

/// Shortest id prefix `POST /api/lookup` will resolve (answers cite notes as
/// `note:7d75fff5`).
const LOOKUP_PREFIX_MIN: usize = 8;

/// A lowercase hex-and-dash id, or a prefix of one long enough to resolve.
pub fn lookup_key(raw: &str) -> Option<String> {
    let id = raw.trim().to_lowercase();
    let ok = id.len() >= LOOKUP_PREFIX_MIN
        && id.len() <= 36
        && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-');
    ok.then_some(id)
}

/// The one node whose id starts with `prefix`, if exactly one does.
async fn lookup_prefix(neo4j: &Neo4jClient, prefix: &str) -> Option<Value> {
    let rows = neo4j
        .execute(
            neo4rs::query(
                "CALL { \
                   MATCH (n:Note) WHERE n.id STARTS WITH $p RETURN 'note' AS kind, n.id AS id, left(n.content, 120) AS label LIMIT 2 \
                   UNION MATCH (n:Entity) WHERE n.id STARTS WITH $p RETURN 'entity' AS kind, n.id AS id, n.name AS label LIMIT 2 \
                   UNION MATCH (n:Task) WHERE n.id STARTS WITH $p RETURN 'task' AS kind, n.id AS id, n.goal AS label LIMIT 2 \
                   UNION MATCH (n:Workflow) WHERE n.id STARTS WITH $p RETURN 'schedule' AS kind, n.id AS id, n.name AS label LIMIT 2 \
                   UNION MATCH (n:Todo) WHERE n.id STARTS WITH $p RETURN 'todo' AS kind, n.id AS id, n.title AS label LIMIT 2 \
                 } RETURN kind, id, label LIMIT 2",
            )
            .param("p", prefix),
        )
        .await
        .ok()?;
    if rows.len() != 1 {
        return None;
    }
    let r = &rows[0];
    Some(json!({
        "kind": r.get::<String>("kind").ok(),
        "id": r.get::<String>("id").ok(),
        "label": r.get::<String>("label").ok(),
        "prefix": prefix,
    }))
}

/// POST /api/lookup `{ids: [...]}` — what many ids name, in one call, for
/// the chat context dock. A full id matches exactly; a hex prefix of 8 or more
/// characters matches when exactly one node has it. Returns `found` (kind, id,
/// label, and `prefix` when matched by one) and `missing`, so a citation that
/// resolves to nothing is shown as such.
pub async fn handle_lookup_many(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let neo4j = graph!(state);
    let mut seen = std::collections::HashSet::new();
    let ids: Vec<String> = body["ids"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|v| v.as_str().and_then(lookup_key))
        .filter(|id| seen.insert(id.clone()))
        .collect();
    let truncated = ids.len() > LOOKUP_MANY_MAX;
    let mut found = Vec::new();
    let mut missing = Vec::new();
    for id in ids.into_iter().take(LOOKUP_MANY_MAX) {
        let hit = if id.len() == 36 {
            lookup_id(neo4j, &id).await
        } else {
            lookup_prefix(neo4j, &id).await
        };
        match hit {
            Some(v) => found.push(v),
            None => missing.push(id),
        }
    }
    Json(json!({ "found": found, "missing": missing, "truncated": truncated, "max": LOOKUP_MANY_MAX }))
        .into_response()
}

/// GET /api/entities/{id} — an entity, the notes that mention it, and the
/// entities mentioned alongside it.
pub async fn handle_entity(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let neo4j = graph!(state);
    let rows = or_500!(
        neo4j
            .execute(
                neo4rs::query(
                    "MATCH (e:Entity {id: $id}) \
                 OPTIONAL MATCH (e)<-[m:MENTIONS]-() \
                 WITH e, count(m) AS mentions \
                 OPTIONAL MATCH (w:WikiItem {qid: e.type_question_qid}) \
                 RETURN e.name AS name, e.entity_type AS entity_type, \
                        toString(e.created_at) AS created_at, mentions, \
                        e.type_source AS type_source, coalesce(e.distinct, false) AS distinct, \
                        e.kind AS kind, e.kind_source AS kind_source, \
                        w.qid AS q_qid, w.label AS q_label, w.description AS q_description, \
                        w.kind AS q_kind, w.kind_type AS q_kind_type",
                )
                .param("id", id.as_str()),
            )
            .await
    );
    let Some(row) = rows.first() else {
        // An entity merged away keeps its id on the alias: say where it went.
        let merged = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (a:EntityAlias {merged_from_id: $id})-[:ALIAS_OF]->(t:Entity) \
                     RETURN a.name AS alias, t.id AS id, t.name AS name LIMIT 1",
                )
                .param("id", id.as_str()),
            )
            .await
            .unwrap_or_default();
        return match merged.first() {
            Some(m) => Json(json!({
                "merged_into": {
                    "alias": m.get::<String>("alias").ok(),
                    "id": m.get::<String>("id").ok(),
                    "name": m.get::<String>("name").ok(),
                }
            }))
            .into_response(),
            None => not_found(),
        };
    };

    let aliases: Vec<Value> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (a:EntityAlias)-[:ALIAS_OF]->(e:Entity {id: $id}) \
                 RETURN a.name AS name, a.method AS method, toString(a.merged_at) AS merged_at, \
                        coalesce(a.case_sensitive, false) AS case_sensitive, \
                        COUNT { (:Note)-[r:MENTIONS]->(e) WHERE r.via = a.name \
                                OR any(x IN coalesce(r.absorbed, []) WHERE x STARTS WITH a.merged_from_id + ':') } AS mentions \
                 ORDER BY mentions DESC, name",
            )
            .param("id", id.as_str()),
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "name": r.get::<String>("name").ok(),
                "method": r.get::<String>("method").ok(),
                "merged_at": r.get::<String>("merged_at").ok(),
                "case_sensitive": r.get::<bool>("case_sensitive").unwrap_or(false),
                "mentions": r.get::<i64>("mentions").unwrap_or(0),
            })
        })
        .collect();

    let note_ids: Vec<String> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note)-[:MENTIONS]->(:Entity {id: $id}) \
                 RETURN n.id AS id ORDER BY n.created_at DESC LIMIT 40",
            )
            .param("id", id.as_str()),
        )
        .await
        .unwrap_or_default()
        .iter()
        .filter_map(|r| r.get::<String>("id").ok())
        .collect();
    let meta = note_meta(neo4j, &note_ids).await;
    let notes: Vec<Value> = note_ids
        .iter()
        .filter_map(|i| meta.get(i).cloned())
        .collect();

    // The topics its notes put it in (how many of each topic's notes mention
    // it), and the ones the user keeps it out of (HBI plan Phase 15).
    let topics: Vec<Value> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (e:Entity {id: $id}), (t:Topic) \
                 WITH e, t, COUNT { (t)<-[:IN_TOPIC]-(:Note)-[:MENTIONS]->(e) } AS notes, \
                      EXISTS { (e)-[:NOT_IN_TOPIC]->(t) } AS hidden \
                 WHERE notes > 0 OR hidden \
                 RETURN t.name AS name, t.label AS label, notes, hidden ORDER BY notes DESC",
            )
            .param("id", id.as_str()),
        )
        .await
        .unwrap_or_default()
        .iter()
        .map(|r| {
            json!({
                "name": r.get::<String>("name").ok(),
                "label": r.get::<String>("label").ok(),
                "notes": r.get::<i64>("notes").unwrap_or(0),
                "hidden": r.get::<bool>("hidden").unwrap_or(false),
            })
        })
        .collect();

    let mentions = row.get::<i64>("mentions").unwrap_or(0);
    Json(json!({
        "topics": topics,
        "entity": {
            "id": id,
            "name": row.get::<String>("name").ok(),
            "entity_type": row.get::<String>("entity_type").ok(),
            "created_at": row.get::<String>("created_at").ok(),
            "mentions": mentions,
            "type_source": row.get::<String>("type_source").ok(),
            "distinct": row.get::<bool>("distinct").unwrap_or(false),
            // HBI plan Phase 19: the fine kind, where it came from, and an
            // open question when the reference disagrees with the type.
            "kind": row.get::<String>("kind").ok(),
            "kind_source": row.get::<String>("kind_source").ok(),
            "type_question": row.get::<String>("q_qid").ok().map(|qid| json!({
                "qid": qid,
                "label": row.get::<String>("q_label").ok(),
                "description": row.get::<String>("q_description").ok(),
                "kind": row.get::<String>("q_kind").ok(),
                "kind_type": row.get::<String>("q_kind_type").ok(),
            })),
        },
        "aliases": aliases,
        "notes": notes,
        "notes_shown": notes.len(),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_keys_are_ids_or_long_enough_prefixes() {
        assert_eq!(lookup_key(" 7D75FFF5 ").as_deref(), Some("7d75fff5"));
        assert!(
            lookup_key("7d75fff").is_none(),
            "7 characters is too short to resolve"
        );
        assert!(lookup_key("not-an-id").is_none());
        let full = "7d75fff5-1111-2222-3333-444455556666";
        assert_eq!(lookup_key(full).as_deref(), Some(full));
        assert!(lookup_key(&format!("{full}0")).is_none());
    }

    #[test]
    fn auto_mode_reads_the_query_shape() {
        assert_eq!(
            resolve_mode("867f7a9d-3b11-4158-ba12-b6f167c0f49a", None),
            Mode::Id
        );
        assert_eq!(
            resolve_mode("  867F7A9D-3B11-4158-BA12-B6F167C0F49A ", Some("auto")),
            Mode::Id
        );
        assert_eq!(resolve_mode("\"sodium batteries\"", None), Mode::Keyword);
        assert_eq!(
            resolve_mode("what do we know about sodium batteries", None),
            Mode::Semantic
        );
        assert_eq!(resolve_mode("\"", None), Mode::Semantic);
    }

    #[test]
    fn an_explicit_mode_wins() {
        assert_eq!(
            resolve_mode("867f7a9d-3b11-4158-ba12-b6f167c0f49a", Some("keyword")),
            Mode::Keyword
        );
        assert_eq!(resolve_mode("anything", Some("id")), Mode::Id);
    }

    #[test]
    fn keyword_requires_every_word_and_escapes() {
        assert_eq!(
            keyword_query("sodium batteries").as_deref(),
            Some("+sodium +batteries")
        );
        assert_eq!(
            keyword_query("site:apnews.com").as_deref(),
            Some("+site\\:apnews.com")
        );
    }

    #[test]
    fn a_quoted_query_is_one_phrase() {
        assert_eq!(
            keyword_query("\"strait of hormuz\"").as_deref(),
            Some("\"strait of hormuz\"")
        );
        assert_eq!(keyword_query("\"a:b\"").as_deref(), Some("\"a\\:b\""));
        assert_eq!(keyword_query("\"  \""), None);
        assert_eq!(keyword_query("   "), None);
    }

    #[test]
    fn uuid_shape_is_strict() {
        assert!(looks_like_uuid("867f7a9d-3b11-4158-ba12-b6f167c0f49a"));
        assert!(!looks_like_uuid("867f7a9d-3b11-4158-ba12-b6f167c0f49"));
        assert!(!looks_like_uuid("867f7a9d_3b11_4158_ba12_b6f167c0f49a"));
    }
}
