//! Chat client adapter.
//!
//! Runs the full LLM ↔ tool-use loop for human-facing chat sessions,
//! streaming [`ChatEvent`]s back to the caller via an SSE endpoint.
//!
//! This is a **client adapter** — it drives a conversational LLM on behalf
//! of a human user and calls into the brain's tool registry to act on the
//! world.  It is intentionally separate from the brain's internal services
//! (`services/`) which use LLMs as a cognitive substrate rather than as a
//! conversational interface.

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

/// Maximum characters of a tool result fed back to the LLM.
/// Prevents context-window overflow (OllamaCloud/Ollama models often have 4K–32K token limits).
/// The display preview uses the same cap so the UI stays consistent.
const MAX_TOOL_RESULT_CHARS: usize = 6000;

/// For OllamaCloud/Ollama streaming loops, cap tool results at this smaller limit.
/// Tool schemas (~9K chars) are resent every round; combined with cumulative tool results
/// the context grows quickly and causes 500s from smaller cloud models.
const CLOUD_TOOL_RESULT_CHARS: usize = 2000;

/// Maximum number of non-system messages kept in the OllamaCloud/Ollama message history.
/// Older messages are dropped (keeping system + user + last N) to prevent context overflow.
const MAX_HISTORY_MESSAGES: usize = 10;

const DEFAULT_SYSTEM_PROMPT_TEMPLATE: &str = "\
You are agent-brain, an autonomous AI assistant backed by a persistent Neo4j \
knowledge graph. Always think step-by-step before acting and use the available \
tools to give the most accurate, grounded answer possible.\n\
Today's date is {DATE}. When searching for recent content, always include the \
current date in your queries (e.g. \"daily news brief {DATE}\").\n\
CRITICAL — interactive chat rules:\n\
1. Always deliver the actual result. Never describe what you are about to do \
   or what you have queued — the user is waiting for the answer RIGHT NOW.\n\
2. Never use enqueue_jobs or manage_scheduled_task in chat. Background jobs run \
   asynchronously and their output will NEVER appear here. Do the work inline: \
   use search_web to fetch data, reason to synthesize it, store_note to save it, \
   then present the result to the user directly.\n\
3. If asked for a news brief that is not in the graph, search_web for current \
   headlines, synthesize a brief with reason, store_note it, then show it here.\n\
Key tools: `search_web` (fetch current info), `search_notes` (knowledge graph), \
`store_note` (save), `reason` (synthesize), `create_task` / `list_tasks` (tasks). \
Only call tools that exist — do not invent tool names. \
Never output XML tags like <invoke> — use only the provided function-call tools.";

fn build_system_prompt() -> String {
    let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
    DEFAULT_SYSTEM_PROMPT_TEMPLATE.replace("{DATE}", &date)
}

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

        // Resolve the effective profile name: use the one from the request, or
        // auto-assign based on the message content when none is provided.
        let resolved_profile: Option<String> = if has_explicit_tools {
            None
        } else if request.context_profile.is_some() {
            request.context_profile.clone()
        } else if let Some(ref cb) = cb_opt {
            let assigned = cb.auto_assign(&request.message).await;
            Some(assigned)
        } else {
            None
        };

        let (tools, _profile_system_prompt, _profile_notes) = if has_explicit_tools {
            let names = request.tools.as_deref().unwrap_or_default();
            (filter_tools(all_tools, names), None, Vec::new())
        } else if let (Some(profile_name), Some(ref cb)) = (&resolved_profile, cb_opt) {
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
            (all_tools, None, Vec::new())
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
                .map(|c| c.provider.to_string())
                .unwrap_or_else(|| "none".into());
            let mode = if let Some(p) = &request.synthesis_provider {
                format!("research → synthesize({})", p)
            } else {
                "direct".into()
            };
            let _ = inner_tx
                .send(ChatEvent::Thinking {
                    content: format!(
                        "⚙ provider={} | model={} | profile={} | tools={} | mode={}",
                        provider_str,
                        config
                            .as_ref()
                            .map(|c| c.model.as_str())
                            .unwrap_or("unknown"),
                        resolved_profile.as_deref().unwrap_or("general"),
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
            Some(cfg) if cfg.provider == LlmProviderType::OllamaCloud => {
                self.run_ollama_cloud_loop(cfg, tools, handler.clone(), request, inner_tx)
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

        // Store an episodic note summarising this chat turn so the brain builds
        // a first-person record of conversations it has had.
        if let Some(h) = &handler
            && !final_text.is_empty()
        {
            let user_snippet: String = user_message.chars().take(300).collect();
            let response_snippet: String = final_text.chars().take(200).collect();
            let profile = resolved_profile.as_deref().unwrap_or("general");
            let session_tag = session_id
                .as_deref()
                .map(|s| format!(" [session: {s}]"))
                .unwrap_or_default();
            let note = format!(
                "Chat turn{session_tag} — profile: {profile}\n\
                 User: {user_snippet}\n\
                 Response: {response_snippet}"
            );
            let _ = h
                .execute(
                    "store_note",
                    Some(json!({
                        "content": note,
                        "note_type": "episodic",
                        "source_context": "chat_session"
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
                "system": build_system_prompt(),
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

                let preview: String = result_text.chars().take(MAX_TOOL_RESULT_CHARS).collect();
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: tool_name.clone(),
                        success,
                        preview: preview.clone(),
                    })
                    .await;

                tool_results.push(json!({
                    "type": "tool_result",
                    "tool_use_id": tool_id,
                    "content": preview,
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
            vec![json!({ "role": "system", "content": build_system_prompt() })];
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

            let mut req = client
                .post(format!("{}/api/chat", base_url))
                .header("content-type", "application/json")
                .timeout(config.timeout)
                .json(&body);
            if let Some(ref key) = config.api_key {
                req = req.header("Authorization", format!("Bearer {}", key));
            }
            let response = match req.send().await {
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
                        break 'stream;
                    }
                }
            }

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

                let preview: String = result_text.chars().take(MAX_TOOL_RESULT_CHARS).collect();
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: tool_name.clone(),
                        success,
                        preview: preview.clone(),
                    })
                    .await;

                // Append the tool result as a tool message (truncated to avoid context overflow).
                messages.push(json!({
                    "role": "tool",
                    "content": preview,
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
    // Ollama Cloud tool-use loop (OpenAI-compatible SSE streaming)
    // ========================================================================
    //
    // Ollama Cloud at https://ollama.com uses the OpenAI-compatible API:
    //   POST /v1/chat/completions  (not /api/chat)
    // The streaming response is SSE ("data: {...}\n\n") not NDJSON.
    // Tool-call deltas arrive piece-by-piece and must be accumulated per index.

    async fn run_ollama_cloud_loop(
        &self,
        config: LlmConfig,
        tools: Vec<agent_brain_protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        tx: mpsc::Sender<ChatEvent>,
    ) {
        let base_url = config.base_url.as_deref().unwrap_or("https://ollama.com");
        let url = format!("{}/v1/chat/completions", base_url);
        let model = config.model.clone();

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
        let mut messages: Vec<Value> =
            vec![json!({ "role": "system", "content": build_system_prompt() })];
        for h in &request.history {
            messages.push(json!({ "role": h.role, "content": h.content }));
        }
        messages.push(json!({ "role": "user", "content": request.message }));

        let client = reqwest::Client::new();

        for _iteration in 0..MAX_TOOL_ITERATIONS {
            let body = json!({
                "model": model,
                "messages": messages,
                "tools": oai_tools,
                "stream": true,
                "temperature": config.temperature,
            });

            let mut req = client
                .post(&url)
                .header("content-type", "application/json")
                .timeout(config.timeout)
                .json(&body);
            if let Some(ref key) = config.api_key {
                req = req.header("Authorization", format!("Bearer {}", key));
            }

            let response = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: format!("OllamaCloud request failed: {e}"),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            if !response.status().is_success() {
                let status = response.status();
                let body_text = response.text().await.unwrap_or_default();
                warn!(
                    status = %status,
                    model = %model,
                    body = %body_text,
                    "OllamaCloud returned non-success status"
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

                    let delta = &chunk_json["choices"][0]["delta"];

                    // Stream content tokens.
                    if let Some(content) = delta["content"].as_str()
                        && !content.is_empty()
                    {
                        full_content.push_str(content);
                        let _ = tx
                            .send(ChatEvent::Token {
                                content: content.to_string(),
                            })
                            .await;
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

                    let finish = chunk_json["choices"][0]["finish_reason"]
                        .as_str()
                        .unwrap_or("");
                    if finish == "stop" || finish == "tool_calls" {
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
                // No tool calls — final answer.
                if !full_content.is_empty() {
                    let _ = tx
                        .send(ChatEvent::Message {
                            content: full_content,
                        })
                        .await;
                }
                break;
            }

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

                let (success, result_text) = if let Some(ref h) = handler {
                    let args = if tool_args.is_object() {
                        Some(tool_args.clone())
                    } else {
                        None
                    };
                    let result = h.execute(tool_name, args).await;
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

                let preview: String = result_text.chars().take(MAX_TOOL_RESULT_CHARS).collect();
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: tool_name.clone(),
                        success,
                        preview: preview.clone(),
                    })
                    .await;

                // OpenAI requires tool results as role="tool" with tool_call_id.
                // Use CLOUD_TOOL_RESULT_CHARS (smaller cap) because tool schemas are resent
                // every round; combined context grows quickly and causes 500s.
                let cloud_preview: String =
                    result_text.chars().take(CLOUD_TOOL_RESULT_CHARS).collect();
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
            build_system_prompt(),
            tools_str
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

/// Parse XML-style `<invoke name="...">` tool calls leaked by models like MiniMax.
///
/// Handles patterns like:
///   `<invoke name="search_web"><query>foo</query></invoke>`
///   `<invoke name="search_web">{"query":"foo"}</invoke>`
///   `<invoke name="list_tasks"></invoke></minimax:tool_call>`
fn parse_xml_tool_calls(text: &str) -> Vec<(String, String, Value)> {
    let mut calls = Vec::new();
    let mut search = text;
    let mut id_counter = 0u32;

    while let Some(open_start) = search.find("<invoke") {
        let rest = &search[open_start..];
        // Extract name attribute
        let name = if let Some(name_start) = rest.find("name=\"") {
            let after = &rest[name_start + 6..];
            if let Some(name_end) = after.find('"') {
                after[..name_end].to_string()
            } else {
                break;
            }
        } else {
            break;
        };

        // Find closing tag
        let close_tag = "</invoke>";
        let body_start = rest.find('>').map(|i| i + 1).unwrap_or(rest.len());
        let body_end = rest.find(close_tag).unwrap_or(rest.len());

        let body = &rest[body_start..body_end];
        let args = if body.trim().is_empty() {
            json!({})
        } else if let Ok(v) = serde_json::from_str::<Value>(body.trim()) {
            v
        } else {
            // Try parsing child XML tags as key=value pairs: <query>foo</query>
            let mut map = serde_json::Map::new();
            let mut rem = body.trim();
            while let Some(tag_open) = rem.find('<') {
                let tag_rest = &rem[tag_open + 1..];
                if tag_rest.starts_with('/') {
                    break;
                }
                if let Some(tag_end) = tag_rest.find('>') {
                    let key = tag_rest[..tag_end].trim().to_string();
                    let after_tag = &tag_rest[tag_end + 1..];
                    let close = format!("</{}>", key);
                    if let Some(val_end) = after_tag.find(&close) {
                        map.insert(key, Value::String(after_tag[..val_end].trim().to_string()));
                        rem = &after_tag[val_end + close.len()..];
                    } else {
                        break;
                    }
                } else {
                    break;
                }
            }
            if map.is_empty() {
                json!({})
            } else {
                Value::Object(map)
            }
        };

        let id = format!("xml-{}", id_counter);
        id_counter += 1;
        calls.push((id, name, args));

        // Advance past </invoke>
        if let Some(end) = rest.find(close_tag) {
            search = &search[open_start + end + close_tag.len()..];
        } else {
            break;
        }
    }

    calls
}

/// Remove XML `<invoke>...</invoke>` and `</minimax:tool_call>` blocks from text.
fn strip_xml_tool_calls(text: &str) -> String {
    let mut result = text.to_string();
    while let Some(start) = result.find("<invoke") {
        if let Some(end) = result.find("</invoke>") {
            result = format!("{}{}", &result[..start], &result[end + 9..]);
        } else {
            result = result[..start].to_string();
            break;
        }
    }
    // Strip any leftover minimax wrapper tags
    result = result.replace("</minimax:tool_call>", "");
    result = result.replace("<minimax:tool_call>", "");
    result.trim().to_string()
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
