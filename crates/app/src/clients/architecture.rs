//! `GET /api/architecture`: the System → Architecture view, computed live
//! (`services/architecture.rs`, HBI_REDESIGN_PLAN.md §4).
//!
//! Returns the layout's groups filled with live nodes, the nodes no group
//! claims (`unplaced`), the selectors that matched nothing (`stale`), the
//! edges and the container topology, the counts, what changed since the last
//! snapshot (live against stored, so it is never out of date), and the recent
//! snapshots with their own diffs.

use std::sync::Arc;

use axum::{Extension, Json, response::IntoResponse};
use serde_json::json;

use super::rest::{RestState, internal, unavailable};
use crate::services::architecture as arch;

const HISTORY: i64 = 12;

pub async fn handle_architecture(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let Some(neo4j) = state.neo4j.as_ref() else {
        return unavailable("Neo4j not available");
    };
    let inv = match arch::collect(neo4j).await {
        Ok(i) => i,
        Err(e) => return internal(e),
    };
    let providers = crate::services::provider_registry::snapshot_json();
    let layout = arch::layout();
    let placement = arch::place(
        layout,
        arch::build_nodes(&inv, &providers, arch::components()),
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
