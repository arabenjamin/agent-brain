//! Checks on the schema files every bundle keeps in `schemas/`
//! (CODEBASE_SPLIT_PLAN Phase 1). `scripts/check_schemas.sh` checks that each
//! file is referenced once and its placeholders match; this checks that each
//! is a schema at all, against the JSON Schema meta-schema.

#[cfg(test)]
mod tests {
    use serde_json::Value;
    use std::path::{Path, PathBuf};

    fn tool_schema_files() -> Vec<PathBuf> {
        let plugins = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../plugins");
        let mut files = vec![];
        for bundle in std::fs::read_dir(&plugins).unwrap().flatten() {
            let dir = bundle.path().join("schemas/tools");
            if let Ok(entries) = std::fs::read_dir(&dir) {
                files.extend(
                    entries
                        .flatten()
                        .map(|e| e.path())
                        .filter(|p| p.extension().is_some_and(|x| x == "json")),
                );
            }
        }
        files.sort();
        files
    }

    /// A placeholder stands for a constant the code fills; give it a value of
    /// the right shape for its keyword so the meta-schema can judge the rest.
    fn stand_in(v: &mut Value, key: &str) {
        match v {
            Value::String(s) if s.starts_with("${") && s.ends_with('}') => {
                *v = match key {
                    "enum" | "examples" => serde_json::json!(["x"]),
                    "default" | "const" => Value::String("x".into()),
                    _ => serde_json::json!(1),
                };
            }
            Value::Array(a) => a.iter_mut().for_each(|x| stand_in(x, key)),
            Value::Object(o) => o.iter_mut().for_each(|(k, x)| stand_in(x, k)),
            _ => {}
        }
    }

    #[test]
    fn every_tool_schema_file_is_a_valid_object_schema() {
        let files = tool_schema_files();
        assert!(
            files.len() >= 60,
            "expected the tool schema files, found {}",
            files.len()
        );
        let mut bad = vec![];
        for f in &files {
            let text = std::fs::read_to_string(f).unwrap();
            let mut schema: Value = match serde_json::from_str(&text) {
                Ok(v) => v,
                Err(e) => {
                    bad.push(format!("{}: not JSON: {e}", f.display()));
                    continue;
                }
            };
            stand_in(&mut schema, "");
            if schema["type"] != "object" {
                bad.push(format!(
                    "{}: a tool's input schema must be type object",
                    f.display()
                ));
            }
            if let Err(e) = jsonschema::validator_for(&schema) {
                bad.push(format!("{}: not a valid JSON Schema: {e}", f.display()));
            }
        }
        assert!(bad.is_empty(), "{}", bad.join("\n"));
    }
}
