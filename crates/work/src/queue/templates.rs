//! Step argument templates: `{{_prev}}`, `{{_prev.path}}`, `{{steps.<id>}}` and how a tool's result becomes the text they carry.

use agent_brain_protocol::{Content, ToolCallResult};
use serde_json::Value;
use std::collections::HashMap;

/// Extract the plain-text content from a ToolCallResult for use as {{_prev}}.
/// Tries `content[0].text` first (standard ToolCallResult shape). When the text is
/// a JSON object with an `"answer"` field (the standard `reason` tool output shape),
/// returns just the answer string so downstream `store_note` steps receive clean
/// markdown instead of a JSON wrapper. Falls back to the full serialised result so
/// `{{_prev}}` is never left unreplaced when a tool returns structured data without
/// a human-readable answer field (e.g. `duckdb_query`, `search_web`).
/// Unwrap a `{"count": N, "rows": [{"col": "…"}, …]}` query result into its
/// concatenated column values, when every row has exactly one column.
///
/// This is the "banking idiom" payoff: chains stash intermediate output in a
/// `WorkingMemory` session and reassemble it with
/// `neo4j_query … RETURN w.content AS content ORDER BY w.turn_index`, because
/// `{{_prev}}` carries only the previous step's output. Without unwrapping, the
/// *envelope* is what flows onward — `workflows/video-learning.yaml` stored notes
/// that began `{"count":2,"rows":[{"content":"## VIDEO SUMMARY\n{…escaped…}"`,
/// so the brain's durable semantic knowledge was JSON scaffolding with the real
/// content escaped inside it, then chunked and embedded that way.
///
/// Multi-column results are left alone: there the shape is the information
/// (a table of tasks, models, usage rows), and flattening it would destroy the
/// association between columns.
pub fn unwrap_single_column_rows(parsed: &serde_json::Value) -> Option<String> {
    let rows = parsed.get("rows")?.as_array()?;
    if rows.is_empty() {
        return None;
    }
    let mut out: Vec<String> = Vec::with_capacity(rows.len());
    for row in rows {
        let obj = row.as_object()?;
        if obj.len() != 1 {
            return None;
        }
        let value = obj.values().next()?;
        match value {
            // Strings pass through as-is — this is the content case.
            serde_json::Value::String(s) => out.push(s.clone()),
            // A single non-string column (counts, ids) is still more useful
            // unwrapped than wrapped, but must not lose its representation.
            serde_json::Value::Null => out.push(String::new()),
            other => out.push(other.to_string()),
        }
    }
    Some(out.join("\n\n"))
}

/// The structured envelope worth carrying alongside `extract_result_text`'s output.
///
/// `extract_result_text` is deliberately lossy: it unwraps `{"id":…,"answer":…}`
/// down to the answer so `{{_prev}}` yields clean markdown rather than JSON
/// scaffolding. That is right for the common case and wrong for the one where a
/// later step needs a sibling field — `store_note` returns the note's id next to
/// its content, and discarding it is why chain-extracted claims had no
/// `ASSERTED_IN` edge back to their source.
///
/// Returns `""` (store nothing) when there is nothing to recover: a non-JSON
/// result, or one where extraction changed nothing. That keeps the duplicate
/// payload off every job in the queue and confines it to the envelope case.
pub(super) fn structured_prev_to_preserve(result: &ToolCallResult, extracted: &str) -> String {
    let text = result
        .content
        .first()
        .and_then(|c| {
            if let Content::Text { text } = c {
                Some(text.as_str())
            } else {
                None
            }
        })
        .unwrap_or("");

    if text.is_empty() || text == extracted {
        return String::new();
    }
    // Only JSON objects are worth keeping — paths cannot index anything else.
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(_)) => text.to_string(),
        _ => String::new(),
    }
}

pub(super) fn extract_result_text(result: &ToolCallResult) -> String {
    let text = result
        .content
        .first()
        .and_then(|c| {
            if let Content::Text { text } = c {
                Some(text.as_str())
            } else {
                None
            }
        })
        .unwrap_or("");

    if !text.is_empty() {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
            // When the text is a JSON object with an "answer" key (reason tool output),
            // return just the answer so {{_prev}} in store_note steps gets clean markdown.
            if let Some(answer) = parsed.get("answer").and_then(|a| a.as_str())
                && !answer.is_empty()
            {
                return answer.to_string();
            }
            // Single-column query results unwrap to their concatenated values.
            if let Some(unwrapped) = unwrap_single_column_rows(&parsed) {
                return unwrapped;
            }
        }
        text.to_string()
    } else {
        // Fallback: serialise the whole result so downstream steps always receive data.
        serde_json::to_string(result).unwrap_or_default()
    }
}

/// Recursively replace `{{key}}` placeholders in every string value of a JSON Value tree.
///
/// Operating at the Value level (rather than on serialized JSON text) means replacement
/// values containing quotes, backslashes, or newlines are inserted safely and can never
/// corrupt the surrounding JSON structure.  This is the substitution primitive shared by
/// `substitute_prev` (chain `{{_prev}}` results) and a workflow run's template
/// expansion (`{{goal}}`, `{{task_id}}`, `{{date}}`, `{{inputs.<name>}}`).
pub fn substitute_template_vars(
    val: &serde_json::Value,
    vars: &[(&str, &str)],
) -> serde_json::Value {
    match val {
        serde_json::Value::String(s) => {
            let mut out = s.clone();
            for (key, replacement) in vars {
                let placeholder = ["{{", key, "}}"].concat();
                out = out.replace(&placeholder, replacement);
            }
            serde_json::Value::String(out)
        }
        serde_json::Value::Object(obj) => serde_json::Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), substitute_template_vars(v, vars)))
                .collect(),
        ),
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| substitute_template_vars(v, vars))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Recursively replace `{{_prev}}` and its alias `{{result}}` in all string values of a JSON
/// Value tree.  Operates at the Value level so there is no risk of JSON injection.
pub(super) fn substitute_prev(val: &serde_json::Value, prev_text: &str) -> serde_json::Value {
    substitute_template_vars(val, &[("_prev", prev_text), ("result", prev_text)])
}

/// Resolve a dotted path against a JSON value: `id`, `answer`, `rows.0.content`.
///
/// Numeric segments index arrays; everything else is an object key.  Returns
/// `None` the moment a segment does not resolve, so a wrong path yields nothing
/// rather than a partial match.
fn lookup_json_path<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = root;
    for seg in path.split('.') {
        if seg.is_empty() {
            return None;
        }
        cur = match cur {
            serde_json::Value::Object(map) => map.get(seg)?,
            serde_json::Value::Array(arr) => arr.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Render a resolved value for insertion into a string argument.
///
/// Strings are inserted raw — a `{{_prev.answer}}` holding a note body must not
/// arrive wrapped in JSON quotes with its newlines escaped.
fn render_prev_path_value(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Replace `{{_prev.<path>}}` / `{{result.<path>}}` occurrences in one string.
///
/// Placeholders that are not path forms (`{{_prev}}`, `{{goal}}`, …) are copied
/// through untouched for the later substitution passes to handle.
fn substitute_prev_paths_in_str(s: &str, root: Option<&serde_json::Value>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            // Unclosed placeholder — emit the remainder verbatim.
            out.push_str(&rest[start..]);
            return out;
        };

        let token = after[..end].trim();
        match token
            .strip_prefix("_prev.")
            .or_else(|| token.strip_prefix("result."))
        {
            Some(path) => {
                // An unresolvable path becomes the empty string, never the
                // literal placeholder: a tool receiving "{{_prev.id}}" as an id
                // would treat it as a real one, while an empty value is simply
                // absent and the optional link is skipped.
                let resolved = root
                    .and_then(|r| lookup_json_path(r, path))
                    .map(render_prev_path_value)
                    .unwrap_or_default();
                out.push_str(&resolved);
            }
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }

    out.push_str(rest);
    out
}

/// A named step's output, as `{{steps.<id>}}` sees it.
pub(super) enum StepOutput {
    /// `text` is what `{{_prev}}` would paste; `raw` the JSON envelope paths
    /// resolve against (empty when the text already is the whole result).
    Done { text: String, raw: String },
    /// The step did not complete; the reason is shown in its place.
    Missing(String),
}

/// Replace `{{steps.<id>}}` and `{{steps.<id>.<path>}}` in every string.
pub(super) fn substitute_step_refs(val: &Value, outputs: &HashMap<String, StepOutput>) -> Value {
    match val {
        Value::String(s) => Value::String(substitute_step_refs_in_str(s, outputs)),
        Value::Object(obj) => Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), substitute_step_refs(v, outputs)))
                .collect(),
        ),
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|v| substitute_step_refs(v, outputs))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn substitute_step_refs_in_str(s: &str, outputs: &HashMap<String, StepOutput>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(start) = rest.find("{{steps.") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 8..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let token = after[..end].trim();
        let (id, path) = match token.split_once('.') {
            Some((id, path)) => (id, Some(path)),
            None => (token, None),
        };
        let resolved = match (outputs.get(id), path) {
            (Some(StepOutput::Done { text, .. }), None) => text.clone(),
            (Some(StepOutput::Done { text, raw }), Some(path)) => {
                let src = if raw.is_empty() { text } else { raw };
                serde_json::from_str::<Value>(src)
                    .ok()
                    .and_then(|root| lookup_json_path(&root, path).map(render_prev_path_value))
                    .unwrap_or_default()
            }
            (Some(StepOutput::Missing(why)), None) => {
                format!("[step '{id}' produced no output ({why}); say this section is unavailable]")
            }
            // A path into a missing output is absent, like an unresolved `{{_prev.x}}`.
            (Some(StepOutput::Missing(_)), Some(_)) => String::new(),
            (None, _) => format!("[step '{id}' is not an earlier step of this chain]"),
        };
        out.push_str(&resolved);
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Recursively resolve `{{_prev.<path>}}` / `{{result.<path>}}` against the
/// previous step's output parsed as JSON.
///
/// This exists because `{{_prev}}` can only paste a step's *entire* output, and
/// most tool results are JSON envelopes. `store_note` returns
/// `{"id":…,"answer":…}`, so a downstream step could get the note's text or its
/// id but never pick one out — which is why chain-extracted claims carried
/// provenance labels but no `ASSERTED_IN` edge to the note they came from.
///
/// Resolution runs against the RAW previous output, never a distilled one:
/// distillation rewrites prose and would destroy the JSON structure paths need.
/// When the output is not JSON, `root` is `None` and every path resolves empty.
pub(super) fn substitute_prev_paths(val: &serde_json::Value, prev_text: &str) -> serde_json::Value {
    let root = serde_json::from_str::<serde_json::Value>(prev_text).ok();
    substitute_prev_paths_inner(val, root.as_ref())
}

fn substitute_prev_paths_inner(
    val: &serde_json::Value,
    root: Option<&serde_json::Value>,
) -> serde_json::Value {
    match val {
        serde_json::Value::String(s) => {
            serde_json::Value::String(substitute_prev_paths_in_str(s, root))
        }
        serde_json::Value::Object(obj) => serde_json::Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), substitute_prev_paths_inner(v, root)))
                .collect(),
        ),
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| substitute_prev_paths_inner(v, root))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::ChainStep;
    use super::*;
    use serde_json::json;

    #[test]
    fn single_column_rows_unwrap_to_their_content() {
        // The banking idiom: neo4j_query reassembles WorkingMemory into one
        // column. Storing the envelope is what polluted video_learning notes.
        let v = serde_json::json!({
            "count": 2,
            "rows": [
                {"content": "## VIDEO SUMMARY\nsomething"},
                {"content": "## NEW vs KNOWN\nsomething else"}
            ]
        });
        assert_eq!(
            unwrap_single_column_rows(&v).unwrap(),
            "## VIDEO SUMMARY\nsomething\n\n## NEW vs KNOWN\nsomething else"
        );
    }

    #[test]
    fn multi_column_rows_are_left_as_json() {
        // Here the shape IS the information — flattening would sever the
        // association between a task's id, goal and status.
        let v = serde_json::json!({
            "count": 1,
            "rows": [{"id": "abc", "goal": "do a thing", "status": "created"}]
        });
        assert!(unwrap_single_column_rows(&v).is_none());
    }

    #[test]
    fn empty_or_shapeless_results_are_left_alone() {
        assert!(unwrap_single_column_rows(&serde_json::json!({"count":0,"rows":[]})).is_none());
        assert!(unwrap_single_column_rows(&serde_json::json!({"answer":"hi"})).is_none());
        assert!(unwrap_single_column_rows(&serde_json::json!([1, 2, 3])).is_none());
    }

    #[test]
    fn extract_result_text_unwraps_a_reassembly_envelope() {
        let payload = serde_json::json!({
            "count": 1,
            "rows": [{"content": "clean prose"}]
        })
        .to_string();
        let result = ToolCallResult::success_text(payload);
        assert_eq!(extract_result_text(&result), "clean prose");
    }

    #[test]
    fn step_refs_resolve_whole_output_paths_and_missing_steps() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "news".to_string(),
            StepOutput::Done {
                text: "three headlines".into(),
                raw: json!({"items": [{"title": "A"}], "count": 1}).to_string(),
            },
        );
        outputs.insert(
            "notes".to_string(),
            StepOutput::Missing("failed: timeout".into()),
        );
        let args = json!({
            "q": "{{steps.news}} | {{steps.news.count}} | {{steps.news.items.0.title}}",
            "c": ["{{steps.notes}}", "[{{steps.notes.x}}]", "{{steps.ghost}}", "{{_prev}}"]
        });
        let out = substitute_step_refs(&args, &outputs);
        assert_eq!(out["q"], "three headlines | 1 | A");
        assert_eq!(
            out["c"][0],
            "[step 'notes' produced no output (failed: timeout); say this section is unavailable]"
        );
        assert_eq!(out["c"][1], "[]");
        assert!(
            out["c"][2]
                .as_str()
                .unwrap()
                .contains("not an earlier step")
        );
        assert_eq!(
            out["c"][3], "{{_prev}}",
            "other placeholders are left for later passes"
        );
    }

    #[test]
    fn step_ref_text_containing_a_placeholder_is_not_resubstituted() {
        let mut outputs = HashMap::new();
        outputs.insert(
            "a".to_string(),
            StepOutput::Done {
                text: "{{steps.a}}".into(),
                raw: String::new(),
            },
        );
        let out = substitute_step_refs_in_str("x {{steps.a}} y", &outputs);
        assert_eq!(out, "x {{steps.a}} y");
    }

    #[test]
    fn extract_result_text_still_prefers_the_answer_field() {
        let payload = serde_json::json!({"answer": "the answer", "rows": [{"c": "x"}]}).to_string();
        let result = ToolCallResult::success_text(payload);
        assert_eq!(extract_result_text(&result), "the answer");
    }

    #[test]
    fn substitute_template_vars_preserves_quotes_and_backslashes() {
        // A goal containing a quote, backslash, and newline must not corrupt the JSON:
        // substitution happens on the parsed Value, so these are inserted verbatim.
        let raw = json!([{
            "tool_name": "reason",
            "arguments": { "question": "{{goal}}" }
        }]);
        let goal = "say \"hello\" \\ world\nnext line";
        let out = substitute_template_vars(&raw, &[("goal", goal)]);
        let steps: Vec<ChainStep> = serde_json::from_value(out).expect("must deserialize");
        assert_eq!(steps.len(), 1);
        let q = steps[0]
            .arguments
            .as_ref()
            .unwrap()
            .get("question")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(q, goal);
    }

    #[test]
    fn substitute_template_vars_replaces_multiple_keys_and_nested() {
        let raw = json!({
            "a": "{{task_id}}",
            "b": ["{{goal}}", { "c": "{{date}}" }],
            "d": 42
        });
        let out = substitute_template_vars(
            &raw,
            &[("task_id", "t-1"), ("goal", "do X"), ("date", "2026-08-03")],
        );
        assert_eq!(out["a"], json!("t-1"));
        assert_eq!(out["b"][0], json!("do X"));
        assert_eq!(out["b"][1]["c"], json!("2026-08-03"));
        assert_eq!(out["d"], json!(42)); // non-strings untouched
    }

    #[test]
    fn substitute_prev_handles_both_aliases() {
        let raw = json!({ "x": "{{_prev}}", "y": "{{result}}" });
        let out = substitute_prev(&raw, "PREV");
        assert_eq!(out["x"], json!("PREV"));
        assert_eq!(out["y"], json!("PREV"));
    }

    /// The ASSERTED_IN case: `store_note` returns an envelope, and the claim
    /// step needs the body from one field and the note id from another.
    /// The envelope is preserved exactly when extraction was lossy — this is
    /// the half of the ASSERTED_IN fix that runs at completion time.
    #[test]
    fn structured_prev_kept_only_when_extraction_lost_something() {
        // store_note: extraction unwraps to `answer`, so the id must be kept.
        let envelope = json!({ "id": "note-1", "answer": "body text" }).to_string();
        let result = ToolCallResult::success_text(envelope.clone());
        let extracted = extract_result_text(&result);
        assert_eq!(extracted, "body text");
        assert_eq!(structured_prev_to_preserve(&result, &extracted), envelope);

        // A plain-text result loses nothing — storing a copy would be waste.
        let result = ToolCallResult::success_text("just prose");
        let extracted = extract_result_text(&result);
        assert_eq!(structured_prev_to_preserve(&result, &extracted), "");

        // JSON with no `answer` passes through extraction unchanged: identical
        // text, nothing to recover.
        let result = ToolCallResult::success_text(r#"{"stored":3}"#);
        let extracted = extract_result_text(&result);
        assert_eq!(structured_prev_to_preserve(&result, &extracted), "");

        // A top-level array cannot be path-indexed by field name; not kept.
        let result = ToolCallResult::success_text(r#"[{"answer":"x"}]"#);
        let extracted = extract_result_text(&result);
        assert_eq!(structured_prev_to_preserve(&result, &extracted), "");
    }

    #[test]
    fn prev_paths_pick_fields_out_of_a_store_note_envelope() {
        // r##"…"## because the payload itself contains the sequence `"#`.
        let prev = r##"{"success":true,"id":"note-123","links_created":2,
                       "answer":"# Report\n\nLine two.","message":"Note stored successfully"}"##;
        let raw = json!({
            "text": "{{_prev.answer}}",
            "source_note_id": "{{_prev.id}}",
            "source_context": "slm_benchmark_watch"
        });
        let out = substitute_prev_paths(&raw, prev);
        // Strings are inserted raw — not re-quoted, newlines not escaped.
        assert_eq!(out["text"], json!("# Report\n\nLine two."));
        assert_eq!(out["source_note_id"], json!("note-123"));
        assert_eq!(out["source_context"], json!("slm_benchmark_watch"));
    }

    #[test]
    fn prev_paths_leave_the_whole_output_placeholder_alone() {
        // "{{_prev}}" must survive the path pass so substitute_prev can handle
        // it — the two forms run in sequence over the same arguments.
        let raw = json!({ "a": "{{_prev}}", "b": "{{_prev.id}}", "c": "{{goal}}" });
        let out = substitute_prev_paths(&raw, r#"{"id":"x1"}"#);
        assert_eq!(out["a"], json!("{{_prev}}"));
        assert_eq!(out["b"], json!("x1"));
        assert_eq!(out["c"], json!("{{goal}}"));
    }

    #[test]
    fn prev_paths_resolve_empty_rather_than_leaking_the_placeholder() {
        // A tool handed the literal "{{_prev.id}}" would treat it as a real id.
        // Non-JSON output, a missing key, and a wrong type must all yield "".
        for prev in [
            "not json at all",
            r#"{"other":"field"}"#,
            r#"{"id":{"nested":"object"}}"#,
        ] {
            let out = substitute_prev_paths(&json!({ "k": "{{_prev.id.deep}}" }), prev);
            assert_eq!(out["k"], json!(""), "prev={prev}");
        }
    }

    #[test]
    fn prev_paths_index_arrays_and_render_non_strings() {
        let prev = r#"{"rows":[{"content":"first"},{"content":"second"}],"count":7}"#;
        let out = substitute_prev_paths(
            &json!({ "a": "{{_prev.rows.1.content}}", "b": "n={{_prev.count}}" }),
            prev,
        );
        assert_eq!(out["a"], json!("second"));
        assert_eq!(out["b"], json!("n=7"));
    }

    #[test]
    fn prev_paths_alias_and_malformed_placeholders() {
        let prev = r#"{"id":"abc"}"#;
        // `result.` is the documented alias for `_prev.`.
        let out = substitute_prev_paths(&json!({ "a": "{{result.id}}" }), prev);
        assert_eq!(out["a"], json!("abc"));
        // An unclosed placeholder is emitted verbatim, not silently swallowed.
        let out = substitute_prev_paths(&json!({ "a": "x {{_prev.id" }), prev);
        assert_eq!(out["a"], json!("x {{_prev.id"));
    }

    #[test]
    fn prev_path_values_cannot_corrupt_surrounding_json() {
        // Substitution is value-level, so a field holding quotes/backslashes/
        // newlines lands intact instead of breaking the argument structure.
        let prev = json!({ "answer": "say \"hi\" \\ then\nnewline" }).to_string();
        let out = substitute_prev_paths(&json!({ "text": "{{_prev.answer}}" }), &prev);
        assert_eq!(out["text"], json!("say \"hi\" \\ then\nnewline"));
    }
}
