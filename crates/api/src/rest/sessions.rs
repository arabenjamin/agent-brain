//! Chat sessions and their feedback.

use super::{RestState, bad_request, graph, internal};
use agent_brain_work::WorkingMemoryStore;
use axum::{
    Extension, Json,
    extract::{Path, Query},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;

/// GET /api/sessions?limit=<n> — list working-memory sessions with metadata.
pub async fn handle_list_sessions(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let neo4j = graph!(state);

    let limit: i64 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(50);

    match neo4j.list_sessions(limit).await {
        Ok(sessions) => {
            Json(json!({ "count": sessions.len(), "sessions": sessions })).into_response()
        }
        Err(e) => internal(format!("Failed to list sessions: {e}")),
    }
}

/// GET /api/sessions/:id/entries?limit=<n> — get working-memory entries for a session.
pub async fn handle_get_session_entries(
    Extension(state): Extension<Arc<RestState>>,
    Path(session_id): Path<String>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let neo4j = graph!(state);

    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);

    match neo4j.get_entries(&session_id, limit).await {
        Ok(entries) => Json(json!({
            "session_id": session_id,
            "count": entries.len(),
            "entries": entries,
        }))
        .into_response(),
        Err(e) => internal(format!("Failed to fetch session entries: {e}")),
    }
}

/// POST /api/sessions/:id/archive — mark a session archived (hidden from list, preserved for training).
pub async fn handle_archive_session(
    Extension(state): Extension<Arc<RestState>>,
    Path(session_id): Path<String>,
) -> impl IntoResponse {
    let neo4j = graph!(state);
    match neo4j.archive_session(&session_id).await {
        Ok(()) => Json(json!({ "status": "archived", "session_id": session_id })).into_response(),
        Err(e) => internal(format!("Failed to archive session: {e}")),
    }
}

/// POST /api/sessions/:id/feedback — the owner's thumbs on one answer.
///
/// Body `{question, score, note?}`: `score` is 1 (up), -1 (down) or 0 (clear).
/// The answer is the first assistant entry after the newest user entry with
/// that exact text in the session; it carries the score in working memory,
/// and a `feedback:` row goes to DuckDB `interactions` for route mining
/// (GRAPH_CONTEXT_PLAN Phase B: a thumbs-down marks a route or behavior that
/// answered wrong).
pub async fn handle_session_feedback(
    Extension(state): Extension<Arc<RestState>>,
    Path(session_id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let neo4j = graph!(state);
    let Some(question) = body["question"].as_str().filter(|q| !q.trim().is_empty()) else {
        return bad_request("question is required: the user message the answer replied to");
    };
    let score = match body["score"].as_i64() {
        Some(s @ (-1..=1)) => s,
        _ => return bad_request("score must be 1 (up), -1 (down) or 0 (clear)"),
    };
    let note = body["note"]
        .as_str()
        .map(str::trim)
        .unwrap_or("")
        .to_string();
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (u:WorkingMemory {session_id: $s, role: 'user'}) WHERE u.content = $q \
                 WITH u ORDER BY u.turn_index DESC LIMIT 1 \
                 MATCH (a:WorkingMemory {session_id: $s, role: 'assistant'}) WHERE a.turn_index > u.turn_index \
                 WITH a ORDER BY a.turn_index ASC LIMIT 1 \
                 SET a.feedback_score = CASE WHEN $score = 0 THEN null ELSE $score END, \
                     a.feedback_note = CASE WHEN $note = '' THEN null ELSE $note END, \
                     a.feedback_at = datetime() \
                 RETURN a.turn_index AS turn",
            )
            .param("s", session_id.as_str())
            .param("q", question)
            .param("score", score)
            .param("note", note.as_str()),
        )
        .await;
    let turn = match rows {
        Ok(r) => match r.first().and_then(|r| r.get::<i64>("turn").ok()) {
            Some(t) => t,
            None => {
                return (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "no answer to that question in this session (is the turn still running?)"})),
                )
                    .into_response();
            }
        },
        Err(e) => return internal(format!("Failed to record feedback: {e}")),
    };
    if let Some(db) = &state.telemetry {
        let _ = db.log_interaction(
            &format!(
                "feedback: {}",
                json!({"message": question, "session_id": session_id})
            ),
            &json!({"score": score, "note": note, "turn": turn}).to_string(),
            None,
            true,
            0,
            "owner",
        );
    }
    Json(json!({"session_id": session_id, "turn": turn, "score": score})).into_response()
}
