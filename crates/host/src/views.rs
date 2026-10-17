//! Views and the `lookup` tool (GRAPH_CONTEXT_PLAN Phase 2).
//!
//! A view is a named thing the brain stores ("the daily brief", "overdue
//! todos"), declared once as a bundle's `views/<name>.yaml`, with typed slots a
//! model fills. The model never writes Cypher: it picks a view from a closed
//! list and fills its slots. Phase 1 measured why: free-form Cypher failed on
//! *where things live in this brain* (briefs looked for in `Workflow`, the
//! résumé in `Task.goal`), not on Cypher.
//!
//! - **`kind: cypher`** is a fixed read query; its slots are its parameters, on
//!   the declared-tool rules (`bundle_tools::check_query`). The caller always
//!   supplies `$today`, `$day_start`, `$now` and `$tz`, so no view does its own
//!   time arithmetic.
//! - **`kind: notes`** is a preset over [`NoteSpec`], the one spec for `Note`
//!   (27 of the 40 graph test cases). Its compiler bounds time in *local* days
//!   (`clock::local_midnight_utc`) and never compares a datetime to a string.
//! - **Not MCP tools.** One tool, `lookup`, runs a view or lists them all, so
//!   the catalog grows without growing the tool list.
//! - **Every query runs through `neo4j_query`'s read path** (read-only, schema
//!   guard, LIMIT), like a declared tool.
//! - **An empty answer is a finding:** it repeats the filter and counts what
//!   exists without the window and without the narrowing slots.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Datelike, Duration, NaiveDate, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use tracing::{info, warn};

use crate::bundle_tools::{check_query, params};
use crate::plugins::Plugins;
use agent_brain_protocol::{Skill, ToolCallResult, ToolDefinition};
use agent_brain_retrieve::QuerySkill;

pub const LOOKUP_TOOL: &str = "lookup";
/// The view kinds.
pub const KINDS: &[&str] = &["cypher", "notes"];
/// Parameters the caller supplies to every `kind: cypher` view.
pub const RESERVED: &[&str] = &["today", "day_start", "now", "tz"];
/// Rows returned when neither the view nor the call sets a limit.
pub const DEFAULT_LIMIT: usize = 30;
const MAX_LIMIT: usize = 200;
/// Groups an aggregate shape returns before it says it was cut off.
const MAX_GROUPS: usize = 1000;
/// Days a thin short window is compared against.
const BASELINE_DAYS: i64 = 7;

// ── The note spec ────────────────────────────────────────────────────────────

/// A time window, in local days. Exactly one form: `hours`, `days`, `day`,
/// `month`, or `since` (with an optional `until`).
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Window {
    /// The last N hours, up to now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hours: Option<u32>,
    /// The last N days, up to now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub days: Option<u32>,
    /// One local day: `today`, `yesterday` or `YYYY-MM-DD`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub day: Option<String>,
    /// One local month: `this`, `last` or `YYYY-MM`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub month: Option<String>,
    /// From the start of this local date (`YYYY-MM-DD`)...
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub since: Option<String>,
    /// ...through the end of this one (inclusive).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
}

/// "Now" for a compile: the instant, the local date, and how a local date's
/// midnight maps to UTC. A struct so tests can fix them.
pub struct Clock {
    pub now: DateTime<Utc>,
    pub today: NaiveDate,
    pub midnight: Box<dyn Fn(NaiveDate) -> DateTime<Utc> + Send + Sync>,
    pub tz: String,
}

impl Clock {
    pub fn live() -> Self {
        use agent_brain_graph::clock;
        Self {
            now: Utc::now(),
            today: clock::now_local().date_naive(),
            midnight: Box::new(clock::local_midnight_utc),
            tz: clock::timezone_name().to_string(),
        }
    }
}

fn parse_date(s: &str, what: &str) -> Result<NaiveDate, String> {
    NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d")
        .map_err(|_| format!("{what} '{s}' is not a date (YYYY-MM-DD)"))
}

fn month_start(d: NaiveDate) -> NaiveDate {
    d.with_day(1).expect("day 1 exists")
}

fn next_month(d: NaiveDate) -> NaiveDate {
    let (y, m) = if d.month() == 12 {
        (d.year() + 1, 1)
    } else {
        (d.year(), d.month() + 1)
    };
    NaiveDate::from_ymd_opt(y, m, 1).expect("day 1 exists")
}

impl Window {
    /// `[since, until)` as UTC instants; `until` is `None` for "up to now".
    pub fn bounds(&self, c: &Clock) -> Result<(DateTime<Utc>, Option<DateTime<Utc>>), String> {
        let forms = [
            self.hours.is_some(),
            self.days.is_some(),
            self.day.is_some(),
            self.month.is_some(),
            self.since.is_some(),
        ];
        if forms.iter().filter(|f| **f).count() != 1
            || (self.until.is_some() && self.since.is_none())
        {
            return Err(
                "window takes exactly one of hours, days, day, month, or since (with until)".into(),
            );
        }
        if let Some(h) = self.hours {
            return Ok((c.now - Duration::hours(h as i64), None));
        }
        if let Some(d) = self.days {
            return Ok((c.now - Duration::days(d as i64), None));
        }
        if let Some(day) = &self.day {
            let d = match day.trim() {
                "today" => c.today,
                "yesterday" => c.today - Duration::days(1),
                s => parse_date(s, "day")?,
            };
            return Ok(((c.midnight)(d), Some((c.midnight)(d + Duration::days(1)))));
        }
        if let Some(m) = &self.month {
            let first = match m.trim() {
                "this" => month_start(c.today),
                "last" => month_start(month_start(c.today) - Duration::days(1)),
                s => parse_date(&format!("{s}-01"), "month")?,
            };
            return Ok(((c.midnight)(first), Some((c.midnight)(next_month(first)))));
        }
        let since = parse_date(self.since.as_deref().unwrap_or(""), "since")?;
        let until = match &self.until {
            Some(u) => Some((c.midnight)(parse_date(u, "until")? + Duration::days(1))),
            None => None,
        };
        Ok(((c.midnight)(since), until))
    }

    /// The window in words, for an empty answer.
    fn describe(&self) -> String {
        if let Some(h) = self.hours {
            return format!("in the last {h} hours");
        }
        if let Some(d) = self.days {
            return format!("in the last {d} days");
        }
        if let Some(d) = &self.day {
            return match d.as_str() {
                "today" | "yesterday" => d.clone(),
                _ => format!("on {d}"),
            };
        }
        if let Some(m) = &self.month {
            return format!("in month {m}");
        }
        match (&self.since, &self.until) {
            (Some(s), Some(u)) => format!("from {s} through {u}"),
            (Some(s), None) => format!("since {s}"),
            _ => String::new(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum On {
    Created,
    Verified,
    Asserted,
}

impl On {
    fn field(self) -> &'static str {
        match self {
            On::Created => "created_at",
            On::Verified => "verified_at",
            On::Asserted => "asserted_at",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Shape {
    Rows,
    Count,
    ByTopic,
    ByKind,
    BySource,
    ByDay,
}

impl Shape {
    fn as_str(self) -> &'static str {
        match self {
            Shape::Rows => "rows",
            Shape::Count => "count",
            Shape::ByTopic => "by_topic",
            Shape::ByKind => "by_kind",
            Shape::BySource => "by_source",
            Shape::ByDay => "by_day",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Order {
    Newest,
    Oldest,
}

/// The one spec for notes. Every slot is optional; the empty spec is "the
/// newest notes".
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NoteSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window: Option<Window>,
    /// Which timestamp the window bounds (default `created`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<On>,
    /// Note types (`note_type`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kinds: Option<Vec<String>>,
    /// Where the notes came from (`source_context`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<Vec<String>>,
    /// A topic's `name`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    /// An entity the notes mention, by name or alias (any case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub entity: Option<String>,
    /// Words the content must all contain (any case).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub words: Option<String>,
    /// A claim's standing: `unverified`, `corroborated` or `refuted`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, rename = "as", skip_serializing_if = "Option::is_none")]
    pub shape: Option<Shape>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order: Option<Order>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
    /// Not a slot: a count taken for rows (their total, a `wide` breakdown)
    /// matches words the way the rows did, any of them.
    #[serde(skip)]
    any_words: bool,
    /// Not a slot: the view's `except_work_kinds`. A note a workflow of one of
    /// these kinds produced is left out (`learned` is not the brain's upkeep).
    #[serde(skip)]
    except_work: Vec<String>,
}

/// The slot names of a [`NoteSpec`], as they appear in a call.
pub const NOTE_SLOTS: &[&str] = &[
    "window", "on", "kinds", "from", "topic", "entity", "words", "status", "as", "order", "limit",
];

const STATUSES: &[&str] = &["unverified", "corroborated", "refuted"];

impl NoteSpec {
    fn set_slots(&self) -> Vec<&'static str> {
        let v = serde_json::to_value(self).unwrap_or_default();
        NOTE_SLOTS
            .iter()
            .copied()
            .filter(|s| v.get(*s).is_some())
            .collect()
    }

    /// `self` with every slot `over` sets replaced by `over`'s.
    fn overlay(&self, over: &NoteSpec) -> NoteSpec {
        let mut base = serde_json::to_value(self).unwrap_or_default();
        if let (Some(b), Value::Object(o)) = (
            base.as_object_mut(),
            serde_json::to_value(over).unwrap_or_default(),
        ) {
            for (k, v) in o {
                b.insert(k, v);
            }
        }
        serde_json::from_value(base).unwrap_or_default()
    }

    /// The Cypher and its parameters. The window is bounded in local days;
    /// names are matched lower-case. Every value is a parameter, never spliced.
    pub fn compile(&self, c: &Clock) -> Result<(String, Map<String, Value>), String> {
        let mut p = Map::new();
        let mut lines = vec!["MATCH (n:Note)".to_string()];
        let mut conds: Vec<String> = Vec::new();
        if let Some(t) = &self.topic {
            lines.push("MATCH (n)-[:IN_TOPIC]->(:Topic {name: $topic})".into());
            p.insert("topic".into(), json!(t.trim()));
        }
        if let Some(e) = &self.entity {
            lines.push("MATCH (n)-[:MENTIONS]->(e:Entity)".into());
            conds.push(
                "(e.name = $entity OR EXISTS { MATCH (:EntityAlias {name: $entity})-[:ALIAS_OF]->(e) })"
                    .into(),
            );
            p.insert("entity".into(), json!(e.trim().to_lowercase()));
        }
        let field = self.on.unwrap_or(On::Created).field();
        if let Some(w) = &self.window {
            let (since, until) = w.bounds(c)?;
            conds.push(format!("n.{field} >= datetime($since)"));
            p.insert("since".into(), json!(since.to_rfc3339()));
            if let Some(u) = until {
                conds.push(format!("n.{field} < datetime($until)"));
                p.insert("until".into(), json!(u.to_rfc3339()));
            }
        } else if field != "created_at" {
            conds.push(format!("n.{field} IS NOT NULL"));
        }
        if let Some(k) = self.kinds.as_ref().filter(|k| !k.is_empty()) {
            conds.push("n.note_type IN $kinds".into());
            p.insert("kinds".into(), json!(k));
        }
        if let Some(f) = self.from.as_ref().filter(|f| !f.is_empty()) {
            conds.push("n.source_context IN $from".into());
            p.insert("from".into(), json!(f));
        }
        if !self.except_work.is_empty() {
            conds.push(
                "NOT EXISTS { MATCH (pt:Task {id: n.produced_by_task}), (pw:Workflow {name: pt.workflow}) \
                 WHERE pw.kind IN $except_work }"
                    .into(),
            );
            p.insert("except_work".into(), json!(self.except_work));
        }
        if let Some(s) = &self.status {
            if !STATUSES.contains(&s.as_str()) {
                return Err(format!(
                    "status '{s}' is not one of {}",
                    STATUSES.join(", ")
                ));
            }
            conds.push("n.claim_status = $status".into());
            p.insert("status".into(), json!(s));
        }
        // Rows match ANY word and rank by how many each note holds (accuracy
        // first: a question's wording rarely matches a note's word for word);
        // a count or breakdown needs every word, or it counts noise.
        let shape = self.shape.unwrap_or(Shape::Rows);
        let words = self.word_list();
        if !words.is_empty() {
            conds.push(if shape == Shape::Rows || self.any_words {
                "any(w IN $words WHERE toLower(n.content) CONTAINS w)".into()
            } else {
                "all(w IN $words WHERE toLower(n.content) CONTAINS w)".into()
            });
            p.insert("words".into(), json!(words));
        }
        if !conds.is_empty() {
            lines.push(format!("WHERE {}", conds.join("\n  AND ")));
        }
        lines.push(if words.is_empty() || shape != Shape::Rows {
            "WITH DISTINCT n".into()
        } else {
            "WITH DISTINCT n, size([w IN $words WHERE toLower(n.content) CONTAINS w]) AS hits"
                .into()
        });
        let limit = self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT);
        let dir = match self.order.unwrap_or(Order::Newest) {
            Order::Newest => "DESC",
            Order::Oldest => "ASC",
        };
        match shape {
            Shape::Rows => {
                p.insert("limit".into(), json!(limit));
                let rank = if words.is_empty() { "" } else { "hits DESC, " };
                lines.push(format!("ORDER BY {rank}n.{field} {dir}\nLIMIT $limit"));
                lines.push(format!(
                    "RETURN toString(n.{field}) AS at, n.note_type AS kind, n.source_context AS from,\n       \
                     coalesce(n.claim_status, CASE n.note_type WHEN 'unsourced_synthesis' THEN 'unsourced' \
                     ELSE 'not checked' END) AS standing,\n       \
                     n.content AS content, n.source_url AS url"
                ));
            }
            Shape::Count => lines.push("RETURN count(n) AS notes".into()),
            // A note sits in several topics or none; the untopiced still count.
            Shape::ByTopic => lines.push(
                "OPTIONAL MATCH (n)-[:IN_TOPIC]->(t:Topic)\n\
                 RETURN coalesce(t.name, '(no topic)') AS topic, count(DISTINCT n) AS notes ORDER BY notes DESC"
                    .into(),
            ),
            Shape::ByKind => {
                lines.push("RETURN n.note_type AS kind, count(n) AS notes ORDER BY notes DESC".into())
            }
            Shape::BySource => lines
                .push("RETURN n.source_context AS from, count(n) AS notes ORDER BY notes DESC".into()),
            Shape::ByDay => {
                p.insert("tz".into(), json!(c.tz));
                lines.push(format!(
                    "RETURN toString(date(datetime({{datetime: n.{field}, timezone: $tz}}))) AS day, \
                     count(n) AS notes ORDER BY day"
                ));
            }
        }
        Ok((lines.join("\n"), p))
    }

    /// The words to match: lower-case, punctuation trimmed, one letter dropped.
    fn word_list(&self) -> Vec<String> {
        self.words
            .as_deref()
            .unwrap_or("")
            .split(|c: char| c.is_whitespace() || c == ',' || c == ';')
            .map(|w| {
                w.trim_matches(|c: char| !c.is_alphanumeric())
                    .to_lowercase()
            })
            .filter(|w| w.chars().count() >= 2)
            .collect()
    }

    /// The filter in words, for an empty answer.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if let Some(k) = &self.kinds {
            parts.push(format!("of kind {}", k.join("/")));
        }
        if let Some(f) = &self.from {
            parts.push(format!("from {}", f.join("/")));
        }
        if let Some(t) = &self.topic {
            parts.push(format!("in topic {t}"));
        }
        if let Some(e) = &self.entity {
            parts.push(format!("mentioning {e}"));
        }
        if let Some(w) = &self.words {
            parts.push(format!("containing any of \"{w}\""));
        }
        if let Some(s) = &self.status {
            parts.push(format!("with status {s}"));
        }
        if !self.except_work.is_empty() {
            parts.push(format!(
                "not made by {} workflows",
                self.except_work.join("/")
            ));
        }
        if let Some(w) = &self.window {
            let on = match self.on.unwrap_or(On::Created) {
                On::Created => "created",
                On::Verified => "verified",
                On::Asserted => "asserted",
            };
            parts.push(format!("{on} {}", w.describe()));
        }
        if parts.is_empty() {
            "notes".into()
        } else {
            format!("notes {}", parts.join(", "))
        }
    }
}

/// The JSON schema of the note slots a call may set (all but `fixed`).
pub fn note_slots_schema(fixed: &[&str]) -> Value {
    let all = json!({
        "window": {"type": "object", "description": "One of: {hours: N}, {days: N}, {day: \"today\"|\"yesterday\"|\"YYYY-MM-DD\"}, {month: \"this\"|\"last\"|\"YYYY-MM\"}, {since: \"YYYY-MM-DD\", until?: \"YYYY-MM-DD\"}. Local days."},
        "on": {"type": "string", "enum": ["created", "verified", "asserted"], "description": "Which timestamp the window bounds (default created)."},
        "kinds": {"type": "array", "items": {"type": "string"}, "description": "Note types (note_type)."},
        "from": {"type": "array", "items": {"type": "string"}, "description": "Where the notes came from (source_context)."},
        "topic": {"type": "string", "description": "A topic name (lookup view topics lists them)."},
        "entity": {"type": "string", "description": "An entity the notes mention, by name or alias, any case."},
        "words": {"type": "string", "description": "A few key words, any case: notes holding any of them, most first (a count needs all)."},
        "status": {"type": "string", "enum": STATUSES, "description": "A claim's standing."},
        "as": {"type": "string", "enum": ["rows", "count", "by_topic", "by_kind", "by_source", "by_day"], "description": "rows (default), a count, or counts by topic/kind/source/day."},
        "order": {"type": "string", "enum": ["newest", "oldest"]},
        "limit": {"type": "integer", "minimum": 1, "maximum": MAX_LIMIT, "description": "Rows (default 30)."}
    });
    let props: Map<String, Value> = all
        .as_object()
        .expect("object")
        .iter()
        .filter(|(k, _)| !fixed.contains(&k.as_str()))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    json!({"type": "object", "properties": props})
}

// ── View files ──────────────────────────────────────────────────────────────

/// A ledger value as a number. DuckDB returns some aggregates as strings
/// ("12", a HUGEINT sum); those count too.
fn number(v: &Value) -> Option<f64> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.parse().ok()))
}

/// A view's `sql` with each `$slot` replaced by that integer slot's value.
/// Load-time checks allow only integer slots; this refuses anything else a
/// caller sent for one, so no text reaches the SQL.
fn integer_sql(sql: &str, params: &Map<String, Value>) -> Result<String, String> {
    let mut out = sql.to_string();
    let mut names = crate::bundle_tools::cypher_params(sql);
    // Longest first, so `$days` is not replaced inside `$days_back`.
    names.sort_by_key(|n| std::cmp::Reverse(n.len()));
    for name in names {
        let n = params
            .get(&name)
            .and_then(Value::as_i64)
            .ok_or_else(|| format!("slot '{name}' must be a whole number"))?;
        out = out.replace(&format!("${name}"), &n.to_string());
    }
    Ok(out)
}

/// Add the SQL rows' columns to the graph rows with the same `join` value.
/// A graph row with no match gets those columns as null, so every row has
/// the same shape; SQL rows with no graph row are counted, not dropped.
///
/// When a graph row's `join` value is a list (a workflow's run ids, against a
/// ledger grouped by task), the SQL rows matching any member are summed into
/// it (numbers only) and the list is dropped from the row: it was the key, not
/// an answer.
fn join_rows(out: &mut Value, extra: &[Value], join: &str) {
    let key = |v: &Value| match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    };
    let mut by_key: std::collections::HashMap<String, &Map<String, Value>> = Default::default();
    let mut columns: Vec<String> = Vec::new();
    for r in extra {
        if let Some(m) = r.as_object() {
            for c in m.keys() {
                if c != join && !columns.contains(c) {
                    columns.push(c.clone());
                }
            }
            if let Some(k) = m.get(join) {
                by_key.insert(key(k), m);
            }
        }
    }
    let mut matched = std::collections::HashSet::new();
    if let Some(rows) = out["rows"].as_array_mut() {
        for row in rows.iter_mut() {
            let Some(obj) = row.as_object_mut() else {
                continue;
            };
            if let Some(Value::Array(members)) = obj.get(join).cloned() {
                let hits: Vec<&Map<String, Value>> = members
                    .iter()
                    .map(key)
                    .filter_map(|k| {
                        matched.insert(k.clone());
                        by_key.get(&k).copied()
                    })
                    .collect();
                for c in &columns {
                    let sum: Option<f64> = hits
                        .iter()
                        .filter_map(|m| m.get(c).and_then(number))
                        .fold(None, |acc, x| Some(acc.unwrap_or(0.0) + x));
                    // A count stays a whole number; money keeps six places.
                    let shown = |x: f64| {
                        if x.fract() == 0.0 && x.abs() < 1e15 {
                            json!(x as i64)
                        } else {
                            json!((x * 1e6).round() / 1e6)
                        }
                    };
                    obj.insert(c.clone(), sum.map(shown).unwrap_or(Value::Null));
                }
                obj.remove(join);
                continue;
            }
            let found = obj.get(join).map(key).and_then(|k| {
                matched.insert(k.clone());
                by_key.get(&k).copied()
            });
            for c in &columns {
                obj.insert(
                    c.clone(),
                    found.and_then(|m| m.get(c)).cloned().unwrap_or(Value::Null),
                );
            }
        }
    }
    let unmatched = by_key.keys().filter(|k| !matched.contains(*k)).count();

    if unmatched > 0 {
        out["sql_unmatched"] = json!(format!(
            "{unmatched} ledger row(s) have no graph row with the same {join}"
        ));
    }
}

/// One `views/*.yaml` file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewFile {
    pub name: String,
    pub kind: String,
    /// What it is: all a model sees, with `words`.
    pub description: String,
    /// What people call it ("daily brief", "news brief").
    pub words: Vec<String>,
    /// `kind: cypher`: the parameters, on the declared-tool rules.
    #[serde(default)]
    pub input_schema: Option<Value>,
    /// `kind: cypher`: the read query.
    #[serde(default)]
    pub cypher: Option<String>,
    /// `kind: notes`: the slots this view fixes; a call may not set them.
    #[serde(default)]
    pub preset: Option<NoteSpec>,
    /// `kind: notes`: slots a call may override.
    #[serde(default)]
    pub defaults: Option<NoteSpec>,
    /// `kind: cypher`: rows when the query has no LIMIT (default 30).
    #[serde(default)]
    pub limit: Option<usize>,
    /// `kind: notes`: the shape a call that asks for none gets when its notes
    /// are more than the limit (`learned`: a long window is counts by topic).
    #[serde(default)]
    pub wide: Option<Shape>,
    /// `kind: notes`: leave out notes a workflow of these kinds produced
    /// (`learned`: `maintenance`, the brain's upkeep, is not learning).
    #[serde(default)]
    pub except_work_kinds: Option<Vec<String>>,
    /// `kind: cypher`: a DuckDB SELECT (telemetry) whose columns join the
    /// graph rows on `join` (the `workflow_scorecard`: runs from the graph,
    /// cost from the ledger). Its `$name`s are integer slots only, so a
    /// caller's text never becomes SQL. It runs through `duckdb_query`.
    #[serde(default)]
    pub sql: Option<String>,
    /// `kind: cypher` with `sql`: the column both sides share.
    #[serde(default)]
    pub join: Option<String>,
}

impl ViewFile {
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
        let v: ViewFile = serde_yaml::from_str(&text).map_err(|e| format!("YAML: {e}"))?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if v.name != stem {
            return Err(format!(
                "name '{}' must equal the file name '{stem}'",
                v.name
            ));
        }
        v.check()?;
        Ok(v)
    }

    pub fn check(&self) -> Result<(), String> {
        if self.name.is_empty()
            || !self
                .name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        {
            return Err(format!(
                "name '{}' must be lowercase letters, digits and underscores",
                self.name
            ));
        }
        if self.description.trim().is_empty() {
            return Err("a view needs a description: it is what a model picks by".into());
        }
        if self.words.iter().all(|w| w.trim().is_empty()) {
            return Err("a view needs words: what people call the thing".into());
        }
        match self.kind.as_str() {
            "cypher" => {
                if self.preset.is_some()
                    || self.defaults.is_some()
                    || self.wide.is_some()
                    || self.except_work_kinds.is_some()
                {
                    return Err(
                        "preset, defaults, wide and except_work_kinds belong to kind: notes".into(),
                    );
                }
                let (Some(schema), Some(cypher)) = (&self.input_schema, &self.cypher) else {
                    return Err("kind: cypher needs input_schema and cypher".into());
                };
                match (&self.sql, &self.join) {
                    (None, None) => {}
                    (Some(sql), Some(join)) => {
                        if join.trim().is_empty() {
                            return Err("join names the column both sides share".into());
                        }
                        for p in crate::bundle_tools::cypher_params(sql) {
                            if schema["properties"][&p]["type"] != "integer" {
                                return Err(format!(
                                    "the sql uses ${p}: only an integer slot may reach the SQL"
                                ));
                            }
                        }
                    }
                    _ => return Err("sql and join go together".into()),
                }
                for (slot, q) in values_from(schema) {
                    if let Some(kw) = agent_brain_retrieve::query::write_keyword(&q) {
                        return Err(format!("slot '{slot}': values_from writes ({kw})"));
                    }
                }
                check_query(schema, cypher, RESERVED)
            }
            "notes" => {
                if matches!(self.wide, Some(Shape::Rows | Shape::Count)) {
                    return Err(
                        "wide is a by_* shape (by_topic, by_kind, by_source, by_day)".into(),
                    );
                }
                if self.input_schema.is_some()
                    || self.cypher.is_some()
                    || self.limit.is_some()
                    || self.sql.is_some()
                    || self.join.is_some()
                {
                    return Err(
                        "kind: notes takes preset and defaults, not input_schema, cypher, limit, sql or join"
                            .into(),
                    );
                }
                let fixed = self.fixed();
                let defaulted = self
                    .defaults
                    .as_ref()
                    .map(NoteSpec::set_slots)
                    .unwrap_or_default();
                if let Some(s) = defaulted.iter().find(|s| fixed.contains(s)) {
                    return Err(format!("slot '{s}' is both preset and defaulted"));
                }
                // The preset and defaults must compile on their own.
                let spec = self.preset.clone().unwrap_or_default();
                let spec = spec.overlay(&self.defaults.clone().unwrap_or_default());
                spec.compile(&Clock::live()).map(|_| ())
            }
            k => Err(format!("kind '{k}' is not one of {}", KINDS.join(", "))),
        }
    }

    /// The note slots this view fixes.
    fn fixed(&self) -> Vec<&'static str> {
        self.preset
            .as_ref()
            .map(NoteSpec::set_slots)
            .unwrap_or_default()
    }

    /// The slots a call may set, as a JSON schema.
    pub fn slots(&self) -> Value {
        match self.kind.as_str() {
            "notes" => note_slots_schema(&self.fixed()),
            _ => self
                .input_schema
                .clone()
                .unwrap_or_else(|| json!({"type": "object"})),
        }
    }

    /// The catalog entry `lookup {}` lists.
    fn entry(&self, plugin: &str) -> Value {
        let mut e = json!({
            "view": self.name,
            "description": self.description,
            "words": self.words,
            "slots": self.slots()["properties"],
            "plugin": plugin,
        });
        if let Some(p) = &self.preset {
            e["fixed"] = serde_json::to_value(p).unwrap_or_default();
        }
        if let Some(d) = &self.defaults {
            e["defaults"] = serde_json::to_value(d).unwrap_or_default();
        }
        e
    }
}

/// The caller-supplied parameters of a `kind: cypher` view.
fn reserved_params(c: &Clock) -> Map<String, Value> {
    let mut p = Map::new();
    p.insert(
        "today".into(),
        json!(c.today.format("%Y-%m-%d").to_string()),
    );
    p.insert(
        "day_start".into(),
        json!((c.midnight)(c.today).to_rfc3339()),
    );
    p.insert("now".into(), json!(c.now.to_rfc3339()));
    p.insert("tz".into(), json!(c.tz));
    p
}

/// The live closed lists of the note slots: the names a model cannot guess
/// (Phase 2 step 5: E4B wrote `entity: "mesh networks"` for the topic
/// `off-grid-networking`, and invented source names). A query returns `value`
/// and an optional `label`.
const NOTE_VALUES: &[(&str, &str)] = &[
    (
        "topic",
        "MATCH (t:Topic) WITH t, coalesce(t.aliases, []) AS al \
         RETURN t.name AS value, coalesce(t.label, t.name) + CASE WHEN size(al) = 0 THEN '' \
           ELSE '; also called ' + reduce(s = '', a IN al | s + CASE WHEN s = '' THEN '' ELSE ', ' END + a) END AS label \
         ORDER BY value",
    ),
    (
        "from",
        "MATCH (n:Note) WHERE n.source_context IS NOT NULL AND size(n.source_context) <= 40 \
         RETURN n.source_context AS value, count(*) AS notes ORDER BY notes DESC LIMIT 40",
    ),
    (
        "kinds",
        "MATCH (n:Note) RETURN n.note_type AS value, count(*) AS notes ORDER BY notes DESC",
    ),
];

/// `""`, `[]`, `{}` or null: a slot left unset.
fn is_empty_value(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.trim().is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(o) => o.is_empty(),
        _ => false,
    }
}

/// A cypher view's `values_from` slots: `(slot, query)`.
fn values_from(schema: &Value) -> Vec<(String, String)> {
    schema["properties"]
        .as_object()
        .map(|m| {
            m.iter()
                .filter_map(|(k, v)| {
                    v["values_from"]
                        .as_str()
                        .map(|q| (k.clone(), q.to_string()))
                })
                .collect()
        })
        .unwrap_or_default()
}

// ── The lookup skill ────────────────────────────────────────────────────────

pub struct LookupSkill {
    query: Arc<QuerySkill>,
    views: Vec<(String, ViewFile)>,
    /// Files left out, with why: listed by `lookup {}` so a broken view is
    /// visible where it is missed.
    skipped: Vec<String>,
}

impl LookupSkill {
    /// The views of every loaded bundle, core first. A file that fails its
    /// checks, or reuses a name, is left out with the reason.
    pub fn load(plugins: &Plugins, query: Arc<QuerySkill>) -> Self {
        let mut views: Vec<(String, ViewFile)> = Vec::new();
        let mut skipped = Vec::new();
        for (plugin, path) in plugins.files("views", "yaml") {
            match ViewFile::load(&path) {
                Ok(v) if views.iter().any(|(_, w)| w.name == v.name) => {
                    skipped.push(format!("{plugin}/{}: name already taken", v.name))
                }
                Ok(v) => views.push((plugin, v)),
                Err(e) => skipped.push(format!("{}: {e}", path.display())),
            }
        }
        for s in &skipped {
            warn!("view skipped: {s}");
        }
        info!(
            views = %views.iter().map(|(_, v)| v.name.as_str()).collect::<Vec<_>>().join(", "),
            "Views loaded"
        );
        Self {
            query,
            views,
            skipped,
        }
    }

    fn view(&self, name: &str) -> Option<&ViewFile> {
        self.views.iter().map(|(_, v)| v).find(|v| v.name == name)
    }

    fn definition(&self) -> ToolDefinition {
        let names: Vec<&str> = self.views.iter().map(|(_, v)| v.name.as_str()).collect();
        let mut props = note_slots_schema(&[])["properties"].clone();
        props["view"] = json!({
            "type": "string",
            "enum": names,
            "description": "The view to run. Call lookup with no arguments to list every view: what it is, the words for it, and its slots."
        });
        ToolDefinition {
            name: LOOKUP_TOOL.to_string(),
            description: "Fetch context from the brain's graph by a named view: a thing the brain \
                stores (the daily brief, open todos, claims, what it learned in a window, the \
                agents, an API's endpoints). Call with no arguments to list the views and their \
                slots; call with `view` and its slots to run one. Rows carry where each note came \
                from and its standing (corroborated, unverified, refuted, not checked), so say it. \
                An empty answer says what was looked for and what does exist. Prefer this to \
                writing Cypher."
                .to_string(),
            input_schema: json!({"type": "object", "properties": props, "additionalProperties": true}),
        }
    }

    /// The `value`s (and `label`s) a values query returns; empty on failure.
    async fn values(&self, q: &str) -> (Vec<Value>, Map<String, Value>) {
        let rows = self
            .run(q, Map::new(), Some(MAX_LIMIT))
            .await
            .ok()
            .and_then(|r| r["rows"].as_array().cloned())
            .unwrap_or_default();
        let mut labels = Map::new();
        let values = rows
            .iter()
            .filter(|r| !r["value"].is_null())
            .map(|r| {
                if let (Some(v), Some(l)) = (r["value"].as_str(), r["label"].as_str()) {
                    labels.insert(v.to_string(), json!(l));
                }
                r["value"].clone()
            })
            .collect();
        (values, labels)
    }

    /// A slot schema with its live values as `enum` (on `items` for a list)
    /// and their `labels`; `values_from` itself is not shown.
    fn with_values(slot: &mut Value, values: Vec<Value>, labels: Map<String, Value>) {
        if let Some(o) = slot.as_object_mut() {
            o.remove("values_from");
        }
        if values.is_empty() {
            return;
        }
        if slot["type"] == "array" {
            slot["items"]["enum"] = Value::Array(values);
        } else {
            slot["enum"] = Value::Array(values);
        }
        if !labels.is_empty() {
            slot["labels"] = Value::Object(labels);
        }
    }

    async fn catalog(&self) -> ToolCallResult {
        let mut memo: std::collections::HashMap<String, (Vec<Value>, Map<String, Value>)> =
            Default::default();
        let mut views = Vec::new();
        for (plugin, v) in &self.views {
            let mut e = v.entry(plugin);
            let queries: Vec<(String, String)> = match v.kind.as_str() {
                "notes" => NOTE_VALUES
                    .iter()
                    .map(|(s, q)| (s.to_string(), q.to_string()))
                    .collect(),
                _ => values_from(v.input_schema.as_ref().unwrap_or(&Value::Null)),
            };
            for (slot, q) in queries {
                if e["slots"].get(&slot).is_none() {
                    continue;
                }
                if !memo.contains_key(&q) {
                    let got = self.values(&q).await;
                    memo.insert(q.clone(), got);
                }
                let (vals, labels) = memo[&q].clone();
                Self::with_values(&mut e["slots"][&slot], vals, labels);
            }
            views.push(e);
        }
        let mut out = json!({"count": views.len(), "views": views});
        if !self.skipped.is_empty() {
            out["skipped"] = json!(self.skipped);
        }
        ToolCallResult::success_json(out)
    }

    async fn run(
        &self,
        cypher: &str,
        params: Map<String, Value>,
        limit: Option<usize>,
    ) -> Result<Value, String> {
        let mut call = json!({"cypher": cypher, "params": params, "readonly": true});
        if let Some(l) = limit {
            call["limit"] = json!(l);
        }
        let r = self
            .query
            .execute("neo4j_query", Some(call))
            .await
            .ok_or("neo4j_query is not available")?;
        let text = match r.content.first() {
            Some(agent_brain_protocol::Content::Text { text }) => text.clone(),
            _ => String::new(),
        };
        if r.is_error == Some(true) {
            return Err(text);
        }
        serde_json::from_str(&text).map_err(|_| text)
    }

    /// Runs with one row past `limit`; when that row comes back, drops it and
    /// says the answer was cut off (`neo4j_query`'s own LIMIT is silent).
    async fn run_capped(
        &self,
        cypher: &str,
        params: Map<String, Value>,
        limit: usize,
    ) -> Result<Value, String> {
        let mut out = self.run(cypher, params, Some(limit + 1)).await?;
        if let Some(rows) = out["rows"].as_array_mut()
            && rows.len() > limit
        {
            rows.truncate(limit);
            out["count"] = json!(limit);
            out["truncated"] = json!(format!("the first {limit} rows; there are more"));
        }
        Ok(out)
    }

    /// How many notes `spec` matches, words matched as its rows match them.
    async fn count(&self, spec: &NoteSpec, c: &Clock) -> Option<i64> {
        let mut s = spec.clone();
        s.any_words = spec.shape.unwrap_or(Shape::Rows) == Shape::Rows;
        s.shape = Some(Shape::Count);
        let (cy, p) = s.compile(c).ok()?;
        let r = self.run(&cy, p, None).await.ok()?;
        r["rows"][0]["notes"].as_i64()
    }

    async fn run_notes(&self, view: &ViewFile, args: &Map<String, Value>) -> ToolCallResult {
        let mut call: NoteSpec = match serde_json::from_value(Value::Object(args.clone())) {
            Ok(s) => s,
            Err(e) => {
                return ToolCallResult::error(format!(
                    "view '{}': {e}. Its slots: {}",
                    view.name,
                    view.slots()["properties"]
                ));
            }
        };
        let fixed = view.fixed();
        if let Some(s) = call.set_slots().iter().find(|s| fixed.contains(s)) {
            return ToolCallResult::error(format!(
                "view '{}' fixes '{s}'; use view 'notes' to choose it yourself",
                view.name
            ));
        }
        // A topic may be named by its label or an alias; it runs as its name.
        let mut resolved = None;
        if let Some(t) = call.topic.clone() {
            let (topics, _) = self.values(NOTE_VALUES[0].1).await;
            if !topics.iter().any(|x| x.as_str() == Some(t.trim()))
                && let Some(name) = self.topic_named(&t).await
            {
                resolved = Some(format!("topic '{}' is '{name}'", t.trim()));
                call.topic = Some(name);
            } else if !topics.is_empty() && !topics.iter().any(|x| x.as_str() == Some(t.trim())) {
                return ToolCallResult::error(format!(
                    "No topic named '{t}'. Topics: {}",
                    topics
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        let preset = view.preset.clone().unwrap_or_default();
        let mut spec = preset
            .overlay(&view.defaults.clone().unwrap_or_default())
            .overlay(&call);
        spec.except_work = view.except_work_kinds.clone().unwrap_or_default();
        let c = Clock::live();
        let (mut cypher, mut params) = match spec.compile(&c) {
            Ok(x) => x,
            Err(e) => return ToolCallResult::error(format!("view '{}': {e}", view.name)),
        };
        let shape = spec.shape.unwrap_or(Shape::Rows);
        // Rows carry their own LIMIT; an aggregate is capped here, and says so.
        let ran = if shape == Shape::Rows {
            self.run(&cypher, params.clone(), None).await
        } else {
            self.run_capped(&cypher, params.clone(), MAX_GROUPS).await
        };
        let mut out = match ran {
            Ok(v) => v,
            Err(e) => return ToolCallResult::error(e),
        };
        let returned = out["count"].as_i64().unwrap_or(0);
        let empty = match shape {
            Shape::Count => out["rows"][0]["notes"].as_i64() == Some(0),
            _ => returned == 0,
        };
        if shape == Shape::Rows && !empty {
            let limit = spec.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, MAX_LIMIT) as i64;
            if returned >= limit
                && let Some(total) = self.count(&spec, &c).await
            {
                out["total"] = json!(total);
                if total > returned {
                    out["truncated"] = json!(format!(
                        "{returned} of {total} rows; raise limit (max {MAX_LIMIT}), narrow the window, or use as: count / by_topic / by_day"
                    ));
                }
                // No shape asked and too many to read: the view's wide shape.
                if let (None, Some(wide)) = (spec.shape, view.wide)
                    && total > returned
                {
                    let mut w = spec.clone();
                    w.shape = Some(wide);
                    w.any_words = true;
                    let (wc, wp) = match w.compile(&c) {
                        Ok(x) => x,
                        Err(e) => {
                            return ToolCallResult::error(format!("view '{}': {e}", view.name));
                        }
                    };
                    out = match self.run_capped(&wc, wp.clone(), MAX_GROUPS).await {
                        Ok(v) => v,
                        Err(e) => return ToolCallResult::error(e),
                    };
                    out["total"] = json!(total);
                    out["wide"] = json!(format!(
                        "{total} notes, more than the {limit} a context reads: counts {} instead. \
                         To read notes, call again with as: rows and a topic, or a shorter window.",
                        wide.as_str()
                    ));
                    (cypher, params) = (wc, wp);
                }
            }
        }
        if empty {
            out["empty"] = json!(self.explain_empty(&spec, &c).await);
        }
        if shape == Shape::Rows {
            let found = out["total"].as_i64().unwrap_or(returned);
            if let Some(t) = self.thin(&spec, found, &c).await {
                out["thin"] = json!(t);
            }
        }
        if let Some(r) = resolved {
            out["resolved"] = json!(r);
        }
        out["view"] = json!(view.name);
        out["cypher"] = json!(cypher);
        out["params"] = Value::Object(params);
        ToolCallResult::success_json(out)
    }

    /// The topic whose name, label or alias is `said` (any case).
    async fn topic_named(&self, said: &str) -> Option<String> {
        let r = self
            .run(
                "MATCH (t:Topic) WHERE toLower(t.name) = $s OR toLower(coalesce(t.label, '')) = $s \
                   OR $s IN [a IN coalesce(t.aliases, []) | toLower(trim(a))] \
                 RETURN t.name AS name LIMIT 1",
                Map::from_iter([("s".to_string(), json!(said.trim().to_lowercase()))]),
                None,
            )
            .await
            .ok()?;
        r["rows"][0]["name"].as_str().map(String::from)
    }

    /// What an empty answer means: the filter in words, then how many exist
    /// without the window and without the narrowing slots.
    /// A short window that found well under its usual share says so, with the
    /// days before it: one note "today" reads as all there is unless the
    /// answer can say the days before had hundreds (a stalled feed, not a
    /// quiet day). `None` for a long window, or a count near the usual.
    async fn thin(&self, spec: &NoteSpec, found: i64, c: &Clock) -> Option<String> {
        let w = spec.window.as_ref()?;
        let day = match (&w.day, w.hours, w.days) {
            (Some(d), _, _) => match d.trim() {
                "today" => c.today,
                "yesterday" => c.today - Duration::days(1),
                s => parse_date(s, "day").ok()?,
            },
            (None, Some(h), _) if h <= 24 => c.today,
            (None, None, Some(1)) => c.today,
            _ => return None,
        };
        let mut before = spec.clone();
        before.shape = Some(Shape::ByDay);
        before.any_words = true;
        before.window = Some(Window {
            since: Some((day - Duration::days(BASELINE_DAYS)).to_string()),
            until: Some((day - Duration::days(1)).to_string()),
            ..Default::default()
        });
        let (cy, p) = before.compile(c).ok()?;
        let rows = self.run(&cy, p, None).await.ok()?["rows"]
            .as_array()?
            .clone();
        let per_day: Vec<String> = rows
            .iter()
            .filter_map(|r| Some(format!("{} {}", r["day"].as_str()?, r["notes"].as_i64()?)))
            .collect();
        let sum: i64 = rows.iter().filter_map(|r| r["notes"].as_i64()).sum();
        let avg = sum as f64 / BASELINE_DAYS as f64;
        // Today is partly over: its usual share is the part of a day gone by.
        let share = if day == c.today && w.day.is_some() {
            let gone = (c.now - (c.midnight)(c.today)).num_minutes() as f64 / 1440.0;
            gone.clamp(0.0, 1.0)
        } else if w.hours.is_some() {
            w.hours.unwrap_or(24) as f64 / 24.0
        } else {
            1.0
        };
        let usual = avg * share;
        if usual < 4.0 || found as f64 >= usual / 2.0 {
            return None;
        }
        Some(format!(
            "{found} found, against about {usual:.0} usual for this window (the {BASELINE_DAYS} days before averaged {avg:.0} a day: {}). \
             Far fewer than usual: say so, and give the days before.",
            if per_day.is_empty() {
                "none".to_string()
            } else {
                per_day.join(", ")
            }
        ))
    }

    async fn explain_empty(&self, spec: &NoteSpec, c: &Clock) -> String {
        let mut said = vec![format!("No {}.", spec.describe())];
        if let Some(e) = &spec.entity {
            let known = self
                .run(
                    "MATCH (e:Entity) WHERE e.name = $n OR EXISTS { MATCH (:EntityAlias {name: $n})-[:ALIAS_OF]->(e) } RETURN count(e) AS n",
                    Map::from_iter([("n".to_string(), json!(e.trim().to_lowercase()))]),
                    None,
                )
                .await
                .ok()
                .and_then(|r| r["rows"][0]["n"].as_i64())
                .unwrap_or(0);
            if known == 0 {
                let last = e.split_whitespace().last().unwrap_or(e).to_lowercase();
                let near = self
                    .run(
                        "MATCH (x:Entity) WHERE x.name CONTAINS $w RETURN x.name AS name ORDER BY size(x.name) LIMIT 5",
                        Map::from_iter([("w".to_string(), json!(last))]),
                        None,
                    )
                    .await
                    .ok()
                    .map(|r| {
                        r["rows"]
                            .as_array()
                            .map(|a| a.iter().filter_map(|x| x["name"].as_str().map(String::from)).collect::<Vec<_>>())
                            .unwrap_or_default()
                    })
                    .unwrap_or_default();
                said.push(if near.is_empty() {
                    format!("No entity is named '{e}'.")
                } else {
                    format!("No entity is named '{e}'; nearest: {}.", near.join(", "))
                });
            }
        }
        if spec.window.is_some() {
            let mut s = spec.clone();
            s.window = None;
            s.on = None;
            if let Some(n) = self.count(&s, c).await {
                said.push(format!("Without the window: {n}."));
            }
        }
        if spec.topic.is_some() || spec.entity.is_some() || spec.words.is_some() {
            let mut s = spec.clone();
            s.topic = None;
            s.entity = None;
            s.words = None;
            if let Some(n) = self.count(&s, c).await {
                said.push(format!("Without topic, entity and words: {n}."));
            }
        }
        said.join(" ")
    }

    async fn run_cypher(&self, view: &ViewFile, args: &Map<String, Value>) -> ToolCallResult {
        let schema = view.input_schema.clone().unwrap_or_default();
        let declared: Vec<String> = schema["properties"]
            .as_object()
            .map(|m| m.keys().cloned().collect())
            .unwrap_or_default();
        if let Some(k) = args.keys().find(|k| !declared.contains(k)) {
            return ToolCallResult::error(format!(
                "view '{}' has no slot '{k}'. Its slots: {}",
                view.name, schema["properties"]
            ));
        }
        if let Some(req) = schema["required"].as_array()
            && let Some(missing) = req
                .iter()
                .filter_map(Value::as_str)
                .find(|r| !args.contains_key(*r))
        {
            return ToolCallResult::error(format!(
                "view '{}' needs '{missing}'. Its slots: {}",
                view.name, schema["properties"]
            ));
        }
        for (slot, q) in values_from(&schema) {
            let Some(given) = args
                .get(&slot)
                .and_then(Value::as_str)
                .filter(|g| !g.is_empty())
            else {
                continue;
            };
            let (vals, _) = self.values(&q).await;
            if !vals.is_empty() && !vals.iter().any(|v| v.as_str() == Some(given)) {
                return ToolCallResult::error(format!(
                    "view '{}': '{given}' is not a known {slot}. Known: {}",
                    view.name,
                    vals.iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(", ")
                ));
            }
        }
        let cypher = view.cypher.clone().unwrap_or_default();
        let mut p = params(&schema, args);
        let c = Clock::live();
        for (k, v) in reserved_params(&c) {
            if cypher.contains(&format!("${k}")) {
                p.insert(k, v);
            }
        }
        let mut out = match self
            .run_capped(&cypher, p.clone(), view.limit.unwrap_or(DEFAULT_LIMIT))
            .await
        {
            Ok(v) => v,
            Err(e) => return ToolCallResult::error(e),
        };
        if out["count"].as_i64() == Some(0) {
            out["empty"] = json!(format!(
                "View '{}' found nothing: {}",
                view.name, view.description
            ));
        }
        if let (Some(sql), Some(join)) = (&view.sql, &view.join) {
            match integer_sql(sql, &p) {
                Ok(sql) => {
                    match self.run_sql(&sql).await {
                        Ok(extra) => join_rows(&mut out, &extra, join),
                        // The graph half stands; the missing half says why.
                        Err(e) => out["sql_error"] = json!(e),
                    }
                    out["sql"] = json!(sql);
                }
                Err(e) => return ToolCallResult::error(format!("view '{}': {e}", view.name)),
            }
        }
        out["view"] = json!(view.name);
        out["cypher"] = json!(cypher);
        out["params"] = Value::Object(p);
        ToolCallResult::success_json(out)
    }

    /// A view's DuckDB half, through `duckdb_query` (its read-only gate).
    async fn run_sql(&self, sql: &str) -> Result<Vec<Value>, String> {
        let r = self
            .query
            .execute("duckdb_query", Some(json!({"sql": sql, "limit": 20000})))
            .await
            .ok_or("duckdb_query is not available")?;
        let text = match r.content.first() {
            Some(agent_brain_protocol::Content::Text { text }) => text.clone(),
            _ => String::new(),
        };
        if r.is_error == Some(true) {
            return Err(text);
        }
        let v: Value = serde_json::from_str(&text).map_err(|e| format!("duckdb_query: {e}"))?;
        Ok(v["rows"].as_array().cloned().unwrap_or_default())
    }
}

#[async_trait]
impl Skill for LookupSkill {
    fn name(&self) -> &str {
        "Lookup"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![self.definition()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        if tool_name != LOOKUP_TOOL {
            return None;
        }
        let mut args = arguments
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let Some(name) = args.remove("view") else {
            return Some(self.catalog().await);
        };
        // An empty value is an unset slot: a small model filling a schema
        // writes `entity: ""`, `kinds: []` or `window: {}` for slots it skips.
        args.retain(|_, v| !is_empty_value(v));
        // The queue stamps `__owner_task_id`, `__data_class`, … on every job
        // a workflow step runs; they are plumbing, not slots.
        args.retain(|k, _| !k.starts_with("__"));
        if let Some(Value::Object(w)) = args.get_mut("window") {
            w.retain(|_, v| !is_empty_value(v));
            if w.is_empty() {
                args.remove("window");
            }
        }
        let Some(name) = name.as_str().map(str::to_string) else {
            return Some(ToolCallResult::error("`view` must be a view name (string)"));
        };
        let Some(view) = self.view(&name) else {
            let names: Vec<&str> = self.views.iter().map(|(_, v)| v.name.as_str()).collect();
            return Some(ToolCallResult::error(format!(
                "No view named '{name}'. Views: {}. Call lookup with no arguments for what each is.",
                names.join(", ")
            )));
        };
        // A list of strings for a text slot is the same words: join them
        // (codestral sent `words: ["Nebula", "Tailscale"]`).
        let slots = view.slots();
        for (k, v) in args.iter_mut() {
            if slots["properties"][k.as_str()]["type"] == "string"
                && let Value::Array(items) = v
                && items.iter().all(Value::is_string)
            {
                *v = json!(
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>()
                        .join(" ")
                );
            }
        }
        Some(match view.kind.as_str() {
            "notes" => self.run_notes(view, &args).await,
            _ => self.run_cypher(view, &args).await,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_integer_slots_reach_a_views_sql() {
        let mut p = Map::new();
        p.insert("days".into(), json!(30));
        p.insert("days_back".into(), json!(7));
        assert_eq!(
            integer_sql(
                "WHERE a > now() - INTERVAL $days DAY AND b < $days_back",
                &p
            )
            .unwrap(),
            "WHERE a > now() - INTERVAL 30 DAY AND b < 7"
        );
        p.insert("days".into(), json!("30; DROP TABLE x"));
        assert!(integer_sql("INTERVAL $days DAY", &p).is_err());
    }

    #[test]
    fn ledger_columns_join_the_graph_rows() {
        let mut out = json!({"rows": [
            {"workflow": "a", "runs": 3},
            {"workflow": "b", "runs": 1},
        ]});
        let extra = vec![
            json!({"workflow": "a", "usd": 0.5}),
            json!({"workflow": "z", "usd": 9.0}),
        ];
        join_rows(&mut out, &extra, "workflow");
        assert_eq!(out["rows"][0]["usd"], json!(0.5));
        assert_eq!(out["rows"][1]["usd"], Value::Null);
        assert!(
            out["sql_unmatched"]
                .as_str()
                .unwrap()
                .starts_with("1 ledger row")
        );
    }

    #[test]
    fn a_list_key_sums_its_members_and_is_dropped() {
        let mut out = json!({"rows": [
            {"workflow": "a", "tasks": ["t1", "t2", "t3"]},
            {"workflow": "b", "tasks": []},
        ]});
        let extra = vec![
            json!({"tasks": "t1", "usd": 0.25, "calls": "2"}),
            json!({"tasks": "t3", "usd": 0.5, "calls": 1}),
        ];
        join_rows(&mut out, &extra, "tasks");
        assert_eq!(out["rows"][0]["usd"], json!(0.75));
        assert_eq!(out["rows"][0]["calls"], json!(3));
        assert!(out["rows"][0].get("tasks").is_none());
        assert_eq!(out["rows"][1]["usd"], Value::Null);
        assert!(out.get("sql_unmatched").is_none());
    }

    #[test]
    fn a_view_sql_may_use_integer_slots_only() {
        let v = |sql: &str| ViewFile {
            name: "x".into(),
            kind: "cypher".into(),
            description: "d".into(),
            words: vec!["x".into()],
            input_schema: Some(json!({"type": "object", "properties": {
                "days": {"type": "integer", "default": 30},
                "name": {"type": "string", "default": "a"}}})),
            cypher: Some("MATCH (n) RETURN n.name AS workflow".into()),
            preset: None,
            defaults: None,
            limit: None,
            wide: None,
            except_work_kinds: None,
            sql: Some(sql.into()),
            join: Some("workflow".into()),
        };
        assert!(v("SELECT 1 WHERE x > $days").check().is_ok());
        let e = v("SELECT 1 WHERE x = '$name'").check().unwrap_err();
        assert!(e.contains("only an integer slot"), "{e}");
    }

    /// 2026-10-13 12:00 UTC; local midnight is 04:00 UTC (America/Detroit, EDT).
    fn clock() -> Clock {
        Clock {
            now: DateTime::parse_from_rfc3339("2026-10-13T12:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            today: NaiveDate::from_ymd_opt(2026, 10, 13).unwrap(),
            midnight: Box::new(|d| d.and_hms_opt(4, 0, 0).unwrap().and_utc()),
            tz: "America/Detroit".into(),
        }
    }

    fn spec(v: Value) -> NoteSpec {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_day_is_bounded_by_local_midnights_never_by_strings() {
        let (cy, p) = spec(json!({"window": {"day": "today"}}))
            .compile(&clock())
            .unwrap();
        assert!(cy.contains("n.created_at >= datetime($since)"), "{cy}");
        assert!(cy.contains("n.created_at < datetime($until)"), "{cy}");
        assert_eq!(p["since"], "2026-10-13T04:00:00+00:00");
        assert_eq!(p["until"], "2026-10-14T04:00:00+00:00");
        let (_, p) = spec(json!({"window": {"day": "yesterday"}}))
            .compile(&clock())
            .unwrap();
        assert_eq!(p["since"], "2026-10-12T04:00:00+00:00");
    }

    #[test]
    fn windows_cover_hours_months_and_ranges() {
        let c = clock();
        let (_, p) = spec(json!({"window": {"hours": 24}})).compile(&c).unwrap();
        assert_eq!(p["since"], "2026-10-12T12:00:00+00:00");
        assert!(p.get("until").is_none());
        let (_, p) = spec(json!({"window": {"month": "2026-09"}}))
            .compile(&c)
            .unwrap();
        assert_eq!(
            (p["since"].as_str(), p["until"].as_str()),
            (
                Some("2026-09-01T04:00:00+00:00"),
                Some("2026-10-01T04:00:00+00:00")
            )
        );
        let (_, p) = spec(json!({"window": {"month": "last"}}))
            .compile(&c)
            .unwrap();
        assert_eq!(p["since"], "2026-09-01T04:00:00+00:00");
        let (_, p) = spec(json!({"window": {"month": "this"}}))
            .compile(&c)
            .unwrap();
        assert_eq!(p["since"], "2026-10-01T04:00:00+00:00");
        let (_, p) = spec(json!({"window": {"since": "2026-04-16", "until": "2026-04-16"}}))
            .compile(&c)
            .unwrap();
        assert_eq!(p["until"], "2026-04-17T04:00:00+00:00");
        assert!(
            spec(json!({"window": {"days": 2, "hours": 3}}))
                .compile(&c)
                .is_err()
        );
        assert!(
            spec(json!({"window": {"day": "13/10/2026"}}))
                .compile(&c)
                .unwrap_err()
                .contains("YYYY-MM-DD")
        );
    }

    #[test]
    fn a_view_can_leave_out_what_its_upkeep_wrote() {
        let mut s = spec(json!({"window": {"day": "today"}}));
        s.except_work = vec!["maintenance".into()];
        let (cy, p) = s.compile(&clock()).unwrap();
        assert!(
            cy.contains("NOT EXISTS { MATCH (pt:Task {id: n.produced_by_task})")
                && cy.contains("pw.kind IN $except_work"),
            "{cy}"
        );
        assert_eq!(p["except_work"], json!(["maintenance"]));
        assert!(s.describe().contains("not made by maintenance workflows"));
        let (plain, _) = spec(json!({})).compile(&clock()).unwrap();
        assert!(!plain.contains("except_work"), "{plain}");
    }

    #[test]
    fn names_are_matched_lower_case_and_every_value_is_a_parameter() {
        let (cy, p) = spec(json!({"entity": "Adam Kinzinger", "topic": "nia", "words": "Emergency Shutdown", "kinds": ["claim"]}))
            .compile(&clock())
            .unwrap();
        assert_eq!(p["entity"], "adam kinzinger");
        assert_eq!(p["words"], json!(["emergency", "shutdown"]));
        assert!(
            cy.contains("any(w IN $words") && cy.contains("ORDER BY hits DESC"),
            "{cy}"
        );
        let (count, p2) = spec(json!({"words": "a, Emergency-shutdown!", "as": "count"}))
            .compile(&clock())
            .unwrap();
        assert!(count.contains("all(w IN $words"), "{count}");
        let mut total = spec(json!({"words": "nebula tailscale", "as": "count"}));
        total.any_words = true;
        let (total, _) = total.compile(&clock()).unwrap();
        assert!(total.contains("any(w IN $words"), "{total}");
        assert_eq!(p2["words"], json!(["emergency-shutdown"]));
        assert!(cy.contains("EntityAlias {name: $entity}"), "{cy}");
        assert!(cy.contains("Topic {name: $topic}"), "{cy}");
        assert!(!cy.contains("Kinzinger") && !cy.contains("'nia'"), "{cy}");
    }

    #[test]
    fn rows_say_where_a_note_came_from_and_its_standing() {
        let (cy, p) = spec(json!({})).compile(&clock()).unwrap();
        assert!(
            cy.contains("AS standing") && cy.contains("'not checked'") && cy.contains("AS from"),
            "{cy}"
        );
        assert_eq!(p["limit"], DEFAULT_LIMIT);
        let (cy, _) = spec(json!({"as": "by_day", "on": "verified", "window": {"days": 2}}))
            .compile(&clock())
            .unwrap();
        assert!(
            cy.contains("n.verified_at >= datetime($since)") && cy.contains("AS day"),
            "{cy}"
        );
        let (cy, _) = spec(json!({"on": "verified"})).compile(&clock()).unwrap();
        assert!(cy.contains("n.verified_at IS NOT NULL"), "{cy}");
        let (cy, _) = spec(json!({"as": "by_topic"})).compile(&clock()).unwrap();
        assert!(
            cy.contains("OPTIONAL MATCH (n)-[:IN_TOPIC]") && cy.contains("'(no topic)'"),
            "{cy}"
        );
        assert!(spec(json!({"status": "true"})).compile(&clock()).is_err());
        assert!(serde_json::from_value::<NoteSpec>(json!({"colour": 1})).is_err());
    }

    fn view(v: Value) -> ViewFile {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn a_notes_view_fixes_its_preset_and_lists_the_rest_as_slots() {
        let v = view(json!({
            "name": "daily_brief", "kind": "notes", "description": "d", "words": ["daily brief"],
            "preset": {"from": ["scheduled_daily_news_brief"], "kinds": ["news"]},
            "defaults": {"limit": 1}
        }));
        assert!(v.check().is_ok());
        assert_eq!(v.fixed(), ["kinds", "from"]);
        let slots = v.slots();
        assert!(
            slots["properties"].get("from").is_none()
                && slots["properties"].get("window").is_some()
        );
        let clash = view(json!({
            "name": "x", "kind": "notes", "description": "d", "words": ["x"],
            "preset": {"limit": 1}, "defaults": {"limit": 2}
        }));
        assert!(clash.check().unwrap_err().contains("both"));
        let no_words = view(json!({"name": "x", "kind": "notes", "description": "d", "words": []}));
        assert!(no_words.check().unwrap_err().contains("words"));
        let wide = |w: &str| {
            view(
                json!({"name": "x", "kind": "notes", "description": "d", "words": ["x"], "wide": w}),
            )
        };
        assert!(wide("by_topic").check().is_ok());
        assert!(wide("rows").check().unwrap_err().contains("by_*"));
        assert!(wide("count").check().is_err());
    }

    #[test]
    fn a_cypher_view_may_use_the_reserved_parameters_only() {
        let ok = view(json!({
            "name": "overdue", "kind": "cypher", "description": "d", "words": ["overdue"],
            "input_schema": {"type": "object", "properties": {}},
            "cypher": "MATCH (t:Todo) WHERE date(t.due_at) < date($today) RETURN t.title AS title"
        }));
        assert!(ok.check().is_ok());
        let undeclared = view(json!({
            "name": "x", "kind": "cypher", "description": "d", "words": ["x"],
            "input_schema": {"type": "object", "properties": {}},
            "cypher": "MATCH (t:Todo {id: $id}) RETURN t"
        }));
        assert!(undeclared.check().unwrap_err().contains("$id"));
        let shadow = view(json!({
            "name": "x", "kind": "cypher", "description": "d", "words": ["x"],
            "input_schema": {"type": "object", "properties": {"today": {"type": "string", "default": "x"}}},
            "cypher": "RETURN $today AS t"
        }));
        assert!(shadow.check().unwrap_err().contains("reserved"));
    }

    #[test]
    fn empty_values_are_unset_slots() {
        assert!(
            is_empty_value(&json!("")) && is_empty_value(&json!([])) && is_empty_value(&json!({}))
        );
        assert!(
            !is_empty_value(&json!(0))
                && !is_empty_value(&json!(false))
                && !is_empty_value(&json!("x"))
        );
    }

    #[test]
    fn the_shipped_views_pass_their_checks() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let mut n = 0;
        for bundle in std::fs::read_dir(&root).unwrap().flatten() {
            let dir = bundle.path().join("views");
            let Ok(files) = std::fs::read_dir(&dir) else {
                continue;
            };
            for f in files.flatten() {
                ViewFile::load(&f.path()).unwrap_or_else(|e| panic!("{}: {e}", f.path().display()));
                n += 1;
            }
        }
        assert!(n >= 1, "no views shipped");
    }
}
