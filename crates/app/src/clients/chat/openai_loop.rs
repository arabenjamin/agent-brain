//! OpenAI-compatible tool loop (Ollama Cloud, llama.cpp, any `OpenAiCompat` provider).

use super::*;

impl ChatService {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_ollama_cloud_loop(
        &self,
        config: LlmConfig,
        tools: Vec<agent_brain_protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        system_prompt: String,
        tx: mpsc::Sender<ChatEvent>,
        // Whether a `delegate` tool call is intercepted and run as a sub-agent.
        // True only for the top-level orchestrator loop; a sub-agent's own loop
        // passes false so it cannot delegate further (depth cap 1).
        allow_delegate: bool,
        max_iterations: usize,
    ) {
        // Sized against THIS loop's actual schema pressure, not a blanket
        // constant — see `tool_result_budget`. Computed once: the allowlist does
        // not change mid-loop.
        let result_budget = tool_result_budget(&tools, self.context_window_tokens(&config.model));
        // At INFO on purpose: once per loop, and it is the number that explains
        // why a complete tool result can still arrive clipped. Without it, the
        // only symptom is a worker hedging an answer it actually had.
        tracing::info!(
            tools = tools.len(),
            result_budget,
            "tool-result budget for this loop"
        );
        let base_url = config.base_url.as_deref().unwrap_or("https://ollama.com");
        let url = format!("{}/v1/chat/completions", base_url);
        let model = config.model.clone();
        let provider_label = config.provider_label();

        // OpenAI-format tools array.
        let oai_tools: Vec<Value> = tools
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

        // Initial messages.
        let mut messages: Vec<Value> = vec![json!({ "role": "system", "content": system_prompt })];
        for h in &request.history {
            messages.push(json!({ "role": h.role, "content": h.content }));
        }
        messages.push(json!({ "role": "user", "content": request.message }));

        let client = reqwest::Client::new();
        let mut empty_completions = 0usize;

        // Rounds that actually spent a tool call, which is what the budget is
        // for. Empty-completion retries deliberately do not count against it:
        // a provider failure is not the model using up its allowance.
        let mut tool_rounds = 0usize;
        let mut answered = false;
        let mut final_round = false;

        // Worker/voice split: when synthesis is configured, this loop is the
        // *worker*. Its own prose is surfaced as thinking rather than as the
        // answer, and a second model composes the reply from the tool results.
        let do_synthesis = request.synthesis_provider.is_some();
        let mut weak_model_answer = String::new();
        let mut gathered: Vec<String> = Vec::new();
        // Every tool call this turn actually made (name, success), regardless of
        // synthesis mode — the ground-truth receipt the final answer is anchored
        // to. See tool_trace_receipt.
        let mut tool_trace: Vec<(String, bool)> = Vec::new();

        // The upper bound only guarantees termination — the loop exits on an
        // answer, on a hard failure, or after the wrap-up round below.
        // One guard per turn: grounding state and duplicate fingerprints are
        // per-turn, and the ladder may run this loop once per rung.
        let mut write_guard = TurnWriteGuard::default();

        for _ in 0..=(max_iterations + MAX_EMPTY_COMPLETION_RETRIES + 1) {
            // One extra pass beyond the tool budget: the wrap-up round. See
            // FINAL_ROUND_NUDGE — it is offered no tools, so a looping model
            // has to answer from what it has instead of searching again.
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
                // Ask OpenAI-compat servers to include a usage block in the
                // final stream chunk — without it token accounting is NULL.
                "stream_options": { "include_usage": true },
                "temperature": config.temperature,
            });
            if !final_round {
                body["tools"] = Value::Array(oai_tools.clone());
            }

            let mut req = client
                .post(&url)
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
                            message: format!("OllamaCloud request failed: {e}"),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };
            // Rate-limit headers feed the budget whatever the status: a success
            // can report the last request of a window as well as a 429 can.
            crate::services::budget::observe_headers(
                &provider_label,
                &model,
                response.status().as_u16(),
                response.headers(),
            );

            if !response.status().is_success() {
                let status = response.status();
                let body_text = response.text().await.unwrap_or_default();
                warn!(
                    status = %status,
                    model = %model,
                    body = %body_text,
                    "OllamaCloud returned non-success status"
                );
                // 429s are the quota-pressure signal — mark them in the ledger
                // and start the model's cooldown in the budget.
                let error_kind = Some(crate::services::shared_llm::failure_kind_of(&format!(
                    "Status {status}: {body_text}"
                )));
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
                    error_kind,
                    Some(body_text.as_str()),
                );
                let _ = tx
                    .send(ChatEvent::Error {
                        message: format!("OllamaCloud error ({status}): {body_text}"),
                    })
                    .await;
                let _ = tx.send(ChatEvent::Done).await;
                return;
            }

            // Parse SSE stream: lines look like "data: {...}" or "data: [DONE]".
            let mut byte_stream = response.bytes_stream();
            let mut line_buf = String::new();
            let mut full_content = String::new();
            // Usage block (OpenAI-compat servers send it in the final chunk).
            let mut usage_tokens: (Option<i64>, Option<i64>) = (None, None);
            // Tool call accumulator: index -> (call_id, name, accumulated_args)
            let mut tc_acc: std::collections::BTreeMap<u64, (String, String, String)> =
                std::collections::BTreeMap::new();
            'stream: while let Some(chunk_result) = byte_stream.next().await {
                let chunk = match chunk_result {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx
                            .send(ChatEvent::Error {
                                message: format!("OllamaCloud stream error: {e}"),
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
                    if line == "data: [DONE]" {
                        break 'stream;
                    }

                    let json_str = line.strip_prefix("data: ").unwrap_or(&line);
                    let chunk_json: Value = match serde_json::from_str(json_str) {
                        Ok(v) => v,
                        Err(_) => continue,
                    };

                    // Surface API-level errors.
                    if let Some(err) = chunk_json.get("error").and_then(|v| v.as_str()) {
                        let _ = tx
                            .send(ChatEvent::Error {
                                message: format!("OllamaCloud error: {err}"),
                            })
                            .await;
                        let _ = tx.send(ChatEvent::Done).await;
                        return;
                    }

                    // Capture usage when the server includes it (final chunk).
                    if chunk_json.get("usage").is_some() {
                        usage_tokens = (
                            chunk_json["usage"]["prompt_tokens"].as_i64(),
                            chunk_json["usage"]["completion_tokens"].as_i64(),
                        );
                    }

                    let delta = &chunk_json["choices"][0]["delta"];

                    // Stream content tokens — unless a voice model will compose
                    // the reply. Then the worker's prose is not the answer: it
                    // goes out as Thinking once the round ends. Streaming it
                    // as Tokens sent the user TWO answers, the worker's and the
                    // voice's (seen 2026-10-02, first turn with gpt-oss as the
                    // front door and a Gemini voice; the Gemini front door ran
                    // the non-streaming text loop and so never showed it).
                    if let Some(content) = delta["content"].as_str()
                        && !content.is_empty()
                    {
                        full_content.push_str(content);
                        if !do_synthesis {
                            let _ = tx
                                .send(ChatEvent::Token {
                                    content: content.to_string(),
                                })
                                .await;
                        }
                    }

                    // Accumulate tool-call deltas.
                    if let Some(tcs) = delta["tool_calls"].as_array() {
                        for tc in tcs {
                            let idx = tc["index"].as_u64().unwrap_or(0);
                            let entry = tc_acc.entry(idx).or_default();
                            if let Some(id) = tc["id"].as_str() {
                                entry.0 = id.to_string();
                            }
                            if let Some(name) = tc["function"]["name"].as_str() {
                                entry.1 = name.to_string();
                            }
                            if let Some(args) = tc["function"]["arguments"].as_str() {
                                entry.2.push_str(args);
                            }
                        }
                    }

                    // Do NOT break on finish_reason: the usage chunk arrives
                    // AFTER it (choices is empty there), followed by [DONE].
                    // Breaking early was silently discarding token counts.
                    let finish = chunk_json["choices"][0]["finish_reason"]
                        .as_str()
                        .unwrap_or("");
                    if (finish == "stop" || finish == "tool_calls") && usage_tokens.0.is_some() {
                        break 'stream;
                    }
                }
            }

            // Convert accumulated tool calls to a stable ordered list.
            let mut tool_calls: Vec<(String, String, Value)> = tc_acc
                .into_values()
                .map(|(id, name, args_str)| {
                    let args = serde_json::from_str(&args_str).unwrap_or(Value::Null);
                    (id, name, args)
                })
                .collect();

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

            // Fallback: some models (e.g. MiniMax) leak XML-style tool calls into the
            // text stream instead of the function-call delta channel. Parse them here.
            if tool_calls.is_empty() {
                tool_calls = parse_xml_tool_calls(&full_content);
                if !tool_calls.is_empty() {
                    // Strip the XML from the displayed content.
                    full_content = strip_xml_tool_calls(&full_content);
                }
            }

            if tool_calls.is_empty() {
                // Nothing at all came back: no text and no tool call, despite the
                // usage block counting generated tokens. That is a failed call, not
                // a finished turn — re-send the identical request rather than
                // exiting silently. See MAX_EMPTY_COMPLETION_RETRIES.
                if full_content.is_empty() {
                    if empty_completions < MAX_EMPTY_COMPLETION_RETRIES {
                        empty_completions += 1;
                        warn!(
                            model = %model,
                            completion_tokens = ?usage_tokens.1,
                            attempt = empty_completions,
                            "OllamaCloud returned an empty completion — retrying"
                        );
                        continue;
                    }
                    warn!(
                        model = %model,
                        attempts = empty_completions + 1,
                        "OllamaCloud returned an empty completion on every attempt — giving up"
                    );
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: EMPTY_COMPLETION_MESSAGE.into(),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }

                // No tool calls — final answer.
                answered = true;
                if do_synthesis {
                    // Surface the worker's answer as thinking so the user can
                    // see what was gathered before the voice model speaks.
                    weak_model_answer = full_content.clone();
                    let _ = tx
                        .send(ChatEvent::Thinking {
                            content: full_content,
                        })
                        .await;
                } else {
                    let _ = tx
                        .send(ChatEvent::Message {
                            content: full_content,
                        })
                        .await;
                }
                break;
            }

            // A tool call is what the budget counts. The wrap-up round offers no
            // tools, so reaching here on it means the model smuggled one through
            // the text channel (`parse_xml_tool_calls`); let it run, but do not
            // let it buy another round.
            tool_rounds += 1;

            // Emit any reasoning text that accompanied the tool calls.
            if !full_content.trim().is_empty() {
                let _ = tx
                    .send(ChatEvent::Thinking {
                        content: full_content.clone(),
                    })
                    .await;
            }

            // Append assistant message with tool_calls in OpenAI format.
            // content must be null (not "") when the model emits no text before tool calls;
            // sending an empty string causes 500s from strict OpenAI-compatible servers.
            let oai_tc: Vec<Value> = tool_calls
                .iter()
                .map(|(id, name, args)| {
                    // OpenAI spec: `arguments` must be a JSON-encoded object string.
                    // Value::Null (model sent no args) serialises to "null" which strict
                    // servers reject; normalise to "{}" instead.
                    let args_str = if args.is_null() || !args.is_object() {
                        "{}".to_string()
                    } else {
                        serde_json::to_string(args).unwrap_or_else(|_| "{}".to_string())
                    };
                    json!({
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": args_str
                        }
                    })
                })
                .collect();
            let content_val: Value = if full_content.is_empty() {
                Value::Null
            } else {
                Value::String(full_content.clone())
            };
            messages.push(json!({
                "role": "assistant",
                "content": content_val,
                "tool_calls": oai_tc,
            }));

            // Execute each tool call and append results.
            for (tool_id, tool_name, tool_args) in &tool_calls {
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
                        tool_name,
                        tool_args,
                        &tx,
                    )
                    .await;

                // THE HUMAN GETS THE WHOLE THING. The model's ration and the
                // user's view are different concerns and must not share a cap.
                //
                // They were briefly unified so the UI would stop flagging
                // TRUNCATED on results the model had in fact received whole.
                // That fixed a false signal and created a worse one: it capped
                // what the USER can see at what a 4B local model can read.
                // Asked to "see all of what we learned from video learning",
                // the answer is the 50 rows — not a 4B model's ration of them.
                // The frontend already renders this preview in full inside an
                // expandable block, so the data path to the person existed the
                // whole time; only this cap was standing in it.
                //
                // Honesty about the model's narrower view is preserved by
                // `model_saw_chars` on the event, not by degrading the view.
                let preview: String = user_facing_result(
                    &annotate_tool_result(tool_name, &result_text),
                    bulk.as_deref(),
                );
                // The model's ration: schema- and window-aware, because tool
                // schemas are resent every round. Computed separately from the
                // user's copy above, on purpose — and NEVER including `bulk`,
                // which is the whole reason a bulk result's context cost stays
                // flat no matter how many rows it carries.
                let model_preview: String =
                    prepare_tool_result(tool_name, &result_text, result_budget);
                let model_saw = (model_preview.chars().count() < preview.chars().count())
                    .then(|| model_preview.chars().count());

                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: tool_name.clone(),
                        success,
                        preview: preview.clone(),
                        model_saw_chars: model_saw,
                    })
                    .await;

                tool_trace.push((tool_name.clone(), success));
                if do_synthesis && success {
                    gathered.push(format!("### {tool_name}\n{model_preview}"));
                }

                // OpenAI requires tool results as role="tool" with tool_call_id.
                // Capped at `result_budget` (schema-aware) because tool schemas
                // are resent every round; combined context grows quickly and
                // causes 500s. A lean sub-agent allowlist earns the full cap.
                let cloud_preview: String =
                    prepare_tool_result(tool_name, &result_text, result_budget);
                messages.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_id,
                    "content": cloud_preview,
                }));
            }

            // Trim message history to prevent unbounded context growth.
            // Keep system message [0] + user message [1] + last MAX_HISTORY_MESSAGES.
            if messages.len() > MAX_HISTORY_MESSAGES + 2 {
                let keep_from = messages.len() - MAX_HISTORY_MESSAGES;
                let tail: Vec<Value> = messages.drain(keep_from..).collect();
                messages.truncate(2); // system + user
                messages.extend(tail);
            }
        }

        // Synthesis runs whether or not the worker produced closing prose: the
        // tool results are what the voice model composes from, and a worker that
        // looped until its budget ran out is exactly the case where its own
        // summary is least worth relaying. This also means a turn that would
        // otherwise report NO_ANSWER_AFTER_TOOLS_MESSAGE still gets an answer.
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
            // Falling out of the loop having answered nothing is a real outcome
            // and needs a real explanation: the tool calls above did run, so the
            // user has to be told work may have happened even though nothing was
            // written back.
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
    // Sub-agent delegation — runs a worker profile's loop inside the turn
    // ========================================================================
}
