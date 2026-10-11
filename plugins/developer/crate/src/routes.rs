//! `GET /api/plugins/developer/architecture`: the System → Architecture view,
//! computed live (`arch.rs`, HBI_REDESIGN_PLAN.md §4).
//!
//! Returns the layout's groups filled with live nodes, the nodes no group
//! claims (`unplaced`), the selectors that matched nothing (`stale`), the
//! edges and the container topology, the counts, what changed since the last
//! snapshot (live against stored, so it is never out of date), and the recent
//! snapshots with their own diffs.

use std::sync::Arc;

use agent_brain_plugin_api::Graph;
use agent_brain_plugin_api::axum::{
    Json, Router,
    extract::State,
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::json;

use crate::arch;

const HISTORY: i64 = 12;

pub fn router(graph: Arc<dyn Graph>) -> Router {
    Router::new()
        .route("/architecture", get(handle_architecture))
        .with_state(graph)
}

async fn handle_architecture(State(graph): State<Arc<dyn Graph>>) -> Response {
    let neo4j = graph.as_ref();
    let inv = match arch::collect(neo4j).await {
        Ok(i) => i,
        Err(e) => {
            return (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({ "error": e.to_string() })),
            )
                .into_response();
        }
    };
    let providers = arch::providers();
    let layout = arch::layout();
    let placement = arch::place(
        layout,
        arch::build_nodes(&inv, &providers, &arch::components()),
    );

    let latest = arch::latest(neo4j).await.unwrap_or_else(|e| {
        tracing::warn!(error = %e, "architecture: latest snapshot unreadable");
        None
    });
    let since_snapshot = latest.as_ref().map(|(s, old)| {
        let d = arch::diff(old, &inv);
        json!({
            "snapshot_id": s.id,
            "taken_at": s.taken_at,
            "version": s.version,
            "summary": arch::summarize(&d),
            "diff": d,
        })
    });
    let history = arch::history(neo4j, HISTORY).await.unwrap_or_default();

    Json(json!({
        "generated_at": chrono::Utc::now().to_rfc3339(),
        "counts": inv.counts(),
        "groups": placement.groups,
        "unplaced": placement.unplaced,
        "stale": placement.stale,
        "edges": layout.edges,
        "topology": layout.topology,
        "layout_loaded": !layout.groups.is_empty(),
        "since_snapshot": since_snapshot,
        "history": history,
    }))
    .into_response()
}
