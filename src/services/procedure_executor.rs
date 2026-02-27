//! Procedure Executor — runs a list of procedure steps with template substitution.
//!
//! Steps are JSON objects with at minimum `tool` and `purpose` fields:
//! ```json
//! {
//!   "tool": "search_notes",
//!   "args": { "query": "{{input.topic}}", "limit": 5 },
//!   "purpose": "Find notes",
//!   "output_var": "search_result",
//!   "condition": "{{input.topic}} != \"\""
//! }
//! ```
//!
//! Template variables:
//! - `{{input.field}}` — substituted from caller's input arguments
//! - `{{context.var}}` — output captured from a previous step's `output_var`
//!
//! Conditions are evaluated after substitution; an empty, "false", or "null"
//! result causes the step to be skipped.

use std::collections::HashMap;

use serde_json::{Map, Value};
use tracing::{debug, warn};

use crate::mcp::protocol::Content;
use crate::mcp::tools::ToolHandler;

/// Result of executing a single procedure step.
#[derive(Debug, Clone)]
pub struct StepResult {
    pub step_index: usize,
    pub tool: String,
    pub success: bool,
    pub output_preview: String,
    pub output: Value,
}

/// Execute a list of procedure steps against a `ToolHandler`.
///
/// Returns `(step_results, overall_success)`.
/// When `dry_run` is `true`, steps are validated and substitutions are resolved
/// but tools are NOT called.
pub async fn execute_procedure(
    steps: &[Value],
    input: &Map<String, Value>,
    handler: &ToolHandler,
    dry_run: bool,
) -> (Vec<StepResult>, bool) {
    let mut results = Vec::new();
    let mut context: HashMap<String, Value> = HashMap::new();
    let mut all_success = true;

    for (idx, step) in steps.iter().enumerate() {
        let tool_name = match step.get("tool").and_then(|v| v.as_str()) {
            Some(t) => t.to_string(),
            None => {
                warn!(step = idx, "Step missing 'tool' field, skipping");
                continue;
            }
        };

        // Determine if this is a loop
        let is_loop = step.get("loop").and_then(|v| v.as_bool()).unwrap_or(false);
        let loop_condition = step.get("loop_until").and_then(|v| v.as_str());
        
        let mut loop_count = 0;
        let max_loop = 10; // Safety cap

        loop {
            // Evaluate condition if present
            if let Some(condition) = step.get("condition").and_then(|v| v.as_str()) {
                let resolved = substitute_str(condition, input, &context);
                if !eval_condition(&resolved) {
                    debug!(step = idx, tool = %tool_name, condition = %resolved, "Skipping step (condition false)");
                    results.push(StepResult {
                        step_index: idx,
                        tool: tool_name.clone(),
                        success: true,
                        output_preview: "(skipped — condition false)".to_string(),
                        output: Value::Null,
                    });
                    break; // Exit loop/step
                }
            }

            // Resolve args with template substitution
            let raw_args = step.get("args").cloned().unwrap_or(Value::Object(Map::new()));
            let resolved_args = substitute_value(&raw_args, input, &context);

            debug!(step = idx, tool = %tool_name, "Executing step");

            if dry_run {
                results.push(StepResult {
                    step_index: idx,
                    tool: tool_name.clone(),
                    success: true,
                    output_preview: format!("(dry-run) args: {}", resolved_args),
                    output: resolved_args,
                });
                break;
            }

            // Retry logic
            let retry_policy = step.get("retry_policy").and_then(|v| v.as_object());
            let max_retries = retry_policy.and_then(|p| p.get("max_attempts").and_then(|v| v.as_u64())).unwrap_or(1) as usize;
            let retry_delay = retry_policy.and_then(|p| p.get("delay_ms").and_then(|v| v.as_u64())).unwrap_or(0);

            let mut step_success = false;
            let mut last_call_result = None;

            for attempt in 1..=max_retries {
                if attempt > 1 {
                    debug!(step = idx, tool = %tool_name, attempt = attempt, "Retrying step");
                    if retry_delay > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(retry_delay)).await;
                    }
                }

                let call_result = handler.execute(&tool_name, Some(resolved_args.clone())).await;
                let success = !call_result.is_error.unwrap_or(false);
                
                last_call_result = Some(call_result);
                if success {
                    step_success = true;
                    break;
                }
            }

            let call_result = last_call_result.unwrap();
            if !step_success {
                all_success = false;
            }

            // Extract text content for output_var capture
            let output_text = call_result
                .content
                .first()
                .and_then(|c| {
                    if let Content::Text { text } = c { Some(text.as_str()) } else { None }
                })
                .unwrap_or("")
                .to_string();

            let preview: String = output_text.chars().take(200).collect();
            let parsed_output = serde_json::from_str(&output_text).unwrap_or(Value::String(output_text.clone()));

            // Store output in context if output_var is defined
            if let Some(var) = step.get("output_var").and_then(|v| v.as_str()) {
                context.insert(var.to_string(), parsed_output.clone());
            }

            let res = StepResult {
                step_index: idx,
                tool: tool_name.clone(),
                success: step_success,
                output_preview: preview,
                output: parsed_output,
            };

            // Check loop condition
            if is_loop && loop_count < max_loop {
                if let Some(until) = loop_condition {
                    let resolved = substitute_str(until, input, &context);
                    if eval_condition(&resolved) {
                        results.push(res);
                        break; // Condition met, exit loop
                    }
                }
                loop_count += 1;
                debug!(step = idx, loop_count = loop_count, "Repeating loop step");
                // Note: We don't push all loop results to the final list, just the last or accumulated?
                // For now, let's just keep going and only push the final result when loop ends.
                continue;
            }

            results.push(res);
            break;
        }
    }

    (results, all_success)
}

// ============================================================================
// Template substitution helpers
// ============================================================================

/// Substitute `{{input.path}}` and `{{context.path}}` placeholders in a string.
/// Supports deep paths like `{{context.status.device_status.Arm.IsOperational}}`.
fn substitute_str(template: &str, input: &Map<String, Value>, context: &HashMap<String, Value>) -> String {
    let mut result = template.to_string();
    
    // Find all occurrences of {{...}}
    let re = regex::Regex::new(r"\{\{([^}]+)\}\}").expect("Invalid regex");
    
    let mut substitutions = Vec::new();
    for cap in re.captures_iter(template) {
        let full_match = &cap[0];
        let path = cap[1].trim();
        
        let value = if let Some(stripped) = path.strip_prefix("input.") {
            resolve_path(&Value::Object(input.clone()), stripped)
        } else if let Some(stripped) = path.strip_prefix("context.") {
            // First segment of context path is the key in our HashMap
            let parts: Vec<&str> = stripped.splitn(2, '.').collect();
            if let Some(root_val) = context.get(parts[0]) {
                if parts.len() > 1 {
                    resolve_path(root_val, parts[1])
                } else {
                    Some(root_val.clone())
                }
            } else {
                None
            }
        } else {
            None
        };
        
        if let Some(val) = value {
            substitutions.push((full_match.to_string(), value_to_string(&val)));
        }
    }
    
    // Apply substitutions
    for (placeholder, replacement) in substitutions {
        result = result.replace(&placeholder, &replacement);
    }

    result
}

/// Helper to resolve a dot-notated path in a JSON Value.
fn resolve_path(val: &Value, path: &str) -> Option<Value> {
    let mut current = val;
    for part in path.split('.') {
        match current {
            Value::Object(map) => {
                current = map.get(part)?;
            }
            Value::Array(arr) => {
                let idx: usize = part.parse().ok()?;
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current.clone())
}

/// Recursively apply template substitution to a JSON Value.
pub fn substitute_value(val: &Value, input: &Map<String, Value>, context: &HashMap<String, Value>) -> Value {
    match val {
        Value::String(s) => Value::String(substitute_str(s, input, context)),
        Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), substitute_value(v, input, context)))
                .collect(),
        ),
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|v| substitute_value(v, input, context)).collect())
        }
        other => other.clone(),
    }
}

/// Evaluate a condition string after template substitution.
/// Returns `false` for empty, "false", "null", or `""`.
fn eval_condition(resolved: &str) -> bool {
    let trimmed = resolved.trim();

    // Simple inequality check: "X != Y" or "X == Y"
    if let Some(idx) = trimmed.find("!=") {
        let left = trimmed[..idx].trim().trim_matches('"');
        let right = trimmed[idx + 2..].trim().trim_matches('"');
        return left != right;
    }
    if let Some(idx) = trimmed.find("==") {
        let left = trimmed[..idx].trim().trim_matches('"');
        let right = trimmed[idx + 2..].trim().trim_matches('"');
        return left == right;
    }

    // Falsy check
    !matches!(trimmed, "" | "false" | "null" | "\"\"")
}

fn value_to_string(val: &Value) -> String {
    match val {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        other => other.to_string(),
    }
}
