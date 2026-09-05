//! Flexible deserializers for tool arguments authored by an LLM.
//!
//! Chat- and chain-authored tool calls routinely emit every argument as a JSON
//! *string*, booleans included — a scheduled `reason` step stored
//! `"store_inference": "true"` and dead-lettered every run with
//! `invalid type: string "true", expected a boolean`. Strict serde typing turns
//! that predictable model behaviour into a hard, recurring failure. These
//! deserializers accept the native type OR its common string spelling so a
//! stringified bool coerces instead of failing the job.

use serde::{Deserialize, Deserializer};

/// Deserialize a `bool` that may arrive as a native boolean, a string
/// (`"true"`/`"false"`, `"1"`/`"0"`, `"yes"`/`"no"`, case-insensitive), or an
/// integer (`0`/`1`). Anything else is a real error.
pub fn deserialize_flex_bool<'de, D>(deserializer: D) -> Result<bool, D::Error>
where
    D: Deserializer<'de>,
{
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum FlexBool {
        Bool(bool),
        Int(i64),
        Str(String),
    }

    match FlexBool::deserialize(deserializer)? {
        FlexBool::Bool(b) => Ok(b),
        FlexBool::Int(n) => Ok(n != 0),
        FlexBool::Str(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "1" | "yes" | "y" | "on" => Ok(true),
            "false" | "f" | "0" | "no" | "n" | "off" | "" => Ok(false),
            other => Err(serde::de::Error::custom(format!(
                "invalid boolean string {other:?}: expected true/false"
            ))),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;

    #[derive(Deserialize)]
    struct Holder {
        #[serde(default, deserialize_with = "deserialize_flex_bool")]
        flag: bool,
    }

    fn parse(json: &str) -> bool {
        serde_json::from_str::<Holder>(json).unwrap().flag
    }

    #[test]
    fn accepts_native_bool() {
        assert!(parse(r#"{"flag": true}"#));
        assert!(!parse(r#"{"flag": false}"#));
    }

    #[test]
    fn accepts_string_bool() {
        assert!(parse(r#"{"flag": "true"}"#));
        assert!(parse(r#"{"flag": "TRUE"}"#));
        assert!(parse(r#"{"flag": " Yes "}"#));
        assert!(!parse(r#"{"flag": "false"}"#));
        assert!(!parse(r#"{"flag": "no"}"#));
    }

    #[test]
    fn accepts_int_bool() {
        assert!(parse(r#"{"flag": 1}"#));
        assert!(!parse(r#"{"flag": 0}"#));
    }

    #[test]
    fn default_when_absent() {
        assert!(!parse(r#"{}"#));
    }

    #[test]
    fn rejects_garbage_string() {
        assert!(serde_json::from_str::<Holder>(r#"{"flag": "maybe"}"#).is_err());
    }
}
