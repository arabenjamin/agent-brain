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
//!
//! ## Layout
//!
//! - `mod.rs` — public types ([`ChatEvent`], [`ChatRequest`]), the [`ChatService`]
//!   struct and constructors, iteration caps, tool dispatch.
//! - `turn` — one turn: profile/model resolution, the fallback ladder, one attempt.
//! - Provider loops, one `impl ChatService` each: `anthropic_loop`, `ollama_loop`,
//!   `openai_loop` (Ollama Cloud / llama.cpp / OpenAI-compatible), `text_loop`
//!   (`<tool_call>` text protocol — Gemini).
//! - `synthesis` — the worker/voice split. `delegate` — the `delegate` pseudo-tool
//!   and the [`SubagentLoopRunner`] impl that `AgentRunner` drives.
//! - Shared by every loop: `loop_policy` (caps, nudges, tool receipt),
//!   `write_guard`, `tool_results` (budgets, truncation, markers), `tool_calls`
//!   (text tool-call parsing), `handover`, `prompt`, `citations`, `events`.

use std::sync::{Arc, Weak};
use std::time::Duration;

use futures_util::StreamExt as _;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::{RwLock, mpsc};
use tracing::{debug, warn};

use crate::mcp::tools::{ToolHandler, ToolRegistry};
use crate::services::agent_runner::{AgentBudget, AgentRunner, SubagentLoopRunner};
use crate::services::context_builder::ContextBuilderService;
use crate::services::llm::{ChatMessage, LlmClient, LlmConfig, LlmProviderType};
use crate::services::routing::{DataClass, Purpose, ROUTE_CONTEXT, RouteContext, RoutingPolicy};
use agent_brain_protocol::Content;

mod anthropic_loop;
mod citations;
mod delegate;
mod events;
mod handover;
mod loop_policy;
mod ollama_loop;
mod openai_loop;
mod prompt;
mod round_loop;
mod synthesis;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
mod text_loop;
mod tool_calls;
mod tool_results;
mod turn;
mod write_guard;

use citations::*;
pub(crate) use delegate::DELEGATE_TOOL;
use delegate::*;
use events::*;
use handover::*;
use loop_policy::*;
pub(crate) use prompt::build_system_prompt;
use round_loop::*;
use tool_calls::*;
use tool_results::*;
use turn::*;
pub(crate) use write_guard::WRITE_TOOLS;
use write_guard::*;

/// Maximum tool-use iterations per chat turn (prevents infinite loops).
const MAX_TOOL_ITERATIONS: usize = 10;

/// Maximum tool-use iterations for a **sub-agent** loop — tighter than
/// [`MAX_TOOL_ITERATIONS`] (a turn's worst-case model calls is the product of
/// orchestrator rounds and sub-agent rounds), but not so tight that it starves
/// legitimate query refinement.
///
/// Was 3 on 2026-09-06 to stop a worker looping `search_notes` to exhaustion —
/// but that looping came from the empty-`entity` degenerate search, now hardened
/// (`skills/knowledge.rs`). With that gone, 3 was too tight: on 2026-09-07 a
/// delegated worker asked "the latest on Our Niche Intelligence Agency" ran out
/// after 3 rounds and reported "no information" — a false negative the `general`
/// profile answered correctly in 5 query-refinement rounds. Raised to 6: enough
/// to refine a query the way `general` does, still well under the orchestrator's
/// cap, still bounding the call product.
///
/// Raised again to 8 on 2026-09-08: a delegated `knowledge-worker` asked "what
/// did we learn today?" spent its rounds on a doomed retrieval (semantic search
/// on the literal date string), got guillotined mid-recovery, and the turn
/// reported "no substantive findings" on a 377-note day. The real fix is the
/// deterministic `recall_recent` tool (a whole-day question resolves in one
/// call), but the tight cap turned a recoverable wrong-first-move into a hard
/// false negative, so give the refinement loop the headroom `general` had. Still
/// under the orchestrator's cap and still bounds the round product.
const MAX_SUBAGENT_ITERATIONS: usize = 8;

// Usage-ledger `tool_name` override for LLM calls made inside a sub-agent loop.
// A normal chat turn records under "chat"; `AgentRunner` scopes this task-local
// to `subagent:<agent>` around the bounded loop so `delegate` / `run_agent` cost
// stops billing as chat and becomes attributable by agent. Propagates through
// `join!` (the whole chat loop chain runs in one task, never a `tokio::spawn`);
// absent the scope, `usage_tool_label` falls back to "chat".
tokio::task_local! {
    pub static USAGE_TOOL_LABEL: String;
}

/// The usage-ledger `tool_name` for the current LLM call: the sub-agent label if
/// one is scoped ([`USAGE_TOOL_LABEL`]), else `"chat"`.
fn usage_tool_label() -> String {
    USAGE_TOOL_LABEL
        .try_with(|s| s.clone())
        .unwrap_or_else(|_| "chat".to_string())
}

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
        /// The FULL tool output, for the person reading. Not capped to the
        /// model's ration — see the comment at the cloud loop's assignment.
        preview: String,
        /// How much of `preview` the model was given, when that is less than
        /// all of it. `None` means the model read the same thing you can see.
        /// This is how the UI stays honest about the narrower view without
        /// narrowing yours.
        #[serde(skip_serializing_if = "Option::is_none")]
        model_saw_chars: Option<usize>,
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
///
/// `Clone` because one turn may be attempted on more than one model — see
/// [`ChatService::fallback_ladder`].
#[derive(Debug, Clone, Deserialize)]
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
    /// Usage ledger. Chat is the main cloud consumer — every LLM call made by
    /// the chat loops must land in `model_usage` or quota accounting is fiction.
    telemetry: Option<crate::repository::TelemetryClient>,
    /// Models to try, in order, when a turn produces nothing the user can see.
    /// Resolved from `models.yaml`'s `chat_fallback_ladder` at startup; empty
    /// means the active model is the only one that will ever be tried.
    fallback_ladder: Vec<LlmConfig>,
    /// The shared agent-execution primitive. `run_subagent` delegates to it; it
    /// holds a `Weak` back to this `ChatService` as its loop engine. See
    /// `services/agent_runner.rs`.
    agent_runner: Arc<AgentRunner>,
}

/// Filter a tool list to only those whose names appear in `names`.
/// Returns `all` unchanged if `names` is empty.
pub(crate) fn filter_tools(
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

impl ChatService {
    /// Create a new `ChatService` backed by the server's live registries.
    pub fn new(
        tool_handler: Arc<RwLock<Option<ToolHandler>>>,
        tool_registry: Arc<RwLock<ToolRegistry>>,
        llm_config: Arc<RwLock<Option<LlmConfig>>>,
    ) -> Arc<Self> {
        Self::build(
            tool_handler,
            tool_registry,
            llm_config,
            Arc::new(RwLock::new(None)),
            None,
            Vec::new(),
        )
    }

    /// Shared constructor. Uses `Arc::new_cyclic` so the owned [`AgentRunner`]
    /// can hold a `Weak<dyn SubagentLoopRunner>` back to this `ChatService`
    /// (the loop engine) without a reference cycle keeping either alive.
    fn build(
        tool_handler: Arc<RwLock<Option<ToolHandler>>>,
        tool_registry: Arc<RwLock<ToolRegistry>>,
        llm_config: Arc<RwLock<Option<LlmConfig>>>,
        context_builder: Arc<RwLock<Option<Arc<ContextBuilderService>>>>,
        telemetry: Option<crate::repository::TelemetryClient>,
        fallback_ladder: Vec<LlmConfig>,
    ) -> Arc<Self> {
        Arc::new_cyclic(|weak: &Weak<ChatService>| {
            let engine: Weak<dyn SubagentLoopRunner> = weak.clone();
            let agent_runner = Arc::new(AgentRunner::new(
                Arc::clone(&tool_handler),
                Arc::clone(&tool_registry),
                Arc::clone(&llm_config),
                Arc::clone(&context_builder),
                engine,
            ));
            Self {
                tool_handler,
                tool_registry,
                llm_config,
                context_builder,
                telemetry,
                fallback_ladder,
                agent_runner,
            }
        })
    }

    /// Create a `ChatService` sharing the context-builder Arc from `McpServerCore`.
    /// Profiles loaded by `build_skills()` are immediately visible without restart.
    pub fn with_context_builder(
        tool_handler: Arc<RwLock<Option<ToolHandler>>>,
        tool_registry: Arc<RwLock<ToolRegistry>>,
        llm_config: Arc<RwLock<Option<LlmConfig>>>,
        context_builder: Arc<RwLock<Option<Arc<ContextBuilderService>>>>,
        telemetry: Option<crate::repository::TelemetryClient>,
        fallback_ladder: Vec<LlmConfig>,
    ) -> Arc<Self> {
        Self::build(
            tool_handler,
            tool_registry,
            llm_config,
            context_builder,
            telemetry,
            fallback_ladder,
        )
    }

    /// Hand out this service's shared [`AgentRunner`] so the background
    /// `run_agent` tool can reach the same loop engine chat uses. Published into
    /// `BrainCore::agent_runner` when the chat service is created — see
    /// `McpServerCore::chat_service` / `main.rs`.
    pub fn agent_runner(&self) -> Arc<AgentRunner> {
        Arc::clone(&self.agent_runner)
    }

    /// Dispatch one tool call from a provider loop: intercept `delegate` as a
    /// sub-agent (grounding the write guard when the worker retrieved), and run
    /// everything else through `execute_guarded`.
    ///
    /// **All four provider loops call this**, so the interception and grounding
    /// are defined once rather than copied per loop — the "handled in one path,
    /// not the other" shape this codebase keeps re-growing. `allow_delegate` is
    /// false inside a sub-agent's own loop, so a sub-agent cannot delegate
    /// further (depth cap 1); a hallucinated `delegate` call there just falls
    /// through to `execute_guarded` and fails as an unknown tool.
    async fn dispatch_tool_call(
        &self,
        allow_delegate: bool,
        handler: &Option<ToolHandler>,
        write_guard: &mut TurnWriteGuard,
        tool_name: &str,
        tool_args: &Value,
        tx: &mpsc::Sender<ChatEvent>,
    ) -> (bool, String, Option<String>) {
        if allow_delegate && subagent_enabled() && tool_name == DELEGATE_TOOL {
            let sub_agent = tool_args.get("agent").and_then(Value::as_str).unwrap_or("");
            let sub_task = tool_args.get("task").and_then(Value::as_str).unwrap_or("");
            // Box the recursive future: a sub-agent runs these same loops, an
            // async cycle the compiler requires boxing to size.
            let (ok, retrieved, findings) =
                Box::pin(self.run_subagent(sub_agent, sub_task, tx)).await;
            // A worker that searched on our behalf grounds this turn — the
            // delegate result never reaches execute_guarded/observe, so mark it
            // here or a store_note written afterward is wrongly downgraded.
            if retrieved {
                write_guard.mark_retrieval();
            }
            // A sub-agent's bulk already reached the user directly, streamed
            // from its own loop — it must not be re-emitted here.
            (ok, findings, None)
        } else {
            execute_guarded(handler, write_guard, tool_name, tool_args).await
        }
    }
}
