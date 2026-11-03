//! `/api/context-profiles`.

use super::{RestState, bad_request, not_found, refused, unavailable};
use axum::{Extension, Json, extract::Path, response::IntoResponse};
use serde_json::{Value, json};
use std::sync::Arc;

/// GET /api/contexts — list all loaded context profiles.
pub async fn handle_list_context_profiles(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(ref handle) = state.context_builder else {
        return unavailable("Context builder not available");
    };
    let guard = handle.read().await;
    let Some(ref cb) = *guard else {
        return unavailable("Context builder not yet initialised");
    };

    let profiles = cb.list_profiles().await;
    let items: Vec<Value> = profiles
        .iter()
        .map(|p| {
            json!({
                "name":             p.name,
                "description":      p.description,
                "tools":            p.tools,
                "tool_count":       p.tools.len(),
                "model_preference": p.model_preference,
                "provider_hint":    p.provider_hint,
                "spawnable":        p.spawnable,
                "specialty":        p.specialty,
                "data_class":       p.data_class,
                // An item assigned to it runs on its own (null) or waits for
                // the user's approval, with the reason (`items::side_effects`).
                "asks_first": match agent_brain_scheduler::items::side_effects(Some(p)) {
                    agent_brain_scheduler::items::SideEffects::ReadOnly => Value::Null,
                    agent_brain_scheduler::items::SideEffects::AsksFirst(why) => Value::String(why),
                },
            })
        })
        .collect();
    // Who owns each profile, where it is defined, and what it names that
    // does not exist (services/profiles.rs).
    let mut items = items;
    for item in &mut items {
        let name = item["name"].as_str().unwrap_or_default().to_string();
        if let (Some(obj), Value::Object(info)) =
            (item.as_object_mut(), cb.profile_info(&name).await)
        {
            obj.extend(info);
        }
    }

    // What a chat that names no profile runs under: the deployment's front
    // door (CHAT_DEFAULT_PROFILE), or — when none is set — a profile picked
    // per message by `auto_assign` (`default_profile: null`). The HBI's picker
    // defaults to this rather than to a hardcoded name.
    let front_door = std::env::var("CHAT_DEFAULT_PROFILE")
        .ok()
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    Json(json!({
        "count": items.len(),
        "profiles": items,
        "default_profile": front_door,
    }))
    .into_response()
}

/// GET /api/contexts/:name — full detail of a single context profile.
pub async fn handle_get_context_profile(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let Some(ref handle) = state.context_builder else {
        return unavailable("Context builder not available");
    };
    let guard = handle.read().await;
    let Some(ref cb) = *guard else {
        return unavailable("Context builder not yet initialised");
    };

    match cb.get_profile(&name).await {
        Some(p) => {
            // The whole stored profile, so an edit form can send back what it
            // does not edit; plus ownership, origin and audit findings.
            let mut v = serde_json::to_value(&p).unwrap_or(Value::Null);
            if let (Some(obj), Value::Object(info)) =
                (v.as_object_mut(), cb.profile_info(&name).await)
            {
                obj.extend(info);
            }
            v["rendered_prompt"] = json!(cb.rendered_prompt(&name).await);
            // What a save names as `based_on`. Fresh, not the builder's cache.
            if let Some(neo4j) = state.neo4j.as_ref() {
                let version = neo4j
                    .live_version(agent_brain_repository::RevisionKind::Profile, &name)
                    .await
                    .unwrap_or(0);
                v["version"] = json!(version);
            }
            Json(v).into_response()
        }
        None => not_found(),
    }
}

/// POST /api/contexts, PUT /api/contexts/:name — create or update a profile.
/// The body is laid over the stored profile (fields it omits keep their
/// value) and the profile becomes runtime-owned (services/profiles.rs).
pub async fn handle_upsert_context_profile(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(ref handle) = state.context_builder else {
        return unavailable("Context builder not available");
    };
    let guard = handle.read().await;
    let Some(ref cb) = *guard else {
        return unavailable("Context builder not yet initialised");
    };
    match cb.save_profile(&body).await {
        Ok((p, warnings)) => Json(json!({
            "success": true,
            "name": p.name,
            "managed_by": "runtime",
            "warnings": warnings,
        }))
        .into_response(),
        Err(e) => match e.downcast_ref::<agent_brain_work::revisions::Refused>() {
            Some(r) => refused(r),
            None => bad_request(format!("Profile not saved: {e}")),
        },
    }
}

/// POST /api/contexts/:name/reset — hand a runtime-owned profile back to its
/// bundle file (rewritten from it now, yaml-owned again).
pub async fn handle_reset_context_profile(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let Some(ref handle) = state.context_builder else {
        return unavailable("Context builder not available");
    };
    let guard = handle.read().await;
    let Some(ref cb) = *guard else {
        return unavailable("Context builder not yet initialised");
    };
    match cb.reset_profile(&name).await {
        Ok(p) => {
            Json(json!({ "reset": true, "name": p.name, "managed_by": "yaml" })).into_response()
        }
        Err(e) => bad_request(e),
    }
}

/// DELETE /api/contexts/:name — delete a context profile.
pub async fn handle_delete_context_profile(
    Extension(state): Extension<Arc<RestState>>,
    Path(name): Path<String>,
) -> impl IntoResponse {
    let Some(ref handle) = state.context_builder else {
        return unavailable("Context builder not available");
    };
    let guard = handle.read().await;
    let Some(ref cb) = *guard else {
        return unavailable("Context builder not yet initialised");
    };
    match cb.delete_profile(&name).await {
        Ok(true) => Json(json!({ "deleted": true, "name": name })).into_response(),
        Ok(false) => not_found(),
        Err(e) => bad_request(format!("Failed to delete profile: {e}")),
    }
}
