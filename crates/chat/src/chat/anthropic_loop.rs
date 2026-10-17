//! Anthropic wire format: `/v1/messages` with native `tool_use` /
//! `tool_result` content blocks (non-streaming).

/// Appended to a reply the model stopped at its output limit.
pub const TRUNCATED_MARK: &str =
    "\n\n[TRUNCATED: the model reached its output limit (max_tokens); this answer is cut off.]";

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

// ---- Prompt caching -------------------------------------------------------
//
// Every round of a tool loop re-sends the system prompt, the tool schemas and
// the whole conversation so far. A 16-round Claude Sonnet sub-agent run on
// 2026-10-05 sent 903K input tokens for $2.03, most of it the same prefix
// again. Two cache breakpoints (Anthropic allows four) — the system prompt
// (tools render before it, so it caches the tool schemas too) and the last
// block of the latest message — let each round read everything before its
// newest turn from cache at a tenth of the price. Sonnet 5 caches nothing
// under a 1,024-token prefix; every tool loop here is well over that.
// Breakpoints are added to a copy of the history for the request body only, so
// they never pile up in the stored turns.

fn ephemeral() -> Value {
    json!({ "type": "ephemeral" })
}

fn cached_system(system: &str) -> Value {
    json!([{ "type": "text", "text": system, "cache_control": ephemeral() }])
}

fn cached_messages(messages: &[Value]) -> Value {
    let mut messages = messages.to_vec();
    if let Some(last) = messages.last_mut() {
        match last["content"].take() {
            Value::String(text) => {
                last["content"] =
                    json!([{ "type": "text", "text": text, "cache_control": ephemeral() }]);
            }
            Value::Array(mut blocks) => {
                if let Some(block) = blocks.last_mut() {
                    block["cache_control"] = ephemeral();
                }
                last["content"] = Value::Array(blocks);
            }
            other => last["content"] = other,
        }
    }
    Value::Array(messages)
}

/// Input tokens priced as the provider bills them, so the ledger's cost (tokens
/// × the catalog's input rate) and the spend cap stay right under caching: an
/// uncached token counts once, a cache write 1.25×, a cache read 0.1×.
fn billed_input_tokens(usage: &Value) -> Option<i64> {
    let input = usage["input_tokens"].as_i64()?;
    let write = usage["cache_creation_input_tokens"].as_i64().unwrap_or(0);
    let read = usage["cache_read_input_tokens"].as_i64().unwrap_or(0);
    Some(input + (write * 5 + 3) / 4 + (read + 9) / 10)
}

#[async_trait::async_trait]
impl ProviderTurn for AnthropicTurn {
    fn provider_name(&self) -> Option<String> {
        Some(self.provider_label.clone())
    }

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
            // The provider's ceiling, not the old 4096: thinking counts against
            // it, and on 2026-10-17 Haiku 5.5 thought, then ran out ~1K tokens
            // into a program review's JSON. A ceiling is not a charge.
            "max_tokens": agent_brain_llm::llm_providers::anthropic::DEFAULT_MAX_TOKENS
                .max(self.config.max_tokens.unwrap_or(0)),
            "system": cached_system(&self.system_prompt),
            "tools": self.tools,
            "messages": cached_messages(&self.messages),
        });
        // The wrap-up round keeps `tools` defined — the history holds tool_use
        // blocks — and forbids using them instead.
        if !offer_tools {
            body["tool_choice"] = json!({ "type": "none" });
        }

        // One provider concurrency slot for this request only
        // (services/provider_gate.rs), dropped before any tool runs.
        let gate_permit = agent_brain_llm::provider_gate::acquire(&self.provider_label).await;
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
            agent_brain_llm::budget::record_call(
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
        let usage = &resp_json["usage"];
        // INFO, not debug: the usage fields are the only proof caching works,
        // and a silent cache miss just makes every round cost full price.
        tracing::info!(
            model = %model,
            input = usage["input_tokens"].as_i64(),
            cache_write = usage["cache_creation_input_tokens"].as_i64(),
            cache_read = usage["cache_read_input_tokens"].as_i64(),
            "Anthropic token usage"
        );
        svc.record_llm_call(
            &self.provider_label,
            &model,
            true,
            call_start.elapsed().as_millis() as i64,
            billed_input_tokens(usage),
            usage["output_tokens"].as_i64(),
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
        // A reply cut off at the output limit says so, instead of reading as
        // a whole answer that happens to end mid-sentence.
        if resp_json["stop_reason"].as_str() == Some("max_tokens") {
            tracing::warn!(model = %model, "Anthropic reply hit max_tokens — marked truncated");
            text.push_str(TRUNCATED_MARK);
        }

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

    fn transcript(&self) -> String {
        format!(
            "{}\n{}",
            self.system_prompt,
            serde_json::to_string(&self.messages).unwrap_or_default()
        )
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
    use crate::chat::test_support::test_request;

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
    fn cache_breakpoints_go_on_the_system_prompt_and_the_last_block_only() {
        let system = cached_system("be brief");
        assert_eq!(system[0]["cache_control"]["type"], "ephemeral");

        let history = vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "x"}]}),
            json!({"role": "user", "content": [{"type": "tool_result", "tool_use_id": "u", "content": "r"}]}),
        ];
        let sent = cached_messages(&history);
        assert_eq!(sent[2]["content"][0]["cache_control"]["type"], "ephemeral");
        assert_eq!(sent[0]["content"], "first", "earlier turns are left alone");
        assert!(
            history[2]["content"][0].get("cache_control").is_none(),
            "stored history untouched"
        );

        let plain = cached_messages(&[json!({"role": "user", "content": "hi"})]);
        assert_eq!(plain[0]["content"][0]["text"], "hi");
        assert_eq!(plain[0]["content"][0]["cache_control"]["type"], "ephemeral");
    }

    #[test]
    fn cached_tokens_are_billed_at_their_own_rates() {
        let usage = json!({"input_tokens": 100, "cache_creation_input_tokens": 1000, "cache_read_input_tokens": 10000, "output_tokens": 5});
        assert_eq!(billed_input_tokens(&usage), Some(100 + 1250 + 1000));
        assert_eq!(billed_input_tokens(&json!({"input_tokens": 42})), Some(42));
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
