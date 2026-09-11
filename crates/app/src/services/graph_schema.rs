//! Live graph-schema introspection.
//!
//! The Neo4j schema is defined by the Rust source and `init_schema()` — there is
//! no `schema_version` node and no hand-authored schema file. So the schema a
//! *tool* needs (which labels exist, which properties each carries, which of
//! those are native `DATETIME`, which relationship types exist) is read straight
//! from the live database at first use, via the `db.schema` metadata procedures.
//! The graph is the source of truth; nothing here can drift against it.
//!
//! Two consumers:
//! - `describe_graph` (a tool) renders [`GraphSchema::render_compact`] so the
//!   model can *see* that e.g. `Task.updated_at` exists rather than inventing its
//!   absence (observed: a chat turn claimed "completion timestamps were not
//!   stored" — they are, as a native datetime, on every Task).
//! - `services/cypher_guard.rs` validates a Cypher read against it before
//!   dispatch.
//!
//! Everything here **fails open**: any introspection error yields `None` and the
//! caller proceeds without the schema (the guard simply does not run). It must
//! never wedge a query.

use std::collections::{BTreeMap, BTreeSet};

use crate::repository::Neo4jClient;

/// One property of a node label.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PropInfo {
    pub name: String,
    /// True when Neo4j reports a temporal type (`DateTime`, `Date`, `Time`,
    /// `LocalDateTime`, `ZonedDateTime`). These must be compared with
    /// `datetime('…')`, never a quoted string (a datetime-vs-string comparison
    /// evaluates to null and silently drops the row).
    pub is_temporal: bool,
}

/// A snapshot of the graph's label/property/relationship shape.
#[derive(Debug, Clone, Default)]
pub struct GraphSchema {
    /// label → its properties (sorted, deduplicated).
    pub labels: BTreeMap<String, Vec<PropInfo>>,
    /// distinct relationship types.
    pub rel_types: BTreeSet<String>,
}

impl GraphSchema {
    pub fn has_label(&self, label: &str) -> bool {
        self.labels.contains_key(label)
    }

    pub fn has_rel_type(&self, rel: &str) -> bool {
        self.rel_types.contains(rel)
    }

    /// True when `label` is known AND does not carry `prop`. A label we don't
    /// know about returns `false` (we can't judge its properties — fail open).
    pub fn label_lacks_prop(&self, label: &str, prop: &str) -> bool {
        match self.labels.get(label) {
            Some(props) => !props.iter().any(|p| p.name == prop),
            None => false,
        }
    }

    /// Known property names for a label, for a "did you mean" hint.
    pub fn prop_names(&self, label: &str) -> Vec<&str> {
        self.labels
            .get(label)
            .map(|ps| ps.iter().map(|p| p.name.as_str()).collect())
            .unwrap_or_default()
    }

    /// Sorted list of known labels, for rejection messages.
    pub fn label_names(&self) -> Vec<&str> {
        self.labels.keys().map(String::as_str).collect()
    }

    /// A compact, model-facing rendering: one line per label with its properties
    /// (temporal ones flagged), then the relationship types. Deterministic order
    /// so it is stable across boots.
    pub fn render_compact(&self) -> String {
        let mut out = String::new();
        out.push_str(
            "GRAPH SCHEMA (live, introspected from Neo4j). ⏱ = native DateTime (UTC): \
             compare with datetime('…'), never a quoted string.\n\nNodes:\n",
        );
        for (label, props) in &self.labels {
            let rendered: Vec<String> = props
                .iter()
                .map(|p| {
                    if p.is_temporal {
                        format!("{}⏱", p.name)
                    } else {
                        p.name.clone()
                    }
                })
                .collect();
            out.push_str(&format!("  {}({})\n", label, rendered.join(", ")));
        }
        out.push_str("\nRelationships: ");
        out.push_str(
            &self
                .rel_types
                .iter()
                .cloned()
                .collect::<Vec<_>>()
                .join(", "),
        );
        out.push('\n');
        out
    }
}

/// True when a Neo4j property-type name is temporal.
fn is_temporal_type(t: &str) -> bool {
    let u = t.to_ascii_uppercase();
    u.contains("DATETIME") || u.contains("DATE") || u.contains("TIME") || u.contains("DURATION")
}

/// Strip Neo4j's `:` prefix and backtick quoting from a label/rel token
/// (`":`Note`"` → `"Note"`). Multi-label node types keep only the first label.
fn clean_token(raw: &str) -> Option<String> {
    let first = raw
        .trim()
        .trim_start_matches(':')
        .split(':')
        .next()
        .unwrap_or("")
        .trim()
        .trim_matches('`')
        .trim();
    if first.is_empty() {
        None
    } else {
        Some(first.to_string())
    }
}

/// Introspect the live graph. Returns `None` (fail open) on any error — Neo4j
/// down, a proc unavailable, an unexpected row shape. The caller runs without a
/// schema in that case.
pub async fn introspect(neo4j: &Neo4jClient) -> Option<GraphSchema> {
    let mut schema = GraphSchema::default();

    // Node labels + properties + property types.
    let node_rows = neo4j
        .execute(neo4rs::query(
            "CALL db.schema.nodeTypeProperties() \
             YIELD nodeLabels, propertyName, propertyTypes \
             RETURN nodeLabels AS labels, propertyName AS name, propertyTypes AS types",
        ))
        .await
        .ok()?;

    for row in node_rows {
        let labels: Vec<String> = row.get("labels").unwrap_or_default();
        let Some(label) = labels.into_iter().next() else {
            continue;
        };
        let entry = schema.labels.entry(label).or_default();
        // A label with no properties still registers (empty vec); propertyName
        // comes back null for such a node.
        let Ok(name) = row.get::<String>("name") else {
            continue;
        };
        if name.is_empty() {
            continue;
        }
        let types: Vec<String> = row.get("types").unwrap_or_default();
        let is_temporal = types.iter().any(|t| is_temporal_type(t));
        if !entry.iter().any(|p| p.name == name) {
            entry.push(PropInfo { name, is_temporal });
        }
    }
    for props in schema.labels.values_mut() {
        props.sort_by(|a, b| a.name.cmp(&b.name));
    }

    // Relationship types. `relTypeProperties` reports every rel type (even ones
    // with no properties, as a null-property row), which is what we want.
    if let Ok(rel_rows) = neo4j
        .execute(neo4rs::query(
            "CALL db.schema.relTypeProperties() YIELD relType RETURN relType AS rel",
        ))
        .await
    {
        for row in rel_rows {
            if let Ok(raw) = row.get::<String>("rel")
                && let Some(rel) = clean_token(&raw)
            {
                schema.rel_types.insert(rel);
            }
        }
    }

    // A completely empty schema is indistinguishable from an introspection
    // failure and is useless to the guard — treat it as "no schema".
    if schema.labels.is_empty() {
        return None;
    }
    Some(schema)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> GraphSchema {
        let mut s = GraphSchema::default();
        s.labels.insert(
            "Task".to_string(),
            vec![
                PropInfo { name: "goal".to_string(), is_temporal: false },
                PropInfo { name: "status".to_string(), is_temporal: false },
                PropInfo { name: "updated_at".to_string(), is_temporal: true },
            ],
        );
        s.labels.insert(
            "Note".to_string(),
            vec![PropInfo { name: "content".to_string(), is_temporal: false }],
        );
        s.rel_types.insert("EXPOSES".to_string());
        s.rel_types.insert("MENTIONS".to_string());
        s
    }

    #[test]
    fn temporal_type_detection() {
        assert!(is_temporal_type("DateTime"));
        assert!(is_temporal_type("ZonedDateTime"));
        assert!(is_temporal_type("Duration"));
        assert!(!is_temporal_type("String"));
        assert!(!is_temporal_type("Long"));
    }

    #[test]
    fn clean_token_strips_colon_and_backticks() {
        assert_eq!(clean_token(":`Note`").as_deref(), Some("Note"));
        assert_eq!(clean_token("`EXPOSES`").as_deref(), Some("EXPOSES"));
        assert_eq!(clean_token(":`A`:`B`").as_deref(), Some("A"));
        assert_eq!(clean_token(":").as_deref(), None);
    }

    #[test]
    fn label_and_prop_lookups() {
        let s = sample();
        assert!(s.has_label("Task"));
        assert!(!s.has_label("Taskk"));
        assert!(s.has_rel_type("EXPOSES"));
        assert!(!s.has_rel_type("EXPOSE"));
        // Task has updated_at, not completed_at — this is the fabrication guard.
        assert!(s.label_lacks_prop("Task", "completed_at"));
        assert!(!s.label_lacks_prop("Task", "updated_at"));
        // Unknown label: we cannot judge, so fail open (not "lacks").
        assert!(!s.label_lacks_prop("Ghost", "anything"));
    }

    #[test]
    fn compact_render_flags_temporal_and_lists_rels() {
        let r = sample().render_compact();
        assert!(r.contains("updated_at⏱"), "temporal flagged: {r}");
        assert!(r.contains("Task("));
        assert!(r.contains("EXPOSES"));
    }
}
