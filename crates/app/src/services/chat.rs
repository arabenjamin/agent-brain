//! Server-side agentic chat service.
//!
//! Provides a `/chat` SSE endpoint that runs the full LLM ↔ tool-use loop
//! server-side, streaming events back to the caller.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

use crate::mcp::tools::{ToolHandler, ToolRegistry};
use crate::services::context_builder::ContextBuilderService;
use crate::services::llm::{ChatMessage, LlmClient, LlmConfig, LlmProviderType};
use agent_brain_protocol::Content;

/// Maximum tool-use iterations per chat turn (prevents infinite loops).
const MAX_TOOL_ITERATIONS: usize = 10;

const DEFAULT_SYSTEM_PROMPT: &str = "\
You are agent-brain, an autonomous AI assistant backed by a persistent Neo4j \
knowledge graph. You can search notes, manage tasks, reason over stored \
knowledge, and use many other tools. Always think step-by-step before acting \
and use the available tools to give the most accurate, grounded answer possible.";

const CHAT_SYSTEM_PROMPT: &str = DEFAULT_SYSTEM_PROMPT;

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
    ToolResult {
        tool: String,
        success: bool,
        preview: String,
    },
    /// Streaming token chunk from the LLM (Ollama only).
    Token { content: String },
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
    /// Optional context profile name. When set and `tools` is empty/absent, the
    /// profile's tool allowlist and system prompt are applied automatically.
    pub context_profile: Option<String>,
    /// Research mode: after the tool-use loop, synthesize gathered findings with
    /// a stronger model. Accepted values: "gemini", "anthropic".
    pub synthesis_provider: Option<String>,
    /// Optional model override for synthesis (e.g. "gemini-2.5-flash").
    /// Falls back to the provider's default when absent.
    pub synthesis_model: Option<String>,
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
    /// Lazily-read: shares the same Arc as McpServerCore so profiles loaded after
    /// ChatService creation are immediately visible (no restart needed).
    context_builder: Arc<RwLock<Option<Arc<ContextBuilderService>>>>,
}

impl ChatService {
    /// Create a new `ChatService` backed by the server's live registries.
    pub fn new(
        tool_handler: Arc<RwLock<Option<ToolHandler>>>,
        tool_registry: Arc<RwLock<ToolRegistry>>,
        llm_config: Arc<RwLock<Option<LlmConfig>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            tool_handler,
            tool_registry,
            llm_config,
            context_builder: Arc::new(RwLock::new(None)),
        })
    }

    /// Create a `ChatService` sharing the context-builder Arc from `McpServerCore`.
    /// Profiles loaded by `build_skills()` are immediately visible without restart.
    pub fn with_context_builder(
        tool_handler: Arc<RwLock<Option<ToolHandler>>>,
        tool_registry: Arc<RwLock<ToolRegistry>>,
        llm_config: Arc<RwLock<Option<LlmConfig>>>,
        context_builder: Arc<RwLock<Option<Arc<ContextBuilderService>>>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            tool_handler,
            tool_registry,
            llm_config,
            context_builder,
        })
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

        // Apply context profile when set and no explicit tool allowlist is given.
        let has_explicit_tools = request
            .tools
            .as_ref()
            .map(|v| !v.is_empty())
            .unwrap_or(false);
        let cb_opt = self.context_builder.read().await.clone();
        let (tools, _profile_system_prompt, _profile_notes) = if has_explicit_tools {
            let names = request.tools.as_deref().unwrap_or_default();
            (filter_tools(all_tools, names), None, Vec::new())
        } else if let (Some(profile_name), Some(cb)) = (&request.context_profile, cb_opt) {
            if let Ok(bundle) = cb.build_bundle(profile_name).await {
                let filtered = filter_tools(all_tools, &bundle.profile.tools);
                let prompt = if bundle.profile.system_prompt.is_empty() {
                    None
                } else {
                    Some(bundle.profile.system_prompt.clone())
                };
                let notes = bundle.pre_loaded_notes.clone();
                (filtered, prompt, notes)
            } else {
                (all_tools, None, Vec::new())
            }
        } else {
            let names = request.tools.as_deref().unwrap_or_default();
            (filter_tools(all_tools, names), None, Vec::new())
        };

        let handler = self.tool_handler.read().await.clone();

        // Persist the user message to working memory before running the loop.
        if let (Some(sid), Some(h)) = (&session_id, &handler) {
            let _ = h
                .execute(
                    "push_context",
                    Some(json!({
                        "session_id": sid,
                        "content":    user_message,
                        "role":       "user"
                    })),
                )
                .await;
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

        // Emit a diagnostic context event so the client can see what configuration
        // was active for this turn (provider, profile, tool count, mode).
        {
            let provider_str = config
                .as_ref()
                .map(|c| format!("{:?}", c.provider).to_lowercase())
                .unwrap_or_else(|| "none".into());
            let mode = if let Some(p) = &request.synthesis_provider {
                format!("research → synthesize({})", p)
            } else {
                "direct".into()
            };
            let _ = inner_tx
                .send(ChatEvent::Thinking {
                    content: format!(
                        "⚙ provider={} | profile={} | tools={} | mode={}",
                        provider_str,
                        request.context_profile.as_deref().unwrap_or("none"),
                        tools.len(),
                        mode,
                    ),
                })
                .await;
        }

        match config {
            Some(cfg) if cfg.provider == LlmProviderType::Anthropic => {
                self.run_anthropic_loop(cfg, tools, handler.clone(), request, inner_tx)
                    .await;
            }
            Some(cfg) if cfg.provider == LlmProviderType::Ollama => {
                self.run_ollama_tool_loop(cfg, tools, handler.clone(), request, inner_tx)
                    .await;
            }
            Some(cfg) => {
                self.run_text_loop(cfg, tools, handler.clone(), request, inner_tx)
                    .await;
            }
            None => {
                let _ = inner_tx
                    .send(ChatEvent::Error {
                        message: "No LLM provider configured. Use `use_model` to set one.".into(),
                    })
                    .await;
                let _ = inner_tx.send(ChatEvent::Done).await;
            }
        }
        // inner_tx is dropped here, which closes inner_rx and lets the
        // forwarding task finish.

        // Wait for the forwarder to return the captured assistant text.
        let final_text = result_rx.recv().await.unwrap_or_default();

        // Persist the assistant response to working memory.
        if let (Some(sid), Some(h)) = (&session_id, &handler)
            && !final_text.is_empty()
        {
            let _ = h
                .execute(
                    "push_context",
                    Some(json!({
                        "session_id": sid,
                        "content":    final_text,
                        "role":       "assistant"
                    })),
                )
                .await;
        }
    }

    // ========================================================================
    // Anthropic native tool-use loop
    // ========================================================================

    async fn run_anthropic_loop(
        &self,
        config: LlmConfig,
        tools: Vec<agent_brain_protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        tx: mpsc::Sender<ChatEvent>,
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
                let _ = tx.send(ChatEvent::Error { message: msg }).await;
                let _ = tx.send(ChatEvent::Done).await;
                return;
            }

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

                let (success, result_text) = if let Some(ref h) = handler {
                    let args = if tool_input.is_object() {
                        Some(tool_input)
                    } else {
                        None
                    };
                    let result = h.execute(&tool_name, args).await;
                    let is_err = result.is_error.unwrap_or(false);
                    let text = result
                        .content
                        .iter()
                        .filter_map(|c| {
                            if let Content::Text { text } = c {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!is_err, text)
                } else {
                    (false, "No tool handler available".to_string())
                };

                let preview: String = result_text.chars().take(4000).collect();
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: tool_name.clone(),
                        success,
                        preview,
                    })
                    .await;

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
        tools: Vec<agent_brain_protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        tx: mpsc::Sender<ChatEvent>,
    ) {
        let do_synthesis = request.synthesis_provider.is_some();
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or("http://localhost:11434");
        let model = config.model.clone();

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
        let mut messages: Vec<Value> =
            vec![json!({ "role": "system", "content": CHAT_SYSTEM_PROMPT })];
        for h in &request.history {
            messages.push(json!({ "role": h.role, "content": h.content }));
        }
        messages.push(json!({ "role": "user", "content": request.message }));

        let client = reqwest::Client::new();
        let mut weak_model_answer = String::new();

        for _iteration in 0..MAX_TOOL_ITERATIONS {
            let body = json!({
                "model": model,
                "messages": messages,
                "tools": ollama_tools,
                "stream": true,
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
                        let _ = tx.send(ChatEvent::Token { content: token }).await;
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
                        break 'stream;
                    }
                }
            }

            let content = full_content;

            if tool_calls.is_empty() {
                // No tool calls — weak model has a final answer.
                if !content.is_empty() {
                    if do_synthesis {
                        // Surface weak model's answer as a thinking event so the
                        // user can see what was researched before synthesis.
                        weak_model_answer = content.clone();
                        let _ = tx.send(ChatEvent::Thinking { content }).await;
                    } else {
                        let _ = tx.send(ChatEvent::Message { content }).await;
                    }
                }
                break;
            }

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

                let (success, result_text) = if let Some(ref h) = handler {
                    let args = if tool_args.is_object() {
                        Some(tool_args)
                    } else {
                        None
                    };
                    let result = h.execute(&tool_name, args).await;
                    let is_err = result.is_error.unwrap_or(false);
                    let text = result
                        .content
                        .iter()
                        .filter_map(|c| {
                            if let Content::Text { text } = c {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!is_err, text)
                } else {
                    (false, "No tool handler available".to_string())
                };

                let preview: String = result_text.chars().take(4000).collect();
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: tool_name.clone(),
                        success,
                        preview,
                    })
                    .await;

                // Append the tool result as a tool message.
                messages.push(json!({
                    "role": "tool",
                    "content": result_text,
                }));
            }
        }

        // Research mode: synthesize gathered findings with a stronger model.
        if do_synthesis {
            self.run_synthesis(&request, &messages, &weak_model_answer, tx.clone())
                .await;
        }

        let _ = tx.send(ChatEvent::Done).await;
    }

    // ========================================================================
    // Synthesis step — called after the tool-use loop in research mode
    // ========================================================================

    async fn run_synthesis(
        &self,
        request: &ChatRequest,
        conversation: &[Value],
        weak_answer: &str,
        tx: mpsc::Sender<ChatEvent>,
    ) {
        let provider_str = match &request.synthesis_provider {
            Some(p) => p.to_lowercase(),
            None => return,
        };

        // Read the current live config so we can fall back to its API key if the
        // env var isn't set (e.g. key was supplied via use_model, not via env).
        let live_config = self.llm_config.read().await.clone();

        let (provider, default_model, api_key) = match provider_str.as_str() {
            "gemini" => {
                let key = std::env::var("GEMINI_API_KEY")
                    .ok()
                    .filter(|k| !k.is_empty())
                    .or_else(|| {
                        live_config
                            .as_ref()
                            .filter(|c| c.provider == LlmProviderType::Gemini)
                            .and_then(|c| c.api_key.clone())
                            .filter(|k| !k.is_empty())
                    });
                (
                    LlmProviderType::Gemini,
                    std::env::var("GEMINI_MODEL").unwrap_or_else(|_| "gemini-2.5-flash".into()),
                    key,
                )
            }
            "anthropic" | "claude" => {
                let key = std::env::var("ANTHROPIC_API_KEY")
                    .ok()
                    .filter(|k| !k.is_empty())
                    .or_else(|| {
                        live_config
                            .as_ref()
                            .filter(|c| c.provider == LlmProviderType::Anthropic)
                            .and_then(|c| c.api_key.clone())
                            .filter(|k| !k.is_empty())
                    });
                (
                    LlmProviderType::Anthropic,
                    "claude-haiku-4-5-20251001".to_string(),
                    key,
                )
            }
            other => {
                let _ = tx
                    .send(ChatEvent::Error {
                        message: format!(
                            "Unknown synthesis provider: {other}. Use 'gemini' or 'anthropic'."
                        ),
                    })
                    .await;
                return;
            }
        };

        let model = request.synthesis_model.clone().unwrap_or(default_model);

        let synth_config = LlmConfig {
            provider,
            model: model.clone(),
            api_key,
            base_url: None,
            temperature: 0.7,
            timeout: Duration::from_secs(120),
            ..LlmConfig::default()
        };

        let llm = match LlmClient::with_config(synth_config) {
            Ok(c) => c,
            Err(e) => {
                let _ = tx
                    .send(ChatEvent::Error {
                        message: format!("Failed to initialize synthesis model ({model}): {e}"),
                    })
                    .await;
                return;
            }
        };

        // Collect tool results from the conversation history.
        let mut tool_results: Vec<String> = Vec::new();
        for msg in conversation {
            if msg["role"].as_str() == Some("tool")
                && let Some(content) = msg["content"].as_str()
            {
                let trimmed = content.trim();
                if !trimmed.is_empty() {
                    tool_results.push(trimmed.to_string());
                }
            }
        }

        let research_block = if tool_results.is_empty() {
            let _ = tx
                .send(ChatEvent::Thinking {
                    content: format!(
                        "⚠ Research phase called 0 tools — the local model did not invoke any \
                     tool. Synthesis will proceed with the model's direct answer only \
                     (weak_answer len={}).",
                        weak_answer.len()
                    ),
                })
                .await;
            "(no tool results gathered)".to_string()
        } else {
            let _ = tx
                .send(ChatEvent::Thinking {
                    content: format!(
                        "Synthesizing {} tool result(s) with {model}…",
                        tool_results.len()
                    ),
                })
                .await;
            tool_results.join("\n\n---\n\n")
        };

        let synthesis_prompt = format!(
            "You are a research synthesizer. An AI research agent gathered the following \
             information using multiple tools to answer a question. \
             Your job is to synthesize all gathered material into a comprehensive, \
             well-structured, and clearly written response.\n\n\
             Original question: {question}\n\n\
             Research gathered:\n{research}\n\n\
             {analysis}\
             Please synthesize the above into a clear, informative, and complete response \
             to the original question.",
            question = request.message,
            research = research_block,
            analysis = if !weak_answer.is_empty() {
                format!("Initial analysis from research agent:\n{weak_answer}\n\n")
            } else {
                String::new()
            },
        );

        // (synthesis-start thinking event already emitted in the research_block block above)

        let messages = vec![ChatMessage::user(&synthesis_prompt)];
        match llm.chat(&messages).await {
            Ok(response) if !response.text.is_empty() => {
                let _ = tx
                    .send(ChatEvent::Message {
                        content: response.text,
                    })
                    .await;
            }
            Ok(_) => {}
            Err(e) => {
                let _ = tx
                    .send(ChatEvent::Error {
                        message: format!("Synthesis failed: {e}"),
                    })
                    .await;
            }
        }
    }

    // ========================================================================
    // Text-based loop (Gemini fallback)
    // ========================================================================

    async fn run_text_loop(
        &self,
        config: LlmConfig,
        tools: Vec<agent_brain_protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        tx: mpsc::Sender<ChatEvent>,
    ) {
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
            if let Some((before, call, after)) = extract_tool_call(&text) {
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

                let (success, result_text) = if let Some(ref h) = handler {
                    let args = if tool_args.is_object() {
                        Some(tool_args)
                    } else {
                        None
                    };
                    let result = h.execute(&tool_name, args).await;
                    let is_err = result.is_error.unwrap_or(false);
                    let text = result
                        .content
                        .iter()
                        .filter_map(|c| {
                            if let Content::Text { text } = c {
                                Some(text.as_str())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                        .join("\n");
                    (!is_err, text)
                } else {
                    (false, "No tool handler available".to_string())
                };

                let preview: String = result_text.chars().take(4000).collect();
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: tool_name.clone(),
                        success,
                        preview,
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

/// Filter a tool list to only those whose names appear in `names`.
/// Returns `all` unchanged if `names` is empty.
fn filter_tools(
    all: Vec<crate::mcp::protocol::ToolDefinition>,
    names: &[String],
) -> Vec<crate::mcp::protocol::ToolDefinition> {
    if names.is_empty() {
        return all;
    }
    let allowed: std::collections::HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    all.into_iter()
        .filter(|t| allowed.contains(t.name.as_str()))
        .collect()
}

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
            if v.get("tool").is_none()
                && let Some(name) = v["name"].as_str().map(|s| s.to_string())
                && let Some(obj) = v.as_object_mut()
            {
                obj.insert("tool".to_string(), Value::String(name));
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
