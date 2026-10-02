//! Anthropic native tool_use loop.

use super::*;

impl ChatService {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_anthropic_loop(
        &self,
        config: LlmConfig,
        tools: Vec<agent_brain_protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        system_prompt: String,
        tx: mpsc::Sender<ChatEvent>,
        allow_delegate: bool,
        max_iterations: usize,
    ) {
        let api_key = match &config.api_key {
            Some(k) => k.clone(),
            None => {
                let _ = tx
                    .send(ChatEvent::Error {
                        message: "Anthropic API key not set in LLM config.".into(),
                    })
                    .await;
                let _ = tx.send(ChatEvent::Done).await;
                return;
            }
        };

        let model = config.model.clone();

        let provider_label = config.provider_label();

        // Build Anthropic tools array.
        let anthropic_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        // Build initial messages array.
        let mut messages: Vec<Value> = request
            .history
            .iter()
            .map(|h| json!({ "role": h.role, "content": h.content }))
            .collect();
        messages.push(json!({ "role": "user", "content": request.message }));

        let client = reqwest::Client::new();
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com");

        let mut empty_completions = 0usize;

        // One guard per turn: grounding state and duplicate fingerprints are
        // per-turn, and the ladder may run this loop once per rung.
        let mut write_guard = TurnWriteGuard::default();

        for _iteration in 0..max_iterations {
            let body = json!({
                "model": model,
                "max_tokens": 4096,
                "system": system_prompt,
                "tools": anthropic_tools,
                "messages": messages,
            });

            // One provider concurrency slot for this request only (services/provider_gate.rs):

            // taken before the clock starts so a wait is not billed as call time, and

            // dropped once the response is read, before any tool runs.

            let gate_permit = crate::services::provider_gate::acquire(&provider_label).await;

            let call_start = std::time::Instant::now();
            let response = match client
                .post(format!("{}/v1/messages", base_url))
                .header("x-api-key", &api_key)
                .header("anthropic-version", "2023-06-01")
                .header("content-type", "application/json")
                .json(&body)
                .timeout(config.timeout)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    self.record_llm_call(
                        &provider_label,
                        &model,
                        false,
                        call_start.elapsed().as_millis() as i64,
                        None,
                        None,
                        Some(&e.to_string()),
                    );
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: format!("Anthropic request failed: {e}"),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            let resp_json: Value = match response.json().await {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: format!("Failed to parse Anthropic response: {e}"),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            // Check for API-level error.
            if let Some(err_type) = resp_json.get("type").and_then(|v| v.as_str())
                && err_type == "error"
            {
                let msg = resp_json["error"]["message"]
                    .as_str()
                    .unwrap_or("Unknown Anthropic error")
                    .to_string();
                let rate_limited = msg.contains("rate") || msg.contains("429");
                let label = usage_tool_label();
                crate::services::budget::record_call(
                    self.telemetry.as_ref(),
                    &provider_label,
                    &model,
                    Some(label.as_str()),
                    false,
                    Some(call_start.elapsed().as_millis() as i64),
                    None,
                    None,
                    rate_limited.then_some("rate_limited"),
                    Some(msg.as_str()),
                );
                let _ = tx.send(ChatEvent::Error { message: msg }).await;
                let _ = tx.send(ChatEvent::Done).await;
                return;
            }

            // The response is fully read: free the provider slot before tools run. A

            // delegated sub-agent on this same provider would otherwise wait on it.

            drop(gate_permit);

            self.record_llm_call(
                &provider_label,
                &model,
                true,
                call_start.elapsed().as_millis() as i64,
                resp_json["usage"]["input_tokens"].as_i64(),
                resp_json["usage"]["output_tokens"].as_i64(),
                None,
            );

            let stop_reason = resp_json["stop_reason"].as_str().unwrap_or("").to_string();
            let content_blocks = match resp_json["content"].as_array() {
                Some(arr) => arr.clone(),
                None => {
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: "No content in Anthropic response".into(),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            // Collect tool-use blocks and text blocks.
            let mut tool_use_blocks: Vec<Value> = Vec::new();
            let mut final_text = String::new();

            for block in &content_blocks {
                match block["type"].as_str() {
                    Some("text") => {
                        let text = block["text"].as_str().unwrap_or("").to_string();
                        if !text.is_empty() {
                            if stop_reason == "tool_use" {
                                let _ = tx.send(ChatEvent::Thinking { content: text }).await;
                            } else {
                                final_text.push_str(&text);
                            }
                        }
                    }
                    Some("tool_use") => {
                        tool_use_blocks.push(block.clone());
                    }
                    _ => {}
                }
            }

            if stop_reason == "end_turn" || tool_use_blocks.is_empty() {
                // Nothing at all came back: no text and no tool use. Same failure
                // as the other loops — retry rather than exit silently.
                // See MAX_EMPTY_COMPLETION_RETRIES.
                if final_text.is_empty() && tool_use_blocks.is_empty() {
                    if empty_completions < MAX_EMPTY_COMPLETION_RETRIES {
                        empty_completions += 1;
                        warn!(
                            model = %model,
                            stop_reason = %stop_reason,
                            attempt = empty_completions,
                            "Anthropic returned an empty completion — retrying"
                        );
                        continue;
                    }
                    warn!(
                        model = %model,
                        attempts = empty_completions + 1,
                        "Anthropic returned an empty completion on every attempt — giving up"
                    );
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: EMPTY_COMPLETION_MESSAGE.into(),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }

                // Emit the final message.
                if !final_text.is_empty() {
                    let _ = tx
                        .send(ChatEvent::Message {
                            content: final_text,
                        })
                        .await;
                }
                break;
            }

            // Append assistant turn to messages.
            messages.push(json!({ "role": "assistant", "content": content_blocks }));

            // Execute each tool call and build the user tool_result turn.
            let mut tool_results: Vec<Value> = Vec::new();
            for tool_block in &tool_use_blocks {
                let tool_name = tool_block["name"].as_str().unwrap_or("").to_string();
                let tool_id = tool_block["id"].as_str().unwrap_or("").to_string();
                let tool_input = tool_block["input"].clone();

                let _ = tx
                    .send(ChatEvent::ToolCall {
                        tool: tool_name.clone(),
                        args: tool_input.clone(),
                    })
                    .await;

                let (success, result_text, bulk) = self
                    .dispatch_tool_call(
                        allow_delegate,
                        &handler,
                        &mut write_guard,
                        &tool_name,
                        &tool_input,
                        &tx,
                    )
                    .await;

                let preview: String =
                    prepare_tool_result(&tool_name, &result_text, MAX_TOOL_RESULT_CHARS);
                // The person's copy carries the bulk; the model's `preview`
                // below never does.
                let user_copy = user_facing_result(&preview, bulk.as_deref());
                let model_saw = (preview.chars().count() < user_copy.chars().count())
                    .then(|| preview.chars().count());
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: tool_name.clone(),
                        success,
                        preview: user_copy,
                        model_saw_chars: model_saw,
                    })
                    .await;

                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_id,
                    "content": preview,
                    "is_error": !success,
                }));
            }

            // Append tool results as a user message.
            messages.push(json!({ "role": "user", "content": tool_results }));
        }

        let _ = tx.send(ChatEvent::Done).await;
    }

    // ========================================================================
    // Ollama native tool-use loop
    // ========================================================================
    //
    // Uses Ollama's /api/chat endpoint with the `tools` field (OpenAI-compatible
    // function-calling format), rather than injecting tool descriptions into the
    // system prompt and hoping the model emits a magic XML tag.
    //
    // Ollama response when a tool is called:
    //   message.tool_calls = [{ function: { name, arguments: {…} } }]
    // When no tool is called the message has normal `content` text.
}
