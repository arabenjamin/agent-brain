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
/// (`workflow.json` → `"workflow"`). Unreadable or unparseable files
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
/// `manage_workflow` write path and its `audit` action so a
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
    "optional",
    "id",
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
    out.extend(step_ref_violations(steps));
    out
}

/// Every `{{steps.<id>}}` must name an EARLIER step's `id`, and ids must be
/// unique. A forward or unknown reference would resolve to a marker on every
/// run, so it is rejected when written, not discovered in a digest.
fn step_ref_violations(steps: &[Value]) -> Vec<Violation> {
    let mut out = Vec::new();
    let mut seen: Vec<&str> = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let tool = step.get("tool_name").and_then(Value::as_str).unwrap_or("");
        let args = step
            .get("arguments")
            .map(Value::to_string)
            .unwrap_or_default();
        for r in step_refs(&args) {
            if !seen.contains(&r.as_str()) {
                let named: Vec<&str> = seen.clone();
                out.push(Violation::hard(
                    format!("steps[{i}] (`{tool}`)"),
                    format!(
                        "`{{{{steps.{r}}}}}` names no earlier step (earlier step ids: {}) \
                         — a step can only use the output of a step before it; give the \
                         step you mean a top-level `\"id\": \"{r}\"`, or use `{{{{_prev}}}}` \
                         for the step right before",
                        if named.is_empty() {
                            "none".to_string()
                        } else {
                            named.join(", ")
                        }
                    ),
                ));
            }
        }
        if let Some(id) = step.get("id").and_then(Value::as_str) {
            if id.is_empty() || id.contains('.') || id.contains('}') {
                out.push(Violation::hard(
                    format!("steps[{i}] (`{tool}`)"),
                    format!("step id `{id}` must be non-empty with no `.` or `}}`"),
                ));
            } else if seen.contains(&id) {
                out.push(Violation::hard(
                    format!("steps[{i}] (`{tool}`)"),
                    format!("step id `{id}` is used twice"),
                ));
            } else {
                seen.push(id);
            }
        }
    }
    out
}

/// Every `{{inputs.<name>}}` a step uses must be a declared input of its
/// workflow; an undeclared one would never be supplied and resolve to nothing.
pub fn validate_input_refs(steps: &[Value], declared: &[String]) -> Vec<Violation> {
    let mut out = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let tool = step.get("tool_name").and_then(Value::as_str).unwrap_or("");
        let text = step.to_string();
        for name in text
            .split("{{inputs.")
            .skip(1)
            .filter_map(|r| r.find("}}").map(|e| r[..e].trim().to_string()))
        {
            if !declared.contains(&name) {
                out.push(Violation::hard(
                    format!("steps[{i}] (`{tool}`)"),
                    format!(
                        "`{{{{inputs.{name}}}}}` is not a declared input (declared: {})",
                        if declared.is_empty() {
                            "none".to_string()
                        } else {
                            declared.join(", ")
                        }
                    ),
                ));
            }
        }
    }
    out
}

/// The step ids a string refers to via `{{steps.<id>}}` / `{{steps.<id>.<path>}}`.
fn step_refs(s: &str) -> Vec<String> {
    s.split("{{steps.")
        .skip(1)
        .filter_map(|rest| {
            let token = &rest[..rest.find("}}")?];
            Some(token.split('.').next().unwrap_or("").trim().to_string())
        })
        .collect()
}

/// Every step's pinned `model` must be in the catalog, and a `provider_hint`
/// written beside it must agree with that model's provider. A hint that
/// contradicts its model is the Zero Trust schedule: `ollama-cloud` on a step
/// naming the host-local `gemma4:latest`, which ran on cloud while it said
/// local. An empty catalog disables the check (fail-open on our own wiring).
pub fn validate_step_models(
    steps: &[Value],
    catalog: &[agent_brain_llm::model_router::CatalogModel],
) -> Vec<Violation> {
    use agent_brain_llm::model_router::{find_model, hint_agrees, provider_hint_for};
    if catalog.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let Some(model) = step
            .get("model")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|m| !m.is_empty())
        else {
            continue;
        };
        let tool = step.get("tool_name").and_then(Value::as_str).unwrap_or("");
        let path = format!("steps[{i}] (`{tool}`).model");
        let Some(row) = find_model(catalog, model) else {
            out.push(Violation::hard(
                path,
                format!(
                    "`{model}` is not in the model catalog (models.yaml) — this step would \
                     fail every run; use a catalog model id or name, or remove `model`"
                ),
            ));
            continue;
        };
        if let Some(hint) = step.get("provider_hint").and_then(Value::as_str)
            && !hint_agrees(hint, &row.provider)
        {
            out.push(Violation::hard(
                path,
                format!(
                    "`provider_hint: {hint}` contradicts `{}` (provider {}) — set \
                     `provider_hint: {}` or remove it (it is derived from the model)",
                    row.model,
                    row.provider,
                    provider_hint_for(&row.provider)
                ),
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn a_step_model_must_exist_and_agree_with_its_hint() {
        use agent_brain_llm::model_router::CatalogModel;
        let catalog = vec![
            CatalogModel {
                name: "gemma4:latest".into(),
                provider: "ollama".into(),
                model: "gemma4:latest".into(),
            },
            CatalogModel {
                name: "gpt-oss:120b-cloud".into(),
                provider: "ollama-cloud".into(),
                model: "gpt-oss:120b-cloud".into(),
            },
        ];
        // The Zero Trust schedule as stored on 2026-10-09.
        let zero_trust = vec![
            json!({"tool_name": "search_web", "provider_hint": "ollama-cloud", "model": "gemma4:latest"}),
            json!({"tool_name": "reason", "provider_hint": "ollama", "model": "gemma4:latest"}),
        ];
        let v = validate_step_models(&zero_trust, &catalog);
        assert_eq!(v.len(), 1, "{v:?}");
        assert!(v[0].path.starts_with("steps[0]"), "{v:?}");
        assert!(
            render_hard(&v).contains("contradicts"),
            "{}",
            render_hard(&v)
        );

        let unknown = vec![json!({"tool_name": "reason", "model": "gemma4"})];
        assert!(
            render_hard(&validate_step_models(&unknown, &catalog))
                .contains("not in the model catalog")
        );

        let ok = vec![
            json!({"tool_name": "reason", "model": "gpt-oss:120b-cloud"}),
            json!({"tool_name": "reason", "model": "GPT-OSS:120B-CLOUD", "provider_hint": "ollama-cloud"}),
            json!({"tool_name": "reason", "provider_hint": "ollama"}),
        ];
        assert!(validate_step_models(&ok, &catalog).is_empty());
        // No catalog: nothing to check against.
        assert!(validate_step_models(&unknown, &[]).is_empty());
    }

    /// Every schema checked into `schemas/` must be valid JSON *and* a schema
    /// the validator can compile — otherwise the envelope layer fails open and
    /// silently validates nothing. This guards against a typo landing in a
    /// committed schema.
    #[test]
    fn all_committed_schemas_compile() {
        let dir = concat!(env!("CARGO_MANIFEST_DIR"), "/../../plugins/core/schemas");
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
            "/../../plugins/core/schemas"
        )))
    }

    #[test]
    fn workflow_envelope_rejects_empty_steps_bad_names_and_bad_periods() {
        let schema = committed_schemas().remove("workflow").unwrap();
        for bad in [
            json!({"name": "x", "steps": [], "managed_by": "somebody"}),
            json!({"name": "Daily News", "steps": [{"tool_name": "a"}]}),
            json!({"name": "x", "every": "daily", "steps": [{"tool_name": "a"}]}),
            json!({"name": "x", "checks": ["adversarial"], "steps": [{"tool_name": "a"}]}),
        ] {
            let v = validate_against_schema("workflow", &schema, &bad);
            assert!(has_hard(&v), "must reject: {bad}");
        }
    }

    #[test]
    fn workflow_envelope_accepts_a_valid_object() {
        let schema = committed_schemas().remove("workflow").unwrap();
        let good = json!({
            "action": "upsert",
            "name": "ai-dev-board-job-search",
            "every": "1d",
            "inputs": [{"name": "topic"}],
            "checks": ["evaluator"],
            "managed_by": "runtime",
            "steps": [ { "id": "fetch", "tool_name": "http_request", "provider_hint": "ollama",
                         "arguments": { "method": "GET", "url": "https://x" } } ]
        });
        let v = validate_against_schema("workflow", &schema, &good);
        assert!(
            !has_hard(&v),
            "valid workflow must pass: {}",
            render_hard(&v)
        );
    }

    #[test]
    fn template_vars_in_string_fields_are_fine() {
        // `{{_prev}}` etc. stay strings; a string field accepts them.
        let args = json!({
            "method": "GET",
            "url": "https://example.com/{{inputs.topic}}",
            "query_params": { "q": "{{date}}" }
        });
        let v = validate_step_arguments(0, "http_request", Some(&args), &schemas());
        assert!(!has_hard(&v), "templated strings must pass: {:?}", v);
    }

    #[test]
    fn step_refs_must_name_an_earlier_step() {
        let steps = vec![
            json!({"tool_name": "a", "id": "news", "arguments": {}}),
            json!({"tool_name": "b", "arguments": {"x": "{{steps.news.items}} {{steps.later}}"}}),
            json!({"tool_name": "c", "id": "later", "arguments": {}}),
            json!({"tool_name": "d", "id": "news", "arguments": {}}),
        ];
        let v = step_ref_violations(&steps);
        assert_eq!(v.len(), 2, "{v:?}");
        assert!(v[0].render().contains("steps.later"));
        assert!(v[0].render().contains("earlier step ids: news"));
        assert!(v[1].render().contains("used twice"));
    }

    #[test]
    fn input_refs_must_be_declared() {
        let steps =
            vec![json!({"tool_name": "a", "arguments": {"q": "{{inputs.topic}} {{inputs.url}}"}})];
        let v = validate_input_refs(&steps, &["topic".to_string()]);
        assert_eq!(v.len(), 1);
        assert!(v[0].render().contains("inputs.url"));
    }
}
