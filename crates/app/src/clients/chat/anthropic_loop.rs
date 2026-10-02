//! Anthropic wire format: `/v1/messages` with native `tool_use` /
//! `tool_result` content blocks (non-streaming).

use super::*;

struct AnthropicTurn {
    url: String,
    api_key: String,
    config: LlmConfig,
    provider_label: String,
    client: reqwest::Client,
    system_prompt: String,
    tools: Vec<Value>,
    messages: Vec<Value>,
    /// The content blocks of the last reply, echoed back verbatim (thinking
    /// blocks included) in the assistant turn.
    last_blocks: Vec<Value>,
}

impl AnthropicTurn {
    fn new(
        config: LlmConfig,
        tools: &[agent_brain_protocol::ToolDefinition],
        request: &ChatRequest,
        system_prompt: String,
    ) -> Result<Self, String> {
        let api_key = config
            .api_key
            .clone()
            .ok_or_else(|| "Anthropic API key not set in LLM config.".to_string())?;
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com");
        let mut messages: Vec<Value> = request
            .history
            .iter()
            .map(|h| json!({ "role": h.role, "content": h.content }))
            .collect();
        messages.push(json!({ "role": "user", "content": request.message }));
        Ok(Self {
            url: format!("{base_url}/v1/messages"),
            api_key,
            provider_label: config.provider_label(),
            client: reqwest::Client::new(),
            system_prompt,
            tools: tools
                .iter()
                .map(|t| {
                    json!({
                        "name": t.name,
                        "description": t.description,
                        "input_schema": t.input_schema,
                    })
                })
                .collect(),
            messages,
            last_blocks: Vec::new(),
            config,
        })
    }
}

#[async_trait::async_trait]
impl ProviderTurn for AnthropicTurn {
    fn label(&self) -> &'static str {
        "Anthropic"
    }

    fn model(&self) -> &str {
        &self.config.model
    }

    async fn call(
        &mut self,
        svc: &ChatService,
        offer_tools: bool,
        _stream_to: Option<&mpsc::Sender<ChatEvent>>,
    ) -> Result<RoundReply, String> {
        let model = self.config.model.clone();
        let mut body = json!({
            "model": model,
            "max_tokens": 4096,
            "system": self.system_prompt,
            "tools": self.tools,
            "messages": self.messages,
        });
        // The wrap-up round keeps `tools` defined — the history holds tool_use
        // blocks — and forbids using them instead.
        if !offer_tools {
            body["tool_choice"] = json!({ "type": "none" });
        }

        // One provider concurrency slot for this request only
        // (services/provider_gate.rs), dropped before any tool runs.
        let gate_permit = crate::services::provider_gate::acquire(&self.provider_label).await;
        let call_start = std::time::Instant::now();
        let response = match self
            .client
            .post(&self.url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .header("content-type", "application/json")
            .json(&body)
            .timeout(self.config.timeout)
            .send()
            .await
        {
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
                return Err(format!("Anthropic request failed: {e}"));
            }
        };

        let resp_json: Value = response
            .json()
            .await
            .map_err(|e| format!("Failed to parse Anthropic response: {e}"))?;

        if resp_json["type"].as_str() == Some("error") {
            let msg = resp_json["error"]["message"]
                .as_str()
                .unwrap_or("Unknown Anthropic error")
                .to_string();
            let rate_limited = msg.contains("rate") || msg.contains("429");
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
                rate_limited.then_some("rate_limited"),
                Some(msg.as_str()),
            );
            return Err(msg);
        }

        drop(gate_permit);
        svc.record_llm_call(
            &self.provider_label,
            &model,
            true,
            call_start.elapsed().as_millis() as i64,
            resp_json["usage"]["input_tokens"].as_i64(),
            resp_json["usage"]["output_tokens"].as_i64(),
            None,
        );

        let blocks = resp_json["content"]
            .as_array()
            .cloned()
            .ok_or_else(|| "No content in Anthropic response".to_string())?;
        let end_turn = resp_json["stop_reason"].as_str() == Some("end_turn");

        let mut text = String::new();
        let mut calls = Vec::new();
        for block in &blocks {
            match block["type"].as_str() {
                Some("text") => text.push_str(block["text"].as_str().unwrap_or("")),
                // A tool_use block on an `end_turn` reply is not a request to
                // run anything.
                Some("tool_use") if !end_turn => calls.push(ToolCallReq {
                    id: block["id"].as_str().unwrap_or("").to_string(),
                    name: block["name"].as_str().unwrap_or("").to_string(),
                    args: block["input"].clone(),
                }),
                _ => {}
            }
        }
        self.last_blocks = blocks;

        Ok(RoundReply {
            text,
            calls,
            malformed: None,
        })
    }

    fn push_user(&mut self, text: String) {
        // Fold into a trailing user turn (the tool_result turn, on the wrap-up
        // round) rather than sending two user turns in a row.
        if let Some(last) = self.messages.last_mut()
            && last["role"] == "user"
        {
            let block = json!({ "type": "text", "text": text });
            match last["content"].take() {
                Value::Array(mut blocks) => {
                    blocks.push(block);
                    last["content"] = Value::Array(blocks);
                }
                Value::String(s) => {
                    last["content"] = json!([{ "type": "text", "text": s }, block]);
                }
                _ => last["content"] = json!([block]),
            }
            return;
        }
        self.messages
            .push(json!({ "role": "user", "content": text }));
    }

    fn push_assistant(&mut self, _reply: &RoundReply) {
        self.messages.push(json!({
            "role": "assistant",
            "content": std::mem::take(&mut self.last_blocks),
        }));
    }

    fn push_tool_results(&mut self, results: Vec<ToolOutcome>) {
        // Every tool_result goes back in ONE user turn.
        let blocks: Vec<Value> = results
            .into_iter()
            .map(|r| {
                json!({
                    "type": "tool_result",
                    "tool_use_id": r.id,
                    "content": r.model_text,
                    "is_error": !r.success,
                })
            })
            .collect();
        self.messages
            .push(json!({ "role": "user", "content": blocks }));
    }
}

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
        let turn = match AnthropicTurn::new(config, &tools, &request, system_prompt) {
            Ok(t) => t,
            Err(message) => {
                let _ = tx.send(ChatEvent::Error { message }).await;
                let _ = tx.send(ChatEvent::Done).await;
                return;
            }
        };
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

#[cfg(test)]
mod anthropic_turn_tests {
    use super::*;
    use crate::clients::chat::test_support::test_request;

    fn turn() -> AnthropicTurn {
        let config = LlmConfig {
            provider: LlmProviderType::Anthropic,
            api_key: Some("k".into()),
            ..Default::default()
        };
        AnthropicTurn::new(config, &[], &test_request(), "sys".into()).unwrap()
    }

    /// The wrap-up nudge follows a tool_result turn; it must join that turn as
    /// a text block, not open a second user turn in a row.
    #[test]
    fn a_nudge_after_tool_results_joins_the_same_user_turn() {
        let mut t = turn();
        t.last_blocks = vec![json!({"type": "tool_use", "id": "u1", "name": "x", "input": {}})];
        t.push_assistant(&RoundReply::default());
        t.push_tool_results(vec![ToolOutcome {
            id: "u1".into(),
            name: "x".into(),
            success: true,
            model_text: "result".into(),
        }]);
        t.push_user("wrap up".into());

        let roles: Vec<&str> = t
            .messages
            .iter()
            .filter_map(|m| m["role"].as_str())
            .collect();
        assert_eq!(roles, vec!["user", "assistant", "user"]);
        let last = t.messages.last().unwrap()["content"]
            .as_array()
            .unwrap()
            .clone();
        assert_eq!(last[0]["type"], "tool_result");
        assert_eq!(last[1], json!({"type": "text", "text": "wrap up"}));
    }

    #[test]
    fn a_missing_api_key_is_reported_not_sent() {
        let config = LlmConfig {
            provider: LlmProviderType::Anthropic,
            api_key: None,
            ..Default::default()
        };
        let err = AnthropicTurn::new(config, &[], &test_request(), "sys".into())
            .err()
            .unwrap();
        assert!(err.contains("API key"), "{err}");
    }
}
