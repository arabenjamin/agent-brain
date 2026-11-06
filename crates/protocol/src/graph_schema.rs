//! A bundle's Neo4j constraints and indexes, read from its
//! `schemas/graph/schema.cypher` at compile time (CODEBASE_SPLIT_PLAN Phase 1):
//! one reviewable file per bundle instead of Cypher strings inside Rust.
//!
//! The file is Cypher statements, each ending with `;`, and `//` comment lines.
//! Every statement must be idempotent (`IF NOT EXISTS`), because it runs on
//! every start; `scripts/check_schemas.sh` checks that, and that no name is
//! defined twice across bundles.
//!
//! ```ignore
//! for stmt in graph_schema::statements(graph_schema!(core)) { ... }
//! agent_brain_plugin_api::graph::apply_schema(g, graph_schema!(plugin)).await?;
//! ```

/// The calling bundle's `schemas/graph/schema.cypher`, as a `&'static str`:
/// `core` from a core crate, `plugin` from a plugin crate. A missing file
/// fails the build.
#[macro_export]
macro_rules! graph_schema {
    (core) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../plugins/core/schemas/graph/schema.cypher"
        ))
    };
    (plugin) => {
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../schemas/graph/schema.cypher"
        ))
    };
}

/// The statements in a schema file, in order: `//` comment lines dropped, each
/// statement's lines joined by single spaces, split on `;`.
pub fn statements(src: &str) -> Vec<String> {
    src.lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n")
        .split(';')
        .map(|s| s.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|s| !s.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::statements;

    #[test]
    fn statements_drop_comments_and_join_lines() {
        let src = "// Notes\nCREATE CONSTRAINT note_id IF NOT EXISTS\n  FOR (n:Note) REQUIRE n.id IS UNIQUE;\n\n// a vector index\nCREATE VECTOR INDEX v IF NOT EXISTS FOR (n:Note) ON (n.embedding)\n  OPTIONS {indexConfig: {`vector.dimensions`: 1024}};\n";
        assert_eq!(
            statements(src),
            vec![
                "CREATE CONSTRAINT note_id IF NOT EXISTS FOR (n:Note) REQUIRE n.id IS UNIQUE",
                "CREATE VECTOR INDEX v IF NOT EXISTS FOR (n:Note) ON (n.embedding) OPTIONS {indexConfig: {`vector.dimensions`: 1024}}",
            ]
        );
    }

    #[test]
    fn an_empty_or_comment_only_file_has_none() {
        assert!(statements("").is_empty());
        assert!(statements("// nothing yet\n").is_empty());
    }
}
