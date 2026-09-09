//! Deterministic write-boundary validation — the "classic automated pushback".
//!
//! Objects the LLM writes into the graph (schedules, tasks, chains, agents) are
//! validated here BEFORE they are persisted, independent of how confidently the
//! model narrated success. A HARD violation rejects the write outright; a SOFT
//! violation is advisory (the existing `lint_chain_steps` warnings).
//!
//! The high-value check is [`validate_step_arguments`]: every registered tool
//! already ships a JSON Schema (`ToolDefinition.input_schema`), so a step's
//! `arguments` are validated against the *tool's own* schema with zero drift.
//! This catches the class that killed the AI Dev Board Job Search schedule —
//! `query_params: "[object Object]"` (string where the schema says object),
//! `timeout_ms: "15000"` (string where it says integer), `pick` as a
//! comma-string — none of which surfaced at write time before.
//!
//! See `project-docs/VALIDATION_AND_APPLY_PLAN.md`.

use serde_json::Value;
use std::collections::HashMap;

/// How hard a violation bites.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Reject the write. The object is structurally invalid and would fail at runtime.
    Hard,
    /// Advisory. The object is legal but probably not what the author intended.
    Soft,
}

/// One problem found in a candidate object.
#[derive(Debug, Clone)]
pub struct Violation {
    pub severity: Severity,
    /// Dotted/indexed location, e.g. `steps[0].arguments/query_params`.
    pub path: String,
    pub message: String,
}

impl Violation {
    pub fn hard(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { severity: Severity::Hard, path: path.into(), message: message.into() }
    }

    pub fn soft(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self { severity: Severity::Soft, path: path.into(), message: message.into() }
    }

    /// Human-readable one-liner for a rejection message.
    pub fn render(&self) -> String {
        format!("{}: {}", self.path, self.message)
    }
}

/// True if any violation is [`Severity::Hard`].
pub fn has_hard(violations: &[Violation]) -> bool {
    violations.iter().any(|v| v.severity == Severity::Hard)
}

/// Render only the hard violations into a single rejection string.
pub fn render_hard(violations: &[Violation]) -> String {
    violations
        .iter()
        .filter(|v| v.severity == Severity::Hard)
        .map(Violation::render)
        .collect::<Vec<_>>()
        .join("; ")
}

/// Validate a JSON instance against a JSON Schema, one HARD [`Violation`] per error.
///
/// A schema that fails to compile is OUR bug, not the caller's data — it logs
/// and returns no violations, so a broken schema never blocks a legitimate
/// write. We fail *closed* on the caller's data and *open* on our own mistakes.
pub fn validate_against_schema(prefix: &str, schema: &Value, instance: &Value) -> Vec<Violation> {
    let validator = match jsonschema::validator_for(schema) {
        Ok(v) => v,
        Err(e) => {
            tracing::error!(error = %e, prefix, "validation: schema failed to compile — skipping");
            return Vec::new();
        }
    };
    validator
        .iter_errors(instance)
        .map(|err| {
            let ptr = err.instance_path.to_string();
            let path = if ptr.is_empty() {
                prefix.to_string()
            } else {
                format!("{prefix}{ptr}")
            };
            Violation::hard(path, err.to_string())
        })
        .collect()
}

/// Validate one chain step's `arguments` against the registered tool's
/// `input_schema`. Absent `arguments` validate as `{}` so a missing *required*
/// field still fires. A tool with no schema in the map is skipped (not every
/// runtime/dynamic tool is guaranteed present); the caller flags an
/// *unregistered* tool separately as its own hard violation.
pub fn validate_step_arguments(
    step_index: usize,
    tool_name: &str,
    arguments: Option<&Value>,
    tool_schemas: &HashMap<String, Value>,
) -> Vec<Violation> {
    let Some(schema) = tool_schemas.get(tool_name) else {
        return Vec::new();
    };
    let empty = Value::Object(serde_json::Map::new());
    let args = arguments.unwrap_or(&empty);
    let prefix = format!("steps[{step_index}] (`{tool_name}`).arguments");
    validate_against_schema(&prefix, schema, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn http_request_schema() -> Value {
        // The shape the real `http_request` tool registers.
        json!({
            "type": "object",
            "properties": {
                "method": { "type": "string", "enum": ["GET","POST","PUT","PATCH","DELETE","HEAD"] },
                "url": { "type": "string" },
                "query_params": { "type": "object" },
                "timeout_ms": { "type": "integer" },
                "pick": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["method", "url"]
        })
    }

    fn schemas() -> HashMap<String, Value> {
        HashMap::from([("http_request".to_string(), http_request_schema())])
    }

    #[test]
    fn rejects_the_stringified_object_corruption() {
        // Exactly what the HBI editor produced and what killed the job schedule.
        let args = json!({
            "method": "GET",
            "url": "https://aidevboard.com/api/v1/jobs",
            "query_params": "[object Object]",
            "timeout_ms": "15000",
            "pick": "jobs[].url,jobs[].apply_url"
        });
        let v = validate_step_arguments(0, "http_request", Some(&args), &schemas());
        assert!(has_hard(&v), "must reject: {:?}", v);
        let msg = render_hard(&v);
        assert!(msg.contains("query_params"), "should name query_params: {msg}");
        assert!(msg.contains("timeout_ms"), "should name timeout_ms: {msg}");
        assert!(msg.contains("pick"), "should name pick: {msg}");
    }

    #[test]
    fn accepts_the_correctly_typed_args() {
        let args = json!({
            "method": "GET",
            "url": "https://aidevboard.com/api/v1/jobs",
            "query_params": { "workplace": "remote", "posted_within_days": "3" },
            "timeout_ms": 15000,
            "pick": ["jobs[].url", "jobs[].apply_url"]
        });
        let v = validate_step_arguments(0, "http_request", Some(&args), &schemas());
        assert!(!has_hard(&v), "clean args must pass: {:?}", v);
    }

    #[test]
    fn missing_required_field_is_rejected() {
        let args = json!({ "url": "https://example.com" }); // no method
        let v = validate_step_arguments(0, "http_request", Some(&args), &schemas());
        assert!(has_hard(&v), "missing required must reject: {:?}", v);
    }

    #[test]
    fn unknown_tool_is_skipped_here() {
        // A tool with no schema is not this function's job to reject.
        let args = json!({ "anything": 1 });
        let v = validate_step_arguments(0, "not_a_real_tool", Some(&args), &schemas());
        assert!(v.is_empty(), "unknown tool should be skipped, got: {:?}", v);
    }

    #[test]
    fn template_vars_in_string_fields_are_fine() {
        // `{{_prev}}` etc. stay strings; a string field accepts them.
        let args = json!({
            "method": "GET",
            "url": "https://example.com/{{goal_topic}}",
            "query_params": { "q": "{{date}}" }
        });
        let v = validate_step_arguments(0, "http_request", Some(&args), &schemas());
        assert!(!has_hard(&v), "templated strings must pass: {:?}", v);
    }
}
