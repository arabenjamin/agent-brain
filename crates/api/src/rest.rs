//! REST handlers — todos, workflows, scheduler config and the rest of `/api/*`.
//! Routes are declared in `mcp/http_transport.rs`; [`RestAdapter`] builds the
//! shared [`RestState`] they read through Axum's `Extension`.
//!
//! This file holds the state and the helpers every handler uses. `rest/` has a
//! file per resource.

use std::collections::HashMap;
use std::sync::Arc;

use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use tokio::sync::RwLock;

use agent_brain_llm::LlmConfig;
use agent_brain_repository::{Neo4jClient, TelemetryClient};
use agent_brain_scheduler::scheduler::SchedulerService;
use agent_brain_work::context_builder::ContextBuilderService;

mod graph_panel;
mod models;
mod notes;
mod ops;
mod profiles;
mod sessions;
mod todos;
mod workflows;
pub use graph_panel::*;
pub use models::*;
pub use notes::*;
pub use ops::*;
pub use profiles::*;
pub use sessions::*;
pub use todos::*;
pub use workflows::*;

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
    pub log_buffer: Option<Arc<agent_brain_graph::logging::LogBuffer>>,
    pub tool_registry: Option<Arc<RwLock<agent_brain_work::tools::ToolRegistry>>>,
    /// Read-only access to the retrieval pipeline for the HBI's search
    /// (`clients/search.rs`). Built from `neo4j` + `llm_config`; embeddings
    /// go wherever that config sends them (the local engine), as for every search.
    pub knowledge: Option<Arc<agent_brain_knowledge::knowledge::KnowledgeService>>,
}

// ============================================================================
// Adapter / builder
// ============================================================================

/// Builder for the REST adapter.
///
/// Attach resources and call `build_state` for the handlers' shared state.
#[derive(Default)]
pub struct RestAdapter {
    neo4j: Option<Arc<Neo4jClient>>,
    scheduler: Option<Arc<RwLock<Option<Arc<SchedulerService>>>>>,
    context_builder: Option<Arc<RwLock<Option<Arc<ContextBuilderService>>>>>,
    llm_config: Option<Arc<RwLock<Option<LlmConfig>>>>,
    chat_llm_config: Option<Arc<RwLock<Option<LlmConfig>>>>,
    telemetry: Option<TelemetryClient>,
    log_buffer: Option<Arc<agent_brain_graph::logging::LogBuffer>>,
    tool_registry: Option<Arc<RwLock<agent_brain_work::tools::ToolRegistry>>>,
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

    pub fn with_log_buffer_opt(
        mut self,
        buf: Option<Arc<agent_brain_graph::logging::LogBuffer>>,
    ) -> Self {
        self.log_buffer = buf;
        self
    }

    pub fn with_tool_registry_opt(
        mut self,
        registry: Option<Arc<RwLock<agent_brain_work::tools::ToolRegistry>>>,
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
            (Some(n), Some(cfg)) => Some(Arc::new(
                agent_brain_knowledge::knowledge::KnowledgeService::new(
                    (**n).clone(),
                    Some(agent_brain_llm::shared_llm::SharedLlm::new(Arc::clone(cfg))),
                ),
            )),
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
}

// ============================================================================
// Helpers
// ============================================================================

/// The graph client from a handler's state, or return a 503 from the handler:
/// `let neo4j = graph!(state);` (default message) or
/// `graph!(state, "Todo storage not available")`.
macro_rules! graph {
    ($state:expr) => {
        graph!($state, "Neo4j not available")
    };
    ($state:expr, $msg:expr) => {
        match $state.neo4j.as_ref() {
            Some(n) => n,
            None => return $crate::rest::unavailable($msg),
        }
    };
}
pub(crate) use graph;

/// The `Ok` value, or return a 500 from the handler: `or_500!(expr)` reports
/// the error, `or_500!(expr, "Failed to list notes")` prefixes it.
macro_rules! or_500 {
    ($e:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return $crate::rest::internal(e),
        }
    };
    ($e:expr, $prefix:expr) => {
        match $e {
            Ok(v) => v,
            Err(e) => return $crate::rest::internal(format!("{}: {e}", $prefix)),
        }
    };
}
pub(crate) use or_500;

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
// Agent Notification endpoints
// ============================================================================

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
