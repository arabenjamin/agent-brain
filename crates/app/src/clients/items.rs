//! The user's side of agent work (HBI plan Phase 13): hand an item to an
//! agent, approve a run or a result, send a result back with a comment.
//! These are REST only: each is the user's decision, and the brain's own
//! work may not assign an agent or approve a run (`services/items.rs`).

use std::sync::Arc;

use axum::{Extension, Json, extract::Path, response::IntoResponse};
use serde_json::{Value, json};

use super::rest::{RestState, bad_request, internal, not_found, or_500, unavailable};
use crate::services::item_dispatch::{Dispatcher, MAX_RUNS, Outcome, post_to_thread};
use crate::services::items;

async fn dispatcher(state: &RestState) -> Result<Dispatcher, Box<axum::response::Response>> {
    let neo4j = state
        .neo4j
        .as_ref()
        .ok_or_else(|| unavailable("Neo4j not available"))?;
    let scheduler = match state.scheduler.as_ref() {
        Some(s) => s.read().await.clone(),
        None => None,
    };
    let Some(scheduler) = scheduler else {
        return Err(Box::new(unavailable(
            "The scheduler (and its job queue) is not running",
        )));
    };
    let cb = match state.context_builder.as_ref() {
        Some(slot) => slot.read().await.clone(),
        None => None,
    };
    Ok(Dispatcher::new((**neo4j).clone(), scheduler.queue(), cb))
}

async fn item_or_404(
    state: &RestState,
    id: &str,
) -> Result<crate::repository::Todo, Box<axum::response::Response>> {
    let neo4j = state
        .neo4j
        .as_ref()
        .ok_or_else(|| Box::new(unavailable("Neo4j not available")))?;
    match neo4j.get_todo(id).await {
        Ok(Some(t)) => Ok(t),
        Ok(None) => Err(Box::new(not_found())),
        Err(e) => Err(Box::new(internal(e))),
    }
}

async fn respond(state: &RestState, id: &str, outcome: Value) -> axum::response::Response {
    let item = match state.neo4j.as_ref() {
        Some(n) => n
            .get_todo(id)
            .await
            .ok()
            .flatten()
            .map(|t| items::item_json(&t)),
        None => None,
    };
    Json(json!({ "item": item, "run": outcome })).into_response()
}

/// POST /api/items/{id}/assign — `{assignee, prompt?, model?}`. An agent
/// assignee is offered a run when the item is due (today, if it has no due
/// date): it starts, or waits for approval when the agent asks first; a later
/// due date leaves it for the hourly dispatch. `user` takes it back.
pub async fn handle_assign(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let t = match item_or_404(&state, &id).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    if matches!(t.status.as_str(), "in_progress" | "done") {
        return bad_request(format!(
            "The item is {}; it can't be reassigned now",
            t.status
        ));
    }
    let Some(assignee) = body
        .get("assignee")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|a| !a.is_empty())
    else {
        return bad_request("assignee is required ('user' or 'agent:<name>')");
    };
    let mut patch = items::patch_from_json(&json!({
        "assignee": assignee,
        "prompt": body.get("prompt").cloned().unwrap_or(Value::Null),
        "model": body.get("model").cloned().unwrap_or(Value::Null),
    }));
    if body.get("prompt").is_none() {
        patch.prompt = None;
    }
    if body.get("model").is_none() {
        patch.model = None;
    }
    let cb = match state.context_builder.as_ref() {
        Some(slot) => slot.read().await.clone(),
        None => None,
    };
    let roster = items::roster(cb.as_deref()).await;
    if let Err(e) = items::validate(None, &patch, &roster, false) {
        return bad_request(format!("Not assigned: {e}"));
    }
    let neo4j = state.neo4j.as_ref().expect("checked");
    if let Err(e) = neo4j.patch_todo_item(&id, &patch).await {
        return internal(e);
    }
    // A new assignment starts from pending; any earlier approval is void.
    let _ = neo4j
        .run(
            neo4rs::query(
                "MATCH (t:Todo {id: $id}) SET t.status = 'pending', t.approved_dispatch = null, t.updated_at = datetime()",
            )
            .param("id", id.as_str()),
        )
        .await;
    if !assignee.starts_with("agent:") {
        return respond(&state, &id, json!({ "outcome": "assigned_to_user" })).await;
    }
    let d = match dispatcher(&state).await {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let fresh = match item_or_404(&state, &id).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    post_to_thread(
        neo4j,
        &fresh,
        "observation",
        &format!("You handed this to **{}**.", &assignee[6..]),
    )
    .await;
    let outcome = match d.dispatch(&fresh, false).await {
        Ok(o) => o.to_json(),
        Err(e) => return internal(e),
    };
    respond(&state, &id, outcome).await
}

/// POST /api/items/{id}/approve — start a run waiting for approval (this
/// run only), or accept a result waiting for review (the item is done).
pub async fn handle_approve(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let t = match item_or_404(&state, &id).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let neo4j = state.neo4j.as_ref().expect("checked");
    match t.status.as_str() {
        "awaiting_approval" => {
            let run = t.dispatch_count + 1;
            if let Err(e) = neo4j
                .run(
                    neo4rs::query(
                        "MATCH (t:Todo {id: $id}) WHERE t.status = 'awaiting_approval' SET t.approved_dispatch = $run",
                    )
                    .param("id", id.as_str())
                    .param("run", run),
                )
                .await
            {
                return internal(e);
            }
            let d = match dispatcher(&state).await {
                Ok(d) => d,
                Err(r) => return *r,
            };
            let fresh = match item_or_404(&state, &id).await {
                Ok(t) => t,
                Err(r) => return *r,
            };
            post_to_thread(
                neo4j,
                &fresh,
                "observation",
                &format!("You approved run {run}."),
            )
            .await;
            let outcome = match d.dispatch(&fresh, true).await {
                Ok(o) => o.to_json(),
                Err(e) => return internal(e),
            };
            respond(&state, &id, outcome).await
        }
        "awaiting_review" => {
            if let Err(e) = neo4j
                .update_todo(&id, None, None, Some("done"), None, None, None)
                .await
            {
                return internal(e);
            }
            post_to_thread(
                neo4j,
                &t,
                "observation",
                "You approved the result; the item is done.",
            )
            .await;
            respond(&state, &id, json!({ "outcome": "done" })).await
        }
        other => bad_request(format!("The item is {other}; there is nothing to approve")),
    }
}

/// POST /api/items/{id}/send-back — `{comment}`. The result goes back to the
/// agent with the comment and its previous answer; the send-back is the
/// user's approval for that one run.
pub async fn handle_send_back(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let t = match item_or_404(&state, &id).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    if t.status != "awaiting_review" {
        return bad_request(format!(
            "The item is {}; only a result awaiting review can be sent back",
            t.status
        ));
    }
    if crate::services::item_dispatch::assigned_agent(&t).is_none() {
        return bad_request("The item is not assigned to an agent; hand it to one instead");
    }
    if t.dispatch_count >= MAX_RUNS {
        return bad_request(format!(
            "It has run {} times, the most an item may. Approve it, edit it, or make it a schedule.",
            t.dispatch_count
        ));
    }
    let Some(comment) = body
        .get("comment")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|c| !c.is_empty())
    else {
        return bad_request("A comment is required: say what to change");
    };
    let neo4j = state.neo4j.as_ref().expect("checked");
    if let Err(e) = neo4j
        .run(
            neo4rs::query(
                "MATCH (t:Todo {id: $id}) WHERE t.status = 'awaiting_review' \
                 SET t.status = 'pending', t.review_comment = $comment, \
                     t.approved_dispatch = coalesce(t.dispatch_count, 0) + 1, t.updated_at = datetime()",
            )
            .param("id", id.as_str())
            .param("comment", comment),
        )
        .await
    {
        return internal(e);
    }
    post_to_thread(neo4j, &t, "user", &format!("Sent back: {comment}")).await;
    let d = match dispatcher(&state).await {
        Ok(d) => d,
        Err(r) => return *r,
    };
    let fresh = match item_or_404(&state, &id).await {
        Ok(t) => t,
        Err(r) => return *r,
    };
    let outcome = or_500!(d.dispatch(&fresh, true).await);
    if let Outcome::NotReady(_) = &outcome {
        // Leave it where the user can act on it again.
        let _ = neo4j
            .run(
                neo4rs::query("MATCH (t:Todo {id: $id}) WHERE t.status = 'pending' SET t.status = 'awaiting_review'")
                    .param("id", id.as_str()),
            )
            .await;
    }
    respond(&state, &id, outcome.to_json()).await
}
