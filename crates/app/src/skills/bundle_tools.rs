//! Declared tools: a bundle's `tools/*.yaml` (CORE_AND_PLUGINS_PLAN Phase 3d).
//!
//! A declared tool is data, not code: `{name, description, kind: cypher,
//! input_schema, cypher, limit?}`. It runs through `neo4j_query`'s own read
//! path (read-only, schema guard, LIMIT injection) and answers in the same
//! `{count, rows}` shape. Its arguments become Cypher parameters; every
//! declared property is required or has a default, so a query never meets a
//! missing parameter. Only `cypher` exists: a kind arrives with its first tool.
//!
//! Replaced `DynamicSkill`, `manage_dynamic_tool` and the procedure executor.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tracing::{info, warn};

use crate::services::plugins::{CORE, CodeStatus, Plugins};
use crate::skills::{QuerySkill, Skill};
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

/// The kinds a declared tool may have.
pub const KINDS: &[&str] = &["cypher"];

/// One `tools/*.yaml` file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolFile {
    pub name: String,
    pub description: String,
    pub kind: String,
    pub input_schema: Value,
    pub cypher: String,
    /// Rows returned when the query has no LIMIT of its own (default 100).
    #[serde(default)]
    pub limit: Option<usize>,
}

impl ToolFile {
    /// Parse and check one file; `Err` says what is wrong.
    pub fn load(path: &Path) -> Result<Self, String> {
        let text = std::fs::read_to_string(path).map_err(|e| format!("cannot read: {e}"))?;
        let t: ToolFile = serde_yaml::from_str(&text).map_err(|e| format!("YAML: {e}"))?;
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if t.name != stem {
            return Err(format!(
                "name '{}' must equal the file name '{stem}'",
                t.name
            ));
        }
        t.check()?;
        Ok(t)
    }

    /// The checks a file must pass before it is offered as a tool.
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
        if !KINDS.contains(&self.kind.as_str()) {
            return Err(format!(
                "kind '{}' is not one of {}",
                self.kind,
                KINDS.join(", ")
            ));
        }
        if self.description.trim().is_empty() {
            return Err("a tool needs a description: it is all a model sees".into());
        }
        if self.input_schema.get("type").and_then(Value::as_str) != Some("object") {
            return Err("input_schema must be `type: object`".into());
        }
        if let Some(kw) = crate::skills::query::write_keyword(&self.cypher) {
            return Err(format!(
                "the cypher writes ({kw}); a declared tool only reads"
            ));
        }
        let props = self.properties();
        let required: Vec<&str> = self
            .input_schema
            .get("required")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(Value::as_str).collect())
            .unwrap_or_default();
        for (name, schema) in &props {
            if !required.contains(&name.as_str()) && schema.get("default").is_none() {
                return Err(format!(
                    "parameter '{name}' must be required or have a default (the query always receives it)"
                ));
            }
        }
        for p in cypher_params(&self.cypher) {
            if !props.iter().any(|(n, _)| *n == p) {
                return Err(format!(
                    "the cypher uses ${p}, which input_schema does not declare"
                ));
            }
        }
        Ok(())
    }

    fn properties(&self) -> Vec<(String, Value)> {
        self.input_schema
            .get("properties")
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: self.description.clone(),
            input_schema: self.input_schema.clone(),
        }
    }

    /// The Cypher parameters for one call: each declared property, from the
    /// arguments or its default.
    fn params(&self, args: &Map<String, Value>) -> Map<String, Value> {
        self.properties()
            .into_iter()
            .filter_map(|(name, schema)| {
                args.get(&name)
                    .cloned()
                    .or_else(|| schema.get("default").cloned())
                    .map(|v| (name, v))
            })
            .collect()
    }
}

/// The `$name` parameters a Cypher query uses.
fn cypher_params(cypher: &str) -> Vec<String> {
    let mut out = Vec::new();
    let b = cypher.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'$' {
            let start = i + 1;
            let mut j = start;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            if j > start {
                let p = cypher[start..j].to_string();
                if !out.contains(&p) {
                    out.push(p);
                }
            }
            i = j;
        } else {
            i += 1;
        }
    }
    out
}

pub struct BundleToolSkill {
    query: Arc<QuerySkill>,
    tools: Vec<ToolFile>,
}

impl BundleToolSkill {
    /// The declared tools of every loaded bundle, core first. A file that
    /// fails its checks, or names a tool already in `taken` (or declared by
    /// an earlier file), is left out with the reason. Returns the skill and
    /// what each plugin contributed.
    pub fn load(
        plugins: &Plugins,
        taken: &[String],
        query: Arc<QuerySkill>,
    ) -> (Self, std::collections::HashMap<String, CodeStatus>) {
        let mut claimed: Vec<String> = taken.to_vec();
        let mut status: std::collections::HashMap<String, CodeStatus> = Default::default();
        let mut tools = Vec::new();
        for (plugin, path) in plugins.files("tools", "yaml") {
            let st = status.entry(plugin.clone()).or_default();
            let reason = match ToolFile::load(&path) {
                Ok(t) if claimed.contains(&t.name) => {
                    Err(format!("{}: tool name already taken; left out", t.name))
                }
                Ok(t) => Ok(t),
                Err(e) => Err(format!("{}: {e}", path.display())),
            };
            match reason {
                Ok(t) => {
                    claimed.push(t.name.clone());
                    st.tools.push(t.name.clone());
                    tools.push(t);
                }
                Err(e) => {
                    warn!(%plugin, "declared tool skipped: {e}");
                    st.error = Some(match st.error.take() {
                        Some(prev) => format!("{prev}; {e}"),
                        None => e,
                    });
                }
            }
        }
        if !tools.is_empty() {
            info!(
                tools = %tools.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(", "),
                "Declared tools registered"
            );
        }
        (Self { query, tools }, status)
    }

    /// The plugin each declared tool came from, for tools outside the core
    /// (whose disappearance with their plugin is not a removal).
    pub fn plugin_tools(
        status: &std::collections::HashMap<String, CodeStatus>,
    ) -> impl Iterator<Item = (String, String)> + '_ {
        status
            .iter()
            .filter(|(p, _)| p.as_str() != CORE)
            .flat_map(|(p, c)| c.tools.iter().map(move |t| (t.clone(), p.clone())))
    }
}

#[async_trait]
impl Skill for BundleToolSkill {
    fn name(&self) -> &str {
        "Declared Tools"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        self.tools.iter().map(ToolFile::definition).collect()
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        let t = self.tools.iter().find(|t| t.name == tool_name)?;
        let args = arguments
            .as_ref()
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        let mut call = json!({
            "cypher": t.cypher,
            "params": t.params(&args),
            "readonly": true,
        });
        if let Some(limit) = t.limit {
            call["limit"] = json!(limit);
        }
        self.query.execute("neo4j_query", Some(call)).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool(v: Value) -> ToolFile {
        let mut base = json!({
            "name": "t", "description": "d", "kind": "cypher",
            "input_schema": {"type": "object", "properties": {}},
            "cypher": "MATCH (n:Note) RETURN n.id AS id",
        });
        for (k, x) in v.as_object().unwrap() {
            base[k] = x.clone();
        }
        serde_json::from_value(base).unwrap()
    }

    #[test]
    fn the_shipped_tools_pass_their_checks() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins/core/tools");
        let mut n = 0;
        for e in std::fs::read_dir(&dir).unwrap().flatten() {
            ToolFile::load(&e.path()).unwrap_or_else(|err| panic!("{}: {err}", e.path().display()));
            n += 1;
        }
        assert!(n >= 1);
    }

    #[test]
    fn a_declared_tool_reads_and_declares_what_it_uses() {
        assert!(tool(json!({})).check().is_ok());
        let writes = tool(json!({"cypher": "MATCH (n) SET n.x = 1"}));
        assert!(writes.check().unwrap_err().contains("writes"));
        let undeclared = tool(json!({"cypher": "MATCH (n:Note {id: $id}) RETURN n"}));
        assert!(undeclared.check().unwrap_err().contains("$id"));
        let optional = tool(json!({
            "cypher": "MATCH (n:Note {id: $id}) RETURN n",
            "input_schema": {"type": "object", "properties": {"id": {"type": "string"}}}
        }));
        assert!(
            optional
                .check()
                .unwrap_err()
                .contains("required or have a default")
        );
        let ok = tool(json!({
            "cypher": "MATCH (n:Note) WHERE n.id = $id RETURN n LIMIT $n",
            "input_schema": {"type": "object", "required": ["id"], "properties": {
                "id": {"type": "string"}, "n": {"type": "integer", "default": 5}}}
        }));
        assert!(ok.check().is_ok());
        assert_eq!(
            ok.params(json!({"id": "x"}).as_object().unwrap()),
            *json!({"id": "x", "n": 5}).as_object().unwrap()
        );
        assert!(
            tool(json!({"kind": "http"}))
                .check()
                .unwrap_err()
                .contains("kind")
        );
        assert!(tool(json!({"name": "Bad-Name"})).check().is_err());
    }

    #[test]
    fn params_are_found_once_each() {
        assert_eq!(
            cypher_params("WHERE a = $x AND b = $x_2 OR c = $x RETURN '$'"),
            ["x", "x_2"]
        );
    }
}
