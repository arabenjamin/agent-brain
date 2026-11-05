//! `/api/todos`.

use super::{RestState, bad_request, graph, internal, not_found};
use axum::{
    Extension, Json,
    extract::{Path, Query},
    http::StatusCode,
    response::IntoResponse,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

/// The spawnable roster, for checking an item's assignee.
async fn item_roster(state: &RestState) -> Vec<String> {
    let cb = match state.context_builder.as_ref() {
        Some(slot) => slot.read().await.clone(),
        None => None,
    };
    agent_brain_scheduler::items::roster(cb.as_deref()).await
}

/// Check a todo body's status and item fields. REST is the user's path, so
/// it may assign an agent.
async fn check_todo_body(
    state: &RestState,
    body: &Value,
) -> Result<agent_brain_repository::TodoItemPatch, String> {
    let patch = agent_brain_scheduler::items::patch_from_json(body);
    let roster = item_roster(state).await;
    agent_brain_scheduler::items::validate(
        body.get("status").and_then(Value::as_str),
        &patch,
        &roster,
        false,
    )?;
    if let Some(neo4j) = state.neo4j.as_ref() {
        agent_brain_scheduler::items::check_program(neo4j, &patch).await?;
    }
    Ok(patch)
}

/// GET /todos[?status=pending|in_progress|done]
pub async fn handle_list_todos(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let neo4j = graph!(state, "Todo storage not available");

    let status_filter = params.get("status").map(String::as_str);
    let program = params.get("program").filter(|p| !p.is_empty());
    match neo4j.list_todos(status_filter).await {
        Ok(todos) => {
            let items: Vec<Value> = todos
                .iter()
                .filter(|t| program.is_none_or(|p| t.program.as_ref() == Some(p)))
                .map(agent_brain_scheduler::items::item_json)
                .collect();
            Json(serde_json::json!({"todos": items})).into_response()
        }
        Err(e) => internal(e),
    }
}

/// POST /todos
pub async fn handle_create_todo(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let neo4j = graph!(state, "Todo storage not available");

    let Some(title) = body.get("title").and_then(|v| v.as_str()) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "title is required"})),
        )
            .into_response();
    };
    let title = title.to_string();
    let mut patch = match check_todo_body(&state, &body).await {
        Ok(p) => p,
        Err(e) => return bad_request(format!("Item rejected — nothing was saved: {e}")),
    };
    // An item the HBI creates is the user's unless it says otherwise.
    if patch.origin_kind.is_none() {
        patch.origin_kind = Some(Some("user".to_string()));
    }

    let description = body
        .get("description")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let status = body
        .get("status")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let priority = body.get("priority").and_then(serde_json::Value::as_i64);
    let tags: Vec<String> = body
        .get("tags")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
    let tags = match crate::refs::check_tags(&state, &tags, &[]).await {
        Ok(t) => t,
        Err(e) => return bad_request(format!("Item rejected — nothing was saved: {e}")),
    };
    let due_at = body
        .get("due_at")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    match neo4j
        .create_todo(
            &title,
            description.as_deref(),
            status.as_deref(),
            priority,
            Some(&tags),
            due_at.as_deref(),
        )
        .await
    {
        Ok(todo) => match neo4j.patch_todo_item(&todo.id, &patch).await {
            Ok(Some(todo))
                if crate::refs::sync_links(neo4j, &todo.id, &todo.tags)
                    .await
                    .is_err() =>
            {
                internal("created, but its tag links were not saved")
            }
            Ok(Some(todo)) => (
                StatusCode::CREATED,
                Json(agent_brain_scheduler::items::item_json(&todo)),
            )
                .into_response(),
            Ok(None) => not_found(),
            Err(e) => internal(format!("created, but its item fields were not saved: {e}")),
        },
        Err(e) => internal(e),
    }
}

/// GET /todos/:id
pub async fn handle_get_todo(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let neo4j = graph!(state, "Todo storage not available");

    match neo4j.get_todo(&id).await {
        Ok(Some(todo)) => Json(agent_brain_scheduler::items::item_json(&todo)).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// PUT /todos/:id
pub async fn handle_update_todo(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let neo4j = graph!(state, "Todo storage not available");

    let patch = match check_todo_body(&state, &body).await {
        Ok(p) => p,
        Err(e) => return bad_request(format!("Item rejected — nothing was saved: {e}")),
    };
    let title = body
        .get("title")
        .and_then(|v| v.as_str())
        .map(str::to_string);

    // `None` key = not in body (leave unchanged); `Some(null)` = clear the field
    let description: Option<Option<String>> = if body
        .as_object()
        .is_some_and(|o| o.contains_key("description"))
    {
        Some(
            body.get("description")
                .and_then(|v| v.as_str())
                .map(str::to_string),
        )
    } else {
        None
    };

    let status = body
        .get("status")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    let priority = body.get("priority").and_then(serde_json::Value::as_i64);

    let tags: Option<Vec<String>> = if body.as_object().is_some_and(|o| o.contains_key("tags")) {
        Some(
            body.get("tags")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default(),
        )
    } else {
        None
    };
    let tags = match tags {
        Some(new) => {
            let old = match neo4j.get_todo(&id).await {
                Ok(Some(t)) => t.tags,
                Ok(None) => return not_found(),
                Err(e) => return internal(e),
            };
            match crate::refs::check_tags(&state, &new, &old).await {
                Ok(t) => Some(t),
                Err(e) => return bad_request(format!("Item rejected — nothing was saved: {e}")),
            }
        }
        None => None,
    };

    let due_at: Option<Option<String>> =
        if body.as_object().is_some_and(|o| o.contains_key("due_at")) {
            Some(
                body.get("due_at")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
            )
        } else {
            None
        };

    let description_ref = description.as_ref().map(|d| d.as_deref());
    let due_at_ref = due_at.as_ref().map(|d| d.as_deref());

    match neo4j
        .update_todo(
            &id,
            title.as_deref(),
            description_ref,
            status.as_deref(),
            priority,
            tags.as_deref(),
            due_at_ref,
        )
        .await
    {
        Ok(Some(_)) => match neo4j.patch_todo_item(&id, &patch).await {
            Ok(Some(todo))
                if tags.is_some()
                    && crate::refs::sync_links(neo4j, &todo.id, &todo.tags)
                        .await
                        .is_err() =>
            {
                internal("saved, but its tag links were not")
            }
            Ok(Some(todo)) => Json(agent_brain_scheduler::items::item_json(&todo)).into_response(),
            Ok(None) => not_found(),
            Err(e) => internal(format!("saved, but its item fields were not: {e}")),
        },
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// DELETE /todos/:id
pub async fn handle_delete_todo(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let neo4j = graph!(state, "Todo storage not available");

    match neo4j.delete_todo(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}
