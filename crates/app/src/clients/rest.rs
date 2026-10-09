//! REST API adapter — todo, scheduled-task, and scheduler-config endpoints.
//!
//! This module extracts the REST resource routes from the HTTP transport so that
//! they are testable and configurable independently of MCP session management.
//!
//! # Usage
//!
//! ```ignore
//! let rest = RestAdapter::new()
//!     .with_neo4j(neo4j_arc)
//!     .with_scheduler(scheduler_handle);
//!
//! // Merge into an Axum router:
//! let router = Router::new().merge(rest.into_router());
//! ```

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Extension, Json, Router,
    extract::{Path, Query},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::{Value, json};
use tokio::sync::RwLock;

use crate::repository::{Neo4jClient, TelemetryClient, node_ts};
use crate::services::LlmConfig;
use crate::services::context_builder::{ContextBuilderService, ContextProfile};
use crate::services::scheduler::SchedulerService;
use crate::services::traits::WorkingMemoryStore;

// ============================================================================
// State
// ============================================================================

/// Resources available to the REST route handlers.
#[derive(Clone)]
pub struct RestState {
    pub neo4j: Option<Arc<Neo4jClient>>,
    pub scheduler: Option<Arc<RwLock<Option<Arc<SchedulerService>>>>>,
    pub context_builder: Option<Arc<RwLock<Option<Arc<ContextBuilderService>>>>>,
    pub llm_config: Option<Arc<RwLock<Option<LlmConfig>>>>,
    /// Config the human-facing `/chat` actually runs on — may differ from
    /// `llm_config` (the brain's) whenever `CHAT_LLM_*` is set. Used by
    /// `/api/models` to report the model the ChatPanel is talking to.
    pub chat_llm_config: Option<Arc<RwLock<Option<LlmConfig>>>>,
    pub telemetry: Option<TelemetryClient>,
    pub log_buffer: Option<Arc<crate::logging::LogBuffer>>,
    pub tool_registry: Option<Arc<RwLock<crate::mcp::tools::ToolRegistry>>>,
    /// Read-only access to the retrieval pipeline for the HBI's search
    /// (`clients/search.rs`). Built from `neo4j` + `llm_config`; embeddings
    /// go wherever that config sends them (host Ollama), as for every search.
    pub knowledge: Option<Arc<crate::services::knowledge::KnowledgeService>>,
}

// ============================================================================
// Adapter / builder
// ============================================================================

/// Builder for the REST adapter.
///
/// Attach resources and call [`into_router`] to get the Axum router.
#[derive(Default)]
pub struct RestAdapter {
    neo4j: Option<Arc<Neo4jClient>>,
    scheduler: Option<Arc<RwLock<Option<Arc<SchedulerService>>>>>,
    context_builder: Option<Arc<RwLock<Option<Arc<ContextBuilderService>>>>>,
    llm_config: Option<Arc<RwLock<Option<LlmConfig>>>>,
    chat_llm_config: Option<Arc<RwLock<Option<LlmConfig>>>>,
    telemetry: Option<TelemetryClient>,
    log_buffer: Option<Arc<crate::logging::LogBuffer>>,
    tool_registry: Option<Arc<RwLock<crate::mcp::tools::ToolRegistry>>>,
}

impl RestAdapter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_neo4j(mut self, neo4j: Arc<Neo4jClient>) -> Self {
        self.neo4j = Some(neo4j);
        self
    }

    pub fn with_neo4j_opt(mut self, neo4j: Option<Arc<Neo4jClient>>) -> Self {
        self.neo4j = neo4j;
        self
    }

    pub fn with_scheduler(mut self, handle: Arc<RwLock<Option<Arc<SchedulerService>>>>) -> Self {
        self.scheduler = Some(handle);
        self
    }

    pub fn with_scheduler_opt(
        mut self,
        handle: Option<Arc<RwLock<Option<Arc<SchedulerService>>>>>,
    ) -> Self {
        self.scheduler = handle;
        self
    }

    pub fn with_context_builder_opt(
        mut self,
        handle: Option<Arc<RwLock<Option<Arc<ContextBuilderService>>>>>,
    ) -> Self {
        self.context_builder = handle;
        self
    }

    pub fn with_llm_config_opt(mut self, cfg: Option<Arc<RwLock<Option<LlmConfig>>>>) -> Self {
        self.llm_config = cfg;
        self
    }

    pub fn with_chat_llm_config_opt(mut self, cfg: Option<Arc<RwLock<Option<LlmConfig>>>>) -> Self {
        self.chat_llm_config = cfg;
        self
    }

    pub fn with_telemetry_opt(mut self, telemetry: Option<TelemetryClient>) -> Self {
        self.telemetry = telemetry;
        self
    }

    pub fn with_log_buffer_opt(mut self, buf: Option<Arc<crate::logging::LogBuffer>>) -> Self {
        self.log_buffer = buf;
        self
    }

    pub fn with_tool_registry_opt(
        mut self,
        registry: Option<Arc<RwLock<crate::mcp::tools::ToolRegistry>>>,
    ) -> Self {
        self.tool_registry = registry;
        self
    }

    /// Build the [`RestState`] that must be injected as an Extension into the
    /// router returned by [`Self::routes`].
    ///
    /// Call pattern (inside `build_router`):
    /// ```ignore
    /// let rest_state = RestAdapter::new()
    ///     .build_state();
    ///
    /// let router = Router::new()
    ///     .merge(RestAdapter::routes())
    ///     .layer(axum::Extension(rest_state));
    /// ```
    pub fn build_state(self) -> Arc<RestState> {
        let knowledge = match (&self.neo4j, &self.llm_config) {
            (Some(n), Some(cfg)) => {
                Some(Arc::new(crate::services::knowledge::KnowledgeService::new(
                    (**n).clone(),
                    Some(crate::services::shared_llm::SharedLlm::new(Arc::clone(cfg))),
                )))
            }
            _ => None,
        };
        Arc::new(RestState {
            knowledge,
            neo4j: self.neo4j,
            scheduler: self.scheduler,
            context_builder: self.context_builder,
            llm_config: self.llm_config,
            chat_llm_config: self.chat_llm_config,
            telemetry: self.telemetry,
            log_buffer: self.log_buffer,
            tool_registry: self.tool_registry,
        })
    }

    /// Return a `Router<()>` containing all REST routes.
    ///
    /// Handlers extract [`Arc<RestState>`] via Axum's `Extension` mechanism.
    /// The caller must add the Extension to the parent router:
    /// ```ignore
    /// router.layer(axum::Extension(rest_state))
    /// ```
    pub fn routes() -> Router {
        Router::new()
            // --- logs ---
            .route("/api/logs", get(handle_get_logs))
            // --- todos ---
            .route("/todos", get(handle_list_todos).post(handle_create_todo))
            .route(
                "/todos/{id}",
                get(handle_get_todo)
                    .put(handle_update_todo)
                    .delete(handle_delete_todo),
            )
            // --- scheduled tasks ---
            .route(
                "/scheduled-tasks",
                get(handle_list_scheduled_tasks).post(handle_create_scheduled_task),
            )
            .route(
                "/scheduled-tasks/{id}",
                get(handle_get_scheduled_task)
                    .put(handle_update_scheduled_task)
                    .delete(handle_delete_scheduled_task),
            )
            // --- scheduler config ---
            .route(
                "/scheduler-config",
                get(handle_get_scheduler_config).put(handle_put_scheduler_config),
            )
    }

    /// Convenience: build both state and routes and return the complete router
    /// with the Extension already injected.
    ///
    /// Only call this when you want a fully self-contained `Router<()>` — e.g.
    /// for testing.  In production, use [`build_state`] + [`routes`] separately
    /// so the Extension can be applied after merging.
    pub fn into_router(self) -> Router {
        let state = self.build_state();
        Self::routes().layer(Extension(state))
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Return a 503 JSON response.
pub(crate) fn unavailable(msg: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

/// Return a 500 JSON response.
pub(crate) fn internal(msg: impl std::fmt::Display) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": msg.to_string()})),
    )
        .into_response()
}

/// Return a 404 JSON response.
pub(crate) fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "Not found"})),
    )
        .into_response()
}

pub(crate) fn bad_request(msg: impl std::fmt::Display) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({"error": msg.to_string()})),
    )
        .into_response()
}

/// `?since=` as an RFC 3339 instant, normalised to UTC.
pub(crate) fn parse_since(params: &HashMap<String, String>) -> Result<Option<String>, String> {
    parse_instant(params, "since")
}

/// An RFC 3339 instant query param, normalised to UTC. Callers must compare
/// it through `datetime($param)`: Cypher compares a datetime to a string as
/// null and silently drops every row.
pub(crate) fn parse_instant(
    params: &HashMap<String, String>,
    key: &str,
) -> Result<Option<String>, String> {
    match params.get(key).map(|s| s.trim()).filter(|s| !s.is_empty()) {
        None => Ok(None),
        Some(s) => chrono::DateTime::parse_from_rfc3339(s)
            .map(|d| Some(d.with_timezone(&chrono::Utc).to_rfc3339()))
            .map_err(|_| {
                format!(
                    "`{key}` must be an RFC 3339 timestamp (e.g. 2026-10-08T00:00:00Z), got {s:?}"
                )
            }),
    }
}

/// `?status=a,b` as a list (one status is a list of one). Bound as
/// `$statuses` with `IN`, so a value is never spliced into Cypher.
pub(crate) fn parse_statuses(params: &HashMap<String, String>) -> Option<Vec<String>> {
    let list: Vec<String> = params
        .get("status")?
        .split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    (!list.is_empty()).then_some(list)
}

/// `?hours=` for a look-back window: default 24, clamped to 1..=720 (30 days).
fn window_hours(params: &HashMap<String, String>) -> i64 {
    params
        .get("hours")
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(24)
        .clamp(1, 720)
}

/// `WHERE a AND b ` for the given conditions, or nothing. Conditions are
/// fixed strings with `$param` placeholders — never request values.
pub(crate) fn where_clause(conds: &[&str]) -> String {
    if conds.is_empty() {
        String::new()
    } else {
        format!("WHERE {} ", conds.join(" AND "))
    }
}

// ============================================================================
// Todo handlers
// ============================================================================

/// The spawnable roster, for checking an item's assignee.
async fn item_roster(state: &RestState) -> Vec<String> {
    let cb = match state.context_builder.as_ref() {
        Some(slot) => slot.read().await.clone(),
        None => None,
    };
    crate::services::items::roster(cb.as_deref()).await
}

/// Check a todo body's status and item fields. REST is the user's path, so
/// it may assign an agent.
async fn check_todo_body(
    state: &RestState,
    body: &Value,
) -> Result<crate::repository::TodoItemPatch, String> {
    let patch = crate::services::items::patch_from_json(body);
    let roster = item_roster(state).await;
    crate::services::items::validate(
        body.get("status").and_then(Value::as_str),
        &patch,
        &roster,
        false,
    )?;
    Ok(patch)
}

/// GET /todos[?status=pending|in_progress|done]
pub async fn handle_list_todos(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Todo storage not available");
    };

    let status_filter = params.get("status").map(String::as_str);
    match neo4j.list_todos(status_filter).await {
        Ok(todos) => {
            let items: Vec<Value> = todos
                .iter()
                .map(crate::services::items::item_json)
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
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Todo storage not available");
    };

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
    let priority = body.get("priority").and_then(|v| v.as_i64());
    let tags: Vec<String> = body
        .get("tags")
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();
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
            Ok(Some(todo)) => (
                StatusCode::CREATED,
                Json(crate::services::items::item_json(&todo)),
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
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Todo storage not available");
    };

    match neo4j.get_todo(&id).await {
        Ok(Some(todo)) => Json(crate::services::items::item_json(&todo)).into_response(),
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
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Todo storage not available");
    };

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
        .map(|o| o.contains_key("description"))
        .unwrap_or(false)
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
    let priority = body.get("priority").and_then(|v| v.as_i64());

    let tags: Option<Vec<String>> = if body
        .as_object()
        .map(|o| o.contains_key("tags"))
        .unwrap_or(false)
    {
        Some(
            body.get("tags")
                .and_then(|v| serde_json::from_value(v.clone()).ok())
                .unwrap_or_default(),
        )
    } else {
        None
    };

    let due_at: Option<Option<String>> = if body
        .as_object()
        .map(|o| o.contains_key("due_at"))
        .unwrap_or(false)
    {
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
            Ok(Some(todo)) => Json(crate::services::items::item_json(&todo)).into_response(),
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
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Todo storage not available");
    };

    match neo4j.delete_todo(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

// ============================================================================
// ScheduledTask handlers
// ============================================================================

/// GET /scheduled-tasks[?enabled_only=true]
pub async fn handle_list_scheduled_tasks(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Scheduled task storage not available");
    };

    let enabled_only = params
        .get("enabled_only")
        .map(|v| v == "true")
        .unwrap_or(false);

    match neo4j.list_scheduled_tasks(enabled_only).await {
        Ok(tasks) => Json(serde_json::json!({"scheduled_tasks": tasks})).into_response(),
        Err(e) => internal(e),
    }
}

/// The HBI saves schedules here, not through `manage_scheduled_task`, so the
/// checks that need no tool registry run here too: each step's pinned model
/// against the catalog, and the declared topic and kind. `Err` is the 400 text.
fn check_schedule_body(
    state: &RestState,
    body: &Value,
    steps_json: Option<&str>,
) -> Result<(), String> {
    if let (Some(steps), Some(tel)) = (steps_json, state.telemetry.as_ref()) {
        let steps: Vec<Value> = serde_json::from_str(steps).map_err(|e| format!("steps: {e}"))?;
        let catalog = crate::services::model_router::catalog_models(tel);
        let hard = crate::services::validation::validate_step_models(&steps, &catalog);
        if crate::services::validation::has_hard(&hard) {
            return Err(crate::services::validation::render_hard(&hard));
        }
    }
    crate::services::schedule_seeder::check_labels(
        body.get("topic").and_then(Value::as_str),
        body.get("kind").and_then(Value::as_str),
        &crate::services::schedule_seeder::declared_topics(),
    )
    .map(|_| ())
}

/// Write `topic` / `kind` from a create or update body. A key that is absent
/// keeps the stored value; null or "" clears it. Already checked.
async fn apply_schedule_labels(
    neo4j: &crate::repository::Neo4jClient,
    task: &mut crate::repository::ScheduledTask,
    body: &Value,
) -> Result<(), String> {
    if body.get("topic").is_none() && body.get("kind").is_none() {
        return Ok(());
    }
    let pick = |k: &str, current: &Option<String>| match body.get(k) {
        None => current.clone(),
        Some(v) => v
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from),
    };
    let topic = pick("topic", &task.topic);
    let kind = pick("kind", &task.kind);
    neo4j
        .set_scheduled_task_labels((&task.id, true), topic.as_deref(), kind.as_deref(), false)
        .await
        .map_err(|e| e.to_string())?;
    task.topic = topic;
    task.kind = kind;
    Ok(())
}

/// POST /scheduled-tasks
pub async fn handle_create_scheduled_task(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Scheduled task storage not available");
    };

    let Some(name) = body["name"].as_str() else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "name is required"})),
        )
            .into_response();
    };
    let name = name.to_string();

    let Some(interval_seconds) = body["interval_seconds"].as_i64().filter(|&v| v >= 60) else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "interval_seconds (>=60) is required"})),
        )
            .into_response();
    };

    // steps may be a JSON string or a JSON array
    let steps = if let Some(s) = body["steps"].as_str() {
        s.to_string()
    } else if body["steps"].is_array() {
        match serde_json::to_string(&body["steps"]) {
            Ok(s) => s,
            Err(_) => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(serde_json::json!({"error": "steps serialization failed"})),
                )
                    .into_response();
            }
        }
    } else {
        return (
            StatusCode::BAD_REQUEST,
            Json(serde_json::json!({"error": "steps is required (array or JSON string)"})),
        )
            .into_response();
    };

    if let Err(e) = check_schedule_body(&state, &body, Some(&steps)) {
        return bad_request(format!("Schedule rejected — nothing was saved: {e}"));
    }

    let description = body["description"].as_str();
    let enabled = body["enabled"].as_bool().unwrap_or(true);
    let next_run_at = body["next_run_at"]
        .as_str()
        .map(|s| s.to_string())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

    match neo4j
        .create_scheduled_task(
            &name,
            description,
            enabled,
            interval_seconds,
            &steps,
            &next_run_at,
            // API-created tasks are runtime-owned: the YAML seeder never touches them.
            body["managed_by"].as_str().unwrap_or("runtime"),
        )
        .await
    {
        Ok(mut task) => match apply_schedule_labels(neo4j, &mut task, &body).await {
            Ok(()) => (StatusCode::CREATED, Json(serde_json::json!(task))).into_response(),
            Err(e) => internal(format!("saved, but topic/kind were not: {e}")),
        },
        Err(e) => internal(e),
    }
}

/// GET /scheduled-tasks/:id
pub async fn handle_get_scheduled_task(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Scheduled task storage not available");
    };

    match neo4j.get_scheduled_task(&id).await {
        Ok(Some(task)) => Json(serde_json::json!(task)).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// PUT /scheduled-tasks/:id
pub async fn handle_update_scheduled_task(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Scheduled task storage not available");
    };

    let name = body["name"].as_str();
    let enabled = body["enabled"].as_bool();
    let interval_seconds = body["interval_seconds"].as_i64();
    let next_run_at = body["next_run_at"].as_str();

    let description: Option<Option<&str>> = if body.get("description").is_some() {
        Some(body["description"].as_str())
    } else {
        None
    };

    let steps_string: Option<String> = if body["steps"].is_array() {
        serde_json::to_string(&body["steps"]).ok()
    } else {
        body["steps"].as_str().map(|s| s.to_string())
    };
    if let Err(e) = check_schedule_body(&state, &body, steps_string.as_deref()) {
        return bad_request(format!("Schedule rejected — nothing was saved: {e}"));
    }

    match neo4j
        .update_scheduled_task(
            &id,
            name,
            description,
            enabled,
            interval_seconds,
            steps_string.as_deref(),
            next_run_at,
            body["managed_by"].as_str(),
        )
        .await
    {
        Ok(Some(mut task)) => match apply_schedule_labels(neo4j, &mut task, &body).await {
            Ok(()) => Json(serde_json::json!(task)).into_response(),
            Err(e) => internal(format!("saved, but topic/kind were not: {e}")),
        },
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

/// DELETE /scheduled-tasks/:id
pub async fn handle_delete_scheduled_task(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Scheduled task storage not available");
    };

    match neo4j.delete_scheduled_task(&id).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

// ============================================================================
// Scheduler config handlers
// ============================================================================

/// GET /scheduler-config — return current scheduler config + state.
pub async fn handle_get_scheduler_config(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };
    Json(svc.status().await).into_response()
}

/// PUT /scheduler-config — update scheduler settings.
pub async fn handle_put_scheduler_config(
    Extension(state): Extension<Arc<RestState>>,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };

    let local_model = body["local_model"].as_str().map(|s| s.to_string());
    let interval_secs = body["interval_secs"].as_u64();
    let enabled = body["enabled"].as_bool();

    let cfg = svc
        .update_config(
            interval_secs,
            enabled,
            None,
            None,
            None,
            None,
            None,
            local_model,
        )
        .await;

    Json(serde_json::json!({
        "updated": true,
        "config": {
            "interval_secs": cfg.interval_secs,
            "enabled": cfg.enabled,
            "local_model": cfg.local_model,
            "idle_sleep_after_ticks": cfg.idle_sleep_after_ticks,
            "sleep_interval_secs": cfg.sleep_interval_secs,
        }
    }))
    .into_response()
}

// ============================================================================
// Read-only API endpoints (formerly MCP tools)
// ============================================================================

/// Most seeds `/api/graph` takes; more are ignored.
const GRAPH_MAX_SEEDS: usize = 60;
/// RELATES_TO neighbours kept per seed (most similar first).
const GRAPH_NEIGHBOURS_PER_SEED: i64 = 4;
/// Entities shown around a seeded graph (most-mentioned by its notes first).
const GRAPH_SEED_ENTITIES: i64 = 40;

/// Nodes for a seeded graph: the seed notes (flagged `seed`), each one's
/// nearest RELATES_TO neighbours, and the entities those notes mention most
/// (URLs and dates left out: they are noise at this scale).
async fn seeded_graph_nodes(neo4j: &Neo4jClient, seeds: &[String]) -> Vec<Value> {
    let label = |content: &str| -> String { content.chars().take(60).collect() };
    let mut nodes = Vec::new();
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (s:Note) WHERE s.id IN $seeds \
                 OPTIONAL MATCH (s)-[r:RELATES_TO]-(m:Note) \
                 WITH s, m, r ORDER BY r.similarity DESC \
                 WITH s, collect(m)[0..$per] AS ms \
                 UNWIND ([s] + ms) AS n \
                 WITH DISTINCT n \
                 RETURN n.id AS id, n.content AS content, n.note_type AS note_type, \
                        toString(n.created_at) AS created_at",
            )
            .param("seeds", seeds.to_vec())
            .param("per", GRAPH_NEIGHBOURS_PER_SEED),
        )
        .await
        .unwrap_or_default();
    let seed_set: std::collections::HashSet<&str> = seeds.iter().map(String::as_str).collect();
    let mut note_ids = Vec::new();
    for row in rows {
        let id = row.get::<String>("id").unwrap_or_default();
        if id.is_empty() {
            continue;
        }
        let content = row.get::<String>("content").unwrap_or_default();
        nodes.push(json!({
            "id": id,
            "label": label(&content),
            "type": "note",
            "note_type": row.get::<String>("note_type").unwrap_or_else(|_| "semantic".to_string()),
            "created_at": row.get::<String>("created_at").unwrap_or_default(),
            "seed": seed_set.contains(id.as_str()),
        }));
        note_ids.push(id);
    }
    let entity_rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note)-[:MENTIONS]->(e:Entity) \
                 WHERE n.id IN $ids AND NOT e.entity_type IN ['url', 'date'] \
                 WITH e, count(DISTINCT n) AS c ORDER BY c DESC LIMIT $limit \
                 RETURN e.id AS id, e.name AS name, e.entity_type AS entity_type, \
                        toString(e.created_at) AS created_at",
            )
            .param("ids", note_ids)
            .param("limit", GRAPH_SEED_ENTITIES),
        )
        .await
        .unwrap_or_default();
    for row in entity_rows {
        nodes.push(json!({
            "id": row.get::<String>("id").unwrap_or_default(),
            "label": row.get::<String>("name").unwrap_or_default(),
            "type": "entity",
            "entity_type": row.get::<String>("entity_type").unwrap_or_else(|_| "unknown".to_string()),
            "created_at": row.get::<String>("created_at").unwrap_or_default(),
        }));
    }
    nodes
}

/// GET /api/graph?max_nodes=N | ?seeds=id,id — graph nodes and edges for visualisation.
pub async fn handle_get_graph(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

    let max_nodes: i64 = params
        .get("max_nodes")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);

    // `?seeds=a,b,c`: the neighbourhood of those notes (the HBI's search
    // results) instead of the most recently used part of the graph.
    let seeds: Vec<String> = params
        .get("seeds")
        .map(|s| {
            s.split(',')
                .map(|x| x.trim().to_string())
                .filter(|x| !x.is_empty())
                .take(GRAPH_MAX_SEEDS)
                .collect()
        })
        .unwrap_or_default();

    let mut nodes: Vec<Value> = Vec::new();

    if !seeds.is_empty() {
        nodes = seeded_graph_nodes(neo4j, &seeds).await;
    } else {
        // Notes
        let note_rows = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (n:Note) \
                 RETURN n.id AS id, n.content AS content, n.note_type AS note_type, \
                        toString(n.created_at) AS created_at \
                 ORDER BY n.last_accessed_at DESC LIMIT $limit",
                )
                .param("limit", max_nodes),
            )
            .await
            .unwrap_or_default();
        for row in note_rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let content = row.get::<String>("content").unwrap_or_default();
            let note_type = row
                .get::<String>("note_type")
                .unwrap_or_else(|_| "semantic".to_string());
            let created_at = row.get::<String>("created_at").unwrap_or_default();
            let label: String = content.chars().take(60).collect();
            nodes.push(json!({ "id": id, "label": label, "type": "note", "note_type": note_type, "created_at": created_at }));
        }

        // Entities (most-mentioned first)
        let entity_limit = (max_nodes / 4).max(20);
        let entity_rows = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (e:Entity)<-[r:MENTIONS]-() \
                 RETURN e.id AS id, e.name AS name, e.entity_type AS entity_type, count(r) AS mentions, \
                        toString(e.created_at) AS created_at \
                 ORDER BY mentions DESC LIMIT $limit",
                )
                .param("limit", entity_limit),
            )
            .await
            .unwrap_or_default();
        for row in entity_rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let name = row.get::<String>("name").unwrap_or_default();
            let entity_type = row
                .get::<String>("entity_type")
                .unwrap_or_else(|_| "unknown".to_string());
            let created_at = row.get::<String>("created_at").unwrap_or_default();
            nodes.push(json!({ "id": id, "label": name, "type": "entity", "entity_type": entity_type, "created_at": created_at }));
        }

        // Tasks
        let task_limit = (max_nodes / 8).max(10);
        let task_rows = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (t:Task) \
                 RETURN t.id AS id, t.goal AS goal, t.status AS status, \
                        toString(t.created_at) AS created_at \
                 ORDER BY t.created_at DESC LIMIT $limit",
                )
                .param("limit", task_limit),
            )
            .await
            .unwrap_or_default();
        for row in task_rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let goal = row.get::<String>("goal").unwrap_or_default();
            let status = row.get::<String>("status").unwrap_or_default();
            let created_at = row.get::<String>("created_at").unwrap_or_default();
            let label: String = goal.chars().take(60).collect();
            nodes.push(json!({ "id": id, "label": label, "type": "task", "status": status, "created_at": created_at }));
        }
    }

    let node_ids: std::collections::HashSet<String> = nodes
        .iter()
        .filter_map(|n| n["id"].as_str().map(str::to_string))
        .collect();
    let ids_vec: Vec<String> = node_ids.iter().cloned().collect();

    // Edges
    let mut edges: Vec<Value> = Vec::new();
    let edge_queries: &[(&str, &str)] = &[
        (
            "MATCH (a:Note)-[r:RELATES_TO]->(b:Note) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, r.similarity AS weight",
            "relates_to",
        ),
        (
            "MATCH (a:Note)-[:MENTIONS]->(b:Entity) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
            "mentions",
        ),
        (
            "MATCH (a:Note)-[:SUMMARIZED_BY]->(b:Note) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
            "summarized_by",
        ),
        (
            "MATCH (a:Note)-[:REFLECTS_ON]->(b:Task) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
            "reflects_on",
        ),
        (
            "MATCH (a:Task)-[:SUBTASK_OF]->(b:Task) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
            "subtask_of",
        ),
        (
            "MATCH (a:Note)-[:DERIVED_FROM]->(b:Note) WHERE a.id IN $ids AND b.id IN $ids \
          RETURN a.id AS src, b.id AS tgt, 1.0 AS weight",
            "derived_from",
        ),
    ];

    for (cypher, label) in edge_queries {
        let rows = neo4j
            .execute(neo4rs::query(cypher).param("ids", ids_vec.clone()))
            .await
            .unwrap_or_default();
        for row in rows {
            let src = row.get::<String>("src").unwrap_or_default();
            let tgt = row.get::<String>("tgt").unwrap_or_default();
            let weight = row.get::<f64>("weight").unwrap_or(1.0);
            edges.push(json!({ "source": src, "target": tgt, "type": label, "weight": weight }));
        }
    }

    Json(json!({
        "node_count": nodes.len(),
        "edge_count": edges.len(),
        "seeded": !seeds.is_empty(),
        "nodes": nodes,
        "edges": edges,
    }))
    .into_response()
}

/// GET /api/health — rich runtime health snapshot (scheduler + queue + knowledge + tasks).
pub async fn handle_api_health(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };

    let scheduler_status = svc.status().await;
    let queue_stats = svc.queue_stats().await;

    let note_rows = neo4j
        .execute(neo4rs::query(
            "MATCH (n:Note) RETURN n.note_type AS note_type, count(n) AS n ORDER BY n DESC",
        ))
        .await
        .unwrap_or_default();
    let mut notes_by_type = serde_json::Map::new();
    let mut total_notes: i64 = 0;
    for row in &note_rows {
        let nt = row
            .get::<String>("note_type")
            .unwrap_or_else(|_| "unknown".to_string());
        let n = row.get::<i64>("n").unwrap_or(0);
        notes_by_type.insert(nt, json!(n));
        total_notes += n;
    }
    notes_by_type.insert("total".to_string(), json!(total_notes));

    let task_rows = neo4j
        .execute(neo4rs::query(
            "MATCH (t:Task) RETURN t.status AS status, count(t) AS n ORDER BY n DESC",
        ))
        .await
        .unwrap_or_default();
    let mut tasks_by_status = serde_json::Map::new();
    let mut total_tasks: i64 = 0;
    for row in &task_rows {
        let s = row
            .get::<String>("status")
            .unwrap_or_else(|_| "unknown".to_string());
        let n = row.get::<i64>("n").unwrap_or(0);
        tasks_by_status.insert(s, json!(n));
        total_tasks += n;
    }
    tasks_by_status.insert("total".to_string(), json!(total_tasks));

    let dead_24h: i64 = neo4j
        .execute(neo4rs::query(
            "MATCH (j:AgentJob) \
             WHERE j.status IN ['dead', 'dead_letter'] \
               AND j.updated_at >= datetime() - duration({hours: 24}) \
             RETURN count(j) AS n",
        ))
        .await
        .ok()
        .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("n").ok()))
        .unwrap_or(0);

    // The commit the running binary was built from (written at startup).
    let version = neo4j
        .execute(neo4rs::query(
            "MATCH (b:BrainVersion {id: 'current'}) \
             RETURN b.sha AS sha, b.branch AS branch, b.dirty AS dirty, \
                    b.subject AS subject, toString(b.deployed_at) AS deployed_at",
        ))
        .await
        .ok()
        .and_then(|rows| {
            rows.first().map(|r| {
                json!({
                    "sha": r.get::<String>("sha").ok(),
                    "branch": r.get::<String>("branch").ok(),
                    "dirty": r.get::<bool>("dirty").ok(),
                    "subject": r.get::<String>("subject").ok(),
                    "deployed_at": r.get::<String>("deployed_at").ok(),
                })
            })
        });

    Json(json!({
        "scheduler": scheduler_status,
        "queue": queue_stats,
        "knowledge": { "notes_by_type": serde_json::Value::Object(notes_by_type) },
        "tasks": { "by_status": serde_json::Value::Object(tasks_by_status) },
        // `dead` and `dead_letter` both mean a job that will not run again.
        "alerts": { "dead_jobs_last_24h": dead_24h },
        "version": version,
    }))
    .into_response()
}

/// GET /api/scheduler/status — current scheduler config and runtime state.
pub async fn handle_get_scheduler_status(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };
    Json(svc.status().await).into_response()
}

/// GET /api/scheduler/chains — list all SchedulerChain nodes.
pub async fn handle_list_scheduler_chains(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

    let cypher = "MATCH (c:SchedulerChain) \
                  RETURN c.id AS id, c.name AS name, c.pattern AS pattern, \
                         coalesce(c.patterns, []) AS patterns, \
                         c.priority AS priority, c.description AS description, \
                         c.no_evaluator AS no_evaluator, \
                         c.evaluation_rubric AS evaluation_rubric, \
                         c.steps AS steps, toString(c.updated_at) AS updated_at \
                  ORDER BY c.priority ASC, c.name ASC";

    match neo4j.execute(neo4rs::query(cypher)).await {
        Ok(rows) => {
            let chains: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let steps_json = row.get::<String>("steps").unwrap_or_default();
                    let step_count = serde_json::from_str::<Value>(&steps_json)
                        .ok()
                        .and_then(|v| v.as_array().map(|a| a.len()))
                        .unwrap_or(0);
                    let patterns: Vec<String> =
                        row.get::<Vec<String>>("patterns").unwrap_or_default();
                    json!({
                        "id":                row.get::<String>("id").unwrap_or_default(),
                        "name":              row.get::<String>("name").unwrap_or_default(),
                        "pattern":           row.get::<String>("pattern").unwrap_or_default(),
                        "patterns":          patterns,
                        "priority":          row.get::<i64>("priority").unwrap_or(100),
                        "description":       row.get::<String>("description").unwrap_or_default(),
                        "no_evaluator":      row.get::<bool>("no_evaluator").unwrap_or(false),
                        "evaluation_rubric": row.get::<String>("evaluation_rubric").ok(),
                        "step_count":        step_count,
                        "steps":             steps_json,
                        "updated_at":        row.get::<String>("updated_at").ok(),
                    })
                })
                .collect();
            Json(json!({ "count": chains.len(), "chains": chains })).into_response()
        }
        Err(e) => internal(format!("Failed to list chains: {e}")),
    }
}

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
            })
        })
        .collect();

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
        Some(p) => Json(json!({
            "name":             p.name,
            "description":      p.description,
            "tools":            p.tools,
            "system_prompt":    p.system_prompt,
            "token_budget":     p.token_budget,
            "pre_load_query":   p.pre_load_query,
            "model_preference": p.model_preference,
            "provider_hint":    p.provider_hint,
            // Returned so an edit form can send them back: `save_profile` keeps
            // a stored data_class when a save omits it, but has no such guard
            // for spawnable/specialty.
            "spawnable":        p.spawnable,
            "specialty":        p.specialty,
            "data_class":       p.data_class,
        }))
        .into_response(),
        None => not_found(),
    }
}

/// POST /api/contexts — create or update a context profile (upsert by name).
pub async fn handle_upsert_context_profile(
    Extension(state): Extension<Arc<RestState>>,
    Json(profile): Json<ContextProfile>,
) -> impl IntoResponse {
    let Some(ref handle) = state.context_builder else {
        return unavailable("Context builder not available");
    };
    let guard = handle.read().await;
    let Some(ref cb) = *guard else {
        return unavailable("Context builder not yet initialised");
    };
    match cb.save_profile(profile).await {
        Ok(()) => Json(json!({ "success": true })).into_response(),
        Err(e) => internal(format!("Failed to save profile: {e}")),
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
        Err(e) => internal(format!("Failed to delete profile: {e}")),
    }
}

/// GET /api/http-contexts — list all ApiContext nodes (name, base_url, auth_scheme).
pub async fn handle_list_http_contexts(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

    let cypher = "MATCH (c:ApiContext) \
                  RETURN c.name AS name, c.base_url AS base_url, \
                         c.auth_scheme AS auth_scheme, c.description AS description \
                  ORDER BY c.name ASC";

    match neo4j.execute(neo4rs::query(cypher)).await {
        Ok(rows) => {
            let contexts: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "name":        row.get::<String>("name").unwrap_or_default(),
                        "base_url":    row.get::<String>("base_url").unwrap_or_default(),
                        "auth_scheme": row.get::<String>("auth_scheme").unwrap_or_default(),
                        "description": row.get::<String>("description").unwrap_or_default(),
                    })
                })
                .collect();
            Json(json!({ "count": contexts.len(), "contexts": contexts })).into_response()
        }
        Err(e) => internal(format!("Failed to list API contexts: {e}")),
    }
}

/// GET /api/models — active provider + catalog models.
pub async fn handle_list_models(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    use crate::services::LlmProviderType;

    // Read a (provider, model) pair off an optional config Arc.
    async fn read_cfg(arc: &Option<Arc<RwLock<Option<LlmConfig>>>>) -> (String, String) {
        match arc {
            Some(a) => {
                let cfg = a.read().await;
                (
                    cfg.as_ref()
                        .map(|c| c.provider.to_string())
                        .unwrap_or_else(|| "None".to_string()),
                    cfg.as_ref().map(|c| c.model.clone()).unwrap_or_default(),
                )
            }
            None => ("None".to_string(), String::new()),
        }
    }

    // `active_*` is the brain's model (background/scheduled cognition); `chat_*`
    // is what the human-facing /chat runs on. They diverge whenever CHAT_LLM_*
    // is set — the ChatPanel reads `chat_*` so its indicator names the model the
    // user is actually talking to, not the brain's.
    let (active_provider, active_model) = read_cfg(&state.llm_config).await;
    let (chat_provider, chat_model) = read_cfg(&state.chat_llm_config).await;

    let catalog_models = if let Some(ref db) = state.telemetry {
        let mut rows = db.list_models().unwrap_or_default();
        for row in rows.iter_mut() {
            let admission = model_admission(
                row["provider"].as_str().unwrap_or(""),
                row["model"].as_str().unwrap_or(""),
            );
            row["admission"] = admission;
        }
        Value::Array(rows)
    } else {
        json!([])
    };

    Json(json!({
        "active_provider": active_provider,
        "active_model":    active_model,
        "chat_provider":   chat_provider,
        "chat_model":      chat_model,
        // `cost` describes the tier this deployment uses, not the vendor's whole
        // price list: Ollama Cloud and Gemini are consumed on their free tiers
        // (429 at the ceiling, no billing). Anthropic is the only metered path.
        "available_providers": [
            { "name": "Ollama (local)", "type": LlmProviderType::Ollama.to_string(),      "cost": "free" },
            { "name": "Ollama Cloud",   "type": LlmProviderType::OllamaCloud.to_string(), "cost": "free tier" },
            { "name": "Anthropic",      "type": LlmProviderType::Anthropic.to_string(),   "cost": "paid" },
            { "name": "Gemini",         "type": LlmProviderType::Gemini.to_string(),      "cost": "free tier" },
        ],
        // The provider registry: kind, billing, data_policy, limits, spend, and
        // whether each provider's key env var is set — never the key.
        "providers": crate::services::provider_registry::snapshot_json(),
        // Which billing classes are routable and which purposes may spend.
        "routing_policy": crate::services::routing::RoutingPolicy::from_env().describe(),
        "catalog_models": catalog_models,
    })).into_response()
}

/// Whether the routing policy would let `model` run, per purpose and data
/// class, right now: `null` when it may, else the refusal (spend cap, cooldown,
/// a private-work rule, a missing key). The model picker shows these, so a
/// choice that would be refused says so before it is saved.
fn model_admission(provider: &str, model: &str) -> Value {
    use crate::services::routing::{DataClass, Purpose, RouteContext, RoutingPolicy};
    let policy = RoutingPolicy::from_env();
    let cfg = crate::services::model_router::config_for_catalog_entry(
        &LlmConfig::default(),
        provider,
        model,
    );
    let mut out = serde_json::Map::new();
    for purpose in [Purpose::Background, Purpose::Subagent, Purpose::Chat] {
        let mut by_class = serde_json::Map::new();
        for class in [DataClass::Private, DataClass::Public] {
            let verdict = policy.admit(&cfg, RouteContext::new(purpose, class));
            by_class.insert(
                class.as_str().to_string(),
                verdict.err().map(Value::String).unwrap_or(Value::Null),
            );
        }
        out.insert(purpose.as_str().to_string(), Value::Object(by_class));
    }
    Value::Object(out)
}

/// GET /api/llm/budget — the live budget ledger: per-provider usage against
/// declared limits and spend caps, plus per-model cooldowns and headers.
pub async fn handle_llm_budget() -> impl IntoResponse {
    Json(crate::services::budget::snapshot_json()).into_response()
}

/// GET /api/gpu-lease — whether the GPU is leased, to whom and until when.
pub async fn handle_gpu_lease_status() -> impl IntoResponse {
    Json(crate::services::gpu_lease::status_json()).into_response()
}

/// POST /api/gpu-lease — `{action: status|acquire|release, holder, reason,
/// ttl_hours, force}`. A refused action (the GPU held by someone else, a
/// missing holder) is 409 with the reason.
pub async fn handle_gpu_lease(Json(body): Json<Value>) -> impl IntoResponse {
    match crate::services::gpu_lease::handle(body).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::CONFLICT, Json(json!({ "error": e }))).into_response(),
    }
}

/// GET /api/tools/dynamic — list all runtime-defined DynamicTool nodes.
/// GET /api/jobs?status=<filter>&limit=<n>
///
/// Returns agent jobs from Neo4j. The frontend uses this instead of an MCP
/// tool call so the agent's tool surface stays clean.
pub async fn handle_list_jobs(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

    let statuses = parse_statuses(&params);
    let limit: i64 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
        .min(500);
    // `since` filters on updated_at: a job "in the window" is one whose state
    // changed in it, which is what /api/activity counts.
    let since = match parse_since(&params) {
        Ok(s) => s,
        Err(e) => return bad_request(e),
    };

    let mut conds = Vec::new();
    if statuses.is_some() {
        conds.push("j.status IN $statuses");
    }
    if since.is_some() {
        conds.push("j.updated_at >= datetime($since)");
    }
    let cypher = format!(
        "MATCH (j:AgentJob) {}RETURN j ORDER BY j.created_at DESC LIMIT $limit",
        where_clause(&conds)
    );
    let mut q = neo4rs::query(&cypher).param("limit", limit);
    if let Some(ref s) = statuses {
        q = q.param("statuses", s.clone());
    }
    if let Some(ref s) = since {
        q = q.param("since", s.as_str());
    }

    match neo4j.execute(q).await {
        Ok(rows) => {
            let jobs: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let node: neo4rs::Node = match row.get("j") {
                        Ok(n) => n,
                        Err(_) => return json!(null),
                    };
                    let args_json: String = node.get("args_json").unwrap_or_default();
                    let args: Option<Value> = if args_json.is_empty() {
                        None
                    } else {
                        serde_json::from_str(&args_json).ok()
                    };
                    json!({
                        "id":            node.get::<String>("id").unwrap_or_default(),
                        "tool_name":     node.get::<String>("tool_name").unwrap_or_default(),
                        "status":        node.get::<String>("status").unwrap_or_default(),
                        "priority":      node.get::<i64>("priority").unwrap_or(1),
                        "attempt_count": node.get::<i64>("attempt_count").unwrap_or(0),
                        "max_attempts":  node.get::<i64>("max_attempts").unwrap_or(3),
                        "provider_hint": node.get::<String>("provider_hint").ok().filter(|s| !s.is_empty()),
                        "args":          args,
                        "error":         node.get::<String>("error").ok().filter(|s| !s.is_empty()),
                        "parent_job_id": node.get::<String>("parent_job_id").ok().filter(|s| !s.is_empty()),
                        "created_at":    node_ts(&node, "created_at").unwrap_or_default(),
                        "updated_at":    node_ts(&node, "updated_at").unwrap_or_default(),
                    })
                })
                .filter(|v| !v.is_null())
                .collect();
            Json(json!({ "count": jobs.len(), "jobs": jobs })).into_response()
        }
        Err(e) => internal(format!("Failed to list jobs: {e}")),
    }
}

pub async fn handle_list_dynamic_tools(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

    let cypher = "MATCH (d:DynamicTool) \
                  RETURN d.id AS id, d.name AS name, d.description AS description, \
                         toString(d.created_at) AS created_at \
                  ORDER BY d.created_at DESC";

    match neo4j.execute(neo4rs::query(cypher)).await {
        Ok(rows) => {
            let tools: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "id":          row.get::<String>("id").unwrap_or_default(),
                        "name":        row.get::<String>("name").unwrap_or_default(),
                        "description": row.get::<String>("description").unwrap_or_default(),
                        "created_at":  row.get::<String>("created_at").unwrap_or_default(),
                    })
                })
                .collect();
            Json(json!({ "count": tools.len(), "tools": tools })).into_response()
        }
        Err(e) => internal(format!("Failed to list dynamic tools: {e}")),
    }
}

/// GET /api/notes?limit=<n>&note_type=<type>&q=<search>
///
/// List notes from Neo4j. Pass `q` for text search; `note_type` to filter by type.
pub async fn handle_list_notes(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

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
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

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
                Some(crate::models::provenance::ProvenanceFlag::UserInput),
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
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

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
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

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
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

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
                        let r = crate::services::claims::source_reference(
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
                    "topics":              crate::clients::search::topic_names(row.get::<Vec<neo4rs::BoltMap>>("topics").unwrap_or_default()),
                    // false: not classified yet for the current topic definitions.
                    "topics_assessed":     row.get::<String>("topics_version").unwrap_or_default() == crate::services::topics::Topics::global().version,
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
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

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

/// GET /api/tasks?status=<filter>&limit=<n> — list tasks from Neo4j.
pub async fn handle_list_tasks(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

    let limit: i64 = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
        .min(500);
    let statuses = parse_statuses(&params);
    let since = match parse_since(&params) {
        Ok(s) => s,
        Err(e) => return bad_request(e),
    };

    let mut conds = Vec::new();
    if statuses.is_some() {
        conds.push("t.status IN $statuses");
    }
    if since.is_some() {
        conds.push("t.created_at >= datetime($since)");
    }
    let cypher = format!(
        "MATCH (t:Task) {}\
         RETURN t.id AS id, t.goal AS goal, t.status AS status, \
                t.context AS context, toString(t.created_at) AS created_at, \
                [(t)-[:SUBTASK_OF]->(p) | p.id][0] AS parent_id \
         ORDER BY t.created_at DESC LIMIT $limit",
        where_clause(&conds)
    );
    let mut q = neo4rs::query(&cypher).param("limit", limit);
    if let Some(ref s) = statuses {
        q = q.param("statuses", s.clone());
    }
    if let Some(ref s) = since {
        q = q.param("since", s.as_str());
    }

    match neo4j.execute(q).await {
        Ok(rows) => {
            let tasks: Vec<Value> = rows
                .iter()
                .map(|row| {
                    json!({
                        "id":         row.get::<String>("id").unwrap_or_default(),
                        "goal":       row.get::<String>("goal").unwrap_or_default(),
                        "status":     row.get::<String>("status").unwrap_or_default(),
                        "context":    row.get::<String>("context").ok().filter(|s| !s.is_empty()),
                        "created_at": row.get::<String>("created_at").unwrap_or_default(),
                        "parent_id":  row.get::<String>("parent_id").ok().filter(|s| !s.is_empty()),
                    })
                })
                .collect();
            Json(json!({ "count": tasks.len(), "tasks": tasks })).into_response()
        }
        Err(e) => internal(format!("Failed to list tasks: {e}")),
    }
}

/// GET /api/queue/status — current queue depth and per-provider utilization.
pub async fn handle_queue_status(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };
    Json(svc.queue_stats().await).into_response()
}

/// POST /api/queue/drain — cancel all queued (in-memory) jobs.
pub async fn handle_queue_drain(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let Some(ref handle) = state.scheduler else {
        return unavailable("Scheduler not available");
    };
    let guard = handle.read().await;
    let Some(ref svc) = *guard else {
        return unavailable("Scheduler not yet initialised");
    };
    match svc.queue_drain().await {
        Ok(n) => Json(json!({ "drained": n })).into_response(),
        Err(e) => internal(e),
    }
}

/// GET /api/sessions?limit=<n> — list working-memory sessions with metadata.
pub async fn handle_list_sessions(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

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
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };

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
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };
    match neo4j.archive_session(&session_id).await {
        Ok(()) => Json(json!({ "status": "archived", "session_id": session_id })).into_response(),
        Err(e) => internal(format!("Failed to archive session: {e}")),
    }
}

/// GET /api/models/usage?hours=<n> — aggregated model usage stats from DuckDB.
/// `hours` restricts the window (quota budgets are time-windowed); omit for all-time.
pub async fn handle_model_usage(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref db) = state.telemetry else {
        return Json(json!({ "models": [], "available": false })).into_response();
    };
    let hours: Option<i64> = params.get("hours").and_then(|h| h.parse().ok());
    match db.get_model_stats(None, hours) {
        Ok(stats) => {
            let models = stats.get("models").cloned().unwrap_or(json!([]));
            Json(json!({ "models": models, "available": true, "window_hours": hours }))
                .into_response()
        }
        Err(e) => internal(format!("Failed to get model usage: {e}")),
    }
}

/// One `RETURN <key> AS k, count(*) AS c` breakdown over a window: the total
/// and the per-key counts.
async fn count_by(neo4j: &Neo4jClient, cypher: &str, since: &str) -> Result<(i64, Value), String> {
    let rows = neo4j
        .execute(neo4rs::query(cypher).param("since", since))
        .await
        .map_err(|e| e.to_string())?;
    let mut total = 0;
    let mut by = serde_json::Map::new();
    for row in rows {
        let k = row
            .get::<String>("k")
            .unwrap_or_else(|_| "unknown".to_string());
        let c = row.get::<i64>("c").unwrap_or(0);
        total += c;
        by.insert(k, json!(c));
    }
    Ok((total, Value::Object(by)))
}

/// GET /api/activity?hours=24 — what the brain did in a look-back window.
///
/// Every count has rows behind it: `notes` → `/api/notes?since=…&note_type=…`,
/// `tasks` → `/api/tasks?since=…&status=…`, `jobs` (state changed in the
/// window) → `/api/jobs?since=…&status=…`, `claims_checked` →
/// `/api/notes?note_type=claim&verified_since=…`. A section whose query fails is
/// `null` and named in `errors` — never reported as zero.
pub async fn handle_get_activity(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return unavailable("Neo4j not available");
    };
    let hours = window_hours(&params);
    let since = (chrono::Utc::now() - chrono::Duration::hours(hours)).to_rfc3339();

    let sections: [(&str, &str); 4] = [
        (
            "notes",
            "MATCH (n:Note) WHERE n.created_at >= datetime($since) \
             RETURN coalesce(n.note_type, 'untyped') AS k, count(*) AS c",
        ),
        (
            "tasks",
            "MATCH (t:Task) WHERE t.created_at >= datetime($since) \
             RETURN coalesce(t.status, 'unknown') AS k, count(*) AS c",
        ),
        (
            "jobs",
            "MATCH (j:AgentJob) WHERE j.updated_at >= datetime($since) \
             RETURN coalesce(j.status, 'unknown') AS k, count(*) AS c",
        ),
        (
            "claims_checked",
            "MATCH (n:Note {note_type: 'claim'}) WHERE n.verified_at >= datetime($since) \
             RETURN coalesce(n.claim_status, 'unknown') AS k, count(*) AS c",
        ),
    ];

    let mut out = serde_json::Map::new();
    let mut errors: Vec<String> = Vec::new();
    for (name, cypher) in sections {
        match count_by(neo4j, cypher, &since).await {
            Ok((total, by)) => {
                out.insert(name.to_string(), json!({ "total": total, "by": by }));
            }
            Err(e) => {
                out.insert(name.to_string(), Value::Null);
                errors.push(format!("{name}: {e}"));
            }
        }
    }

    let singles: [(&str, &str); 2] = [
        (
            "media_ingested",
            "MATCH (m:Media) WHERE m.ingested_at >= datetime($since) RETURN 'all' AS k, count(*) AS c",
        ),
        (
            "schedule_runs",
            "MATCH (s:ScheduledTask) WHERE s.last_run_at >= datetime($since) \
             RETURN 'all' AS k, count(*) AS c",
        ),
    ];
    for (name, cypher) in singles {
        match count_by(neo4j, cypher, &since).await {
            Ok((total, _)) => {
                out.insert(name.to_string(), json!(total));
            }
            Err(e) => {
                out.insert(name.to_string(), Value::Null);
                errors.push(format!("{name}: {e}"));
            }
        }
    }

    out.insert("window_hours".to_string(), json!(hours));
    out.insert("since".to_string(), json!(since));
    out.insert("errors".to_string(), json!(errors));
    Json(Value::Object(out)).into_response()
}

/// GET /api/logs?limit=&level=&target=&q=&after=
/// Recent in-process log lines from the ring buffer, newest first.
/// `target` is a comma list of target prefixes to keep, `-prefix` to drop;
/// `quiet` a comma list of target prefixes shown only at WARN or above;
/// `q` is text in the message or target; `after` a timestamp for live tailing.
/// `matched`, `buffered` and `oldest` say how much there is and how far back
/// the buffer reaches.
pub async fn handle_get_logs(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200)
        .min(2000);

    let Some(ref buf) = state.log_buffer else {
        return Json(json!({ "count": 0, "entries": [], "matched": 0, "buffered": 0 }))
            .into_response();
    };
    let (include, exclude) = params
        .get("target")
        .map(|t| crate::logging::LogFilter::parse_targets(t))
        .unwrap_or_default();
    let filter = crate::logging::LogFilter {
        min_level: params.get("level").cloned(),
        include,
        exclude,
        quiet: params
            .get("quiet")
            .map(|q| {
                q.split(',')
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default(),
        text: params
            .get("q")
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty()),
        after: params.get("after").cloned().filter(|s| !s.is_empty()),
    };
    let page = buf.query(&filter, limit);
    Json(json!({
        "count": page.entries.len(),
        "entries": page.entries,
        "matched": page.matched,
        "buffered": page.buffered,
        "capacity": page.capacity,
        "oldest": page.oldest,
    }))
    .into_response()
}

/// GET /api/skills — live skill registry: each skill name with its tool list.
pub async fn handle_list_skills(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let Some(ref registry_arc) = state.tool_registry else {
        return Json(json!({ "skills": [] })).into_response();
    };
    let registry = registry_arc.read().await;
    let skills = registry.list_skills();
    Json(json!({ "skills": skills })).into_response()
}

/// GET /api/tools — the COMPLETE registered tool set.
///
/// Unlike MCP `tools/list`, this is NOT filtered by `MCP_TOOL_PROFILE`, so a UI
/// (the Scheduled Tasks step editor) can offer every tool a background step may
/// call — a scheduled step is dispatched through the skill registry regardless
/// of any chat profile. Returns a flat `tools` name list (for membership checks)
/// plus `groups` keyed by skill, each tool as `{name, description}`.
pub async fn handle_list_all_tools(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(ref registry_arc) = state.tool_registry else {
        return unavailable("Tool registry not available");
    };
    let registry = registry_arc.read().await;

    let groups: Vec<Value> = registry
        .skills_with_tools()
        .into_iter()
        .map(|(skill, tools)| {
            json!({
                "group": skill,
                "tools": tools
                    .iter()
                    .map(|t| json!({ "name": t.name, "description": t.description }))
                    .collect::<Vec<_>>(),
            })
        })
        .collect();

    let names: Vec<String> = registry.list().into_iter().map(|t| t.name).collect();

    Json(json!({ "count": names.len(), "tools": names, "groups": groups })).into_response()
}

// ============================================================================
// Agent Notification endpoints
// ============================================================================

/// GET /api/notifications?unread=true
pub async fn handle_list_notifications(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "No database"})),
        )
            .into_response();
    };
    let unread_only = params.get("unread").map(|v| v == "true").unwrap_or(false);
    match neo4j.list_notifications(unread_only).await {
        Ok(items) => Json(json!({ "notifications": items })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/notifications/{id}/read
pub async fn handle_mark_notification_read(
    Extension(state): Extension<Arc<RestState>>,
    Path(id): Path<String>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "No database"})),
        )
            .into_response();
    };
    match neo4j.mark_notification_read(&id).await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

/// POST /api/notifications/read-all
pub async fn handle_mark_all_notifications_read(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(ref neo4j) = state.neo4j else {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": "No database"})),
        )
            .into_response();
    };
    match neo4j.mark_all_notifications_read().await {
        Ok(()) => Json(json!({ "ok": true })).into_response(),
        Err(e) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": e.to_string()})),
        )
            .into_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn parse_since_normalises_to_utc() {
        let p = params(&[("since", "2026-10-08T10:00:00-04:00")]);
        let s = parse_since(&p).unwrap().unwrap();
        assert!(s.starts_with("2026-10-08T14:00:00"), "{s}");
        assert!(s.ends_with("+00:00"), "{s}");
    }

    #[test]
    fn parse_since_absent_or_blank_is_none() {
        assert_eq!(parse_since(&params(&[])).unwrap(), None);
        assert_eq!(parse_since(&params(&[("since", "  ")])).unwrap(), None);
    }

    #[test]
    fn parse_since_rejects_non_timestamps() {
        // Includes an injection attempt: a bad `since` is a 400, never Cypher.
        for bad in ["yesterday", "2026-10-08", "x') DETACH DELETE n //"] {
            let err = parse_since(&params(&[("since", bad)])).unwrap_err();
            assert!(err.contains("RFC 3339"), "{bad}: {err}");
        }
    }

    #[test]
    fn parse_instant_names_its_param_in_the_error() {
        let err =
            parse_instant(&params(&[("verified_since", "soon")]), "verified_since").unwrap_err();
        assert!(err.starts_with("`verified_since`"), "{err}");
    }

    #[test]
    fn parse_statuses_splits_and_trims() {
        assert_eq!(parse_statuses(&params(&[])), None);
        assert_eq!(parse_statuses(&params(&[("status", " , ")])), None);
        assert_eq!(
            parse_statuses(&params(&[("status", "failed, dead,dead_letter")])),
            Some(vec!["failed".into(), "dead".into(), "dead_letter".into()])
        );
        // A quote stays inside the bound value; it is never Cypher.
        assert_eq!(
            parse_statuses(&params(&[("status", "x'})")])),
            Some(vec!["x'})".into()])
        );
    }

    #[test]
    fn window_hours_defaults_and_clamps() {
        assert_eq!(window_hours(&params(&[])), 24);
        assert_eq!(window_hours(&params(&[("hours", "6")])), 6);
        assert_eq!(window_hours(&params(&[("hours", "0")])), 1);
        assert_eq!(window_hours(&params(&[("hours", "99999")])), 720);
        assert_eq!(window_hours(&params(&[("hours", "lots")])), 24);
    }

    #[test]
    fn where_clause_joins_or_is_empty() {
        assert_eq!(where_clause(&[]), "");
        assert_eq!(where_clause(&["a = $a"]), "WHERE a = $a ");
        assert_eq!(
            where_clause(&["a = $a", "b = $b"]),
            "WHERE a = $a AND b = $b "
        );
    }
}
