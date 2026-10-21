//! Where a linked item is (MAP_PLAN Phase 1).
//!
//! An item's coordinate location (Wikidata P625) is stored on its
//! `(:WikiItem)` as `location`, a native Neo4j point (WGS 84), with
//! `coord_count` (how many coordinate claims it had) and `coords_at` (when it
//! was read). New lookups store it with the rest of the item (`wiki.rs`);
//! this fills the items stored before, through the same Wikimedia gate, a
//! budget of requests at a time. An item with no coordinate is marked read
//! too, so it is not asked again; a point comes back when the item is
//! fetched afresh.
//!
//! The backfill runs on its own idle loop ([`spawn`]), not inside the
//! entity-linking pass: that workflow needs a model and has been off since
//! 2026-10-11, while reading coordinates needs none.

use std::time::Duration;

use anyhow::Result;
use serde::Serialize;
use serde_json::Value;
use tracing::{info, warn};

use agent_brain_plugin_api::PluginContext;

use crate::db::{Db, query};
use crate::wiki;

/// Requests per background pass: 50 items each.
pub const BACKGROUND_BUDGET: usize = 10;
const BATCH: i64 = 500;

#[derive(Debug, Default, Serialize)]
pub struct Located {
    pub items_checked: usize,
    pub with_point: usize,
    pub items_waiting: i64,
    /// Requests ran out (or Wikimedia throttled us); the rest wait.
    pub incomplete: bool,
}

/// Read coordinates for linked items that have none read yet (`qid` = only
/// that one), spending at most `budget` Wikimedia requests.
pub async fn refresh(neo4j: &Db, qid: Option<&str>, budget: usize) -> Result<Located> {
    let unread = "MATCH (:Entity)-[l:REFERS_TO]->(w:WikiItem) \
                  WHERE l.status IN ['confirmed', 'proposed'] AND w.coords_at IS NULL \
                    AND ($q IS NULL OR w.qid = $q)";
    let qids: Vec<String> = neo4j
        .execute(
            query(&format!("{unread} RETURN DISTINCT w.qid AS qid LIMIT $n"))
                .param("q", qid.map(String::from))
                .param("n", BATCH),
        )
        .await?
        .iter()
        .filter_map(|r| r.get::<String>("qid").ok())
        .collect();
    let mut out = Located::default();
    let mut budget = budget;
    for chunk in qids.chunks(50) {
        if budget == 0 {
            out.incomplete = true;
            break;
        }
        budget -= 1;
        let found = match wiki::coords_of(chunk).await {
            Ok(f) => f,
            Err(e) => {
                warn!(error = %e, "geo: coordinates not fetched");
                out.incomplete = true;
                break;
            }
        };
        let rows: Vec<Value> = chunk
            .iter()
            .map(|q| {
                let (p, n) = found.get(q).copied().unwrap_or((None, 0));
                let mut m = serde_json::Map::new();
                m.insert("qid".into(), q.clone().into());
                m.insert("lat".into(), p.map(|p| p.lat.into()).unwrap_or(Value::Null));
                m.insert("lon".into(), p.map(|p| p.lon.into()).unwrap_or(Value::Null));
                m.insert("n".into(), n.into());
                Value::Object(m)
            })
            .collect();
        out.items_checked += chunk.len();
        out.with_point += rows.iter().filter(|r| !r["lat"].is_null()).count();
        neo4j
            .run(
                query(
                    "UNWIND $rows AS r MATCH (w:WikiItem {qid: r.qid}) \
                     SET w.location = CASE WHEN r.lat IS NULL THEN null \
                                           ELSE point({latitude: r.lat, longitude: r.lon}) END, \
                         w.coord_count = r.n, w.coords_at = datetime()",
                )
                .param("rows", rows),
            )
            .await?;
    }
    out.items_waiting = neo4j
        .execute(
            query(&format!("{unread} RETURN count(DISTINCT w) AS c"))
                .param("q", qid.map(String::from)),
        )
        .await?
        .first()
        .and_then(|r| r.get::<i64>("c").ok())
        .unwrap_or(0);
    Ok(out)
}

/// The idle loop: every half hour, unless foreground work is running, read
/// coordinates for linked items that have none read yet. Only in the
/// deployed brain; nothing to read costs one graph query.
pub fn spawn(ctx: &PluginContext, neo4j: Db) {
    if !ctx.background_allowed() {
        return;
    }
    if wiki::contact().is_none() {
        info!("geo: WIKIMEDIA_CONTACT is not set; coordinates are not read in the background");
        return;
    }
    let ctx = ctx.clone();
    tokio::spawn(async move {
        const FIRST: Duration = Duration::from_secs(120);
        const EVERY: Duration = Duration::from_secs(30 * 60);
        tokio::time::sleep(FIRST).await;
        loop {
            if !ctx.busy().await {
                match refresh(&neo4j, None, BACKGROUND_BUDGET).await {
                    Ok(g) if g.items_checked > 0 => info!(
                        checked = g.items_checked,
                        with_point = g.with_point,
                        waiting = g.items_waiting,
                        "geo: coordinates read"
                    ),
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "geo: background pass failed"),
                }
            }
            tokio::time::sleep(EVERY).await;
        }
    });
}
