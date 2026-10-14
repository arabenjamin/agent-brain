//! Host-Ollama wire format: `/api/chat` with NDJSON streaming and the native
//! `tools` field. Tool calls arrive whole in a non-done chunk.

use super::*;

struct OllamaTurn {
    url: String,
    config: LlmConfig,
    provider_label: String,
    client: reqwest::Client,
    tools: Vec<Value>,
    messages: Vec<Value>,
    /// The raw `tool_calls` array from the last call, echoed back verbatim in
    /// the assistant turn.
    last_raw_calls: Vec<Value>,
}

impl OllamaTurn {
    fn new(
        config: LlmConfig,
        tools: &[agent_brain_protocol::ToolDefinition],
        request: &ChatRequest,
        system_prompt: String,
    ) -> Self {
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or("http://localhost:11434");
        let mut messages: Vec<Value> = vec![json!({ "role": "system", "content": system_prompt })];
        for h in &request.history {
            messages.push(json!({ "role": h.role, "content": h.content }));
        }
        messages.push(json!({ "role": "user", "content": request.message }));
        Self {
            url: format!("{base_url}/api/chat"),
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
            last_raw_calls: Vec::new(),
            config,
        }
    }
}

#[async_trait::async_trait]
impl ProviderTurn for OllamaTurn {
    fn label(&self) -> &'static str {
        "Ollama"
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
            // num_ctx on every call: without it host Ollama reads only the
            // first ~2K tokens, less than the system prompt alone. Measured
            // 2026-10-11 (Ollama 0.40.2, ~15K-token prompt): 2,050 tokens read
            // with no num_ctx, 15,293 with 16384. Its OpenAI-compatible
            // endpoint ignores `options`, which is why host Ollama keeps this
            // native adapter (CORE_AND_PLUGINS_PLAN 6a).
            "options": {
                "temperature": self.config.temperature,
                "num_ctx": self.config.num_ctx.unwrap_or_else(agent_brain_llm::llm::ollama_num_ctx),
            },
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
        // (services/provider_gate.rs), dropped before any tool runs.
        let gate_permit = agent_brain_llm::provider_gate::acquire(&self.provider_label).await;
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
                return Err(format!("Ollama request failed: {e}"));
            }
        };

        let mut byte_stream = response.bytes_stream();
        let mut line_buf = String::new();
        let mut full_content = String::new();
        let mut raw_calls: Vec<Value> = Vec::new();
        let mut usage_tokens: (Option<i64>, Option<i64>) = (None, None);
        // Buffer tokens that arrive before <think> to suppress garbage leading
        // characters. Once <think> is seen, flush from it and stream normally;
        // if the stream ends without one, flush the whole buffer.
        let mut pre_think_buf: Vec<String> = Vec::new();
        let mut think_started = false;

        'stream: while let Some(chunk_result) = byte_stream.next().await {
            let chunk = match chunk_result {
                Ok(c) => c,
                Err(e) => {
                    // A body cut off mid-stream is a failed call: ledger it.
                    let err = format!("Ollama stream read error: {e}");
                    svc.record_llm_call(
                        &self.provider_label,
                        &model,
                        false,
                        call_start.elapsed().as_millis() as i64,
                        None,
                        None,
                        Some(&err),
                    );
                    return Err(err);
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

                // Ollama-level errors (e.g. model not found).
                if let Some(err) = chunk_json.get("error").and_then(|v| v.as_str()) {
                    return Err(format!("Ollama error: {err}"));
                }

                let token = chunk_json["message"]["content"]
                    .as_str()
                    .unwrap_or("")
                    .to_string();
                if !token.is_empty() {
                    full_content.push_str(&token);
                    if let Some(tx) = stream_to {
                        if think_started {
                            let _ = tx.send(ChatEvent::Token { content: token }).await;
                        } else if let Some(start) = full_content.find("<think>") {
                            think_started = true;
                            let flushed = full_content[start..].to_string();
                            let _ = tx.send(ChatEvent::Token { content: flushed }).await;
                        } else {
                            pre_think_buf.push(token);
                        }
                    }
                }

                // Ollama sends tool_calls in a non-done chunk; accumulate from
                // every chunk.
                if let Some(calls) = chunk_json["message"]["tool_calls"].as_array() {
                    raw_calls.extend(calls.iter().cloned());
                }

                if chunk_json["done"].as_bool().unwrap_or(false) {
                    usage_tokens = (
                        chunk_json["prompt_eval_count"].as_i64(),
                        chunk_json["eval_count"].as_i64(),
                    );
                    break 'stream;
                }
            }
        }

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

        // The model never opened a <think> block: flush what was buffered.
        if let Some(tx) = stream_to
            && !think_started
        {
            for t in pre_think_buf {
                let _ = tx.send(ChatEvent::Token { content: t }).await;
            }
        }

        // Strip garbage tokens some small models emit before their <think>.
        let text = match full_content.find("<think>") {
            Some(idx) => full_content[idx..].to_string(),
            None => full_content,
        };

        let calls = raw_calls
            .iter()
            .map(|call| {
                let f = &call["function"];
                // Ollama may send arguments as a JSON string or as an object.
                let args = match f.get("arguments") {
                    Some(Value::String(s)) => serde_json::from_str(s).unwrap_or(Value::Null),
                    Some(v) => v.clone(),
                    None => Value::Null,
                };
                ToolCallReq {
                    id: String::new(),
                    name: f["name"].as_str().unwrap_or("").to_string(),
                    args,
                }
            })
            .collect();
        self.last_raw_calls = raw_calls;

        Ok(RoundReply {
            text,
            calls,
            malformed: None,
        })
    }

    fn push_user(&mut self, text: String) {
        self.messages
            .push(json!({ "role": "user", "content": text }));
    }

    fn push_assistant(&mut self, reply: &RoundReply) {
        self.messages.push(json!({
            "role": "assistant",
            "content": reply.text,
            "tool_calls": std::mem::take(&mut self.last_raw_calls),
        }));
    }

    fn transcript(&self) -> String {
        serde_json::to_string(&self.messages).unwrap_or_default()
    }

    fn push_tool_results(&mut self, results: Vec<ToolOutcome>) {
        for r in results {
            self.messages
                .push(json!({ "role": "tool", "content": r.model_text }));
        }
    }
}

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
        let turn = OllamaTurn::new(config, &tools, &request, system_prompt);
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
