//! OpenAI-compatible wire format (Ollama Cloud, llama.cpp, any `OpenAiCompat`
//! provider): `/v1/chat/completions` with SSE streaming and tool-call deltas
//! accumulated per index.

use super::*;

struct OpenAiTurn {
    url: String,
    config: LlmConfig,
    provider_label: String,
    client: reqwest::Client,
    tools: Vec<Value>,
    messages: Vec<Value>,
}

impl OpenAiTurn {
    fn new(
        config: LlmConfig,
        tools: &[agent_brain_protocol::ToolDefinition],
        request: &ChatRequest,
        system_prompt: String,
    ) -> Self {
        let base_url = config.base_url.as_deref().unwrap_or("https://ollama.com");
        let mut messages: Vec<Value> = vec![json!({ "role": "system", "content": system_prompt })];
        for h in &request.history {
            messages.push(json!({ "role": h.role, "content": h.content }));
        }
        messages.push(json!({ "role": "user", "content": request.message }));
        Self {
            url: format!("{base_url}/v1/chat/completions"),
            provider_label: config.provider_label(),
            client: reqwest::Client::new(),
            tools: tools
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
                .collect(),
            messages,
            config,
        }
    }
}

#[async_trait::async_trait]
impl ProviderTurn for OpenAiTurn {
    fn provider_name(&self) -> Option<String> {
        Some(self.provider_label.clone())
    }

    fn label(&self) -> &'static str {
        "OllamaCloud"
    }

    fn model(&self) -> &str {
        &self.config.model
    }

    async fn call(
        &mut self,
        svc: &ChatService,
        offer_tools: bool,
        stream_to: Option<&mpsc::Sender<ChatEvent>>,
    ) -> Result<RoundReply, String> {
        let model = self.config.model.clone();
        let mut body = json!({
            "model": model,
            "messages": self.messages,
            "stream": true,
            // Ask OpenAI-compat servers to include a usage block in the final
            // stream chunk — without it token accounting is NULL.
            "stream_options": { "include_usage": true },
            "temperature": self.config.temperature,
        });
        if offer_tools {
            body["tools"] = Value::Array(self.tools.clone());
        }

        let mut req = self
            .client
            .post(&self.url)
            .header("content-type", "application/json")
            .timeout(self.config.timeout)
            .json(&body);
        if let Some(ref key) = self.config.api_key {
            req = req.header("Authorization", format!("Bearer {key}"));
        }

        // One provider concurrency slot for this request only
        // (services/provider_gate.rs): taken before the clock starts so a wait
        // is not billed as call time, and dropped once the response is read,
        // before any tool runs.
        let gate_permit = crate::services::provider_gate::acquire(&self.provider_label).await;
        let call_start = std::time::Instant::now();
        let response = match req.send().await {
            Ok(r) => r,
            Err(e) => {
                svc.record_llm_call(
                    &self.provider_label,
                    &model,
                    false,
                    call_start.elapsed().as_millis() as i64,
                    None,
                    None,
                    Some(&e.to_string()),
                );
                return Err(format!("OllamaCloud request failed: {e}"));
            }
        };
        // Rate-limit headers feed the budget whatever the status: a success
        // can report the last request of a window as well as a 429 can.
        crate::services::budget::observe_headers(
            &self.provider_label,
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
            // 429s are the quota-pressure signal — mark them in the ledger and
            // start the model's cooldown in the budget.
            let error_kind = Some(crate::services::shared_llm::failure_kind_of(&format!(
                "Status {status}: {body_text}"
            )));
            let tool_label = usage_tool_label();
            crate::services::budget::record_call(
                svc.telemetry.as_ref(),
                &self.provider_label,
                &model,
                Some(tool_label.as_str()),
                false,
                Some(call_start.elapsed().as_millis() as i64),
                None,
                None,
                error_kind,
                Some(body_text.as_str()),
            );
            return Err(format!("OllamaCloud error ({status}): {body_text}"));
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
            let chunk = chunk_result.map_err(|e| format!("OllamaCloud stream error: {e}"))?;
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

                if let Some(err) = chunk_json.get("error").and_then(|v| v.as_str()) {
                    return Err(format!("OllamaCloud error: {err}"));
                }

                if chunk_json.get("usage").is_some() {
                    usage_tokens = (
                        chunk_json["usage"]["prompt_tokens"].as_i64(),
                        chunk_json["usage"]["completion_tokens"].as_i64(),
                    );
                }

                let delta = &chunk_json["choices"][0]["delta"];
                if let Some(content) = delta["content"].as_str()
                    && !content.is_empty()
                {
                    full_content.push_str(content);
                    if let Some(tx) = stream_to {
                        let _ = tx
                            .send(ChatEvent::Token {
                                content: content.to_string(),
                            })
                            .await;
                    }
                }

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

                // Do NOT break on finish_reason alone: the usage chunk arrives
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

        // The response is fully read: free the provider slot before tools run.
        // A delegated sub-agent on this same provider would otherwise wait on it.
        drop(gate_permit);
        svc.record_llm_call(
            &self.provider_label,
            &model,
            true,
            call_start.elapsed().as_millis() as i64,
            usage_tokens.0,
            usage_tokens.1,
            None,
        );

        let mut calls: Vec<ToolCallReq> = tc_acc
            .into_values()
            .map(|(id, name, args_str)| ToolCallReq {
                id,
                name,
                args: serde_json::from_str(&args_str).unwrap_or(Value::Null),
            })
            .collect();

        // Fallback: some models (e.g. MiniMax) leak XML-style tool calls into
        // the text stream instead of the function-call delta channel.
        if calls.is_empty() {
            calls = parse_xml_tool_calls(&full_content)
                .into_iter()
                .map(|(id, name, args)| ToolCallReq { id, name, args })
                .collect();
            if !calls.is_empty() {
                full_content = strip_xml_tool_calls(&full_content);
            }
        }

        Ok(RoundReply {
            text: full_content,
            calls,
            malformed: None,
        })
    }

    fn push_user(&mut self, text: String) {
        self.messages
            .push(json!({ "role": "user", "content": text }));
    }

    fn push_assistant(&mut self, reply: &RoundReply) {
        let tool_calls: Vec<Value> = reply
            .calls
            .iter()
            .map(|c| {
                // OpenAI spec: `arguments` must be a JSON-encoded object string.
                // Value::Null serialises to "null", which strict servers reject;
                // normalise to "{}".
                let args = if c.args.is_object() {
                    serde_json::to_string(&c.args).unwrap_or_else(|_| "{}".to_string())
                } else {
                    "{}".to_string()
                };
                json!({
                    "id": c.id,
                    "type": "function",
                    "function": { "name": c.name, "arguments": args }
                })
            })
            .collect();
        // content must be null (not "") when the model emitted no text before
        // its tool calls; an empty string 500s on strict OpenAI-compatible servers.
        let content = if reply.text.is_empty() {
            Value::Null
        } else {
            Value::String(reply.text.clone())
        };
        self.messages.push(json!({
            "role": "assistant",
            "content": content,
            "tool_calls": tool_calls,
        }));
    }

    fn push_tool_results(&mut self, results: Vec<ToolOutcome>) {
        // OpenAI requires tool results as role="tool" with tool_call_id.
        for r in results {
            self.messages.push(json!({
                "role": "tool",
                "tool_call_id": r.id,
                "content": r.model_text,
            }));
        }
    }

    fn trim_history(&mut self) {
        // Keep system message [0] + user message [1] + the last
        // MAX_HISTORY_MESSAGES: schemas are resent every round, and combined
        // context grows until the server 500s.
        if self.messages.len() > MAX_HISTORY_MESSAGES + 2 {
            let keep_from = self.messages.len() - MAX_HISTORY_MESSAGES;
            let tail: Vec<Value> = self.messages.drain(keep_from..).collect();
            self.messages.truncate(2);
            self.messages.extend(tail);
        }
    }
}

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
        allow_delegate: bool,
        max_iterations: usize,
    ) {
        let turn = OpenAiTurn::new(config, &tools, &request, system_prompt);
        self.run_round_loop(
            turn,
            &tools,
            handler,
            &request,
            tx,
            allow_delegate,
            max_iterations,
        )
        .await;
    }
}
