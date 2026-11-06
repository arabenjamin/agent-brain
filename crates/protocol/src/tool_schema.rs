//! A tool's input schema, read from its bundle's `schemas/tools/<tool>.json`
//! at compile time (CODEBASE_SPLIT_PLAN Phase 1): one reviewable file per tool,
//! descriptions included, instead of a `json!` block inside the handler.
//!
//! A value the code owns (a limit, a closed list) appears in the file as the
//! string `"${NAME}"` and is filled from the Rust constant, so the constant
//! stays its one source. `scripts/check_schemas.sh` checks that each file's
//! placeholders are exactly the ones its call fills.
//!
//! ```ignore
//! input_schema: tool_schema!(core "recall"),
//! input_schema: tool_schema!(plugin "acled_events", "MAX_LIMIT" => MAX_LIMIT),
//! ```

use serde::Serialize;
use serde_json::Value;

/// Load a tool schema: `core` reads `plugins/core/schemas/tools/`, `plugin`
/// reads the calling plugin crate's `../schemas/tools/`. A missing file fails
/// the build; a malformed one or an unfilled placeholder panics when the
/// definition is built, which every tool's registration does at startup.
#[macro_export]
macro_rules! tool_schema {
    (core $name:literal $(, $key:literal => $val:expr)* $(,)?) => {
        $crate::tool_schema::load(
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../../plugins/core/schemas/tools/", $name, ".json")),
            $name,
            &[$(($key, $crate::tool_schema::value(&$val))),*],
        )
    };
    (plugin $name:literal $(, $key:literal => $val:expr)* $(,)?) => {
        $crate::tool_schema::load(
            include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../schemas/tools/", $name, ".json")),
            $name,
            &[$(($key, $crate::tool_schema::value(&$val))),*],
        )
    };
}

/// A constant as a JSON value, for a placeholder.
pub fn value<T: Serialize + ?Sized>(v: &T) -> Value {
    serde_json::to_value(v).expect("a schema constant serializes")
}

/// Parse a schema file and fill its `"${NAME}"` placeholders.
pub fn load(src: &str, tool: &str, fills: &[(&str, Value)]) -> Value {
    let mut schema: Value = serde_json::from_str(src)
        .unwrap_or_else(|e| panic!("schemas/tools/{tool}.json is not JSON: {e}"));
    fill(&mut schema, tool, fills);
    schema
}

fn fill(v: &mut Value, tool: &str, fills: &[(&str, Value)]) {
    match v {
        Value::String(s) => {
            if let Some(key) = s.strip_prefix("${").and_then(|k| k.strip_suffix('}')) {
                let Some((_, val)) = fills.iter().find(|(k, _)| *k == key) else {
                    panic!("schemas/tools/{tool}.json: placeholder ${{{key}}} has no value");
                };
                *v = val.clone();
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|x| fill(x, tool, fills)),
        Value::Object(o) => o.values_mut().for_each(|x| fill(x, tool, fills)),
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn placeholders_take_the_constant() {
        const LIMIT: u64 = 50;
        const KINDS: &[&str] = &["a", "b"];
        let src = r#"{"type":"object","properties":{"n":{"type":"integer","maximum":"${LIMIT}"},"k":{"enum":"${KINDS}"},"s":{"description":"$ not a placeholder"}}}"#;
        let got = load(
            src,
            "t",
            &[("LIMIT", value(&LIMIT)), ("KINDS", value(KINDS))],
        );
        assert_eq!(got["properties"]["n"]["maximum"], json!(50));
        assert_eq!(got["properties"]["k"]["enum"], json!(["a", "b"]));
        assert_eq!(
            got["properties"]["s"]["description"],
            json!("$ not a placeholder")
        );
    }

    #[test]
    #[should_panic(expected = "placeholder ${MISSING} has no value")]
    fn an_unfilled_placeholder_is_loud() {
        load(r#"{"maximum":"${MISSING}"}"#, "t", &[]);
    }
}
