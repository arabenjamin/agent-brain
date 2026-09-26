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
use std::path::Path;

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
        Self {
            severity: Severity::Hard,
            path: path.into(),
            message: message.into(),
        }
    }

    pub fn soft(path: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            severity: Severity::Soft,
            path: path.into(),
            message: message.into(),
        }
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

/// Load every `*.json` JSON Schema from `dir`, keyed by file stem
/// (`scheduled_task.json` → `"scheduled_task"`). Unreadable or unparseable files
/// are logged and skipped — a broken schema file must never wedge startup. The
/// schemas are not compiled here; each is compiled at use in
/// [`validate_against_schema`], which fails open on a bad schema. A missing dir
/// returns an empty map (envelope validation simply disabled). This is the
/// envelope-layer loader — the per-step argument check needs no disk.
pub fn load_schema_dir(dir: &Path) -> HashMap<String, Value> {
    let mut out = HashMap::new();
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(e) => {
            tracing::warn!(
                dir = %dir.display(), error = %e,
                "validation: schemas dir unreadable — envelope validation disabled"
            );
            return out;
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => match serde_json::from_str::<Value>(&text) {
                Ok(v) => {
                    out.insert(stem.to_string(), v);
                }
                Err(e) => tracing::error!(
                    path = %path.display(), error = %e,
                    "validation: schema file is not valid JSON — skipping"
                ),
            },
            Err(e) => tracing::error!(
                path = %path.display(), error = %e,
                "validation: cannot read schema file — skipping"
            ),
        }
    }
    tracing::info!(count = out.len(), "validation: loaded envelope schemas");
    out
}

/// Validate a whole step array (raw JSON `Value` objects, as stored/authored),
/// returning every HARD violation across steps: a missing/blank `tool_name`, an
/// unregistered tool (only when `live_tools` is non-empty), and each step's
/// `arguments` against the tool's own `input_schema`. Shared by the scheduler's
/// `manage_scheduled_task`/`manage_chain` write path and its `audit` action so a
/// stored (e.g. YAML-seeded) task is checked by exactly the same rules as a new one.
/// Fields a step carries beside `arguments` (`services/queue.rs::ChainStep`).
/// Keep in step with that struct; it is Deserialize-only, so it cannot list
/// them itself.
const CHAIN_STEP_FIELDS: &[&str] = &[
    "tool_name",
    "arguments",
    "priority",
    "max_attempts",
    "provider_hint",
    "context_profile",
    "ttl_secs",
    "description",
    "confidence_threshold",
    "is_evaluator",
    "min_score",
    "evaluator_task_id",
    "is_adversarial",
    "n_hypotheses",
    "min_robustness",
    "adversarial_task_id",
    "required_capabilities",
    "data_class",
    "distill_prev",
    "distill_max_chars",
    "distill_focus",
];

/// A tool argument written at step level instead of inside `arguments`.
///
/// Serde drops an unknown step field, so the argument never reaches the tool
/// and nothing errors. Seen twice: `provider_hint` inside `arguments` (the
/// 2026-09-08 GDELT schedule) and the reverse — `context: "{{_prev}}"` beside
/// `arguments` on the Trump Benefit monitor, whose `reason` step therefore
/// never saw its search results (found 2026-09-26). Only keys the tool's own
/// schema declares are flagged, so harmless extra metadata passes.
fn misplaced_arguments(
    i: usize,
    tool: &str,
    step: &Value,
    tool_schemas: &HashMap<String, Value>,
) -> Vec<Violation> {
    let (Some(obj), Some(props)) = (
        step.as_object(),
        tool_schemas
            .get(tool)
            .and_then(|s| s.get("properties"))
            .and_then(|p| p.as_object()),
    ) else {
        return vec![];
    };
    obj.keys()
        .filter(|k| !CHAIN_STEP_FIELDS.contains(&k.as_str()) && props.contains_key(*k))
        .map(|k| {
            Violation::hard(
                format!("steps[{i}] (`{tool}`)"),
                format!(
                    "`{k}` is an argument of `{tool}` but sits at step level, where it is \
                     silently ignored — move it inside `arguments`"
                ),
            )
        })
        .collect()
}

pub fn validate_step_array(
    steps: &[Value],
    live_tools: &[String],
    tool_schemas: &HashMap<String, Value>,
) -> Vec<Violation> {
    let mut out = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let tool = step.get("tool_name").and_then(|v| v.as_str()).unwrap_or("");
        if tool.is_empty() {
            out.push(Violation::hard(
                format!("steps[{i}]"),
                "missing `tool_name` — cannot dispatch this step",
            ));
            continue;
        }
        if !live_tools.is_empty() && !live_tools.iter().any(|t| t == tool) {
            out.push(Violation::hard(
                format!("steps[{i}] (`{tool}`)"),
                "no such tool is registered — this step would fail every run",
            ));
        }
        out.extend(validate_step_arguments(
            i,
            tool,
            step.get("arguments"),
            tool_schemas,
        ));
        out.extend(misplaced_arguments(i, tool, step, tool_schemas));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Every schema checked into `schemas/` must be valid JSON *and* a schema
    /// the validator can compile — otherwise the envelope layer fails open and
    /// silently validates nothing. This guards against a typo landing in a
    /// committed schema.
    #[test]
    fn all_committed_schemas_compile() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../schemas");
        let loaded = load_schema_dir(Path::new(dir));
        assert!(
            !loaded.is_empty(),
            "expected committed schemas under {dir}, found none"
        );
        for (name, schema) in &loaded {
            assert!(
                jsonschema::validator_for(schema).is_ok(),
                "schema `{name}` does not compile"
            );
        }
    }

    /// The Trump Benefit monitor as it was: `context` beside `arguments`.
    #[test]
    fn a_tool_argument_at_step_level_is_rejected() {
        let schemas = HashMap::from([(
            "reason".to_string(),
            json!({"type":"object","properties":{"question":{"type":"string"},"context":{"type":"string"}}}),
        )]);
        let steps = vec![json!({
            "tool_name": "reason", "provider_hint": "ollama", "description": "d",
            "arguments": {"question": "q"}, "context": "{{_prev}}"
        })];
        let v = validate_step_array(&steps, &[], &schemas);
        assert!(has_hard(&v), "{v:?}");
        assert!(render_hard(&v).contains("`context`"), "{}", render_hard(&v));
        // Step fields and unknown metadata are fine.
        let ok = vec![
            json!({"tool_name": "reason", "arguments": {"question": "q", "context": "x"},
                             "description": "d", "note": "free-form"}),
        ];
        assert!(!has_hard(&validate_step_array(&ok, &[], &schemas)));
    }

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
        assert!(
            msg.contains("query_params"),
            "should name query_params: {msg}"
        );
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
    fn step_array_flags_unregistered_tool_and_bad_args() {
        let live = vec!["http_request".to_string()];
        let steps = vec![
            json!({ "tool_name": "http_request", "arguments": { "method": "GET", "url": "https://x" } }),
            json!({ "tool_name": "no_such_tool", "arguments": {} }),
            json!({ "tool_name": "http_request", "arguments": { "method": "GET", "url": "https://x", "timeout_ms": "nope" } }),
            json!({ "arguments": {} }),
        ];
        let v = validate_step_array(&steps, &live, &schemas());
        let msg = render_hard(&v);
        assert!(msg.contains("no_such_tool"), "unregistered tool: {msg}");
        assert!(msg.contains("timeout_ms"), "bad arg type: {msg}");
        assert!(msg.contains("steps[3]"), "missing tool_name: {msg}");
        // The clean step 0 produced nothing.
        assert!(!msg.contains("steps[0]"), "clean step flagged: {msg}");
    }

    fn committed_schemas() -> HashMap<String, Value> {
        load_schema_dir(Path::new(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../schemas"
        )))
    }

    #[test]
    fn scheduled_task_envelope_rejects_empty_steps_and_bad_managed_by() {
        let schema = committed_schemas().remove("scheduled_task").unwrap();
        let bad = json!({
            "name": "x", "interval_seconds": 86400, "steps": [],
            "managed_by": "somebody"
        });
        let v = validate_against_schema("scheduled_task", &schema, &bad);
        let msg = render_hard(&v);
        assert!(
            has_hard(&v),
            "empty steps + bad managed_by must reject: {msg}"
        );
    }

    #[test]
    fn scheduled_task_envelope_accepts_a_valid_object() {
        let schema = committed_schemas().remove("scheduled_task").unwrap();
        let good = json!({
            "action": "upsert",
            "name": "AI Dev Board Job Search",
            "interval_seconds": 86400,
            "managed_by": "runtime",
            "steps": [ { "tool_name": "http_request", "provider_hint": "ollama",
                         "arguments": { "method": "GET", "url": "https://x" } } ]
        });
        let v = validate_against_schema("scheduled_task", &schema, &good);
        assert!(
            !has_hard(&v),
            "valid schedule must pass: {}",
            render_hard(&v)
        );
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
