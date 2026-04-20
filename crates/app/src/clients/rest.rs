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

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::LlmConfig;
use crate::services::context_builder::ContextBuilderService;
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
    pub telemetry: Option<TelemetryClient>,
    pub log_buffer: Option<Arc<crate::logging::LogBuffer>>,
    pub tool_registry: Option<Arc<RwLock<crate::mcp::tools::ToolRegistry>>>,
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

    /// No-op kept for call-site compatibility. Todos now live in Neo4j.
    #[deprecated(note = "todos are now stored in Neo4j; use with_neo4j instead")]
    pub fn with_todo_store_opt(self, _store: Option<Arc<Neo4jClient>>) -> Self {
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
        Arc::new(RestState {
            neo4j: self.neo4j,
            scheduler: self.scheduler,
            context_builder: self.context_builder,
            llm_config: self.llm_config,
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
fn unavailable(msg: &str) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(serde_json::json!({"error": msg})),
    )
        .into_response()
}

/// Return a 500 JSON response.
fn internal(msg: impl std::fmt::Display) -> Response {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(serde_json::json!({"error": msg.to_string()})),
    )
        .into_response()
}

/// Return a 404 JSON response.
fn not_found() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(serde_json::json!({"error": "Not found"})),
    )
        .into_response()
}

// ============================================================================
// Todo handlers
// ============================================================================

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
        Ok(todos) => Json(serde_json::json!({"todos": todos})).into_response(),
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
        Ok(todo) => (
            StatusCode::CREATED,
            Json(serde_json::to_value(todo).unwrap_or_default()),
        )
            .into_response(),
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
        Ok(Some(todo)) => Json(serde_json::to_value(todo).unwrap_or_default()).into_response(),
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
        Ok(Some(todo)) => Json(serde_json::to_value(todo).unwrap_or_default()).into_response(),
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
        )
        .await
    {
        Ok(task) => (StatusCode::CREATED, Json(serde_json::json!(task))).into_response(),
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

    match neo4j
        .update_scheduled_task(
            &id,
            name,
            description,
            enabled,
            interval_seconds,
            steps_string.as_deref(),
            next_run_at,
        )
        .await
    {
        Ok(Some(task)) => Json(serde_json::json!(task)).into_response(),
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

/// GET /api/graph?max_nodes=N — knowledge graph nodes and edges for visualisation.
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

    let mut nodes: Vec<Value> = Vec::new();

    // Notes
    let note_rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (n:Note) \
             RETURN n.id AS id, n.content AS content, n.note_type AS note_type \
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
        let label: String = content.chars().take(60).collect();
        nodes.push(json!({ "id": id, "label": label, "type": "note", "note_type": note_type }));
    }

    // Entities (most-mentioned first)
    let entity_limit = (max_nodes / 4).max(20);
    let entity_rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (e:Entity)<-[r:MENTIONS]-() \
             RETURN e.id AS id, e.name AS name, e.entity_type AS entity_type, count(r) AS mentions \
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
        nodes
            .push(json!({ "id": id, "label": name, "type": "entity", "entity_type": entity_type }));
    }

    // Tasks
    let task_limit = (max_nodes / 8).max(10);
    let task_rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (t:Task) \
             RETURN t.id AS id, t.goal AS goal, t.status AS status \
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
        let label: String = goal.chars().take(60).collect();
        nodes.push(json!({ "id": id, "label": label, "type": "task", "status": status }));
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
             WHERE j.status = 'dead' \
               AND j.updated_at >= datetime() - duration({hours: 24}) \
             RETURN count(j) AS n",
        ))
        .await
        .ok()
        .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("n").ok()))
        .unwrap_or(0);

    Json(json!({
        "scheduler": scheduler_status,
        "queue": queue_stats,
        "knowledge": { "notes_by_type": serde_json::Value::Object(notes_by_type) },
        "tasks": { "by_status": serde_json::Value::Object(tasks_by_status) },
        "alerts": { "dead_jobs_last_24h": dead_24h },
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
                  RETURN c.id AS id, c.pattern AS pattern, c.priority AS priority, \
                         c.description AS description, c.steps AS steps \
                  ORDER BY c.priority ASC, c.pattern ASC";

    match neo4j.execute(neo4rs::query(cypher)).await {
        Ok(rows) => {
            let chains: Vec<Value> = rows
                .iter()
                .map(|row| {
                    let step_count = row
                        .get::<String>("steps")
                        .ok()
                        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                        .and_then(|v| v.as_array().map(|a| a.len()))
                        .unwrap_or(0);
                    json!({
                        "id":          row.get::<String>("id").unwrap_or_default(),
                        "pattern":     row.get::<String>("pattern").unwrap_or_default(),
                        "priority":    row.get::<i64>("priority").unwrap_or(100),
                        "description": row.get::<String>("description").unwrap_or_default(),
                        "step_count":  step_count,
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
                "tool_count":       p.tools.len(),
                "model_preference": p.model_preference,
                "provider_hint":    p.provider_hint,
            })
        })
        .collect();

    Json(json!({ "count": items.len(), "profiles": items })).into_response()
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
        }))
        .into_response(),
        None => not_found(),
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

    let (active_provider, active_model) = if let Some(ref cfg_arc) = state.llm_config {
        let cfg = cfg_arc.read().await;
        (
            cfg.as_ref()
                .map(|c| c.provider.to_string())
                .unwrap_or_else(|| "None".to_string()),
            cfg.as_ref().map(|c| c.model.clone()).unwrap_or_default(),
        )
    } else {
        ("None".to_string(), String::new())
    };

    let catalog_models = if let Some(ref db) = state.telemetry {
        db.list_models().ok().map(Value::Array).unwrap_or(json!([]))
    } else {
        json!([])
    };

    Json(json!({
        "active_provider": active_provider,
        "active_model":    active_model,
        "available_providers": [
            { "name": "Ollama (local)", "type": LlmProviderType::Ollama.to_string(),      "cost": "free" },
            { "name": "Ollama Cloud",   "type": LlmProviderType::OllamaCloud.to_string(), "cost": "usage-based" },
            { "name": "Anthropic",      "type": LlmProviderType::Anthropic.to_string(),   "cost": "paid" },
            { "name": "Gemini",         "type": LlmProviderType::Gemini.to_string(),      "cost": "paid" },
        ],
        "catalog_models": catalog_models,
    })).into_response()
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

    let status = params.get("status").map(String::as_str);
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(100)
        .min(500);

    let cypher = if let Some(s) = status {
        format!(
            "MATCH (j:AgentJob {{status: '{}'}}) \
             RETURN j ORDER BY j.created_at DESC LIMIT {}",
            s, limit
        )
    } else {
        format!(
            "MATCH (j:AgentJob) \
             RETURN j ORDER BY j.created_at DESC LIMIT {}",
            limit
        )
    };

    match neo4j.execute(neo4rs::query(&cypher)).await {
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
                        "created_at":    node.get::<String>("created_at").unwrap_or_default(),
                        "updated_at":    node.get::<String>("updated_at").unwrap_or_default(),
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

/// GET /api/notes?limit=<n>&note_type=<type>
///
/// List notes from Neo4j for the Knowledge panel. Read-only; agents use neo4j_query.
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

    let note_type_filter = params.get("note_type").cloned();

    let cypher = if let Some(ref nt) = note_type_filter {
        format!(
            "MATCH (n:Note {{note_type: '{}'}}) \
             RETURN n.id AS id, n.content AS content, n.note_type AS note_type, \
                    n.access_count AS access_count, toString(n.created_at) AS created_at \
             ORDER BY n.created_at DESC LIMIT {}",
            nt, limit
        )
    } else {
        format!(
            "MATCH (n:Note) \
             RETURN n.id AS id, n.content AS content, n.note_type AS note_type, \
                    n.access_count AS access_count, toString(n.created_at) AS created_at \
             ORDER BY n.created_at DESC LIMIT {}",
            limit
        )
    };

    match neo4j.execute(neo4rs::query(&cypher)).await {
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

    let q = neo4rs::query(
        "MATCH (n:Note {id: $id}) \
         RETURN n.id AS id, n.content AS content, n.note_type AS note_type, \
                n.access_count AS access_count, toString(n.created_at) AS created_at",
    )
    .param("id", note_id.clone());

    match neo4j.execute(q).await {
        Ok(rows) => {
            if let Some(row) = rows.into_iter().next() {
                Json(json!({
                    "id":           row.get::<String>("id").unwrap_or_default(),
                    "content":      row.get::<String>("content").unwrap_or_default(),
                    "note_type":    row.get::<String>("note_type").ok().filter(|s| !s.is_empty()),
                    "access_count": row.get::<i64>("access_count").unwrap_or(0),
                    "created_at":   row.get::<String>("created_at").unwrap_or_default(),
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
    let status_filter = params.get("status").cloned();

    let cypher = if let Some(ref s) = status_filter {
        format!(
            "MATCH (t:Task {{status: '{}'}}) \
             RETURN t.id AS id, t.goal AS goal, t.status AS status, \
                    t.context AS context, toString(t.created_at) AS created_at, \
                    [(t)-[:SUBTASK_OF]->(p) | p.id][0] AS parent_id \
             ORDER BY t.created_at DESC LIMIT {}",
            s, limit
        )
    } else {
        format!(
            "MATCH (t:Task) \
             RETURN t.id AS id, t.goal AS goal, t.status AS status, \
                    t.context AS context, toString(t.created_at) AS created_at, \
                    [(t)-[:SUBTASK_OF]->(p) | p.id][0] AS parent_id \
             ORDER BY t.created_at DESC LIMIT {}",
            limit
        )
    };

    match neo4j.execute(neo4rs::query(&cypher)).await {
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

/// GET /api/logs?limit=<n>&level=<info|warn|error|debug>
/// Returns recent in-process log lines from the ring buffer.
pub async fn handle_get_logs(
    Extension(state): Extension<Arc<RestState>>,
    Query(params): Query<HashMap<String, String>>,
) -> impl IntoResponse {
    let limit: usize = params
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(200);
    let level = params.get("level").map(|s| s.as_str());

    let Some(ref buf) = state.log_buffer else {
        return Json(json!({ "count": 0, "entries": [] })).into_response();
    };

    let entries = buf.recent(limit, level);
    Json(json!({ "count": entries.len(), "entries": entries })).into_response()
}

/// GET /api/skills — live skill registry: each skill name with its tool list.
pub async fn handle_list_skills(
    Extension(state): Extension<Arc<RestState>>,
) -> impl IntoResponse {
    let Some(ref registry_arc) = state.tool_registry else {
        return Json(json!({ "skills": [] })).into_response();
    };
    let registry = registry_arc.read().await;
    let skills = registry.list_skills();
    Json(json!({ "skills": skills })).into_response()
}
