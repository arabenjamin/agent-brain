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

/// Deserialize an `Option<String>` so that an empty or whitespace-only value is
/// `None` (and the kept value is trimmed).
///
/// Optional FILTER arguments — a `note_type`, an `entity_name`, a search
/// `engine` — are routinely emitted as `""` by LLM-authored tool calls. Read
/// literally, `""` is a constraint that matches nothing, so the query returns an
/// empty result the model reads as a successful "not found" — a false negative
/// this codebase has now hit three times (`entity_name`, `query`, `note_type`,
/// each patched reactively in its own handler). Normalizing at the
/// deserialization boundary makes "absent" and "empty" the same thing, which is
/// what a filter field means by an empty value — and a new filter field is
/// covered by adding the attribute, not by remembering the guard.
pub fn empty_string_as_none<'de, D>(deserializer: D) -> Result<Option<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let opt = Option::<String>::deserialize(deserializer)?;
    Ok(opt.map(|s| s.trim().to_string()).filter(|s| !s.is_empty()))
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

    #[derive(Deserialize)]
    struct OptHolder {
        #[serde(default, deserialize_with = "empty_string_as_none")]
        filter: Option<String>,
    }

    fn parse_opt(json: &str) -> Option<String> {
        serde_json::from_str::<OptHolder>(json).unwrap().filter
    }

    #[test]
    fn empty_and_whitespace_and_absent_are_none() {
        assert_eq!(parse_opt(r#"{"filter": ""}"#), None);
        assert_eq!(parse_opt(r#"{"filter": "   "}"#), None);
        assert_eq!(parse_opt(r#"{"filter": null}"#), None);
        assert_eq!(parse_opt(r#"{}"#), None);
    }

    #[test]
    fn a_real_value_survives_and_is_trimmed() {
        assert_eq!(
            parse_opt(r#"{"filter": "semantic"}"#),
            Some("semantic".into())
        );
        assert_eq!(parse_opt(r#"{"filter": "  news  "}"#), Some("news".into()));
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
