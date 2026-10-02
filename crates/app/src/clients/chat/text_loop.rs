//! Text-protocol loop for providers without native tool calls (`<tool_call>` parsing; Gemini).

use super::*;

impl ChatService {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_text_loop(
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
        let model = config.model.clone();
        let provider_label = config.provider_label();
        let llm = match LlmClient::with_config(config) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx
                    .send(ChatEvent::Error {
                        message: format!("Failed to create LLM client: {e}"),
                    })
                    .await;
                let _ = tx.send(ChatEvent::Done).await;
                return;
            }
        };

        // Serialize tools as a compact JSON block for the system prompt.
        let tools_json = tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema }))
            .collect::<Vec<_>>();
        let tools_str = serde_json::to_string(&tools_json).unwrap_or_else(|_| "[]".into());

        let system = format!(
            "{}\n\nAvailable tools (JSON array):\n{}\n\n\
             To call a tool emit EXACTLY one tag per call — no markdown, no extra text around it:\n\
             <tool_call>{{\"tool\":\"TOOL_NAME\",\"args\":{{...}}}}</tool_call>\n\
             Use the key \"tool\" (not \"name\"). \
             You may call multiple tools in sequence — one <tool_call> block at a time. \
             When you have a final answer write it as plain text with no <tool_call> tag.",
            system_prompt, tools_str
        );

        // Build initial chat message list.
        let mut messages: Vec<ChatMessage> = vec![ChatMessage::system(&system)];
        for h in &request.history {
            messages.push(ChatMessage {
                role: h.role.clone(),
                content: h.content.clone(),
            });
        }
        messages.push(ChatMessage::user(&request.message));

        let mut empty_completions = 0usize;
        let mut malformed_calls = 0usize;
        let mut tool_trace: Vec<(String, bool)> = Vec::new();

        // One guard per turn: grounding state and duplicate fingerprints are
        // per-turn, and the ladder may run this loop once per rung.
        let mut write_guard = TurnWriteGuard::default();

        for _iteration in 0..max_iterations {
            let call_start = std::time::Instant::now();
            let chat_result = llm.chat(&messages).await;
            self.record_llm_call(
                &provider_label,
                &model,
                chat_result.is_ok(),
                call_start.elapsed().as_millis() as i64,
                chat_result
                    .as_ref()
                    .ok()
                    .and_then(|r| r.tokens_in)
                    .map(i64::from),
                chat_result
                    .as_ref()
                    .ok()
                    .and_then(|r| r.tokens_out)
                    .map(i64::from),
                chat_result.as_ref().err().map(|e| e.to_string()).as_deref(),
            );
            let response = match chat_result {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: format!("LLM error: {e}"),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            let text = response.text.trim().to_string();

            // Check for tool calls.
            let parsed = extract_tool_call(&text);
            if let ToolCallParse::Malformed {
                before,
                tool,
                error,
            } = parsed
            {
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: tool
                            .clone()
                            .unwrap_or_else(|| "(unparsed tool call)".into()),
                        success: false,
                        preview: format!("Malformed tool call — not executed: {error}"),
                        model_saw_chars: None,
                    })
                    .await;
                if malformed_calls < MAX_MALFORMED_TOOL_CALL_RETRIES {
                    malformed_calls += 1;
                    warn!(model = %model, tool = ?tool, error = %error, "Malformed tool call — asking the model to re-emit it");
                    messages.push(ChatMessage::assistant(&text));
                    messages.push(ChatMessage::user(format!(
                        "{}\n\n{}",
                        malformed_tool_call_correction(tool.as_deref(), &error),
                        tool_trace_receipt(&tool_trace)
                    )));
                    continue;
                }
                warn!(model = %model, tool = ?tool, error = %error, "Malformed tool call again — reporting it to the user");
                let notice = malformed_tool_call_notice(tool.as_deref(), &error);
                let prose = before.trim();
                let content = if prose.is_empty() {
                    notice
                } else {
                    format!("{prose}\n\n{notice}")
                };
                let _ = tx.send(ChatEvent::Message { content }).await;
                break;
            }
            if let ToolCallParse::Call(before, call, after) = parsed {
                // Emit thinking text before the tool call.
                let thinking = before.trim().to_string();
                if !thinking.is_empty() {
                    let _ = tx.send(ChatEvent::Thinking { content: thinking }).await;
                }

                // Emit the tool call.
                let tool_name = call["tool"].as_str().unwrap_or("").to_string();
                let tool_args = call["args"].clone();

                let _ = tx
                    .send(ChatEvent::ToolCall {
                        tool: tool_name.clone(),
                        args: tool_args.clone(),
                    })
                    .await;

                let (success, result_text, bulk) = self
                    .dispatch_tool_call(
                        allow_delegate,
                        &handler,
                        &mut write_guard,
                        &tool_name,
                        &tool_args,
                        &tx,
                    )
                    .await;
                tool_trace.push((tool_name.clone(), success));

                let preview: String = result_text.chars().take(4000).collect();
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

                // Append assistant response and tool result to history.
                messages.push(ChatMessage::assistant(&text));

                let tool_result_msg = if !after.trim().is_empty() {
                    format!(
                        "Tool `{}` result:\n{}\n\n{}",
                        tool_name,
                        result_text,
                        after.trim()
                    )
                } else {
                    format!("Tool `{}` result:\n{}", tool_name, result_text)
                };
                messages.push(ChatMessage::user(tool_result_msg));
            } else {
                // Nothing at all came back: no text and no tool call. Same failure
                // as the two streaming loops — retry rather than exit silently.
                // See MAX_EMPTY_COMPLETION_RETRIES.
                if text.is_empty() {
                    if empty_completions < MAX_EMPTY_COMPLETION_RETRIES {
                        empty_completions += 1;
                        warn!(
                            model = %model,
                            attempt = empty_completions,
                            "LLM returned an empty completion — retrying"
                        );
                        continue;
                    }
                    warn!(
                        model = %model,
                        attempts = empty_completions + 1,
                        "LLM returned an empty completion on every attempt — giving up"
                    );
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: EMPTY_COMPLETION_MESSAGE.into(),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }

                // No tool call — this is the final response.
                debug!(text = %text, "Chat: final text response");
                let _ = tx.send(ChatEvent::Message { content: text }).await;
                break;
            }
        }

        let _ = tx.send(ChatEvent::Done).await;
    }
}
