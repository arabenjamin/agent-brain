//! QuerySkill — generic Neo4j (Cypher) and DuckDB (SQL) query primitives.
//!
//! These tools give the agent direct read access to both databases without
//! requiring a purpose-built Rust tool for every possible query.  Write access
//! to Neo4j is guarded by a keyword allowlist; DuckDB is always read-only.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::graph_schema::{self, GraphSchema};
use crate::services::{cypher_guard, validation};
use crate::skills::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

pub struct QuerySkill {
    neo4j: Option<Neo4jClient>,
    telemetry: Option<TelemetryClient>,
    /// Live graph schema, introspected lazily on first use and cached for the
    /// life of the process (labels change only on code/`init-db`, like the
    /// live-tools snapshot). Feeds `describe_graph` and the Cypher guard. Never
    /// fatal — a `None` here just means "run without a schema".
    schema_cache: RwLock<Option<Arc<GraphSchema>>>,
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
/// The first write keyword in `cypher`, if any. A keyword counts as a whole
/// word: exact, or followed by a non-word character (`CREATE(` is a write;
/// `created_at` is not).
pub(crate) fn write_keyword(cypher: &str) -> Option<&'static str> {
    let upper = cypher.to_uppercase();
    [
        "CREATE", "MERGE", "SET", "DELETE", "REMOVE", "DETACH", "DROP",
    ]
    .into_iter()
    .find(|kw| {
        upper.split_whitespace().any(|w| {
            w == *kw
                || (w.starts_with(kw)
                    && w[kw.len()..].starts_with(|c: char| !c.is_alphanumeric() && c != '_'))
        })
    })
}

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
        Self {
            neo4j,
            telemetry,
            schema_cache: RwLock::new(None),
        }
    }

    /// Live graph schema, cached after the first introspection. Fails open
    /// (`None`) if Neo4j is absent or introspection errors — the caller then
    /// proceeds without the schema (the guard simply does not run).
    async fn schema(&self) -> Option<Arc<GraphSchema>> {
        if let Some(s) = self.schema_cache.read().await.clone() {
            return Some(s);
        }
        let neo4j = self.neo4j.as_ref()?;
        let introspected = graph_schema::introspect(neo4j).await?;
        let arc = Arc::new(introspected);
        *self.schema_cache.write().await = Some(Arc::clone(&arc));
        Some(arc)
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

    fn describe_graph_def() -> ToolDefinition {
        ToolDefinition {
            name: "describe_graph".to_string(),
            description: "Return the LIVE graph schema — every node label with its properties \
                          (⏱ marks a native DateTime, which must be compared with datetime('…'), \
                          never a quoted string) and the relationship types. Read directly from \
                          Neo4j, so it is never stale. Call this before writing Cypher against \
                          an unfamiliar part of the graph, and ALWAYS before claiming a property \
                          or label does not exist — the schema is the ground truth for that."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn recent_tasks_def() -> ToolDefinition {
        ToolDefinition {
            name: "recent_tasks".to_string(),
            description: "The most recently updated Tasks, newest first (ordered by updated_at). \
                          This is the deterministic answer to 'the last N tasks', 'what tasks \
                          ran', 'recent work' — do NOT semantic-search for it (search cannot \
                          order by time). Optional `status` filter \
                          (created/in_progress/completed/failed/blocked)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "How many to return (default 5)" },
                    "status": {
                        "type": "string",
                        "description": "Optional status filter, e.g. completed or failed"
                    }
                }
            }),
        }
    }

    fn list_apis_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_apis".to_string(),
            description: "Every API known to the graph: each (:ApiContext) with its base_url, \
                          provenance, and how many (:ApiEndpoint)s have been ingested via \
                          [:EXPOSES]. This is the deterministic answer to 'what APIs do we \
                          have / know about' — do NOT semantic-search notes for it (that finds \
                          discussion, not the ingested endpoint graph)."
                .to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn list_endpoints_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_endpoints".to_string(),
            description: "Every endpoint (method + path + summary) an API exposes, for a given \
                          API name. This is the deterministic answer to 'what endpoints does \
                          <API> have' / 'list the endpoints for <API>' — do NOT hand-write \
                          Cypher for it (the graph has more than one endpoint-shaped structure, \
                          and the API name is case-sensitive). Get the exact `api` name from \
                          list_apis first (e.g. `home_assistant`, not `HomeAssistant`)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "api": {
                        "type": "string",
                        "description": "Exact ApiContext name, as shown by list_apis (case-sensitive)."
                    }
                },
                "required": ["api"]
            }),
        }
    }

    /// The deterministic answer to "how many tools / what agents do you have".
    ///
    /// The meta-graph (`(:ToolDef)`, `(:ContextProfile)`, `[:ALLOWS]`) is
    /// re-synced from the live registry at the end of every `build_skills()`
    /// and is the only ground truth for the brain's own inventory. Nothing
    /// surfaced it before, so the question had no correct path at all:
    /// on 2026-09-11 a chat asked "how many tools does the brain have?", the
    /// front door delegated to `toolsmith`, which called `analyze_own_structure`
    /// twice — the right-sounding tool, which returns a directory tree and a
    /// list of `pub mod` lines and **cannot** answer it — and reported "10"
    /// against a real count of 90. The follow-up "how many agents" went to
    /// `researcher`, which spent all 8 rounds semantic-searching the word
    /// "agent" and gave up, while the roster sat verbatim in the orchestrator's
    /// own `delegate` tool description.
    ///
    /// Same doctrine as `recall_recent` / `list_endpoints`: a tool that reports
    /// a count must be able to show the rows behind it, and a zero result must
    /// name the values that DO exist.
    /// Records what the brain is made of and what changed since the last time
    /// (`services/architecture.rs`). Deterministic: set arithmetic over the
    /// meta-graph and the route table, no model. Weekly on `brain-api-sync`.
    fn snapshot_architecture_def() -> ToolDefinition {
        ToolDefinition {
            name: "snapshot_architecture".to_string(),
            description: "Record a snapshot of the brain's own architecture (skills and tools, \
                          HTTP routes, agents, chains, schedules, MCP servers, dynamic tools, \
                          providers, services) and return what changed since the previous \
                          snapshot: added and removed names per category, with counts. \
                          Deterministic; the answer to 'what changed in your architecture / \
                          what tools were added this week'. With `dry_run:true` it reports the \
                          change without storing a snapshot."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "dry_run": {
                        "type": "boolean",
                        "description": "Report the diff against the last snapshot without storing a new one."
                    },
                    "trigger": {
                        "type": "string",
                        "description": "Why it was taken, recorded on the snapshot (default: manual)."
                    }
                }
            }),
        }
    }

    fn list_capabilities_def() -> ToolDefinition {
        ToolDefinition {
            name: "list_capabilities".to_string(),
            description: "The brain's own inventory: how many TOOLS it has, which SKILL each \
                          belongs to, and which AGENTS (spawnable context profiles) exist with \
                          their specialties. This is the deterministic answer to 'how many tools \
                          do you have', 'what agents/sub-agents exist', 'what can you do', 'which \
                          tools can <agent> use' — do NOT semantic-search notes for it and do NOT \
                          use analyze_own_structure (that returns a directory tree and cannot \
                          count tools). Read live from the meta-graph, re-synced from the tool \
                          registry on every startup. No args = a summary with counts; \
                          `kind:\"tools\"` lists every tool, `kind:\"agents\"` the full roster, \
                          `profile:\"<name>\"` one profile's allowlist, `skill:\"<name>\"` one \
                          skill's tools."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "kind": {
                        "type": "string",
                        "enum": ["summary", "tools", "agents", "profiles"],
                        "description": "summary (default): counts + roster + per-skill tallies. \
                                        tools: every tool name, plus `removed` (tools the brain no longer has, with replaced_by). agents: spawnable agents only. \
                                        profiles: every context profile, spawnable or not."
                    },
                    "profile": {
                        "type": "string",
                        "description": "Drill down: the tools one context profile is allowed to use."
                    },
                    "skill": {
                        "type": "string",
                        "description": "Drill down: the tools belonging to one skill, with descriptions."
                    }
                }
            }),
        }
    }

    // =========================================================================
    // Handlers
    // =========================================================================

    /// Map neo4rs rows into JSON objects (queries must alias with `AS` for
    /// predictable keys). Shared by the canned read tools.
    fn rows_to_json(rows: Vec<neo4rs::Row>) -> Vec<Value> {
        rows.iter()
            .map(|row| row.to::<Value>().unwrap_or(Value::Null))
            .collect()
    }

    async fn handle_describe_graph(&self) -> ToolCallResult {
        match self.schema().await {
            Some(schema) => ToolCallResult::success_json(json!({
                "schema": schema.render_compact(),
                "label_count": schema.labels.len(),
                "relationship_type_count": schema.rel_types.len(),
            })),
            None => ToolCallResult::error(
                "Graph schema unavailable (Neo4j not reachable or empty).".to_string(),
            ),
        }
    }

    async fn handle_recent_tasks(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            #[serde(default = "default_task_limit")]
            limit: i64,
            #[serde(default)]
            status: Option<String>,
        }
        fn default_task_limit() -> i64 {
            5
        }
        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };
        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };
        // Deterministic: ordered by updated_at DESC. Task carries created_at and
        // updated_at as native datetimes — the "timestamps aren't stored" claim
        // this tool exists to prevent is simply false.
        let where_clause = if input.status.is_some() {
            "WHERE t.status = $status"
        } else {
            ""
        };
        let cypher = format!(
            "MATCH (t:Task) {where_clause} \
             RETURN t.id AS id, t.goal AS goal, t.status AS status, \
                    toString(t.updated_at) AS updated_at, toString(t.created_at) AS created_at \
             ORDER BY t.updated_at DESC LIMIT $limit"
        );
        let mut q = neo4rs::query(&cypher).param("limit", input.limit.max(1));
        if let Some(ref s) = input.status {
            q = q.param("status", s.clone());
        }
        match neo4j.execute(q).await {
            Ok(rows) => {
                let result = Self::rows_to_json(rows);
                let count = result.len();
                ToolCallResult::success_json(json!({ "tasks": result, "count": count }))
            }
            Err(e) => ToolCallResult::error(format!("recent_tasks query failed: {}", e)),
        }
    }

    async fn handle_list_apis(&self) -> ToolCallResult {
        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };
        let cypher = "MATCH (c:ApiContext) \
             OPTIONAL MATCH (c)-[:EXPOSES]->(e:ApiEndpoint) \
             RETURN c.name AS name, coalesce(c.base_url, '') AS base_url, \
                    coalesce(c.source, '') AS source, count(e) AS endpoints \
             ORDER BY endpoints DESC, name ASC";
        match neo4j.execute(neo4rs::query(cypher)).await {
            Ok(rows) => {
                let result = Self::rows_to_json(rows);
                let count = result.len();
                ToolCallResult::success_json(json!({ "apis": result, "count": count }))
            }
            Err(e) => ToolCallResult::error(format!("list_apis query failed: {}", e)),
        }
    }

    async fn handle_list_endpoints(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize)]
        struct Input {
            api: String,
        }
        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };
        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };
        // The one correct structure: (:ApiContext)-[:EXPOSES]->(:ApiEndpoint).
        // Baking it here is the point — a hand-written query can pick the wrong
        // endpoint-shaped path or mis-case the name and silently return zero.
        let cypher = "MATCH (c:ApiContext {name: $api})-[:EXPOSES]->(e:ApiEndpoint) \
             RETURN e.method AS method, e.path AS path, coalesce(e.summary, '') AS summary \
             ORDER BY e.path, e.method";
        match neo4j
            .execute(neo4rs::query(cypher).param("api", input.api.clone()))
            .await
        {
            Ok(rows) => {
                let result = Self::rows_to_json(rows);
                let count = result.len();
                let mut response = json!({
                    "api": input.api,
                    "endpoints": result,
                    "count": count,
                });
                // Zero is almost always a wrong/mis-cased name, not an API with
                // no endpoints — point at list_apis rather than let the caller
                // conclude the endpoints "aren't stored".
                if count == 0 {
                    response["hint"] = json!(
                        "0 endpoints — the api name is case-sensitive and must match an \
                         ApiContext exactly. Call list_apis to get the exact names (e.g. \
                         `home_assistant`, not `HomeAssistant`), then retry."
                    );
                }
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("list_endpoints query failed: {}", e)),
        }
    }

    /// Read the brain's own inventory out of the meta-graph.
    ///
    /// Every branch returns the top-level counts, so the caller can never see a
    /// drill-down without the total it belongs to — the inversion that made a
    /// sub-agent report absence while quoting a correct count in the same breath.
    async fn handle_snapshot_architecture(&self, args: Option<Value>) -> ToolCallResult {
        use crate::services::architecture as arch;
        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };
        let args = args.unwrap_or_default();
        if args["dry_run"].as_bool().unwrap_or(false) {
            let inv = match arch::collect(neo4j).await {
                Ok(i) => i,
                Err(e) => {
                    return ToolCallResult::error(format!("snapshot_architecture failed: {e}"));
                }
            };
            let out = match arch::latest(neo4j).await {
                Ok(Some((s, old))) => {
                    let d = arch::diff(&old, &inv);
                    json!({ "dry_run": true, "since": s.taken_at, "summary": arch::summarize(&d), "diff": d, "counts": inv.counts() })
                }
                Ok(None) => {
                    json!({ "dry_run": true, "since": null, "summary": "No snapshot yet: nothing to compare with.", "counts": inv.counts() })
                }
                Err(e) => {
                    return ToolCallResult::error(format!("snapshot_architecture failed: {e}"));
                }
            };
            return ToolCallResult::success_json(out);
        }
        let trigger = args["trigger"]
            .as_str()
            .filter(|t| !t.is_empty())
            .unwrap_or("manual");
        match arch::take(neo4j, trigger).await {
            Ok(s) => ToolCallResult::success_json(json!({
                "snapshot_id": s.id,
                "taken_at": s.taken_at,
                "previous_id": s.previous_id,
                "summary": s.summary,
                "diff": s.diff,
                "counts": s.counts,
            })),
            Err(e) => ToolCallResult::error(format!("snapshot_architecture failed: {e}")),
        }
    }

    async fn handle_list_capabilities(&self, args: Option<Value>) -> ToolCallResult {
        #[derive(Deserialize, Default)]
        struct Input {
            #[serde(default)]
            kind: Option<String>,
            #[serde(default)]
            profile: Option<String>,
            #[serde(default)]
            skill: Option<String>,
        }
        let input: Input = match serde_json::from_value(args.unwrap_or_default()) {
            Ok(i) => i,
            Err(e) => return ToolCallResult::error(format!("Invalid args: {}", e)),
        };
        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        // Empty strings are absent, not filters for "" — the same normalization
        // search_notes and recall_recent apply, and for the same reason: a
        // blank filter that matches nothing reads back as "there are none".
        let trim = |o: &Option<String>| {
            o.as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        let profile = trim(&input.profile);
        let skill = trim(&input.skill);
        let kind = trim(&input.kind).unwrap_or_else(|| "summary".to_string());

        // ── Totals: present on every response ────────────────────────────────
        let totals = neo4j
            .execute(neo4rs::query(
                "MATCH (d:ToolDef) WITH count(d) AS tools, count(DISTINCT d.skill) AS skills \
                 MATCH (c:ContextProfile) \
                 RETURN tools AS tool_count, skills AS skill_count, \
                        count(c) AS profile_count, \
                        count(CASE WHEN c.spawnable THEN 1 END) AS agent_count",
            ))
            .await;
        let mut response = match totals {
            Ok(rows) => Self::rows_to_json(rows)
                .into_iter()
                .next()
                .unwrap_or_else(|| json!({})),
            Err(e) => return ToolCallResult::error(format!("list_capabilities failed: {}", e)),
        };

        // ── Drill-down: one profile's allowlist ──────────────────────────────
        if let Some(name) = profile {
            let rows = neo4j
                .execute(
                    neo4rs::query(
                        "MATCH (c:ContextProfile {name: $name}) \
                         OPTIONAL MATCH (c)-[:ALLOWS]->(d:ToolDef) \
                         RETURN c.name AS profile, c.allows_all AS allows_all, \
                                c.spawnable AS spawnable, \
                                coalesce(c.specialty, '') AS specialty, \
                                coalesce(c.model_preference, '') AS model_preference, \
                                coalesce(c.provider_hint, '') AS provider_hint, \
                                collect(d.name) AS tools",
                    )
                    .param("name", name.as_str()),
                )
                .await;
            match rows {
                Ok(rows) => match Self::rows_to_json(rows).into_iter().next() {
                    Some(row) => {
                        response["profile"] = row;
                        // allows_all means "no allowlist", i.e. the whole
                        // registry — an empty `tools` array there would read as
                        // "this profile has no tools", the exact inversion.
                        if response["profile"]["allows_all"] == json!(true) {
                            response["profile"]["note"] = json!(
                                "This profile declares no allowlist, so it may use ALL \
                                 registered tools (tool_count above), not the empty list."
                            );
                        }
                    }
                    None => {
                        response["profile"] = json!(null);
                        response["hint"] = json!(format!(
                            "No context profile named '{name}'. Profile names are \
                             case-sensitive; call list_capabilities with kind=\"profiles\" \
                             for the exact names."
                        ));
                    }
                },
                Err(e) => return ToolCallResult::error(format!("list_capabilities failed: {}", e)),
            }
            return ToolCallResult::success_json(response);
        }

        // ── Drill-down: one skill's tools, with descriptions ─────────────────
        if let Some(name) = skill {
            let rows = neo4j
                .execute(
                    neo4rs::query(
                        "MATCH (d:ToolDef {skill: $skill}) \
                         RETURN d.name AS name, coalesce(d.description, '') AS description \
                         ORDER BY d.name",
                    )
                    .param("skill", name.as_str()),
                )
                .await;
            match rows {
                Ok(rows) => {
                    let tools = Self::rows_to_json(rows);
                    response["skill"] = json!(name);
                    response["count"] = json!(tools.len());
                    response["tools"] = json!(tools);
                    if tools.is_empty() {
                        // Name what exists rather than let the caller conclude
                        // the skill is empty (it is almost always mis-spelled).
                        let known = neo4j
                            .execute(neo4rs::query(
                                "MATCH (d:ToolDef) RETURN DISTINCT d.skill AS skill ORDER BY skill",
                            ))
                            .await
                            .map(Self::rows_to_json)
                            .unwrap_or_default();
                        response["available_skills"] = json!(known);
                        response["hint"] = json!(format!(
                            "No tools registered under skill '{name}'. Skill names are \
                             case-sensitive — see available_skills. The FILTER is the likely \
                             fault here, not the registry: tool_count above is the real total."
                        ));
                    }
                }
                Err(e) => return ToolCallResult::error(format!("list_capabilities failed: {}", e)),
            }
            return ToolCallResult::success_json(response);
        }

        // ── kind=tools: every registered tool, plus the per-skill tally ──────
        if kind == "tools" {
            let rows = neo4j
                .execute(neo4rs::query(
                    "MATCH (d:ToolDef) RETURN d.name AS name, coalesce(d.skill, '') AS skill \
                     ORDER BY d.skill, d.name",
                ))
                .await;
            match rows {
                Ok(rows) => response["tools"] = json!(Self::rows_to_json(rows)),
                Err(e) => return ToolCallResult::error(format!("list_capabilities failed: {}", e)),
            }
            if let Ok(rows) = neo4j
                .execute(neo4rs::query(
                    "MATCH (d:ToolDef) RETURN coalesce(d.skill, '') AS skill, count(d) AS tools \
                     ORDER BY tools DESC, skill",
                ))
                .await
            {
                response["skills"] = json!(Self::rows_to_json(rows));
            }
            // Tools the brain used to have: a call to one of these is not a typo,
            // and `replaced_by` names what to use instead.
            if let Ok(rows) = neo4j
                .execute(neo4rs::query(
                    "MATCH (r:RemovedTool) RETURN r.name AS name, \
                            coalesce(r.replaced_by, '') AS replaced_by, \
                            coalesce(r.reason, '') AS reason, \
                            toString(date(r.removed_at)) AS removed_on \
                     ORDER BY r.name",
                ))
                .await
            {
                response["removed"] = json!(Self::rows_to_json(rows));
            }
            return ToolCallResult::success_json(response);
        }

        // ── kind=profiles / agents / summary ─────────────────────────────────
        // `agents` are the spawnable subset — the roster `delegate`/`run_agent`
        // can actually dispatch to. `profiles` is every profile including the
        // non-spawnable personas (general, boot, the front door).
        let spawnable_only = kind != "profiles";
        let rows = neo4j
            .execute(
                neo4rs::query(
                    "MATCH (c:ContextProfile) \
                     WHERE $all OR c.spawnable \
                     OPTIONAL MATCH (c)-[:ALLOWS]->(d:ToolDef) \
                     RETURN c.name AS name, c.spawnable AS spawnable, \
                            coalesce(c.specialty, c.description) AS specialty, \
                            coalesce(c.model_preference, '') AS model_preference, \
                            coalesce(c.provider_hint, '') AS provider_hint, \
                            c.allows_all AS allows_all, count(d) AS tool_count \
                     ORDER BY c.spawnable DESC, c.name",
                )
                .param("all", !spawnable_only),
            )
            .await;
        match rows {
            Ok(rows) => {
                let mut list = Self::rows_to_json(rows);
                if kind == "summary" {
                    // The summary has ONE job: answer "how many tools / which
                    // agents" in a payload that survives a sub-agent's tool
                    // budget. That budget is CLOUD_TOOL_RESULT_CHARS = 2000 —
                    // NOT the 6000 the chat loop uses — and the full-fat rows
                    // blew past it: measured live at 4380 chars, the roster was
                    // cut mid-list and a `knowledge-worker` reported "at least
                    // six agents" (there are 8) while quoting the correct
                    // tool_count from the surviving head of the same payload.
                    // So the summary carries name + a clipped specialty only,
                    // and `kind:"agents"` is where the model/allowlist detail
                    // lives. A truncated count is a wrong count.
                    for row in &mut list {
                        let name = row.get("name").cloned().unwrap_or(Value::Null);
                        let mut specialty = row
                            .get("specialty")
                            .and_then(Value::as_str)
                            .unwrap_or_default()
                            .to_string();
                        if specialty.chars().count() > 90 {
                            specialty = specialty.chars().take(89).collect::<String>() + "…";
                        }
                        *row = json!({ "name": name, "specialty": specialty });
                    }
                }
                if spawnable_only {
                    response["agents"] = json!(list);
                } else {
                    response["profiles"] = json!(list);
                }
            }
            Err(e) => return ToolCallResult::error(format!("list_capabilities failed: {}", e)),
        }

        if kind == "summary" {
            // `agent_count` above is the authority, not the length of the list —
            // say so, because the list is the half that a truncation eats.
            response["drill_down"] = json!(
                "agent_count is the real number of agents; agents[] above is the roster. \
                 kind=\"tools\" for every tool name + per-skill tallies, skill=\"<name>\" for \
                 one skill's tools with descriptions, profile=\"<name>\" for one profile's \
                 allowlist, kind=\"agents\" for each agent's model and tool count, \
                 kind=\"profiles\" to include non-spawnable personas."
            );
        }

        ToolCallResult::success_json(response)
    }

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
        if input.readonly
            && let Some(kw) = write_keyword(&input.cypher)
        {
            return ToolCallResult::error(format!(
                "Write keyword '{kw}' rejected in readonly mode. Pass readonly=false to allow writes."
            ));
        }

        let Some(ref neo4j) = self.neo4j else {
            return ToolCallResult::error("Neo4j not available".to_string());
        };

        // Schema-aware guard (fail open). Reject a read that names a label or
        // relationship that cannot exist; carry advisory (Soft) findings — an
        // unknown property, a datetime-vs-string literal — out as a `warning`.
        let mut soft_warnings: Vec<String> = Vec::new();
        if let Some(schema) = self.schema().await {
            let violations = cypher_guard::check(&input.cypher, &schema, input.readonly);
            if validation::has_hard(&violations) {
                return ToolCallResult::error(format!(
                    "Cypher rejected by the schema guard: {}. Call describe_graph to see the \
                     valid labels, relationships, and properties before re-running.",
                    validation::render_hard(&violations)
                ));
            }
            soft_warnings = violations
                .iter()
                .filter(|v| v.severity == validation::Severity::Soft)
                .map(validation::Violation::render)
                .collect();
        }

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
                // Advisory schema findings ride out with the rows — the query
                // still ran, but a property may be misspelled or a datetime
                // comparison may silently match nothing.
                if !soft_warnings.is_empty() {
                    response["warning"] = json!(soft_warnings.join(" | "));
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
        let mut tools = vec![
            Self::neo4j_query_def(),
            Self::describe_graph_def(),
            Self::recent_tasks_def(),
            Self::list_apis_def(),
            Self::list_endpoints_def(),
            Self::list_capabilities_def(),
            Self::snapshot_architecture_def(),
        ];
        if self.telemetry.is_some() {
            tools.push(Self::duckdb_query_def());
        }
        tools
    }

    async fn execute(&self, name: &str, args: Option<Value>) -> Option<ToolCallResult> {
        match name {
            "neo4j_query" => Some(self.handle_neo4j_query(args).await),
            "duckdb_query" => Some(self.handle_duckdb_query(args).await),
            "describe_graph" => Some(self.handle_describe_graph().await),
            "recent_tasks" => Some(self.handle_recent_tasks(args).await),
            "list_apis" => Some(self.handle_list_apis().await),
            "list_endpoints" => Some(self.handle_list_endpoints(args).await),
            "list_capabilities" => Some(self.handle_list_capabilities(args).await),
            "snapshot_architecture" => Some(self.handle_snapshot_architecture(args).await),
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

    // -- list_capabilities ----------------------------------------------------

    #[test]
    fn list_capabilities_is_registered() {
        let names: Vec<String> = skill_no_db().tools().into_iter().map(|t| t.name).collect();
        assert!(names.contains(&"list_capabilities".to_string()));
    }

    #[tokio::test]
    async fn snapshot_architecture_is_registered_and_needs_the_graph() {
        let skill = skill_no_db();
        assert!(
            skill
                .tools()
                .iter()
                .any(|t| t.name == "snapshot_architecture")
        );
        let r = skill
            .execute("snapshot_architecture", Some(json!({ "dry_run": true })))
            .await
            .unwrap();
        assert!(r.is_error.unwrap_or(false));
    }

    /// The description has to steer callers AWAY from the two tools that
    /// produced the wrong answers on 2026-09-11 — `analyze_own_structure`
    /// (a directory tree that cannot count) and a semantic note search.
    #[test]
    fn list_capabilities_description_redirects_from_the_tools_that_failed() {
        let def = QuerySkill::list_capabilities_def();
        let d = def.description.to_lowercase();
        assert!(d.contains("analyze_own_structure"));
        assert!(d.contains("semantic-search") || d.contains("semantic search"));
        // The three questions it must claim, verbatim enough to match on.
        assert!(d.contains("how many tools"));
        assert!(d.contains("agents"));
    }

    #[test]
    fn list_capabilities_accepts_every_documented_kind() {
        let def = QuerySkill::list_capabilities_def();
        let kinds = def.input_schema["properties"]["kind"]["enum"]
            .as_array()
            .expect("kind enum")
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_string())
            .collect::<Vec<_>>();
        for k in ["summary", "tools", "agents", "profiles"] {
            assert!(kinds.contains(&k.to_string()), "missing kind {k}");
        }
        // Both drill-downs must exist, or a reported count has no rows behind it.
        assert!(def.input_schema["properties"]["profile"].is_object());
        assert!(def.input_schema["properties"]["skill"].is_object());
    }

    /// No args must be legal: the summary is the entry point, and requiring a
    /// `kind` would make the common question ("how many tools do you have")
    /// fail on the first call.
    #[test]
    fn list_capabilities_requires_no_arguments() {
        let def = QuerySkill::list_capabilities_def();
        assert!(def.input_schema.get("required").is_none());
    }

    #[tokio::test]
    async fn list_capabilities_without_neo4j_errors_rather_than_reporting_zero() {
        // A capability question answered "0 tools" is worse than an error: the
        // caller relays the zero. Same reason the other canned reads error here.
        let r = skill_no_db()
            .execute("list_capabilities", Some(json!({})))
            .await
            .expect("dispatches");
        assert_eq!(r.is_error, Some(true));
    }

    // -- tool registry --------------------------------------------------------

    #[test]
    fn tools_without_telemetry_omit_duckdb_but_keep_reads() {
        let names: Vec<String> = skill_no_db().tools().into_iter().map(|t| t.name).collect();
        // The read/introspection tools do not need telemetry; duckdb does.
        assert!(names.contains(&"neo4j_query".to_string()));
        assert!(names.contains(&"describe_graph".to_string()));
        assert!(names.contains(&"recent_tasks".to_string()));
        assert!(names.contains(&"list_apis".to_string()));
        assert!(names.contains(&"list_endpoints".to_string()));
        assert!(!names.contains(&"duckdb_query".to_string()));
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
