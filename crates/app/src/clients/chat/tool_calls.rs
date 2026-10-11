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
    use crate::clients::chat::test_support::{chat_service, cloud_config, sse, test_request};

    /// A reply cut off inside a tool call's arguments, as the OpenAI wire
    /// streams it (the shape of the 2026-09-24 03:42 turn: the `arguments`
    /// object never closes, and the prose before it promises the schedule).
    const CUT_OFF_CALL_SSE: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
        "\"content\":\"I am now setting this schedule up.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",",
        "\"type\":\"function\",\"function\":{\"name\":\"manage_workflow\",",
        "\"arguments\":\"{\\\"action\\\":\\\"upsert\\\",\\\"steps\\\":[{\\\"tool_name\\\":\\\"reason\\\"\"}}]},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}\n\n",
        "data: [DONE]\n\n",
    );

    const FIXED_ANSWER_SSE: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
        "\"content\":\"I could not save the schedule; the action was not performed.\"},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}\n\n",
        "data: [DONE]\n\n",
    );

    async fn drive_openai_loop(server: &wiremock::MockServer) -> Vec<ChatEvent> {
        let svc = chat_service(vec![]);
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(64);
        svc.run_openai_loop(
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

    /// The model repeats the cut-off call. The turn must NOT end with "I am
    /// now setting this schedule up" standing alone, and nothing may run.
    #[tokio::test]
    async fn a_repeated_malformed_call_ends_with_a_visible_did_not_run_notice() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(CUT_OFF_CALL_SSE))
            // The original attempt plus exactly one retry.
            .expect(1 + MAX_MALFORMED_TOOL_CALL_RETRIES as u64)
            .mount(&server)
            .await;

        let seen = drive_openai_loop(&server).await;
        let msgs = final_messages(&seen);
        assert_eq!(msgs.len(), 1, "{seen:?}");
        let m = &msgs[0];
        assert!(m.contains("setting this schedule up"), "{m}");
        assert!(m.contains("did NOT run"), "{m}");
        assert!(m.contains("manage_workflow"), "{m}");
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
            .respond_with(sse(FIXED_ANSWER_SSE))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(CUT_OFF_CALL_SSE))
            .expect(1)
            .mount(&server)
            .await;

        let seen = drive_openai_loop(&server).await;
        assert_eq!(
            final_messages(&seen),
            vec!["I could not save the schedule; the action was not performed.".to_string()],
            "{seen:?}"
        );
    }
}
