//! `GET /api/feed`: one list of what wants the user (HBI plan §9.3).
//!
//! Three kinds of row:
//! - **items** the user acts on: open and theirs, or awaiting their approval
//!   or review (`items::for_user`), less the snoozed ones;
//! - **reads**: unread notifications that ask nothing (`needs_action` ones are
//!   represented by their item);
//! - **digests**: one card per kind of system signal that is not a row of its
//!   own (entity merges, extracted relationships and entity types to decide, tasks failed and jobs died this week,
//!   schedules with steps that can't run), each with the count behind it.
//!
//! `total` is the number of rows, and is the one number the nav badge, Home's
//! Needs-you tile and the Feed header show, so they cannot disagree.

use std::sync::Arc;

use axum::{Extension, Json, response::IntoResponse};
use chrono::{DateTime, Utc};
use serde_json::{Value, json};

use super::rest::{RestState, graph};
use crate::repository::Todo;
use crate::services::items;

/// Where an item sits in the Feed: overdue first, then awaiting the user,
/// then the rest; newest first within each. `today` is the local date.
pub(crate) fn item_rank(t: &Todo, today: &str) -> (u8, std::cmp::Reverse<String>) {
    let overdue = overdue(t, today);
    let band = if overdue {
        0
    } else if matches!(t.status.as_str(), "awaiting_approval" | "awaiting_review") {
        1
    } else {
        2
    };
    (band, std::cmp::Reverse(t.created_at.clone()))
}

fn parse(ts: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(ts)
        .ok()
        .map(|d| d.with_timezone(&Utc))
}

/// A due date is a calendar date (stored as that date at 00:00 UTC): overdue
/// once it is before today's *local* date, the rule the HBI uses
/// (`isTodoOverdue`). Comparing instants made a todo due today overdue all day
/// anywhere west of UTC.
pub(crate) fn overdue(t: &Todo, today: &str) -> bool {
    t.status != "done"
        && t.due_at
            .as_deref()
            .and_then(|d| d.get(..10))
            .is_some_and(|d| d < today)
}

pub(crate) fn snoozed(t: &Todo, now: DateTime<Utc>) -> bool {
    t.snoozed_until
        .as_deref()
        .and_then(parse)
        .is_some_and(|d| d > now)
}

async fn count(neo4j: &crate::repository::Neo4jClient, cypher: &str) -> Result<i64, String> {
    neo4j
        .execute(neo4rs::query(cypher))
        .await
        .map_err(|e| e.to_string())
        .map(|r| r.first().and_then(|r| r.get::<i64>("c").ok()).unwrap_or(0))
}

pub async fn handle_feed(Extension(state): Extension<Arc<RestState>>) -> impl IntoResponse {
    let neo4j = graph!(state);
    let now = Utc::now();
    let today = crate::services::clock::today();
    let mut errors: Vec<Value> = Vec::new();

    // Items.
    let (mut mine, snoozed_n): (Vec<Todo>, usize) = match neo4j.list_todos(None).await {
        Ok(all) => {
            let open: Vec<Todo> = all.into_iter().filter(items::for_user).collect();
            let n = open.iter().filter(|t| snoozed(t, now)).count();
            (open.into_iter().filter(|t| !snoozed(t, now)).collect(), n)
        }
        Err(e) => {
            errors.push(json!({ "part": "items", "error": e.to_string() }));
            (Vec::new(), 0)
        }
    };
    mine.sort_by_key(|t| item_rank(t, &today));
    let item_rows: Vec<Value> = mine
        .iter()
        .map(|t| {
            let mut v = items::item_json(t);
            v["overdue"] = json!(overdue(t, &today));
            v
        })
        .collect();

    // Reads.
    let reads: Vec<Value> = match neo4j.list_notifications(true).await {
        Ok(n) => n
            .into_iter()
            .filter(|n| !n["needs_action"].as_bool().unwrap_or(false))
            .collect(),
        Err(e) => {
            errors.push(json!({ "part": "reads", "error": e.to_string() }));
            Vec::new()
        }
    };

    // Digests: one card per signal, only when there is something.
    let mut digests: Vec<Value> = Vec::new();
    let mut digest = |kind: &str, title: String, n: Result<i64, String>, drill: Value| match n {
        Ok(0) => {}
        Ok(n) => digests.push(json!({ "kind": kind, "title": title, "count": n, "drill": drill })),
        Err(e) => errors.push(json!({ "part": kind, "error": e })),
    };
    let merges = crate::services::entity_resolution::candidates(neo4j, None, 1)
        .await
        .map(|v| v["total"].as_i64().unwrap_or(0))
        .map_err(|e| e.to_string());
    digest(
        "entity_merges",
        "entity merges to decide".into(),
        merges,
        json!({ "view": "feed", "mode": "merges" }),
    );
    // Typed relationships are the epistemics plugin's: with it off, its
    // review view is gone, so a card would open nothing.
    if crate::services::plugins::Plugins::global().is_loaded("epistemics") {
        digest(
            "relations",
            "relationships to review".into(),
            count(
                neo4j,
                "MATCH ()-[r:RELATES {status: 'proposed'}]->() RETURN count(r) AS c",
            )
            .await,
            json!({ "view": "feed", "mode": "relations" }),
        );
    }
    digest(
        "entity_types",
        "entity types to check".into(),
        count(
            neo4j,
            "MATCH (e:Entity) WHERE e.type_question_qid IS NOT NULL RETURN count(e) AS c",
        )
        .await,
        json!({ "view": "feed", "mode": "types" }),
    );
    digest(
        "failed_tasks",
        "tasks failed this week".into(),
        count(
            neo4j,
            "MATCH (t:Task {status: 'failed'}) WHERE t.created_at >= datetime() - duration('P7D') RETURN count(t) AS c",
        )
        .await,
        json!({ "view": "history", "mode": "tasks", "task_status": "failed" }),
    );
    digest(
        "dead_jobs",
        "jobs died this week".into(),
        count(
            neo4j,
            "MATCH (j:AgentJob) WHERE j.status IN ['failed', 'dead', 'dead_letter'] \
             AND j.created_at >= datetime() - duration('P7D') RETURN count(j) AS c",
        )
        .await,
        json!({ "view": "history", "mode": "jobs", "job_status": "failed,dead,dead_letter" }),
    );
    // Workflows the plugin loader marked unavailable at startup: a tool not
    // registered, an MCP server missing or disabled, a plugin not enabled.
    let broken = count(
        neo4j,
        "MATCH (w:Workflow) WHERE w.unavailable IS NOT NULL RETURN count(w) AS c",
    )
    .await;
    digest(
        "broken_workflows",
        "workflows that can't run".into(),
        broken,
        json!({ "view": "scheduled" }),
    );

    let total = item_rows.len() + reads.len() + digests.len();
    Json(json!({
        "total": total,
        "items": item_rows,
        "snoozed": snoozed_n,
        "reads": reads,
        "digests": digests,
        "errors": errors,
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(status: &str, due: Option<&str>, created: &str) -> Todo {
        serde_json::from_value(json!({
            "id": created, "title": "t", "description": null, "status": status, "priority": 2,
            "tags": [], "due_at": due, "created_at": created, "updated_at": created,
        }))
        .unwrap()
    }

    #[test]
    fn overdue_comes_first_then_awaiting_then_newest() {
        let mut list = [
            todo("pending", None, "2026-10-01T00:00:00Z"),
            todo("awaiting_review", None, "2026-10-02T00:00:00Z"),
            todo(
                "pending",
                Some("2026-10-08T00:00:00Z"),
                "2026-09-01T00:00:00Z",
            ),
            todo("pending", None, "2026-10-05T00:00:00Z"),
        ];
        list.sort_by_key(|t| item_rank(t, "2026-10-09"));
        let order: Vec<&str> = list.iter().map(|t| t.id.as_str()).collect();
        assert_eq!(
            order,
            [
                "2026-09-01T00:00:00Z",
                "2026-10-02T00:00:00Z",
                "2026-10-05T00:00:00Z",
                "2026-10-01T00:00:00Z"
            ]
        );
    }

    #[test]
    fn a_todo_due_today_is_not_overdue() {
        let t = todo("pending", Some("2026-10-09T00:00:00Z"), "x");
        assert!(!overdue(&t, "2026-10-09"));
        assert!(overdue(&t, "2026-10-10"));
        assert!(!overdue(
            &todo("done", Some("2026-01-01T00:00:00Z"), "x"),
            "2026-10-10"
        ));
    }

    #[test]
    fn snoozed_items_hide_until_their_time() {
        let now = parse("2026-10-09T12:00:00Z").unwrap();
        let mut t = todo("pending", None, "2026-10-01T00:00:00Z");
        t.snoozed_until = Some("2026-10-10T09:00:00Z".into());
        assert!(snoozed(&t, now));
        t.snoozed_until = Some("2026-10-09T09:00:00Z".into());
        assert!(!snoozed(&t, now));
    }

    #[test]
    fn the_user_acts_on_their_own_and_on_anything_awaiting_them() {
        let mut t = todo("pending", None, "x");
        assert!(items::for_user(&t), "an unassigned todo is the user's");
        t.assignee = Some("agent:researcher".into());
        assert!(!items::for_user(&t));
        t.status = "awaiting_review".into();
        assert!(items::for_user(&t));
        t.status = "done".into();
        assert!(!items::for_user(&t));
    }
}
