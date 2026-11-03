//! `/api/notes`.

use super::{
    RestState, bad_request, graph, internal, not_found, parse_instant, parse_since, unavailable,
    where_clause,
};
use axum::{
    Extension, Json,
    extract::{Path, Query},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// GET /api/notes?limit=<n>&note_type=<type>&q=<search>
///
/// List notes from Neo4j. Pass `q` for text search; `note_type` to filter by type.
pub async fn handle_list_notes(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let neo4j = graph!(state);

    let limit: i64 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50)
        .min(500);

    let note_type_filter = params.get("note_type").cloned().filter(|s| !s.is_empty());
    let search_q = params.get("q").cloned().filter(|s| !s.trim().is_empty());
    let since = match parse_since(&params) {
        Ok(s) => s,
        Err(e) => return bad_request(e),
    };
    // Claims checked in a window (what /api/activity's `claims_checked` counts).
    let verified_since = match parse_instant(&params, "verified_since") {
        Ok(s) => s,
        Err(e) => return bad_request(e),
    };

    let mut conds = Vec::new();
    if note_type_filter.is_some() {
        conds.push("n.note_type = $nt");
    }
    if search_q.is_some() {
        conds.push("toLower(n.content) CONTAINS toLower($q)");
    }
    if since.is_some() {
        conds.push("n.created_at >= datetime($since)");
    }
    if verified_since.is_some() {
        conds.push("n.verified_at >= datetime($verified_since)");
    }
    let cypher = format!(
        "MATCH (n:Note) {}\
         RETURN n.id AS id, n.content AS content, n.note_type AS note_type, \
                n.access_count AS access_count, toString(n.created_at) AS created_at \
         ORDER BY n.created_at DESC LIMIT $limit",
        where_clause(&conds)
    );

    let mut q = neo4rs::query(&cypher).param("limit", limit);
    if let Some(ref nt) = note_type_filter {
        q = q.param("nt", nt.as_str());
    }
    if let Some(ref sq) = search_q {
        q = q.param("q", sq.as_str());
    }
    if let Some(ref s) = since {
        q = q.param("since", s.as_str());
    }
    if let Some(ref s) = verified_since {
        q = q.param("verified_since", s.as_str());
    }

    match neo4j.execute(q).await {
        Ok(rows) => {
            let notes: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "id":           row.get::<String>("id").unwrap_or_default(),
                        "content":      row.get::<String>("content").unwrap_or_default(),
                        "note_type":    row.get::<String>("note_type").ok().filter(|s| !s.is_empty()),
                        "access_count": row.get::<i64>("access_count").unwrap_or(0),
                        "created_at":   row.get::<String>("created_at").unwrap_or_default(),
                    })
                })
                .collect();
            Json(json!({ "count": notes.len(), "notes": notes })).into_response()
        }
        Err(e) => internal(format!("Failed to list notes: {e}")),
    }
}

/// POST /api/notes — store a note written in the HBI.
///
/// Goes through `KnowledgeService::store_note`, like the `store_note` tool:
/// embedded, chunked, linked to similar notes and scanned for entities. The
/// old raw `CREATE` skipped all of that, so a note written in the UI was
/// invisible to semantic search and had no relationships. The raw path is
/// kept only for a brain started without a knowledge service, and the
/// response says what it skipped.
pub async fn handle_create_note(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let neo4j = graph!(state);

    let Some(content) = body
        .get("content")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty())
    else {
        return bad_request("content is required");
    };
    let note_type = body
        .get("note_type")
        .and_then(|v| v.as_str())
        .unwrap_or("semantic");

    if let Some(ks) = state.knowledge.as_ref() {
        return match ks
            .store_note(
                content,
                Some(note_type),
                Some("hbi"),
                None,
                Some(agent_brain_models::provenance::ProvenanceFlag::UserInput),
            )
            .await
        {
            Ok((note_id, links_created)) => (
                StatusCode::CREATED,
                Json(json!({"note_id": note_id, "id": note_id, "links_created": links_created})),
            )
                .into_response(),
            Err(e) => internal(format!("Failed to store note: {e}")),
        };
    }

    let note_id = uuid::Uuid::new_v4().to_string();
    let ts = chrono::Utc::now().to_rfc3339();
    let q = neo4rs::query(
        "CREATE (n:Note {id: $id, content: $content, note_type: $nt, \
         access_count: 0, review_interval_days: 1.0, source_context: 'hbi', provenance: 'user_input', \
         created_at: datetime($ts), last_accessed_at: datetime($ts), \
         next_review_at: datetime($ts)}) \
         RETURN n.id AS id",
    )
    .param("id", note_id.as_str())
    .param("content", content)
    .param("nt", note_type)
    .param("ts", ts.as_str());

    match neo4j.execute(q).await {
        Ok(_) => (
            StatusCode::CREATED,
            Json(json!({
                "note_id": note_id,
                "id": note_id,
                "warning": "Stored without embedding, links or entities: this brain has no knowledge service",
            })),
        )
            .into_response(),
        Err(e) => internal(format!("Failed to create note: {e}")),
    }
}

/// PUT /api/notes/:id — replace a note's content.
///
/// Uses `rewrite_note_in_place`: the id, creation time and inbound edges
/// (consolidations, inferences built on it) survive, and everything derived
/// from the old text — embedding, similarity links, chunks, entity mentions —
/// is rebuilt. A bare `SET content` left all of those describing the old text.
pub async fn handle_update_note(
    Extension(state): Extension<Arc<RestState>>,
    Path(note_id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let neo4j = graph!(state);

    let Some(content) = body
        .get("content")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|c| !c.is_empty())
    else {
        return bad_request("content is required");
    };

    let current = neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note {id: $id}) RETURN n.note_type AS note_type, n.source_context AS source_context",
            )
            .param("id", note_id.as_str()),
        )
        .await;
    let row = match current {
        Ok(rows) => match rows.into_iter().next() {
            Some(r) => r,
            None => return not_found(),
        },
        Err(e) => return internal(format!("Failed to read note: {e}")),
    };
    let note_type = row.get::<String>("note_type").ok();
    let source_context = row.get::<String>("source_context").ok();

    let Some(ks) = state.knowledge.as_ref() else {
        return unavailable(
            "Editing needs the knowledge service (to re-embed and re-chunk), which this brain was started without",
        );
    };
    match ks
        .rewrite_note_in_place(
            &note_id,
            content,
            note_type.as_deref(),
            source_context.as_deref(),
        )
        .await
    {
        Ok((removed, created)) => Json(json!({
            "id": note_id,
            "note_type": note_type,
            "chunks_removed": removed,
            "chunks_created": created,
        }))
        .into_response(),
        Err(e) => internal(format!("Failed to update note: {e}")),
    }
}

/// DELETE /api/notes/:id — delete a note and all its relationships
pub async fn handle_delete_note(
    Extension(state): Extension<Arc<RestState>>,
    Path(note_id): Path<String>,
) -> impl IntoResponse {
    let neo4j = graph!(state);

    // Check existence first so we can return 404 accurately.
    let check =
        neo4rs::query("MATCH (n:Note {id: $id}) RETURN n.id AS id").param("id", note_id.as_str());
    match neo4j.execute(check).await {
        Ok(rows) if rows.is_empty() => return not_found(),
        Err(e) => return internal(format!("Failed to verify note: {e}")),
        Ok(_) => {}
    }

    let del =
        neo4rs::query("MATCH (n:Note {id: $id}) DETACH DELETE n").param("id", note_id.as_str());
    match neo4j.run(del).await {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(e) => internal(format!("Failed to delete note: {e}")),
    }
}

/// GET /api/notes/:id
///
/// Fetch a single note by ID for the Graph panel node detail view.
pub async fn handle_get_note(
    Extension(state): Extension<Arc<RestState>>,
    Path(note_id): Path<String>,
) -> impl IntoResponse {
    let neo4j = graph!(state);

    // Everything the HBI's note drawer shows: provenance and epistemic status
    // with the content, the note's place among chunks, and what it mentions.
    let q = neo4rs::query(
        "MATCH (n:Note {id: $id}) \
         OPTIONAL MATCH (n)-[:PART_OF]->(parent:Note) \
         OPTIONAL MATCH (chunk:Note)-[:PART_OF]->(n) \
         WITH n, parent, count(chunk) AS chunks \
         OPTIONAL MATCH (n)-[:MENTIONS]->(e:Entity) WHERE NOT e.entity_type IN ['url', 'date'] \
         WITH n, parent, chunks, e ORDER BY e.name \
         WITH n, parent, chunks, collect({id: e.id, name: e.name, entity_type: e.entity_type})[0..30] AS entities \
         RETURN n.id AS id, n.content AS content, n.note_type AS note_type, \
                n.access_count AS access_count, toString(n.created_at) AS created_at, \
                toString(n.last_accessed_at) AS last_accessed_at, \
                n.source_context AS source_context, n.provenance AS provenance, \
                n.claim_status AS claim_status, n.claim_kind AS claim_kind, \
                n.corroboration_tier AS corroboration_tier, n.asserted_by AS asserted_by, \
                toString(n.verified_at) AS verified_at, n.verification AS verification, \
                n.verification_reason AS verification_reason, \
                n.corroborating_count AS corroborating_count, n.contradicting_count AS contradicting_count, \
                n.ambiguous AS ambiguous, n.ambiguous_reason AS ambiguous_reason, \
                n.content_v1 AS content_v1, toString(n.rewritten_at) AS rewritten_at, \
                coalesce(n.about, []) AS about, \
                size(coalesce(n.embedding, [])) > 0 AS embedded, \
                [(n)-[r:IN_TOPIC]->(t:Topic) | {name: t.name, rank: r.rank}] AS topics, \
                coalesce(n.topics_version, '') AS topics_version, \
                coalesce(n.topics_locked, false) AS topics_locked, \
                [(n)-[:ASSERTED_IN]->(s:Note) | {id: s.id, note_type: s.note_type, \
                  source_context: s.source_context, created_at: toString(s.created_at), \
                  content: s.content}][0..5] AS sources, \
                parent.id AS parent_id, chunks, entities",
    )
    .param("id", note_id.clone());

    match neo4j.execute(q).await {
        Ok(rows) => {
            if let Some(row) = rows.into_iter().next() {
                let s = |k: &str| row.get::<String>(k).ok().filter(|v| !v.is_empty());
                let entities: Vec<Value> = row
                    .get::<Vec<neo4rs::BoltMap>>("entities")
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|m| {
                        let id = m.get::<String>("id").ok()?;
                        Some(json!({
                            "id": id,
                            "name": m.get::<String>("name").ok(),
                            "entity_type": m.get::<String>("entity_type").ok(),
                        }))
                    })
                    .collect();
                // A claim's source notes, read for the article or video they record.
                let asserted_in: Vec<Value> = row
                    .get::<Vec<neo4rs::BoltMap>>("sources")
                    .unwrap_or_default()
                    .into_iter()
                    .filter_map(|m| {
                        let id = m.get::<String>("id").ok()?;
                        let r = agent_brain_plugin_api::notes::source_reference(
                            &m.get::<String>("content").unwrap_or_default(),
                        );
                        Some(json!({
                            "id": id,
                            "note_type": m.get::<String>("note_type").ok(),
                            "source_context": m.get::<String>("source_context").ok(),
                            "created_at": m.get::<String>("created_at").ok(),
                            "title": r.title,
                            "url": r.url,
                            "outlet": r.outlet,
                        }))
                    })
                    .collect();
                Json(json!({
                    "id":                  row.get::<String>("id").unwrap_or_default(),
                    "content":             row.get::<String>("content").unwrap_or_default(),
                    "asserted_in":         asserted_in,
                    "topics":              crate::search::topic_names(row.get::<Vec<neo4rs::BoltMap>>("topics").unwrap_or_default()),
                    // false: not classified yet for the current topic definitions.
                    "topics_assessed":     row.get::<String>("topics_version").unwrap_or_default() == agent_brain_graph::topics::Topics::global().version,
                    // true: the user set its topics, and the classifier leaves it alone.
                    "topics_locked":       row.get::<bool>("topics_locked").unwrap_or(false),
                    "note_type":           s("note_type"),
                    "access_count":        row.get::<i64>("access_count").unwrap_or(0),
                    "created_at":          row.get::<String>("created_at").unwrap_or_default(),
                    "last_accessed_at":    s("last_accessed_at"),
                    "source_context":      s("source_context"),
                    "provenance":          s("provenance"),
                    "claim_status":        s("claim_status"),
                    "claim_kind":          s("claim_kind"),
                    "corroboration_tier":  s("corroboration_tier"),
                    "asserted_by":         s("asserted_by"),
                    "verified_at":         s("verified_at"),
                    "verification":        s("verification"),
                    "verification_reason": s("verification_reason"),
                    "corroborating_count": row.get::<i64>("corroborating_count").ok(),
                    "contradicting_count": row.get::<i64>("contradicting_count").ok(),
                    // A claim that could not be made to stand alone, and why;
                    // and, for a rewritten one, the text it replaced.
                    "ambiguous":           row.get::<bool>("ambiguous").unwrap_or(false),
                    "ambiguous_reason":    s("ambiguous_reason"),
                    "content_v1":          s("content_v1"),
                    "rewritten_at":        s("rewritten_at"),
                    "about":               row.get::<Vec<String>>("about").unwrap_or_default(),
                    "embedded":            row.get::<bool>("embedded").unwrap_or(false),
                    "parent_id":           s("parent_id"),
                    "chunks":              row.get::<i64>("chunks").unwrap_or(0),
                    "entities":            entities,
                }))
                .into_response()
            } else {
                not_found()
            }
        }
        Err(e) => internal(format!("Failed to fetch note: {e}")),
    }
}

/// GET /api/notes/:id/related
///
/// Return notes connected via RELATES_TO edges for the Knowledge panel.
pub async fn handle_get_related_notes(
    Extension(state): Extension<Arc<RestState>>,
    Path(note_id): Path<String>,
) -> impl IntoResponse {
    let neo4j = graph!(state);

    let q = neo4rs::query(
        "MATCH (n:Note {id: $id})-[r:RELATES_TO]->(m:Note) \
         RETURN m.id AS id, m.content AS content, m.note_type AS note_type, \
                r.similarity AS similarity \
         ORDER BY r.similarity DESC LIMIT 10",
    )
    .param("id", note_id.as_str());

    match neo4j.execute(q).await {
        Ok(rows) => {
            let related: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "id":         row.get::<String>("id").unwrap_or_default(),
                        "content":    row.get::<String>("content").unwrap_or_default(),
                        "note_type":  row.get::<String>("note_type").ok().filter(|s| !s.is_empty()),
                        "similarity": row.get::<f64>("similarity").unwrap_or(0.0),
                    })
                })
                .collect();
            Json(json!({ "count": related.len(), "related_notes": related })).into_response()
        }
        Err(e) => internal(format!("Failed to fetch related notes: {e}")),
    }
}
