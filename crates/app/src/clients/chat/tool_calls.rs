//! Parsing tool calls out of model text, and handling malformed ones.

use super::*;

/// Parse XML-style `<invoke name="...">` tool calls leaked by models like MiniMax.
///
/// Handles patterns like:
///   `<invoke name="search_web"><query>foo</query></invoke>`
///   `<invoke name="search_web">{"query":"foo"}</invoke>`
///   `<invoke name="list_tasks"></invoke></minimax:tool_call>`
pub(super) fn parse_xml_tool_calls(text: &str) -> Vec<(String, String, Value)> {
    let mut calls = Vec::new();
    let mut search = text;
    let mut id_counter = 0u32;

    while let Some(open_start) = search.find("<invoke") {
        let rest = &search[open_start..];
        // Extract name attribute
        let name = if let Some(name_start) = rest.find("name=\"") {
            let after = &rest[name_start + 6..];
            if let Some(name_end) = after.find('"') {
                after[..name_end].to_string()
            } else {
                break;
            }
        } else {
            break;
        };

        // Find closing tag
        let close_tag = "</invoke>";
        let body_start = rest.find('>').map(|i| i + 1).unwrap_or(rest.len());
        let body_end = rest.find(close_tag).unwrap_or(rest.len());

        let body = &rest[body_start..body_end];
        let args = if body.trim().is_empty() {
            json!({})
        } else if let Ok(v) = serde_json::from_str::<Value>(body.trim()) {
            v
        } else {
            // Try parsing child XML tags as key=value pairs: <query>foo</query>
            let mut map = serde_json::Map::new();
            let mut rem = body.trim();
            while let Some(tag_open) = rem.find('<') {
                let tag_rest = &rem[tag_open + 1..];
                if tag_rest.starts_with('/') {
                    break;
                }
                if let Some(tag_end) = tag_rest.find('>') {
                    let key = tag_rest[..tag_end].trim().to_string();
                    let after_tag = &tag_rest[tag_end + 1..];
                    let close = format!("</{}>", key);
                    if let Some(val_end) = after_tag.find(&close) {
                        map.insert(key, Value::String(after_tag[..val_end].trim().to_string()));
                        rem = &after_tag[val_end + close.len()..];
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            if map.is_empty() {
                json!({})
            } else {
                Value::Object(map)
            }
        };

        let id = format!("xml-{}", id_counter);
        id_counter += 1;
        calls.push((id, name, args));

        // Advance past </invoke>
        if let Some(end) = rest.find(close_tag) {
            search = &search[open_start + end + close_tag.len()..];
        } else {
            break;
        }
    }

    calls
}

/// Remove XML `<invoke>...</invoke>` and `</minimax:tool_call>` blocks from text.
pub(super) fn strip_xml_tool_calls(text: &str) -> String {
    let mut result = text.to_string();
    while let Some(start) = result.find("<invoke") {
        if let Some(end) = result.find("</invoke>") {
            result = format!("{}{}", &result[..start], &result[end + 9..]);
        } else {
            result = result[..start].to_string();
            break;
        }
    }
    // Strip any leftover minimax wrapper tags
    result = result.replace("</minimax:tool_call>", "");
    result = result.replace("<minimax:tool_call>", "");
    result.trim().to_string()
}

/// What a text-loop reply contained, as far as tool calls go.
#[derive(Debug)]
pub(super) enum ToolCallParse {
    /// No `<tool_call>` opener at all — the reply is prose.
    None,
    /// A well-formed call: `(before, parsed_json, after)`.
    Call(String, Value, String),
    /// A `<tool_call>` opener whose payload could not be used. The call did
    /// NOT run. `tool` is the name recovered by a lenient scan, when there is
    /// one; `before` is the prose that preceded the tag.
    Malformed {
        before: String,
        tool: Option<String>,
        error: String,
    },
}

/// Recover a tool name from a payload that is not valid JSON, so the notice
/// can say WHICH action did not run.
pub(super) fn salvage_tool_name(payload: &str) -> Option<String> {
    for key in ["\"tool\"", "\"name\""] {
        let Some(i) = payload.find(key) else { continue };
        let rest = payload[i + key.len()..].trim_start();
        let Some(rest) = rest.strip_prefix(':') else {
            continue;
        };
        let Some(rest) = rest.trim_start().strip_prefix('"') else {
            continue;
        };
        if let Some(end) = rest.find('"') {
            let name = &rest[..end];
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    None
}

/// Extract the first `<tool_call>...</tool_call>` block from a text.
///
/// A malformed block is reported, not swallowed. Returning "no tool call" for
/// it made the loop treat the text as the final answer, so the user read
/// "I am now setting this schedule up" while the upsert never ran — observed
/// 2026-09-24 03:42 UTC on Gemini (a `manage_scheduled_task` payload whose
/// third step object was never closed), with one WARN as the only trace.
pub(super) fn extract_tool_call(text: &str) -> ToolCallParse {
    let open = "<tool_call>";
    let close = "</tool_call>";

    let Some(start) = text.find(open) else {
        return ToolCallParse::None;
    };
    let before = text[..start].to_string();
    let body_start = start + open.len();
    let Some(rel_end) = text[body_start..].find(close) else {
        let payload = &text[body_start..];
        warn!("tool_call block is never closed: {}", payload);
        return ToolCallParse::Malformed {
            before,
            tool: salvage_tool_name(payload),
            error: "the <tool_call> tag was never closed with </tool_call>".to_string(),
        };
    };
    let end = body_start + rel_end;
    let json_str = &text[body_start..end];
    let after = text[end + close.len()..].to_string();

    match serde_json::from_str::<Value>(json_str) {
        Ok(mut v) => {
            // Normalise: some models (e.g. Gemini) emit {"name":"...","args":{}}
            // instead of {"tool":"...","args":{}}.  Accept both.
            if v.get("tool").is_none()
                && let Some(name) = v["name"].as_str().map(|s| s.to_string())
                && let Some(obj) = v.as_object_mut()
            {
                obj.insert("tool".to_string(), Value::String(name));
            }
            if v.get("tool").and_then(|t| t.as_str()).is_some() {
                ToolCallParse::Call(before, v, after)
            } else {
                warn!("Tool call JSON missing 'tool'/'name' key: {}", json_str);
                ToolCallParse::Malformed {
                    before,
                    tool: None,
                    error: "the JSON has no \"tool\" key naming the tool to call".to_string(),
                }
            }
        }
        Err(e) => {
            warn!("Failed to parse tool_call JSON: {} — {}", json_str, e);
            ToolCallParse::Malformed {
                before,
                tool: salvage_tool_name(json_str),
                error: format!("the JSON is invalid: {e}"),
            }
        }
    }
}

/// How many times the text loop asks the model to re-emit a malformed call
/// before giving up and telling the user it did not run.
pub(super) const MAX_MALFORMED_TOOL_CALL_RETRIES: usize = 1;

/// The correction fed back to the model after a malformed tool call.
pub(super) fn malformed_tool_call_correction(tool: Option<&str>, error: &str) -> String {
    let which = tool.map(|t| format!(" to `{t}`")).unwrap_or_default();
    format!(
        "Your tool call{which} did NOT run: {error}. Nothing was executed. Re-emit the \
         complete call as ONE valid JSON object inside <tool_call>...</tool_call>, checking \
         that every {{ and [ is closed. If you cannot, answer without it and say plainly that \
         the action was not performed."
    )
}

/// The visible line appended to a reply whose tool call never ran. Prose that
/// promises an action must not stand when the action did not happen.
pub(super) fn malformed_tool_call_notice(tool: Option<&str>, error: &str) -> String {
    let which = tool
        .map(|t| format!("`{t}`"))
        .unwrap_or_else(|| "a tool".to_string());
    format!(
        "⚠ The call to {which} was malformed and did NOT run ({error}). Nothing described \
         above as being done by that call has happened — ask again to retry it."
    )
}

// ============================================================================

#[cfg(test)]
mod malformed_tool_call_tests {
    use super::*;
    use crate::clients::chat::test_support::{chat_service, cloud_config, test_request};

    /// The real reply from 2026-09-24 03:42 UTC (Gemini, front door). The
    /// third step's `arguments` object is never closed, so the JSON is invalid;
    /// the prose before it promises the schedule is being set up.
    const REPLY_0342: &str = r#"I am now setting this schedule up.

<tool_call>{"tool":"manage_scheduled_task","args":{"action":"upsert","name":"nia-actor-motivation-monitoring","interval_seconds":86400,"steps":[{"tool_name":"search_web","arguments":{"query":"actors influence, blackmail, bribery","limit":5},"provider_hint":"ollama","max_attempts":3,"description":"Fetch reports."},{"tool_name":"neo4j_query","arguments":{"cypher":"MATCH (a:Actor) RETURN a LIMIT 10","params":{},"readonly":true},"provider_hint":"ollama","max_attempts":3,"description":"Query."},{"tool_name":"reason","arguments":{"prompt":"Synthesize.","context":"{{_prev}}","provider_hint":"ollama","max_attempts":3,"description":"Analyze."},{"tool_name":"store_note","arguments":{"content":"{{_prev}}","source_context":"NIA_Actor_Motivation_Monitor","note_type":"source_record","provider_hint":"ollama","max_attempts":3,"description":"Persist."}}]}}</tool_call>"#;

    #[test]
    fn the_0342_payload_is_reported_malformed_with_its_tool_name() {
        match extract_tool_call(REPLY_0342) {
            ToolCallParse::Malformed {
                before,
                tool,
                error,
            } => {
                assert_eq!(tool.as_deref(), Some("manage_scheduled_task"));
                assert!(error.contains("invalid"), "{error}");
                assert!(before.contains("setting this schedule up"));
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn an_unclosed_tag_is_malformed_not_prose() {
        let t = r#"Saving it now. <tool_call>{"tool":"store_note","args":{"content":"x"}}"#;
        match extract_tool_call(t) {
            ToolCallParse::Malformed { tool, error, .. } => {
                assert_eq!(tool.as_deref(), Some("store_note"));
                assert!(error.contains("never closed"), "{error}");
            }
            other => panic!("expected Malformed, got {other:?}"),
        }
    }

    #[test]
    fn a_call_without_a_tool_key_is_malformed() {
        let t = r#"<tool_call>{"args":{"q":"x"}}</tool_call>"#;
        assert!(matches!(
            extract_tool_call(t),
            ToolCallParse::Malformed { tool: None, .. }
        ));
    }

    #[test]
    fn well_formed_calls_and_plain_prose_are_unchanged() {
        let t = r#"Looking. <tool_call>{"name":"search_notes","args":{"query":"x"}}</tool_call>"#;
        match extract_tool_call(t) {
            ToolCallParse::Call(before, v, _) => {
                assert_eq!(before, "Looking. ");
                assert_eq!(v["tool"], "search_notes");
            }
            other => panic!("expected Call, got {other:?}"),
        }
        assert!(matches!(
            extract_tool_call("Just an answer."),
            ToolCallParse::None
        ));
    }

    fn completion(content: &str) -> wiremock::ResponseTemplate {
        wiremock::ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "choices": [{"index": 0, "message": {"role": "assistant", "content": content},
                         "finish_reason": "stop"}]
        }))
    }

    async fn drive_text_loop(server: &wiremock::MockServer) -> Vec<ChatEvent> {
        let svc = chat_service(vec![]);
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(64);
        svc.run_text_loop(
            cloud_config(server),
            vec![],
            None,
            test_request(),
            "sys".into(),
            tx,
            false,
            MAX_TOOL_ITERATIONS,
        )
        .await;
        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            seen.push(e);
        }
        seen
    }

    fn final_messages(seen: &[ChatEvent]) -> Vec<String> {
        seen.iter()
            .filter_map(|e| match e {
                ChatEvent::Message { content } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    /// The 03:42 turn, replayed: the model repeats the malformed call. The turn
    /// must NOT end with "I am now setting this schedule up" standing alone.
    #[tokio::test]
    async fn a_repeated_malformed_call_ends_with_a_visible_did_not_run_notice() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(completion(REPLY_0342))
            // The original attempt plus exactly one retry.
            .expect(1 + MAX_MALFORMED_TOOL_CALL_RETRIES as u64)
            .mount(&server)
            .await;

        let seen = drive_text_loop(&server).await;
        let msgs = final_messages(&seen);
        assert_eq!(msgs.len(), 1, "{seen:?}");
        let m = &msgs[0];
        assert!(m.contains("setting this schedule up"), "{m}");
        assert!(m.contains("did NOT run"), "{m}");
        assert!(m.contains("manage_scheduled_task"), "{m}");
        assert!(!m.contains("<tool_call>"), "raw tag leaked: {m}");
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, ChatEvent::ToolResult { success: true, .. })),
            "nothing ran, so nothing may be reported as succeeding: {seen:?}"
        );
    }

    /// One correction is enough when the model fixes itself: the retry's
    /// answer is delivered and the correction named the failed tool.
    #[tokio::test]
    async fn a_malformed_call_gets_one_correction_round() {
        use wiremock::matchers::{body_string_contains, method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_string_contains("did NOT run"))
            .respond_with(completion(
                "I could not save the schedule; the action was not performed.",
            ))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(completion(REPLY_0342))
            .expect(1)
            .mount(&server)
            .await;

        let seen = drive_text_loop(&server).await;
        assert_eq!(
            final_messages(&seen),
            vec!["I could not save the schedule; the action was not performed.".to_string()],
            "{seen:?}"
        );
    }
}
