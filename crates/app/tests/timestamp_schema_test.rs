//! Guards the one-representation rule for timestamps.
//!
//! Every temporal property in the graph must be a native Neo4j `DATETIME`.
//! When some are datetimes and others are strings, a date filter written for
//! one silently matches nothing against the other — Cypher compares the two as
//! null, so the query succeeds, returns zero rows, and every consumer reads
//! that as "no data". That is not a hypothetical: it made the brain answer
//! "my memory is clear of any new learnings" on a day with 504 new notes, and
//! it made `find_similar_tasks` return 0 instead of 815, silently disabling
//! task deduplication for months.
//!
//! Neo4j does not enforce property types, so nothing but this check stops a
//! single missed `datetime()` on a write from reintroducing the split.
//!
//! Requires a running Neo4j; skips (rather than fails) when unreachable, to
//! match the other live-DB tests in this directory.

use agent_brain::repository::Neo4jClient;

async fn connect() -> Option<Neo4jClient> {
    let uri = std::env::var("NEO4J_URI").unwrap_or_else(|_| "bolt://localhost:7688".to_string());
    let user = std::env::var("NEO4J_USER").unwrap_or_else(|_| "neo4j".to_string());
    let pass = std::env::var("NEO4J_PASSWORD").unwrap_or_else(|_| "password".to_string());

    match Neo4jClient::new(&uri, &user, &pass).await {
        Ok(c) => Some(c),
        Err(e) => {
            println!("Skipping live DB test: could not connect to Neo4j at {uri}: {e}");
            None
        }
    }
}

#[tokio::test]
async fn no_string_timestamps() {
    let Some(client) = connect().await else {
        return;
    };

    let violations = match client.string_timestamp_violations().await {
        Ok(v) => v,
        Err(e) => {
            println!("Skipping live DB test: violation query failed (Neo4j unreachable?): {e}");
            return;
        }
    };

    assert!(
        violations.is_empty(),
        "temporal properties are stored as STRING instead of DATETIME — date \
         filters over these silently match nothing.\nRun \
         scripts/migrate_timestamps_to_datetime.cypher.\nOffenders: {}",
        violations
            .iter()
            .map(|(label, prop, count)| format!("{label}.{prop} ({count} nodes)"))
            .collect::<Vec<_>>()
            .join(", ")
    );
}

/// The bug that started this: a datetime property compared against a string
/// literal matches nothing, and Neo4j reports no error for it.
///
/// Asserting the mismatch behaviour directly means the test stays meaningful
/// even if every current caller is fixed — it documents *why* the rule above
/// has to hold, rather than just restating it.
#[tokio::test]
async fn datetime_vs_string_comparison_matches_nothing() {
    let Some(client) = connect().await else {
        return;
    };

    // `Neo4jClient::new` connects lazily, so an unreachable database surfaces
    // here rather than at connect() — skip on error like the tests above.
    let rows = match client
        .execute(neo4rs::query(
            "WITH datetime('2026-08-13T00:00:00Z') AS dt \
             RETURN (dt >= '2026-01-01T00:00:00Z') AS vs_string, \
                    (dt >= datetime('2026-01-01T00:00:00Z')) AS vs_datetime",
        ))
        .await
    {
        Ok(r) => r,
        Err(e) => {
            println!("Skipping live DB test: comparison query failed: {e}");
            return;
        }
    };

    let row = rows.first().expect("one row");

    // Not `false` — null. The predicate is neither true nor false, so a WHERE
    // clause drops the row without anything surfacing as an error.
    assert!(
        row.get::<Option<bool>>("vs_string")
            .unwrap_or(None)
            .is_none(),
        "datetime >= string should evaluate to null, not a boolean"
    );
    assert_eq!(
        row.get::<Option<bool>>("vs_datetime").unwrap_or(None),
        Some(true),
        "datetime >= datetime should compare normally"
    );
}
