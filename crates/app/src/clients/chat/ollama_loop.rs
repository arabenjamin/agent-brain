//! Host-Ollama `/api/chat` tool loop.

use super::*;

impl ChatService {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_ollama_tool_loop(
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
        let do_synthesis = request.synthesis_provider.is_some();
        let mut gathered: Vec<String> = Vec::new();
        // Ground-truth receipt of what actually ran this turn. See tool_trace_receipt.
        let mut tool_trace: Vec<(String, bool)> = Vec::new();
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or("http://localhost:11434");
        let model = config.model.clone();
        let provider_label = config.provider_label();

        // Build the Ollama tools array (OpenAI function-calling schema).
        let ollama_tools: Vec<Value> = tools
            .iter()
            .map(|t| {
                json!({
                    "type": "function",
                    "function": {
                        "name": t.name,
                        "description": t.description,
                        "parameters": t.input_schema,
                    }
                })
            })
            .collect();

        // Build the initial messages list.
        let mut messages: Vec<Value> = vec![json!({ "role": "system", "content": system_prompt })];
        for h in &request.history {
            messages.push(json!({ "role": h.role, "content": h.content }));
        }
        messages.push(json!({ "role": "user", "content": request.message }));

        let client = reqwest::Client::new();
        let mut weak_model_answer = String::new();
        let mut empty_completions = 0usize;
        let mut tool_rounds = 0usize;
        let mut answered = false;
        let mut final_round = false;

        // One guard per turn: grounding state and duplicate fingerprints are
        // per-turn, and the ladder may run this loop once per rung.
        let mut write_guard = TurnWriteGuard::default();

        for _ in 0..=(max_iterations + MAX_EMPTY_COMPLETION_RETRIES + 1) {
            // The wrap-up round: no tools offered, so a model that keeps
            // choosing to search has to answer instead. See FINAL_ROUND_NUDGE.
            if !final_round && tool_rounds >= max_iterations {
                final_round = true;
                warn!(
                    model = %model,
                    rounds = tool_rounds,
                    "Tool budget exhausted without an answer — asking for a final answer with no tools"
                );
                messages.push(json!({
                    "role": "user",
                    "content": format!("{}\n\n{}", FINAL_ROUND_NUDGE, tool_trace_receipt(&tool_trace)),
                }));
            }

            let mut body = json!({
                "model": model,
                "messages": messages,
                "stream": true,
                "options": {
                    "temperature": config.temperature,
                }
            });
            if !final_round {
                body["tools"] = Value::Array(ollama_tools.clone());
            }

            let mut req = client
                .post(format!("{}/api/chat", base_url))
                .header("content-type", "application/json")
                .timeout(config.timeout)
                .json(&body);
            if let Some(ref key) = config.api_key {
                req = req.header("Authorization", format!("Bearer {}", key));
            }
            // One provider concurrency slot for this request only (services/provider_gate.rs):
            // taken before the clock starts so a wait is not billed as call time, and
            // dropped once the response is read, before any tool runs.
            let gate_permit = crate::services::provider_gate::acquire(&provider_label).await;
            let call_start = std::time::Instant::now();
            let response = match req.send().await {
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
                            message: format!("Ollama request failed: {e}"),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            // Parse NDJSON streaming response, emitting Token events per chunk.
            let mut byte_stream = response.bytes_stream();
            let mut line_buf = String::new();
            let mut full_content = String::new();
            let mut tool_calls: Vec<Value> = Vec::new();
            // Usage from the final (done) chunk, for the model_usage ledger.
            let mut usage_tokens: (Option<i64>, Option<i64>) = (None, None);
            // Buffer tokens that arrive before <think> to suppress garbage
            // leading characters. Once <think> is seen, flush the buffer and
            // stream normally. If the stream ends without <think>, flush the
            // whole buffer (model doesn't use thinking blocks).
            let mut pre_think_buf: Vec<String> = Vec::new();
            let mut think_started = false;

            'stream: while let Some(chunk_result) = byte_stream.next().await {
                let chunk = match chunk_result {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx
                            .send(ChatEvent::Error {
                                message: format!("Ollama stream read error: {e}"),
                            })
                            .await;
                        let _ = tx.send(ChatEvent::Done).await;
                        return;
                    }
                };
                line_buf.push_str(&String::from_utf8_lossy(&chunk));

                while let Some(nl) = line_buf.find('\n') {
                    let line = line_buf[..nl].trim().to_string();
                    line_buf = line_buf[nl + 1..].to_string();
                    if line.is_empty() {
                        continue;
                    }

                    let chunk_json: Value = match serde_json::from_str(&line) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    // Surface Ollama-level errors (e.g. model not found).
                    if let Some(err) = chunk_json.get("error").and_then(|v| v.as_str()) {
                        let _ = tx
                            .send(ChatEvent::Error {
                                message: format!("Ollama error: {err}"),
                            })
                            .await;
                        let _ = tx.send(ChatEvent::Done).await;
                        return;
                    }

                    // Accumulate token content and emit Token event.
                    let token = chunk_json["message"]["content"]
                        .as_str()
                        .unwrap_or("")
                        .to_string();
                    if !token.is_empty() {
                        full_content.push_str(&token);
                        if think_started {
                            let _ = tx.send(ChatEvent::Token { content: token }).await;
                        } else if full_content.contains("<think>") {
                            // First time we see <think>: flush buffered tokens
                            // (from <think> onwards only) then stream normally.
                            think_started = true;
                            let flush_start = full_content.find("<think>").unwrap_or(0);
                            let flushed = full_content[flush_start..].to_string();
                            if !flushed.is_empty() {
                                let _ = tx.send(ChatEvent::Token { content: flushed }).await;
                            }
                        } else {
                            // Haven't seen <think> yet — buffer rather than emit.
                            pre_think_buf.push(token);
                        }
                    }

                    // Ollama sends tool_calls in a non-done chunk; accumulate from every chunk.
                    if let Some(calls) = chunk_json["message"]["tool_calls"].as_array()
                        && !calls.is_empty()
                    {
                        tool_calls.extend(calls.iter().cloned());
                    }

                    if chunk_json
                        .get("done")
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false)
                    {
                        usage_tokens = (
                            chunk_json["prompt_eval_count"].as_i64(),
                            chunk_json["eval_count"].as_i64(),
                        );
                        break 'stream;
                    }
                }
            }

            // The response is fully read: free the provider slot before tools run. A

            // delegated sub-agent on this same provider would otherwise wait on it.

            drop(gate_permit);

            self.record_llm_call(
                &provider_label,
                &model,
                true,
                call_start.elapsed().as_millis() as i64,
                usage_tokens.0,
                usage_tokens.1,
                None,
            );

            // If <think> was never seen, the model doesn't use thinking blocks.
            // Flush the buffered pre-think tokens now so the client sees output.
            if !think_started && !pre_think_buf.is_empty() {
                for t in pre_think_buf {
                    let _ = tx.send(ChatEvent::Token { content: t }).await;
                }
            }

            // Strip any garbage tokens emitted before the <think> block.
            // Some small models output stray characters (e.g. CJK tokens) before
            // beginning their actual reasoning.
            let content = if let Some(idx) = full_content.find("<think>") {
                full_content[idx..].to_string()
            } else {
                full_content
            };

            if tool_calls.is_empty() {
                // Nothing at all came back: no text and no tool call, despite the
                // provider counting generated tokens. That is a failed call, not a
                // finished turn — re-send the identical request rather than exiting
                // silently. See MAX_EMPTY_COMPLETION_RETRIES.
                if content.is_empty() {
                    if empty_completions < MAX_EMPTY_COMPLETION_RETRIES {
                        empty_completions += 1;
                        warn!(
                            model = %model,
                            eval_count = ?usage_tokens.1,
                            attempt = empty_completions,
                            "Ollama returned an empty completion — retrying"
                        );
                        continue;
                    }
                    warn!(
                        model = %model,
                        attempts = empty_completions + 1,
                        "Ollama returned an empty completion on every attempt — giving up"
                    );
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: EMPTY_COMPLETION_MESSAGE.into(),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }

                // No tool calls — weak model has a final answer.
                answered = true;
                if do_synthesis {
                    // Surface weak model's answer as a thinking event so the
                    // user can see what was researched before synthesis.
                    weak_model_answer = content.clone();
                    let _ = tx.send(ChatEvent::Thinking { content }).await;
                } else {
                    let _ = tx.send(ChatEvent::Message { content }).await;
                }
                break;
            }

            // A tool call is what the budget counts; empty-completion retries
            // above deliberately do not.
            tool_rounds += 1;

            // Emit thinking text that accompanied the tool calls (if any).
            if !content.trim().is_empty() {
                let _ = tx
                    .send(ChatEvent::Thinking {
                        content: content.clone(),
                    })
                    .await;
            }

            // Append the assistant message to history.
            messages.push(json!({
                "role": "assistant",
                "content": content,
                "tool_calls": tool_calls,
            }));

            // Execute each tool call and append results.
            for call in &tool_calls {
                let fn_obj = &call["function"];
                let tool_name = fn_obj["name"].as_str().unwrap_or("").to_string();

                // Ollama may send arguments as a JSON string or as an object.
                let tool_args: Value = match fn_obj.get("arguments") {
                    Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
                    Some(v) => v.clone(),
                    None => Value::Null,
                };

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

                tool_trace.push((tool_name.clone(), success));
                if do_synthesis && success {
                    gathered.push(format!("### {tool_name}\n{preview}"));
                }

                // Append the tool result as a tool message (truncated to avoid context overflow).
                messages.push(json!({
                    "role": "tool",
                    "content": preview,
                }));
            }
        }

        // Research mode: synthesize gathered findings with a stronger model.
        if do_synthesis {
            self.run_synthesis(
                &request,
                &gathered,
                &weak_model_answer,
                &tool_trace,
                tx.clone(),
            )
            .await;
        } else if !answered {
            // See the same guard in run_ollama_cloud_loop: the tool calls above
            // ran, so silence here would hide work that may have happened.
            warn!(
                model = %model,
                tool_rounds,
                "Chat loop ended without an answer after the wrap-up round"
            );
            let _ = tx
                .send(ChatEvent::Error {
                    message: NO_ANSWER_AFTER_TOOLS_MESSAGE.into(),
                })
                .await;
        }

        let _ = tx.send(ChatEvent::Done).await;
    }

    // ========================================================================
    // Ollama Cloud tool-use loop (OpenAI-compatible SSE streaming)
    // ========================================================================
    //
    // Ollama Cloud at https://ollama.com uses the OpenAI-compatible API:
    //   POST /v1/chat/completions  (not /api/chat)
    // The streaming response is SSE ("data: {...}\n\n") not NDJSON.
    // Tool-call deltas arrive piece-by-piece and must be accumulated per index.
}
