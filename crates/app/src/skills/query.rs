//! QuerySkill — generic Neo4j (Cypher) and DuckDB (SQL) query primitives.
//!
//! These tools give the agent direct read access to both databases without
//! requiring a purpose-built Rust tool for every possible query.  Write access
//! to Neo4j is guarded by a keyword allowlist; DuckDB is always read-only.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

pub struct QuerySkill {
    neo4j: Option<Neo4jClient>,
    telemetry: Option<TelemetryClient>,
}

/// True when a row-returning query has no `LIMIT` of its own and should get one.
///
/// Both halves are matched on **whitespace-separated words**, not substrings.
/// The substring form (`upper.contains(" LIMIT ")`) misses a `LIMIT` that opens
/// its own line — exactly how a model formats multi-line Cypher — and the tool
/// then appended a second one, producing `LIMIT 20 LIMIT 100`. Neo4j reports
/// that as `RETURN can only be used at the end of the query`, pointing at the
/// RETURN rather than the duplicated clause, so the caller sees a syntax error
/// in Cypher it wrote correctly and has no way to reach the real cause.
fn needs_limit_injection(cypher: &str) -> bool {
    let mut returns = false;
    let mut limits = false;
    for word in cypher.split_whitespace() {
        // Trim punctuation so `RETURN(` / `LIMIT;` still count as the keyword.
        let w = word
            .trim_matches(|c: char| !c.is_alphanumeric() && c != '_')
            .to_uppercase();
        match w.as_str() {
            "RETURN" => returns = true,
            "LIMIT" => limits = true,
            _ => {}
        }
    }
    returns && !limits
}

/// Advisory note attached to an empty result set that filtered on a date literal.
///
/// A Cypher comparison between a temporal value and a string evaluates to null,
/// so the predicate simply matches nothing — **no error, no warning, zero rows**.
/// That is indistinguishable from "the data isn't there", and the caller reports
/// the absence as fact. Observed: `WHERE n.created_at >= '2026-08-13T00:00:00Z'`
/// returned 0 while `datetime('2026-08-13T00:00:00Z')` returned 504.
///
/// Every temporal property is now a native `DATETIME` (see the schema rule in
/// `project-docs/schema.md`), so the storage half of this is fixed and the hint
/// no longer has to talk about mixed types. What the schema cannot fix is the
/// *caller* writing a string literal on the other side of the comparison —
/// that still evaluates to null, and it is the shape this hint catches.
///
/// Kept purely syntactic: it fires on the query text, not on a table of
/// property types, so it cannot go stale against the schema.
fn empty_result_hint(cypher: &str) -> Option<String> {
    if !contains_quoted_date_literal(cypher) {
        return None;
    }
    Some(
        "0 rows, and this query compares against a quoted date literal. Every \
         timestamp in this graph is a native Neo4j datetime, and a datetime \
         compared to a string evaluates to null — the predicate matches nothing \
         and raises no error, so this empty result may be a type mismatch rather \
         than missing data. Wrap the literal: `WHERE n.created_at >= \
         datetime('2026-08-13T00:00:00Z')`, or `datetime() - duration({days: 7})` \
         for a relative window. Re-run before concluding the data is absent."
            .to_string(),
    )
}

/// True when the query contains a `YYYY-MM-DD` run inside a quoted literal.
fn contains_quoted_date_literal(cypher: &str) -> bool {
    let b = cypher.as_bytes();
    // Track quoting so a bare date in a property name or comment doesn't count.
    let mut quote: Option<u8> = None;
    for (i, &c) in b.iter().enumerate() {
        match quote {
            Some(q) => {
                if c == q && (i == 0 || b[i - 1] != b'\\') {
                    quote = None;
                } else if looks_like_date_at(b, i) {
                    return true;
                }
            }
            None => {
                if c == b'\'' || c == b'"' {
                    quote = Some(c);
                }
            }
        }
    }
    false
}

/// True when `YYYY-MM-DD` starts at byte `i`.
fn looks_like_date_at(b: &[u8], i: usize) -> bool {
    if i + 10 > b.len() {
        return false;
    }
    let d = |n: usize| b[i + n].is_ascii_digit();
    d(0) && d(1)
        && d(2)
        && d(3)
        && b[i + 4] == b'-'
        && d(5)
        && d(6)
        && b[i + 7] == b'-'
        && d(8)
        && d(9)
}

impl QuerySkill {
    pub fn new(neo4j: Option<Neo4jClient>, telemetry: Option<TelemetryClient>) -> Self {
        Self { neo4j, telemetry }
    }

    // =========================================================================
    // Tool definitions
    // =========================================================================

    fn neo4j_query_def() -> ToolDefinition {
        ToolDefinition {
            name: "neo4j_query".to_string(),
            description: "Execute a Cypher query against Neo4j. \
                          Read-only by default (readonly=true); set readonly=false to allow \
                          CREATE/MERGE/SET/DELETE. Use params for safe parameter binding. \
                          TIMESTAMPS: every temporal property (created_at, updated_at, \
                          next_review_at, asserted_at, next_run_at, …) is a native Neo4j \
                          DATETIME in UTC — never a string. Comparing one to a quoted \
                          string evaluates to null, so the row is dropped and you get zero \
                          results with no error. Always wrap the literal: \
                          `WHERE n.created_at >= datetime('2026-08-13T00:00:00Z')`, or \
                          `WHERE n.created_at >= datetime() - duration({days: 7})`. \
                          Use `toString(n.created_at)` when you want it rendered as text."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "cypher": {
                        "type": "string",
                        "description": "Cypher query string"
                    },
                    "params": {
                        "type": "object",
                        "description": "Query parameters as key-value pairs (string values only)"
                    },
                    "readonly": {
                        "type": "boolean",
                        "description": "Reject write keywords when true (default: true)"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum rows to return (default: 100)"
                    }
                },
                "required": ["cypher"]
            }),
        }
    }

    fn duckdb_query_def() -> ToolDefinition {
        ToolDefinition {
            name: "duckdb_query".to_string(),
            description: "Execute a read-only SQL SELECT query against the DuckDB analytics \
                          database (telemetry, model usage stats, interaction logs). \
                          Tables: model_usage, model_registry, interactions, knowledge_gaps."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "sql": {
                        "type": "string",
                        "description": "SQL SELECT query"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Maximum rows to return (default: 100)"
                    }
                },
                "required": ["sql"]
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    async fn handle_neo4j_query(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            cypher: String,
            #[serde(default)]
            params: Option<serde_json::Map<String, Value>>,
            #[serde(default = "default_true")]
            readonly: bool,
            #[serde(default = "default_limit")]
            limit: usize,
        }
        fn default_true() -> bool {
            true
        }
        fn default_limit() -> usize {
            100
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        // Safety guard for read-only mode — checked before the DB call so it
        // fires even when Neo4j is unavailable (fail-fast on bad input).
        if input.readonly {
            let upper = input.cypher.to_uppercase();
            for kw in &[
                "CREATE", "MERGE", "SET", "DELETE", "REMOVE", "DETACH", "DROP",
            ] {
                // Match the keyword as a whole word: exact equality or followed by a
                // non-word char (e.g. "CREATE(" is still a write op). This prevents
                // false positives on identifiers like "created_at" or "CREATED_AT,".
                if upper.split_whitespace().any(|w| {
                    w == *kw
                        || (w.starts_with(kw)
                            && w[kw.len()..]
                                .starts_with(|c: char| !c.is_alphanumeric() && c != '_'))
                }) {
                    return ToolCallResult::error(format!(
                        "Write keyword '{}' rejected in readonly mode. Pass readonly=false to allow writes.",
                        kw
                    ));
                }
            }
        }

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        // Inject LIMIT if not already present and query returns rows. The clause
        // goes on its own line so it cannot be swallowed by a trailing `//`
        // comment on the query's last line.
        let cypher = if needs_limit_injection(&input.cypher) {
            format!("{}\nLIMIT {}", input.cypher.trim(), input.limit)
        } else {
            input.cypher.clone()
        };

        // Build the query with params
        let mut q = neo4rs::query(&cypher);
        if let Some(params) = &input.params {
            for (k, v) in params {
                match v {
                    Value::String(s) => q = q.param(k.as_str(), s.clone()),
                    Value::Number(n) => {
                        if let Some(i) = n.as_i64() {
                            q = q.param(k.as_str(), i);
                        } else if let Some(f) = n.as_f64() {
                            q = q.param(k.as_str(), f);
                        }
                    }
                    Value::Bool(b) => q = q.param(k.as_str(), *b),
                    _ => q = q.param(k.as_str(), v.to_string()),
                }
            }
        }

        match neo4j.execute(q).await {
            Ok(rows) => {
                let result: Vec<Value> = rows
                    .iter()
                    .map(|row| {
                        // Convert row to a JSON object by collecting all known keys.
                        // neo4rs rows expose values via typed get; we try common types.
                        // Deserialize the row into a serde_json::Value.
                        // Use RETURN n.field AS field aliases in queries for predictable output.
                        row.to::<Value>().unwrap_or(Value::Null)
                    })
                    .collect();

                let count = result.len();
                let mut response = json!({
                    "rows": result,
                    "count": count,
                });
                // An empty result is a claim about the data, so it has to carry
                // the reason it might instead be a type mismatch.
                if count == 0
                    && let Some(hint) = empty_result_hint(&input.cypher)
                {
                    response["hint"] = json!(hint);
                }
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Neo4j query failed: {}", e)),
        }
    }

    async fn handle_duckdb_query(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            sql: String,
            #[serde(default = "default_limit")]
            limit: usize,
        }
        fn default_limit() -> usize {
            100
        }

        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };

        let Some(ref telemetry) = self.telemetry else {
            return ToolCallResult::error(
                "DuckDB not available (TELEMETRY_DB_PATH not set)".to_string(),
            );
        };

        match telemetry.query_raw(&input.sql, input.limit) {
            Ok(rows) => {
                let count = rows.len();
                let response = json!({
                    "rows": rows,
                    "count": count,
                });
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("DuckDB query failed: {}", e)),
        }
    }
}

#[async_trait]
impl Skill for QuerySkill {
    fn name(&self) -> &str {
        "Query"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        let mut tools = vec![Self::neo4j_query_def()];
        if self.telemetry.is_some() {
            tools.push(Self::duckdb_query_def());
        }
        tools
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "neo4j_query" => Some(self.handle_neo4j_query(args).await),
            "duckdb_query" => Some(self.handle_duckdb_query(args).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skills::test_helpers::*;

    fn skill_no_db() -> QuerySkill {
        QuerySkill::new(None, None)
    }

    // -- tool registry --------------------------------------------------------

    #[test]
    fn tools_without_db_has_only_neo4j_query() {
        let tools = skill_no_db().tools();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0].name, "neo4j_query");
    }

    #[test]
    fn execute_unknown_tool_returns_none() {
        let r = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(skill_no_db().execute("not_a_tool", None));
        assert!(r.is_none());
    }

    // -- neo4j_query: no-db path ----------------------------------------------

    #[tokio::test]
    async fn neo4j_query_without_db_returns_error() {
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({"cypher": "MATCH (n) RETURN n"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("not available"));
    }

    // -- LIMIT injection ------------------------------------------------------

    #[test]
    fn limit_injected_when_query_has_none() {
        assert!(needs_limit_injection("MATCH (n:Note) RETURN n"));
    }

    #[test]
    fn limit_not_injected_when_already_on_its_own_line() {
        // The regression: a substring check for " LIMIT " misses this and the
        // tool appended a second clause, yielding `LIMIT 20 LIMIT 100`.
        let cypher = "MATCH (n:Note)\nWHERE n.note_type = 'semantic'\nRETURN n.content\nORDER BY n.created_at DESC\nLIMIT 20";
        assert!(!needs_limit_injection(cypher));
    }

    #[test]
    fn limit_not_injected_when_inline() {
        assert!(!needs_limit_injection("MATCH (n) RETURN n LIMIT 5"));
    }

    #[test]
    fn limit_not_injected_for_write_query_without_return() {
        assert!(!needs_limit_injection(
            "MATCH (n:Note {id:'1'}) SET n.x = 1"
        ));
    }

    #[test]
    fn limit_matching_is_case_insensitive() {
        assert!(!needs_limit_injection("match (n) return n\nlimit 3"));
    }

    // -- empty-result hint ----------------------------------------------------

    #[test]
    fn hint_offered_for_quoted_date_filter() {
        let h = empty_result_hint(
            "MATCH (n:Note) WHERE n.created_at >= '2026-08-13T00:00:00Z' RETURN n",
        )
        .expect("a quoted date literal should produce a hint");
        // The hint has to name the fix, not just the symptom — an empty result
        // reads as "no data" unless something tells the caller to re-run.
        assert!(h.contains("datetime("));
        assert!(h.contains("null"));
    }

    #[test]
    fn no_hint_without_a_date_literal() {
        assert!(empty_result_hint("MATCH (n:Note) WHERE n.note_type = 'claim' RETURN n").is_none());
    }

    #[test]
    fn no_hint_for_unquoted_date_like_text() {
        // A date produced by the query rather than compared against is not the
        // failure mode this hint describes.
        assert!(empty_result_hint("MATCH (n:Note) RETURN date() AS d").is_none());
    }

    // -- neo4j_query: readonly guard ------------------------------------------

    #[tokio::test]
    async fn neo4j_query_readonly_blocks_create() {
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({
                        "cypher": "CREATE (n:Test {id: '1'})",
                        "readonly": true
                    })),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("CREATE"));
        assert!(msg.contains("readonly"));
    }

    #[tokio::test]
    async fn neo4j_query_readonly_blocks_merge() {
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({"cypher": "MERGE (n:Foo)"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("MERGE"));
    }

    #[tokio::test]
    async fn neo4j_query_readonly_blocks_delete() {
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({"cypher": "MATCH (n) DELETE n"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("DELETE"));
    }

    #[tokio::test]
    async fn neo4j_query_readonly_blocks_set() {
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({"cypher": "MATCH (n) SET n.x = 1"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("SET"));
    }

    #[tokio::test]
    async fn neo4j_query_readonly_allows_match_return() {
        // Should pass guard (blocked by no-db, not by readonly), error is "not available"
        let msg = result_error(
            skill_no_db()
                .execute(
                    "neo4j_query",
                    Some(serde_json::json!({"cypher": "MATCH (n) RETURN n"})),
                )
                .await
                .unwrap(),
        );
        assert!(msg.contains("not available"));
    }

    // -- duckdb_query: no-db path ---------------------------------------------

    #[tokio::test]
    async fn duckdb_query_without_telemetry_returns_error() {
        let msg = result_error(
            skill_no_db()
                .execute("duckdb_query", Some(serde_json::json!({"sql": "SELECT 1"})))
                .await
                .unwrap(),
        );
        assert!(msg.contains("not available"));
    }

    // -- missing required fields ----------------------------------------------

    #[tokio::test]
    async fn neo4j_query_missing_cypher_returns_error() {
        let r = skill_no_db()
            .execute("neo4j_query", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }
}
