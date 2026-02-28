//! Server-side agentic chat service.
//!
//! Provides a `/chat` SSE endpoint that runs the full LLM ↔ tool-use loop
//! server-side, streaming events back to the caller.

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

use crate::mcp::protocol::Content;
use crate::mcp::tools::{ToolHandler, ToolRegistry};
use crate::services::llm::{ChatMessage, LlmClient, LlmConfig, LlmProviderType};

/// Maximum tool-use iterations per chat turn (prevents infinite loops).
const MAX_TOOL_ITERATIONS: usize = 10;

/// System prompt injected into every chat session.
const CHAT_SYSTEM_PROMPT: &str = "\
You are agent-brain, an autonomous AI assistant backed by a persistent Neo4j \
knowledge graph. You can search notes, manage tasks, execute HTTP requests, \
reason over stored knowledge, and use many other tools. \
Always think step-by-step before acting and use the available tools to give \
the most accurate, grounded answer possible.";

// ============================================================================
// Public types
// ============================================================================

/// An event emitted on the `/chat` SSE stream.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ChatEvent {
    /// LLM is reasoning (text block before a tool call).
    Thinking { content: String },
    /// LLM decided to call a tool.
    ToolCall { tool: String, args: Value },
    /// Tool execution finished.
    ToolResult { tool: String, success: bool, preview: String },
    /// Final assistant message (no more tool calls).
    Message { content: String },
    /// An error occurred.
    Error { message: String },
    /// Stream complete.
    Done,
}

/// A single message in the chat history.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatHistoryMessage {
    /// `"user"` or `"assistant"`.
    pub role: String,
    pub content: String,
}

/// Request body for `POST /chat`.
#[derive(Debug, Deserialize)]
pub struct ChatRequest {
    /// The new user message.
    pub message: String,
    /// Optional prior conversation turns.
    #[serde(default)]
    pub history: Vec<ChatHistoryMessage>,
    /// Optional session identifier (stored in working memory if provided).
    pub session_id: Option<String>,
    /// Optional allowlist of tool names. When empty or absent, all tools are available.
    pub tools: Option<Vec<String>>,
}

// ============================================================================
// ChatService
// ============================================================================

/// Server-side agentic chat service.
///
/// Holds shared references into the running `McpServerCore` so that tool
/// execution and LLM provider switches are reflected immediately without
/// restarting the server.
pub struct ChatService {
    tool_handler: Arc<RwLock<Option<ToolHandler>>>,
    tool_registry: Arc<RwLock<ToolRegistry>>,
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
}

impl ChatService {
    /// Create a new `ChatService` backed by the server's live registries.
    pub fn new(
        tool_handler: Arc<RwLock<Option<ToolHandler>>>,
        tool_registry: Arc<RwLock<ToolRegistry>>,
        llm_config: Arc<RwLock<Option<LlmConfig>>>,
    ) -> Arc<Self> {
        Arc::new(Self { tool_handler, tool_registry, llm_config })
    }

    /// Run the agentic loop for a chat request, emitting events on `tx`.
    ///
    /// When `request.session_id` is set the user message and the final
    /// assistant response are persisted to Neo4j working memory automatically.
    pub async fn run(&self, request: ChatRequest, tx: mpsc::Sender<ChatEvent>) {
        let config = self.llm_config.read().await.clone();
        let all_tools = self.tool_registry.read().await.list();
        let session_id = request.session_id.clone();
        let user_message = request.message.clone();

        // Apply per-request tool allowlist if provided.
        let tools = match &request.tools {
            Some(names) if !names.is_empty() => {
                let name_set: std::collections::HashSet<&str> =
                    names.iter().map(|s| s.as_str()).collect();
                all_tools.into_iter().filter(|t| name_set.contains(t.name.as_str())).collect()
            }
            _ => all_tools,
        };

        let handler = self.tool_handler.read().await.clone();

        // Persist the user message to working memory before running the loop.
        if let (Some(sid), Some(h)) = (&session_id, &handler) {
            let _ = h.execute("push_context", Some(json!({
                "session_id": sid,
                "content":    user_message,
                "role":       "user"
            }))).await;
        }

        // Use an inner channel so we can intercept the final Message event and
        // save the assistant response to working memory without changing the
        // loop functions.
        let (inner_tx, mut inner_rx) = mpsc::channel::<ChatEvent>(128);
        let (result_tx, mut result_rx) = mpsc::channel::<String>(1);

        // Forwarding task: relay every event to the caller; capture final text.
        tokio::spawn(async move {
            let mut final_text = String::new();
            while let Some(event) = inner_rx.recv().await {
                if let ChatEvent::Message { content } = &event {
                    final_text = content.clone();
                }
                let _ = tx.send(event).await;
            }
            let _ = result_tx.send(final_text).await;
        });

        match config {
            Some(cfg) if cfg.provider == LlmProviderType::Anthropic => {
                self.run_anthropic_loop(cfg, tools, handler.clone(), request, inner_tx).await;
            }
            Some(cfg) if cfg.provider == LlmProviderType::Ollama => {
                self.run_ollama_tool_loop(cfg, tools, handler.clone(), request, inner_tx).await;
            }
            Some(cfg) => {
                self.run_text_loop(cfg, tools, handler.clone(), request, inner_tx).await;
            }
            None => {
                let _ = inner_tx.send(ChatEvent::Error {
                    message: "No LLM provider configured. Use `use_model` to set one.".into(),
                }).await;
                let _ = inner_tx.send(ChatEvent::Done).await;
            }
        }
        // inner_tx is dropped here, which closes inner_rx and lets the
        // forwarding task finish.

        // Wait for the forwarder to return the captured assistant text.
        let final_text = result_rx.recv().await.unwrap_or_default();

        // Persist the assistant response to working memory.
        if let (Some(sid), Some(h)) = (&session_id, &handler) {
            if !final_text.is_empty() {
                let _ = h.execute("push_context", Some(json!({
                    "session_id": sid,
                    "content":    final_text,
                    "role":       "assistant"
                }))).await;
            }
        }
    }

    // ========================================================================
    // Anthropic native tool-use loop
    // ========================================================================

    async fn run_anthropic_loop(
        &self,
        config: LlmConfig,
        tools: Vec<crate::mcp::protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        tx: mpsc::Sender<ChatEvent>,
    ) {
        let api_key = match &config.api_key {
            Some(k) => k.clone(),
            None => {
                let _ = tx.send(ChatEvent::Error {
                    message: "Anthropic API key not set in LLM config.".into(),
                }).await;
                let _ = tx.send(ChatEvent::Done).await;
                return;
            }
        };

        let model = config.model.clone();

        // Build Anthropic tools array.
        let anthropic_tools: Vec<Value> = tools
            .iter()
            .map(|t| json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.input_schema,
            }))
            .collect();

        // Build initial messages array.
        let mut messages: Vec<Value> = request
            .history
            .iter()
            .map(|h| json!({ "role": h.role, "content": h.content }))
            .collect();
        messages.push(json!({ "role": "user", "content": request.message }));

        let client = reqwest::Client::new();
        let base_url = config.base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com");

        for _iteration in 0..MAX_TOOL_ITERATIONS {
            let body = json!({
                "model": model,
                "max_tokens": 4096,
                "system": CHAT_SYSTEM_PROMPT,
                "tools": anthropic_tools,
                "messages": messages,
            });

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
                    let _ = tx.send(ChatEvent::Error { message: format!("Anthropic request failed: {e}") }).await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            let resp_json: Value = match response.json().await {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(ChatEvent::Error { message: format!("Failed to parse Anthropic response: {e}") }).await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            // Check for API-level error.
            if let Some(err_type) = resp_json.get("type").and_then(|v| v.as_str()) {
                if err_type == "error" {
                    let msg = resp_json["error"]["message"]
                        .as_str()
                        .unwrap_or("Unknown Anthropic error")
                        .to_string();
                    let _ = tx.send(ChatEvent::Error { message: msg }).await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            }

            let stop_reason = resp_json["stop_reason"].as_str().unwrap_or("").to_string();
            let content_blocks = match resp_json["content"].as_array() {
                Some(arr) => arr.clone(),
                None => {
                    let _ = tx.send(ChatEvent::Error { message: "No content in Anthropic response".into() }).await;
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
                // Emit the final message.
                if !final_text.is_empty() {
                    let _ = tx.send(ChatEvent::Message { content: final_text }).await;
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

                let _ = tx.send(ChatEvent::ToolCall {
                    tool: tool_name.clone(),
                    args: tool_input.clone(),
                }).await;

                let (success, result_text) = if let Some(ref h) = handler {
                    let args = if tool_input.is_object() { Some(tool_input) } else { None };
                    let result = h.execute(&tool_name, args).await;
                    let is_err = result.is_error.unwrap_or(false);
                    let text = result.content.iter()
                        .filter_map(|c| if let Content::Text { text } = c { Some(text.as_str()) } else { None })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!is_err, text)
                } else {
                    (false, "No tool handler available".to_string())
                };

                let preview: String = result_text.chars().take(200).collect();
                let _ = tx.send(ChatEvent::ToolResult {
                    tool: tool_name.clone(),
                    success,
                    preview,
                }).await;

                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_id,
                    "content": result_text,
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

    async fn run_ollama_tool_loop(
        &self,
        config: LlmConfig,
        tools: Vec<crate::mcp::protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        tx: mpsc::Sender<ChatEvent>,
    ) {
        let base_url = config.base_url
            .as_deref()
            .unwrap_or("http://localhost:11434");
        let model = config.model.clone();

        // Build the Ollama tools array (OpenAI function-calling schema).
        let ollama_tools: Vec<Value> = tools
            .iter()
            .map(|t| json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.input_schema,
                }
            }))
            .collect();

        // Build the initial messages list.
        let mut messages: Vec<Value> = vec![
            json!({ "role": "system", "content": CHAT_SYSTEM_PROMPT }),
        ];
        for h in &request.history {
            messages.push(json!({ "role": h.role, "content": h.content }));
        }
        messages.push(json!({ "role": "user", "content": request.message }));

        let client = reqwest::Client::new();

        for _iteration in 0..MAX_TOOL_ITERATIONS {
            let body = json!({
                "model": model,
                "messages": messages,
                "tools": ollama_tools,
                "stream": false,
                "options": {
                    "temperature": config.temperature,
                }
            });

            let response = match client
                .post(format!("{}/api/chat", base_url))
                .header("content-type", "application/json")
                .json(&body)
                .timeout(config.timeout)
                .send()
                .await
            {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(ChatEvent::Error {
                        message: format!("Ollama request failed: {e}"),
                    }).await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            let resp_json: Value = match response.json().await {
                Ok(v) => v,
                Err(e) => {
                    let _ = tx.send(ChatEvent::Error {
                        message: format!("Failed to parse Ollama response: {e}"),
                    }).await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            // Surface Ollama-level errors (e.g. model not found).
            if let Some(err) = resp_json.get("error").and_then(|v| v.as_str()) {
                let _ = tx.send(ChatEvent::Error {
                    message: format!("Ollama error: {err}"),
                }).await;
                let _ = tx.send(ChatEvent::Done).await;
                return;
            }

            // Ollama wraps the assistant turn in `message`.
            let msg = &resp_json["message"];
            let content = msg["content"].as_str().unwrap_or("").to_string();
            let tool_calls = msg["tool_calls"].as_array().cloned().unwrap_or_default();

            if tool_calls.is_empty() {
                // No tool calls — this is the final answer.
                if !content.is_empty() {
                    let _ = tx.send(ChatEvent::Message { content }).await;
                }
                break;
            }

            // Emit thinking text that accompanied the tool calls (if any).
            if !content.trim().is_empty() {
                let _ = tx.send(ChatEvent::Thinking { content: content.clone() }).await;
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
                    Some(Value::String(s)) => {
                        serde_json::from_str(s).unwrap_or(Value::Null)
                    }
                    Some(v) => v.clone(),
                    None => Value::Null,
                };

                let _ = tx.send(ChatEvent::ToolCall {
                    tool: tool_name.clone(),
                    args: tool_args.clone(),
                }).await;

                let (success, result_text) = if let Some(ref h) = handler {
                    let args = if tool_args.is_object() { Some(tool_args) } else { None };
                    let result = h.execute(&tool_name, args).await;
                    let is_err = result.is_error.unwrap_or(false);
                    let text = result.content.iter()
                        .filter_map(|c| if let Content::Text { text } = c { Some(text.as_str()) } else { None })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!is_err, text)
                } else {
                    (false, "No tool handler available".to_string())
                };

                let preview: String = result_text.chars().take(200).collect();
                let _ = tx.send(ChatEvent::ToolResult {
                    tool: tool_name.clone(),
                    success,
                    preview,
                }).await;

                // Append the tool result as a tool message.
                messages.push(json!({
                    "role": "tool",
                    "content": result_text,
                }));
            }
        }

        let _ = tx.send(ChatEvent::Done).await;
    }

    // ========================================================================
    // Text-based loop (Gemini fallback)
    // ========================================================================

    async fn run_text_loop(
        &self,
        config: LlmConfig,
        tools: Vec<crate::mcp::protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        tx: mpsc::Sender<ChatEvent>,
    ) {
        let llm = match LlmClient::with_config(config) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx.send(ChatEvent::Error { message: format!("Failed to create LLM client: {e}") }).await;
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
            CHAT_SYSTEM_PROMPT, tools_str
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

        for _iteration in 0..MAX_TOOL_ITERATIONS {
            let response = match llm.chat(&messages).await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx.send(ChatEvent::Error { message: format!("LLM error: {e}") }).await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            let text = response.text.trim().to_string();

            // Check for tool calls.
            if let Some((before, call, after)) = extract_tool_call(&text) {
                // Emit thinking text before the tool call.
                let thinking = before.trim().to_string();
                if !thinking.is_empty() {
                    let _ = tx.send(ChatEvent::Thinking { content: thinking }).await;
                }

                // Emit the tool call.
                let tool_name = call["tool"].as_str().unwrap_or("").to_string();
                let tool_args = call["args"].clone();

                let _ = tx.send(ChatEvent::ToolCall {
                    tool: tool_name.clone(),
                    args: tool_args.clone(),
                }).await;

                let (success, result_text) = if let Some(ref h) = handler {
                    let args = if tool_args.is_object() { Some(tool_args) } else { None };
                    let result = h.execute(&tool_name, args).await;
                    let is_err = result.is_error.unwrap_or(false);
                    let text = result.content.iter()
                        .filter_map(|c| if let Content::Text { text } = c { Some(text.as_str()) } else { None })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!is_err, text)
                } else {
                    (false, "No tool handler available".to_string())
                };

                let preview: String = result_text.chars().take(200).collect();
                let _ = tx.send(ChatEvent::ToolResult {
                    tool: tool_name.clone(),
                    success,
                    preview,
                }).await;

                // Append assistant response and tool result to history.
                messages.push(ChatMessage::assistant(&text));

                let tool_result_msg = if !after.trim().is_empty() {
                    format!("Tool `{}` result:\n{}\n\n{}", tool_name, result_text, after.trim())
                } else {
                    format!("Tool `{}` result:\n{}", tool_name, result_text)
                };
                messages.push(ChatMessage::user(tool_result_msg));
            } else {
                // No tool call — this is the final response.
                debug!(text = %text, "Chat: final text response");
                if !text.is_empty() {
                    let _ = tx.send(ChatEvent::Message { content: text }).await;
                }
                break;
            }
        }

        let _ = tx.send(ChatEvent::Done).await;
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Extract the first `<tool_call>...</tool_call>` block from a text.
///
/// Returns `(before, parsed_json, after)` if found, or `None` if not found.
fn extract_tool_call(text: &str) -> Option<(String, Value, String)> {
    let open = "<tool_call>";
    let close = "</tool_call>";

    let start = text.find(open)?;
    let end = text.find(close)?;
    if end < start {
        return None;
    }

    let before = text[..start].to_string();
    let json_str = &text[start + open.len()..end];
    let after = text[end + close.len()..].to_string();

    match serde_json::from_str::<Value>(json_str) {
        Ok(mut v) => {
            // Normalise: some models (e.g. Gemini) emit {"name":"...","args":{}}
            // instead of {"tool":"...","args":{}}.  Accept both.
            if v.get("tool").is_none() {
                if let Some(name) = v["name"].as_str().map(|s| s.to_string()) {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("tool".to_string(), Value::String(name));
                    }
                }
            }
            if v.get("tool").is_some() {
                Some((before, v, after))
            } else {
                warn!("Tool call JSON missing 'tool'/'name' key: {}", json_str);
                None
            }
        }
        Err(e) => {
            warn!("Failed to parse tool_call JSON: {} — {}", json_str, e);
            None
        }
    }
}
