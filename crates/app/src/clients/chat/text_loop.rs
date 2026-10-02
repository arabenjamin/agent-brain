//! Text-protocol wire format for providers without native tool calls (Gemini):
//! the tool list goes in the system prompt and calls come back as
//! `<tool_call>{…}</tool_call>` tags, one per round.

use super::*;

struct TextTurn {
    llm: LlmClient,
    model: String,
    provider_label: String,
    messages: Vec<ChatMessage>,
    /// The model's whole reply from the last call, echoed back verbatim.
    last_raw: String,
    /// Prose the model wrote after its `<tool_call>` tag, returned to it with
    /// the tool result.
    after_call: String,
}

impl TextTurn {
    fn new(
        config: LlmConfig,
        tools: &[agent_brain_protocol::ToolDefinition],
        request: &ChatRequest,
        system_prompt: String,
    ) -> Result<Self, String> {
        let model = config.model.clone();
        let provider_label = config.provider_label();
        let llm = LlmClient::with_config(config)
            .map_err(|e| format!("Failed to create LLM client: {e}"))?;

        let tools_json: Vec<Value> = tools
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema }))
            .collect();
        let tools_str = serde_json::to_string(&tools_json).unwrap_or_else(|_| "[]".into());
        let system = format!(
            "{system_prompt}\n\nAvailable tools (JSON array):\n{tools_str}\n\n\
             To call a tool emit EXACTLY one tag per call — no markdown, no extra text around it:\n\
             <tool_call>{{\"tool\":\"TOOL_NAME\",\"args\":{{...}}}}</tool_call>\n\
             Use the key \"tool\" (not \"name\"). \
             You may call multiple tools in sequence — one <tool_call> block at a time. \
             When you have a final answer write it as plain text with no <tool_call> tag."
        );

        let mut messages = vec![ChatMessage::system(&system)];
        for h in &request.history {
            messages.push(ChatMessage {
                role: h.role.clone(),
                content: h.content.clone(),
            });
        }
        messages.push(ChatMessage::user(&request.message));
        Ok(Self {
            llm,
            model,
            provider_label,
            messages,
            last_raw: String::new(),
            after_call: String::new(),
        })
    }
}

#[async_trait::async_trait]
impl ProviderTurn for TextTurn {
    fn label(&self) -> &'static str {
        "Text"
    }

    fn model(&self) -> &str {
        &self.model
    }

    /// `offer_tools` has no wire form here: the tools are in the system prompt,
    /// and the wrap-up nudge is what tells the model to stop calling them.
    async fn call(
        &mut self,
        svc: &ChatService,
        _offer_tools: bool,
        _stream_to: Option<&mpsc::Sender<ChatEvent>>,
    ) -> Result<RoundReply, String> {
        // `LlmClient::chat` takes the provider gate itself.
        let call_start = std::time::Instant::now();
        let result = self.llm.chat(&self.messages).await;
        svc.record_llm_call(
            &self.provider_label,
            &self.model,
            result.is_ok(),
            call_start.elapsed().as_millis() as i64,
            result
                .as_ref()
                .ok()
                .and_then(|r| r.tokens_in)
                .map(i64::from),
            result
                .as_ref()
                .ok()
                .and_then(|r| r.tokens_out)
                .map(i64::from),
            result.as_ref().err().map(|e| e.to_string()).as_deref(),
        );
        let text = result
            .map_err(|e| format!("LLM error: {e}"))?
            .text
            .trim()
            .to_string();
        self.last_raw = text.clone();

        Ok(match extract_tool_call(&text) {
            ToolCallParse::Malformed {
                before,
                tool,
                error,
            } => RoundReply {
                text: before,
                calls: vec![],
                malformed: Some(MalformedCall { tool, error }),
            },
            ToolCallParse::Call(before, call, after) => {
                self.after_call = after.trim().to_string();
                RoundReply {
                    text: before.trim().to_string(),
                    calls: vec![ToolCallReq {
                        id: String::new(),
                        name: call["tool"].as_str().unwrap_or("").to_string(),
                        args: call["args"].clone(),
                    }],
                    malformed: None,
                }
            }
            ToolCallParse::None => RoundReply {
                text,
                ..Default::default()
            },
        })
    }

    fn push_user(&mut self, text: String) {
        self.messages.push(ChatMessage::user(text));
    }

    fn push_assistant(&mut self, _reply: &RoundReply) {
        self.messages
            .push(ChatMessage::assistant(std::mem::take(&mut self.last_raw)));
    }

    fn push_tool_results(&mut self, results: Vec<ToolOutcome>) {
        let after = std::mem::take(&mut self.after_call);
        for r in results {
            let msg = if after.is_empty() {
                format!("Tool `{}` result:\n{}", r.name, r.model_text)
            } else {
                format!("Tool `{}` result:\n{}\n\n{after}", r.name, r.model_text)
            };
            self.messages.push(ChatMessage::user(msg));
        }
    }
}

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
        let turn = match TextTurn::new(config, &tools, &request, system_prompt) {
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
