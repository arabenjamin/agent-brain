//! Schema-aware Cypher guard — the read-boundary analogue of
//! `services/validation.rs`.
//!
//! Given a Cypher string and a live [`GraphSchema`], catch the subclass of
//! errors we can judge deterministically BEFORE the query hits Neo4j. Like the
//! write-boundary validator, this is **structural, not semantic**: it cannot
//! know whether `ORDER BY updated_at` was intended, only whether the labels,
//! relationships and properties the query names actually exist. A linter over a
//! known bad-set, not a prover.
//!
//! Severity mirrors the write validator:
//! - **Hard** (readonly reads only): an unknown label / relationship type. A
//!   read of something that cannot exist is a bug; the message names the valid
//!   set so the model self-corrects. Skipped for writes — `CREATE`ing a new
//!   label is legal.
//! - **Soft** (any mode): a `var.prop` where `var:Label` is bound and the label
//!   does not carry `prop`, and the datetime-vs-string comparison anti-pattern.
//!   Advisory because `db.schema.nodeTypeProperties()` only reports properties
//!   present on ≥1 node, so a brand-new optional property must never hard-block.
//!
//! Parsing is token/regex heuristics (there is no mature Rust Cypher parser and
//! the existing query tooling — `needs_limit_injection`, the date-literal hint —
//! is heuristic too). It reads the *shape*, so it degrades gracefully on Cypher
//! it does not fully understand rather than mis-rejecting.

use std::collections::{HashMap, HashSet};
use std::sync::LazyLock;

use regex::Regex;

use crate::graph_schema::GraphSchema;
use crate::validation::Violation;

// `(var:Label` or `(:Label` — a node pattern's label.
static NODE_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\(\s*([A-Za-z_]\w*)?\s*:\s*`?([A-Za-z_]\w*)`?").unwrap());
// `[var:REL` or `[:REL` — a relationship pattern's type.
static REL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\s*([A-Za-z_]\w*)?\s*:\s*`?([A-Za-z_]\w*)`?").unwrap());
// `var.prop` — a property access.
static PROP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\b([A-Za-z_]\w*)\.([A-Za-z_]\w*)").unwrap());

/// Truncate a long "known set" list so a rejection message stays readable.
fn sample(items: &[&str], max: usize) -> String {
    if items.len() <= max {
        items.join(", ")
    } else {
        format!("{}, … ({} total)", items[..max].join(", "), items.len())
    }
}

/// Validate a Cypher query against the live schema. `readonly` gates the Hard
/// label/rel checks (a write may legitimately introduce a new label).
pub fn check(cypher: &str, schema: &GraphSchema, readonly: bool) -> Vec<Violation> {
    let mut out = Vec::new();

    // ── var → labels (node bindings) and the set of relationship vars ────────
    let mut var_labels: HashMap<String, HashSet<String>> = HashMap::new();
    let mut rel_vars: HashSet<String> = HashSet::new();

    for cap in NODE_RE.captures_iter(cypher) {
        let label = cap[2].to_string();
        if readonly && !schema.has_label(&label) {
            let known = schema.label_names();
            out.push(Violation::hard(
                "cypher",
                format!(
                    "unknown node label `:{label}` — no such label exists in the graph. \
                     Known labels: {}",
                    sample(&known, 30)
                ),
            ));
        }
        if let Some(var) = cap.get(1).map(|m| m.as_str()).filter(|s| !s.is_empty()) {
            var_labels.entry(var.to_string()).or_default().insert(label);
        }
    }

    for cap in REL_RE.captures_iter(cypher) {
        let rel = cap[2].to_string();
        if readonly && !schema.has_rel_type(&rel) {
            let known: Vec<&str> = schema.rel_types.iter().map(String::as_str).collect();
            out.push(Violation::hard(
                "cypher",
                format!(
                    "unknown relationship type `:{rel}` — no such relationship exists. \
                     Known types: {}",
                    sample(&known, 40)
                ),
            ));
        }
        if let Some(var) = cap.get(1).map(|m| m.as_str()).filter(|s| !s.is_empty()) {
            rel_vars.insert(var.to_string());
        }
    }

    // ── property references on unambiguously-bound node vars ─────────────────
    let mut seen_prop: HashSet<(String, String)> = HashSet::new();
    for cap in PROP_RE.captures_iter(cypher) {
        let var = cap[1].to_string();
        let prop = cap[2].to_string();
        // A var used as a relationship is not a node — skip (its props are
        // relationship props we don't model). An ambiguous or unbound var is
        // skipped too (fail open).
        if rel_vars.contains(&var) {
            continue;
        }
        let Some(labels) = var_labels.get(&var) else {
            continue;
        };
        if labels.len() != 1 {
            continue;
        }
        let label = labels.iter().next().unwrap();
        if !schema.has_label(label) {
            continue;
        }
        if schema.label_lacks_prop(label, &prop) && seen_prop.insert((var.clone(), prop.clone())) {
            let known = schema.prop_names(label);
            out.push(Violation::soft(
                "cypher",
                format!(
                    "`{var}.{prop}`: label `{label}` has no property `{prop}` on any current \
                     node. Known properties: {}. (Advisory — a brand-new optional property \
                     would also look absent.)",
                    sample(&known, 30)
                ),
            ));
        }
    }

    // ── datetime-vs-string anti-pattern ─────────────────────────────────────
    if contains_quoted_date_literal(cypher) && !cypher.to_ascii_lowercase().contains("datetime(") {
        out.push(Violation::soft(
            "cypher",
            "compares against a quoted date literal but calls no `datetime(...)`. Every \
             timestamp in this graph is a native datetime; a datetime compared to a string \
             evaluates to null (row silently dropped, no error). Wrap it: \
             `datetime('2026-08-13T00:00:00Z')` or `datetime() - duration({days: 7})`."
                .to_string(),
        ));
    }

    out
}

/// True when the query contains a `YYYY-MM-DD` run inside a quoted literal.
/// (Mirrors the syntactic detector in `skills/query.rs`; kept here so the
/// services layer does not depend on the skills layer.)
pub fn contains_quoted_date_literal(cypher: &str) -> bool {
    let b = cypher.as_bytes();
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph_schema::PropInfo;
    use crate::validation::{Severity, has_hard, render_hard};

    fn schema() -> GraphSchema {
        let mut s = GraphSchema::default();
        s.labels.insert(
            "Task".to_string(),
            vec![
                PropInfo {
                    name: "goal".to_string(),
                    is_temporal: false,
                },
                PropInfo {
                    name: "status".to_string(),
                    is_temporal: false,
                },
                PropInfo {
                    name: "updated_at".to_string(),
                    is_temporal: true,
                },
            ],
        );
        s.labels.insert(
            "ApiContext".to_string(),
            vec![PropInfo {
                name: "name".to_string(),
                is_temporal: false,
            }],
        );
        s.rel_types.insert("EXPOSES".to_string());
        s
    }

    #[test]
    fn clean_query_passes() {
        let v = check(
            "MATCH (t:Task) RETURN t.goal, t.status ORDER BY t.updated_at DESC LIMIT 5",
            &schema(),
            true,
        );
        assert!(v.is_empty(), "clean query flagged: {:?}", v);
    }

    #[test]
    fn unknown_label_is_hard_in_readonly() {
        let v = check("MATCH (t:Taskk) RETURN t", &schema(), true);
        assert!(has_hard(&v), "must reject unknown label: {:?}", v);
        assert!(render_hard(&v).contains("Taskk"));
    }

    #[test]
    fn unknown_label_is_allowed_in_write_mode() {
        // Creating a new label is legal — the guard must not block it.
        let v = check("CREATE (n:BrandNewThing {id:'1'})", &schema(), false);
        assert!(!has_hard(&v), "write of a new label must pass: {:?}", v);
    }

    #[test]
    fn unknown_rel_type_is_hard_in_readonly() {
        let v = check(
            "MATCH (a:Task)-[:BOGUS]->(b:ApiContext) RETURN a",
            &schema(),
            true,
        );
        assert!(has_hard(&v));
        assert!(render_hard(&v).contains("BOGUS"));
    }

    #[test]
    fn known_rel_type_passes() {
        let v = check(
            "MATCH (c:ApiContext)-[:EXPOSES]->(x) RETURN c",
            &schema(),
            true,
        );
        assert!(!has_hard(&v), "known rel flagged: {:?}", v);
    }

    #[test]
    fn unknown_property_is_soft_and_named() {
        // The fabrication case: Task has updated_at, not completed_at.
        let v = check("MATCH (t:Task) RETURN t.completed_at", &schema(), true);
        assert!(
            !has_hard(&v),
            "unknown prop must be advisory, not hard: {:?}",
            v
        );
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].severity, Severity::Soft);
        assert!(v[0].message.contains("completed_at"));
        assert!(
            v[0].message.contains("updated_at"),
            "should suggest the real prop"
        );
    }

    #[test]
    fn known_property_passes() {
        let v = check(
            "MATCH (t:Task) RETURN t.goal, t.updated_at",
            &schema(),
            true,
        );
        assert!(v.is_empty(), "known props flagged: {:?}", v);
    }

    #[test]
    fn relationship_var_props_are_not_checked_against_node_labels() {
        // `r` is a relationship var; r.since must not be checked against a node label.
        let v = check(
            "MATCH (c:ApiContext)-[r:EXPOSES]->(x) RETURN r.since",
            &schema(),
            true,
        );
        assert!(v.is_empty(), "rel-var prop should be skipped: {:?}", v);
    }

    #[test]
    fn quoted_date_literal_without_datetime_is_soft() {
        let v = check(
            "MATCH (t:Task) WHERE t.updated_at >= '2026-08-13T00:00:00Z' RETURN t",
            &schema(),
            true,
        );
        assert!(
            v.iter()
                .any(|x| x.severity == Severity::Soft && x.message.contains("datetime("))
        );
    }

    #[test]
    fn wrapped_datetime_literal_is_fine() {
        let v = check(
            "MATCH (t:Task) WHERE t.updated_at >= datetime('2026-08-13T00:00:00Z') RETURN t",
            &schema(),
            true,
        );
        assert!(
            !v.iter().any(|x| x.message.contains("null")),
            "wrapped datetime flagged: {:?}",
            v
        );
    }

    #[test]
    fn no_schema_labels_still_safe() {
        // Empty schema: has_label is always false, but a caller should never
        // pass an empty schema (introspect returns None). If it does, readonly
        // checks would flag everything — so callers gate on Some(schema).
        let empty = GraphSchema::default();
        let v = check("MATCH (t:Task) RETURN t", &empty, true);
        assert!(has_hard(&v)); // documents why the caller must not pass an empty schema
    }
}
