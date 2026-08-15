//! Reading native Neo4j temporal values back into the `String` model fields.
//!
//! Every timestamp in the graph is a native `DATETIME` (see the migration note
//! in `CLAUDE.md`), but the model structs — `Task`, `AgentJob`, `ScheduledTask`,
//! `Todo` — expose them as ISO-8601 `String`, which is what the REST and MCP
//! payloads have always carried. Reads that project explicit columns can bridge
//! that with `toString(prop)` in Cypher; reads that pull a whole node
//! (`RETURN j`) cannot, and this module covers those.

use chrono::{DateTime, FixedOffset, SecondsFormat};
use neo4rs::{Node, query};

use crate::{Neo4jClient, Result};

/// A temporal property still stored as a string: `(label, property, count)`.
pub type StringTimestamp = (String, String, i64);

impl Neo4jClient {
    /// Find temporal properties still stored as strings rather than datetimes.
    ///
    /// The graph used to store timestamps both ways at once, and the two do not
    /// compare: Cypher evaluates `datetime_property >= 'some string'` to null,
    /// so a date filter written against the wrong representation matches
    /// nothing and reports no error. Every consumer reads that as "no data".
    ///
    /// Nothing in Neo4j enforces a property's type, so the split can return one
    /// missed `SET` at a time. This is the check that makes it visible —
    /// `main.rs` warns on a non-empty result at startup, and the
    /// `no_string_timestamps` integration test asserts it is empty.
    ///
    /// A property is temporal by name (`*_at`, `*_time`, `timestamp`), which is
    /// the convention this codebase already follows everywhere.
    pub async fn string_timestamp_violations(&self) -> Result<Vec<StringTimestamp>> {
        let rows = self
            .execute(query(
                "MATCH (n) UNWIND keys(n) AS k \
                 WITH labels(n)[0] AS label, k, valueType(n[k]) AS t \
                 WHERE (k =~ '.*(_at|_time)$' OR k = 'timestamp') \
                   AND t STARTS WITH 'STRING' \
                 RETURN label, k AS property, count(*) AS count \
                 ORDER BY count DESC",
            ))
            .await?;

        Ok(rows
            .iter()
            .filter_map(|r| {
                Some((
                    r.get::<String>("label").ok()?,
                    r.get::<String>("property").ok()?,
                    r.get::<i64>("count").ok()?,
                ))
            })
            .collect())
    }
}

/// Read a temporal node property as an ISO-8601 string.
///
/// Accepts both a native datetime and a legacy string, deliberately. The data
/// migration is not atomic with the redeploy that depends on it, and a row that
/// cannot parse its own `created_at` is a row the scheduler silently drops. The
/// string arm is a compatibility path, not the expected one — the
/// `no_string_timestamps` guard test is what asserts the graph has converged.
///
/// Returns `None` for absent, empty, or unreadable values so callers keep the
/// `Option` distinction between "no timestamp" and "the epoch".
///
/// Output is normalised to Neo4j's own `toString()` form (UTC rendered as `Z`)
/// so a timestamp reads identically whether it arrived through this helper or
/// through an aliased column.
pub fn node_ts(node: &Node, key: &str) -> Option<String> {
    if let Ok(dt) = node.get::<DateTime<FixedOffset>>(key) {
        return Some(dt.to_rfc3339_opts(SecondsFormat::AutoSi, true));
    }
    node.get::<String>(key).ok().filter(|s| !s.is_empty())
}
