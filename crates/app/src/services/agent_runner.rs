//! Shared agent-execution primitive.
//!
//! `AgentRunner::run` is the one place a **profile-driven, bounded, tool-using
//! agent loop** lives. It was lifted verbatim out of
//! `ChatService::run_subagent` (Phase 0 of
//! `project-docs/UNIFIED_AGENT_EXECUTION_PLAN.md`) so that all three goal
//! entrypoints — chat, tasks, schedules — can reach the *same* loop instead of
//! only chat having one. Chat's `run_subagent` is now a thin streaming wrapper
//! over this; the background `run_agent` tool (Phase 1) will call it with no
//! sink.
//!
//! ## Why the loop machinery stays in `ChatService`
//!
//! The four provider loops (`run_anthropic_loop`, `run_ollama_tool_loop`,
//! `run_ollama_cloud_loop`, `run_text_loop`) and profile-model resolution are
//! large, incident-laden `ChatService` methods that encode the fallback ladder,
//! the write guard, empty-completion retries, and the worker/voice split. Moving
//! them wholesale would be a behaviour change dressed as a refactor — exactly
//! what the plan says *not* to do. Instead this service reaches them through the
//! [`SubagentLoopRunner`] trait, which `ChatService` implements by dispatching to
//! its existing loops unchanged. `AgentRunner` owns the *orchestration* (resolve
//! profile → build toolset → run one bounded loop → drain findings); the engine
//! owns the *loop*. The cycle (`ChatService` holds an `Arc<AgentRunner>`;
//! `AgentRunner` holds a `Weak<dyn SubagentLoopRunner>` back to it) is broken
//! with a `Weak`, wired via `Arc::new_cyclic`.

use std::collections::HashMap;
use std::sync::{Arc, Weak};

use tokio::sync::{RwLock, mpsc};

use crate::clients::chat::{ChatEvent, ChatRequest};
use crate::clients::chat::{
    DELEGATE_TOOL, USAGE_TOOL_LABEL, WRITE_TOOLS, build_system_prompt, filter_tools,
};
use crate::mcp::tools::{ToolHandler, ToolRegistry};
use crate::services::context_builder::ContextBuilderService;
use crate::services::llm::{LlmConfig, LlmProviderType};

/// The engine an [`AgentRunner`] drives. Implemented by `ChatService`, whose
/// provider loops and catalog-aware model resolution are the shared machinery we
/// deliberately do not duplicate. See the module docs for why this is a trait
/// rather than a physical move of the loops.
#[async_trait::async_trait]
pub trait SubagentLoopRunner: Send + Sync {
    /// Run one bounded provider loop, streaming events to `tx`. `allow_delegate`
    /// is `false` for an agent run (depth cap 1); `max_iterations` bounds the
    /// tool rounds. This is exactly the match `ChatService` already performs.
    #[allow(clippy::too_many_arguments)]
    async fn run_bounded_loop(
        &self,
        cfg: LlmConfig,
        tools: Vec<agent_brain_protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        system_prompt: String,
        tx: mpsc::Sender<ChatEvent>,
        allow_delegate: bool,
        max_iterations: usize,
    );

    /// Resolve a profile's `model_preference` (+ optional `provider_hint`) to a
    /// full config against the live model catalog. `None` keeps `base`.
    async fn resolve_agent_model(
        &self,
        base: &LlmConfig,
        model_pref: &str,
        provider_hint: Option<&str>,
    ) -> Option<LlmConfig>;
}

/// Execution budget for one agent run.
#[derive(Debug, Clone, Copy)]
pub struct AgentBudget {
    /// Tool-use rounds before the loop is asked for a final answer with no tools.
    pub max_iterations: usize,
    /// Whether the inner agent may call write tools. Chat sub-agents are
    /// read-only (one writer per turn); a background agent writes (writing *is*
    /// the job) but still runs the engine's own `TurnWriteGuard`.
    pub allow_writes: bool,
}

/// One tool the sub-agent actually invoked, with the outcome the loop reported.
/// This is the ground-truth receipt behind a run: an LLM's prose can claim it
/// "ran X and stored Y" when no such call happened (observed live — the toolsmith
/// narrated an execution it never performed), so the trace travels with the
/// findings for the caller to check the narrative against.
#[derive(Debug, Clone)]
pub struct ToolInvocation {
    pub tool: String,
    pub success: bool,
}

/// Structured result of one agent run — the same information
/// `ChatService::run_subagent` returned as a tuple, named.
#[derive(Debug, Clone)]
pub struct AgentFindings {
    /// The agent produced a non-empty final answer.
    pub ok: bool,
    /// The run was rejected **before** the loop ran — a config/infra error:
    /// an empty task, an unknown or non-`spawnable` agent, a missing engine, or
    /// a profile that failed to load. Distinct from `ok: false` after a real
    /// run (the agent ran but produced nothing). A caller exposing this as a
    /// tool surfaces a rejection as a hard error (it is a bug to fix) but a
    /// ran-but-empty result as a non-fatal step result (so it does not
    /// dead-letter the chain).
    pub rejected: bool,
    /// The agent ran ≥1 successful tool — it actually consulted the world. The
    /// caller uses this to ground its own write guard (a delegated retrieval the
    /// guard cannot otherwise see).
    pub retrieved: bool,
    /// The formatted findings block (on success) or the error text (on failure)
    /// — ready to fold into the caller's turn / chain step. On success the block
    /// ends with a ground-truth receipt of the tools that actually ran.
    pub findings: String,
    /// Every tool the sub-agent actually invoked, in order, with outcome. The
    /// receipt behind `findings`: a caller can check the prose's claims against
    /// what really happened (empty = the agent ran no tools at all).
    pub tool_trace: Vec<ToolInvocation>,
}

impl AgentFindings {
    /// A pre-flight rejection (config/infra), before any loop ran.
    fn rejected(msg: impl Into<String>) -> Self {
        Self {
            ok: false,
            rejected: true,
            retrieved: false,
            findings: msg.into(),
            tool_trace: Vec::new(),
        }
    }
}

/// Runs a profile as a bounded agent loop and returns structured findings.
pub struct AgentRunner {
    tool_handler: Arc<RwLock<Option<ToolHandler>>>,
    tool_registry: Arc<RwLock<ToolRegistry>>,
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
    context_builder: Arc<RwLock<Option<Arc<ContextBuilderService>>>>,
    /// The loop engine (`ChatService`). `Weak` breaks the ownership cycle —
    /// `ChatService` owns the `AgentRunner`.
    engine: Weak<dyn SubagentLoopRunner>,
}

impl AgentRunner {
    pub fn new(
        tool_handler: Arc<RwLock<Option<ToolHandler>>>,
        tool_registry: Arc<RwLock<ToolRegistry>>,
        llm_config: Arc<RwLock<Option<LlmConfig>>>,
        context_builder: Arc<RwLock<Option<Arc<ContextBuilderService>>>>,
        engine: Weak<dyn SubagentLoopRunner>,
    ) -> Self {
        Self {
            tool_handler,
            tool_registry,
            llm_config,
            context_builder,
            engine,
        }
    }

    /// Run `agent` (a `spawnable` context profile) against `task`, a
    /// self-contained sub-goal. When `sink` is `Some`, the agent's activity is
    /// forwarded to it (chat streams the worker to the user); when `None`, the
    /// findings are collected silently (the background path).
    ///
    /// This is the body that used to be `ChatService::run_subagent`, unchanged in
    /// logic. The two differences are parameterisation: the loop is reached
    /// through the engine trait, and the progress/finish stream events are gated
    /// on `sink` being present.
    pub async fn run(
        &self,
        agent: &str,
        task: &str,
        budget: AgentBudget,
        sink: Option<mpsc::Sender<ChatEvent>>,
    ) -> AgentFindings {
        let agent = agent.trim();
        if task.trim().is_empty() {
            return AgentFindings::rejected("agent run called with an empty task.");
        }

        // Choose-or-build (plan §4): `agent: "auto"` picks the best-fitting
        // spawnable agent by specialty match — the "choose" half, LLM-free. When
        // nothing matches lexically, it falls back to a generalist rather than
        // rejecting, so an open-ended auto task still runs (the standalone
        // `select_agent` tool stays honest and returns None = a build cue).
        let selected;
        let agent: &str = if agent.eq_ignore_ascii_case("auto") {
            let sel = self.select_agent(task).await;
            match sel.agent.or_else(|| generalist_default(&sel.roster)) {
                Some(name) => {
                    tracing::info!(agent = %name, "run_agent(auto) selected agent");
                    selected = name;
                    &selected
                }
                // Only truly unreachable when the roster is empty (no spawnable
                // agents exist at all) — then there is nothing to run.
                None => {
                    return AgentFindings::rejected(
                        "run_agent(auto): no spawnable agents exist to run.",
                    );
                }
            }
        } else {
            agent
        };

        let Some(engine) = self.engine.upgrade() else {
            return AgentFindings::rejected("Agent unavailable: loop engine dropped.");
        };

        let Some(cb) = self.context_builder.read().await.clone() else {
            return AgentFindings::rejected("Agent unavailable: no context builder.");
        };

        // Spawnability is checked against the profile's `spawnable` flag BEFORE
        // building the bundle, so a rejected agent never runs its pre_load_query.
        // A non-spawnable or unknown name gets the live roster of what it could
        // have picked.
        let spawnable_ok = cb.get_profile(agent).await.is_some_and(|p| p.spawnable);
        if !spawnable_ok {
            let roster = cb
                .spawnable_profiles()
                .await
                .iter()
                .map(|p| p.name.clone())
                .collect::<Vec<_>>()
                .join(", ");
            return AgentFindings::rejected(format!(
                "'{agent}' is not a sub-agent you can delegate to. Available: {roster}."
            ));
        }

        let Ok(bundle) = cb.build_bundle(agent).await else {
            return AgentFindings::rejected(format!(
                "Sub-agent profile '{agent}' could not be loaded."
            ));
        };

        // Base config = the current chat model (unmetered ollama-cloud); a
        // profile `model_preference` may override it, but a sub-agent must never
        // land on a metered (Anthropic) or rate-limited (Gemini free tier — the
        // 5-RPM trap) provider: a multi-round loop there burns paid quota or
        // trips rate limits, and the roster's model pins exist for the human
        // /chat persona, not for background/delegated delegation. A pin that
        // resolves off the unmetered band (local Ollama or free Ollama Cloud) is
        // honoured on the /chat path (ChatService::resolve_profile_model) but
        // clamped to `base` here. This is where the invariant the roster assumes
        // is actually enforced.
        let base = self.llm_config.read().await.clone().unwrap_or_default();
        let cfg = match bundle
            .profile
            .model_preference
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(pref) => {
                let resolved = engine
                    .resolve_agent_model(&base, pref, bundle.profile.provider_hint.as_deref())
                    .await
                    .unwrap_or_else(|| base.clone());
                if matches!(
                    resolved.provider,
                    LlmProviderType::Ollama
                        | LlmProviderType::OllamaCloud
                        | LlmProviderType::LlamaCpp
                ) {
                    // Ollama (host), Ollama Cloud (free tier), and the llama.cpp
                    // sidecar are all unmetered/local — safe for a sub-agent.
                    resolved
                } else {
                    tracing::warn!(
                        agent = %agent,
                        model = %resolved.model,
                        provider = %resolved.provider,
                        "sub-agent model_preference resolves to a metered/rate-limited \
                         provider; clamping to the unmetered base config"
                    );
                    base.clone()
                }
            }
            None => base.clone(),
        };

        // Toolset: never `delegate` (enforces depth cap 1). Write tools are
        // stripped unless the budget allows writes (chat sub-agents are
        // read-only; a background agent writes but keeps the engine write guard).
        let all_tools = self.tool_registry.read().await.list();
        let allowed: Vec<String> = bundle
            .profile
            .tools
            .iter()
            .filter(|t| {
                t.as_str() != DELEGATE_TOOL
                    && (budget.allow_writes || !WRITE_TOOLS.contains(&t.as_str()))
            })
            .cloned()
            .collect();
        let sub_tools = filter_tools(all_tools, &allowed);
        let handler = self.tool_handler.read().await.clone();
        let sub_system = build_system_prompt(
            (!bundle.profile.system_prompt.is_empty())
                .then_some(bundle.profile.system_prompt.as_str()),
            &bundle.pre_loaded_notes,
        );
        let sub_request = ChatRequest {
            message: task.to_string(),
            history: Vec::new(),
            session_id: None,
            tools: None,
            context_profile: None,
            // The sub-agent speaks for itself — no nested worker/voice split.
            synthesis_provider: None,
            synthesis_model: None,
        };

        let started = std::time::Instant::now();
        if let Some(ref tx) = sink {
            let _ = tx
                .send(ChatEvent::Thinking {
                    content: format!("🔎 delegating to `{agent}` sub-agent ({})…", cfg.model),
                })
                .await;
        }

        let model = cfg.model.clone();
        let (sub_tx, mut sub_rx) = mpsc::channel::<ChatEvent>(128);
        // allow_delegate = false: an agent cannot delegate further (depth cap 1).
        // Scope the usage-ledger label to `subagent:<agent>` around the loop so
        // every LLM call it makes (primary + fallback rungs) is attributed to the
        // sub-agent, not to `chat` — the whole loop chain runs inline under
        // `join!`, so the task-local propagates. `delegate` and `run_agent` both
        // reach the loop through here, so this is the one place that fixes both.
        let provider_loop = USAGE_TOOL_LABEL.scope(
            format!("subagent:{agent}"),
            engine.run_bounded_loop(
                cfg,
                sub_tools,
                handler,
                sub_request,
                sub_system,
                sub_tx,
                false,
                budget.max_iterations,
            ),
        );

        let drain = async {
            let mut answer = String::new();
            let mut saw_error: Option<String> = None;
            // Successful tool-result previews, kept to extract the sources the
            // caller must cite. The worker's own tool outputs are the only
            // trustworthy provenance — its prose can paraphrase or drop URLs.
            let mut gathered: Vec<String> = Vec::new();
            // The real tool-call trace — receipts to check the prose against.
            let mut trace: Vec<ToolInvocation> = Vec::new();
            while let Some(ev) = sub_rx.recv().await {
                match ev {
                    ChatEvent::Message { content } => answer = content,
                    ChatEvent::Error { message } => {
                        if saw_error.is_none() {
                            saw_error = Some(message);
                        }
                    }
                    ChatEvent::Done => {}
                    ChatEvent::ToolCall { tool, args } => {
                        if let Some(ref tx) = sink {
                            let _ = tx
                                .send(ChatEvent::ToolCall {
                                    tool: format!("{agent}:{tool}"),
                                    args,
                                })
                                .await;
                        }
                    }
                    ChatEvent::ToolResult {
                        tool,
                        success,
                        preview,
                        model_saw_chars,
                    } => {
                        trace.push(ToolInvocation {
                            tool: tool.clone(),
                            success,
                        });
                        if success {
                            gathered.push(preview.clone());
                        }
                        if let Some(ref tx) = sink {
                            let _ = tx
                                .send(ChatEvent::ToolResult {
                                    tool: format!("{agent}:{tool}"),
                                    success,
                                    // Forwarded intact: a sub-agent's tool
                                    // result reaches the user's stream in full,
                                    // carrying how much of it the WORKER read.
                                    preview,
                                    model_saw_chars,
                                })
                                .await;
                        }
                    }
                    ChatEvent::Thinking { content } => {
                        if let Some(ref tx) = sink {
                            let _ = tx
                                .send(ChatEvent::Thinking {
                                    content: format!("[{agent}] {content}"),
                                })
                                .await;
                        }
                    }
                    ChatEvent::Token { .. } => {}
                }
            }
            (answer, saw_error, gathered, trace)
        };

        let (_, (answer, saw_error, gathered, trace)) = tokio::join!(provider_loop, drain);
        // The worker actually consulted the world iff it ran ≥1 successful tool.
        let retrieved = !gathered.is_empty();

        let ok = !answer.trim().is_empty();
        if let Some(ref tx) = sink {
            // Close the delegation bracket in the stream so the whole sub-agent
            // turn reads as one unit that started and finished.
            let elapsed = started.elapsed().as_secs_f64();
            let finish = if ok {
                format!(
                    "✓ `{agent}` finished — {} result(s) in {elapsed:.1}s",
                    gathered.len()
                )
            } else {
                format!("✗ `{agent}` finished with no answer ({elapsed:.1}s)")
            };
            let _ = tx.send(ChatEvent::Thinking { content: finish }).await;
        }

        if !ok {
            // Ran, but produced nothing — not a rejection. A tool caller reports
            // this as a non-fatal step result, not a hard error.
            return AgentFindings {
                ok: false,
                rejected: false,
                retrieved,
                findings: saw_error
                    .unwrap_or_else(|| format!("Sub-agent `{agent}` ({model}) returned nothing.")),
                tool_trace: trace,
            };
        }
        AgentFindings {
            ok: true,
            rejected: false,
            retrieved,
            findings: format_subagent_findings(agent, &answer, &gathered, &trace),
            tool_trace: trace,
        }
    }

    /// Deterministically pick the spawnable agent whose specialty/description
    /// best matches `goal` by token overlap. LLM-free (plan §4, decision 5), so a
    /// chain or schedule step can route to an agent without a model call.
    ///
    /// `agent` is `None` when the roster is empty or nothing clears
    /// [`MIN_SELECTION_OVERLAP`] — the caller's cue to `construct_agent` instead
    /// (the "build" half of choose-or-build). The full roster is always returned
    /// so a caller with no match can show the options or decide to build.
    pub async fn select_agent(&self, goal: &str) -> AgentSelection {
        let roster_profiles = match self.context_builder.read().await.clone() {
            Some(cb) => cb.spawnable_profiles().await,
            None => Vec::new(),
        };
        let roster: Vec<(String, String)> = roster_profiles
            .iter()
            .map(|p| {
                let blurb = p
                    .specialty
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .unwrap_or(p.description.as_str())
                    .to_string();
                (p.name.clone(), blurb)
            })
            .collect();

        let goal_tokens = tokenize(goal);
        // Score = NAME_WEIGHT × (name-token overlap) + (specialty-token overlap).
        // The name is weighted higher because it is the stronger signal: a goal
        // that says "research …" should pick `researcher` over `knowledge-worker`
        // even though both specialties contain "search" (and `research` ⊃
        // `search` under substring matching). Ties break by roster order (stable);
        // `>` (not `>=`) keeps the first seen.
        const NAME_WEIGHT: usize = 2;
        let mut best: Option<(String, usize)> = None;
        for p in &roster_profiles {
            let name_tokens = tokenize(&p.name);
            let spec_tokens = tokenize(p.specialty.as_deref().unwrap_or(&p.description));
            let score = NAME_WEIGHT * token_overlap(&goal_tokens, &name_tokens)
                + token_overlap(&goal_tokens, &spec_tokens);
            match &best {
                Some((_, best_score)) if score <= *best_score => {}
                _ => best = Some((p.name.clone(), score)),
            }
        }

        match best {
            Some((name, overlap)) if overlap >= MIN_SELECTION_OVERLAP => AgentSelection {
                agent: Some(name),
                score: overlap,
                roster,
            },
            _ => AgentSelection {
                agent: None,
                score: 0,
                roster,
            },
        }
    }
}

/// Minimum goal/specialty token overlap for [`AgentRunner::select_agent`] to
/// return a match rather than `None`. One shared meaningful word is enough to
/// prefer a curated agent over constructing one; below that, build.
const MIN_SELECTION_OVERLAP: usize = 1;

/// The outcome of a deterministic agent selection.
#[derive(Debug, Clone)]
pub struct AgentSelection {
    /// The best-matching spawnable agent, or `None` when nothing fit — build one.
    pub agent: Option<String>,
    /// Token-overlap score of the chosen agent (0 when none).
    pub score: usize,
    /// The full spawnable roster as `(name, specialty)` — always returned.
    pub roster: Vec<(String, String)>,
}

/// The generalist to fall back to when `run_agent(auto)` finds no lexical match.
/// A web+graph researcher is the safe default for an open-ended task the matcher
/// could not place (many topical goals — "off-grid mesh hardware" — share no
/// words with any curated specialty). Prefers a spawnable whose name mentions
/// "research"; else the first in the roster. `select_agent` itself stays honest
/// and returns `None` — only `auto` applies this default, so the standalone
/// choose-or-build signal (None = consider constructing) is preserved.
fn generalist_default(roster: &[(String, String)]) -> Option<String> {
    roster
        .iter()
        .map(|(n, _)| n)
        .find(|n| n.to_ascii_lowercase().contains("research"))
        .or_else(|| roster.first().map(|(n, _)| n))
        .cloned()
}

/// Count goal tokens that match a profile's tokens, by **bidirectional
/// substring** (a goal token matches if it contains, or is contained by, a
/// profile token). Substring rather than equality so `research` hooks
/// `researcher`, `code` hooks `code-analyst`, `schedule` hooks
/// `schedule-author` — the agent NAME is usually the strongest signal, and goals
/// name the domain, not the specialty's exact phrasing. Both sides are already
/// length-≥3 (see [`tokenize`]), which bounds spurious substring hits.
fn token_overlap(
    goal: &std::collections::HashSet<String>,
    haystack: &std::collections::HashSet<String>,
) -> usize {
    goal.iter()
        .filter(|g| {
            haystack
                .iter()
                .any(|h| h.contains(g.as_str()) || g.contains(h.as_str()))
        })
        .count()
}

/// Lowercase, split on non-alphanumerics, drop short/stopword tokens. A tiny
/// stopword set keeps generic words ("the", "and", "with") from matching every
/// agent; a min length of 3 drops the rest of the noise.
fn tokenize(text: &str) -> std::collections::HashSet<String> {
    const STOPWORDS: &[&str] = &[
        "the", "and", "for", "with", "from", "that", "this", "into", "your", "you", "how", "what",
        "why", "who", "was", "are", "its", "our", "out", "get", "use", "any", "all", "can",
    ];
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.len() >= 3)
        .map(|w| w.to_ascii_lowercase())
        .filter(|w| !STOPWORDS.contains(&w.as_str()))
        .collect()
}

/// Extract unique http(s) URLs from an agent's tool-result previews.
///
/// The worker's own tool outputs are the trustworthy provenance — its prose can
/// paraphrase or drop links (the exact laundering the epistemics guards against).
/// Pulling the URLs straight out of what its tools returned gives the caller a
/// concrete list to cite regardless of how the worker wrote them up. Split on
/// whitespace/brackets (no byte indexing, so multibyte text is safe), capped.
fn extract_sources(gathered: &[String]) -> Vec<String> {
    const MAX_SOURCES: usize = 8;
    let mut out: Vec<String> = Vec::new();
    for text in gathered {
        for tok in text.split(|c: char| {
            c.is_whitespace() || matches!(c, '"' | '\'' | '(' | ')' | '[' | ']' | '<' | '>')
        }) {
            if (tok.starts_with("http://") || tok.starts_with("https://")) && tok.len() > 10 {
                let url = tok.trim_end_matches(['.', ',', ';', ':']).to_string();
                if !out.contains(&url) {
                    out.push(url);
                    if out.len() >= MAX_SOURCES {
                        return out;
                    }
                }
            }
        }
    }
    out
}

/// Assemble an agent's answer + its sources into the structured findings block
/// the caller reads. The header instructs faithful relay, source citation, and
/// passing along any stated limitation rather than filling it in — the
/// epistemics the worker/voice split alone does not enforce.
fn format_subagent_findings(
    agent: &str,
    answer: &str,
    gathered: &[String],
    trace: &[ToolInvocation],
) -> String {
    let sources = extract_sources(gathered);
    let mut out = format!(
        "[FINDINGS FROM THE `{agent}` SUB-AGENT — relay these faithfully. Cite the sources \
         listed below as markdown links when you use them. If the findings say something \
         could not be found or established, say so plainly rather than filling it in from your \
         own knowledge. Do not add facts the worker did not report.]\n\n{answer}"
    );
    out.push_str(&format_tool_receipt(agent, trace));
    if !sources.is_empty() {
        out.push_str("\n\nSOURCES:");
        for (i, url) in sources.iter().enumerate() {
            out.push_str(&format!("\n  [S{}] {url}", i + 1));
        }
    }
    out
}

/// Render the ground-truth tool receipt: what the sub-agent ACTUALLY ran,
/// aggregated per tool in first-seen order. This is the check against a
/// fabricated narrative — an answer that claims an action whose tool is absent
/// here did not perform it. Deliberately terse and machine-legible so it survives
/// relay through `{{_prev}}` and the chat stream.
fn format_tool_receipt(agent: &str, trace: &[ToolInvocation]) -> String {
    if trace.is_empty() {
        return format!(
            "\n\n[TOOLS `{agent}` ACTUALLY RAN: none. This sub-agent called no tools — any \
             action its answer claims (a search, a write, an execution) did NOT happen; treat \
             the answer as ungrounded assertion.]"
        );
    }
    // Aggregate per tool, first-seen order, counting ok vs failed.
    let mut order: Vec<String> = Vec::new();
    let mut ok: HashMap<&str, usize> = HashMap::new();
    let mut fail: HashMap<&str, usize> = HashMap::new();
    for inv in trace {
        if !order.iter().any(|t| t == &inv.tool) {
            order.push(inv.tool.clone());
        }
        if inv.success {
            *ok.entry(inv.tool.as_str()).or_insert(0) += 1;
        } else {
            *fail.entry(inv.tool.as_str()).or_insert(0) += 1;
        }
    }
    let parts: Vec<String> = order
        .iter()
        .map(|t| {
            let o = ok.get(t.as_str()).copied().unwrap_or(0);
            let f = fail.get(t.as_str()).copied().unwrap_or(0);
            match (o, f) {
                (o, 0) => format!("{t} ✓×{o}"),
                (0, f) => format!("{t} ✗×{f}"),
                (o, f) => format!("{t} ✓×{o} ✗×{f}"),
            }
        })
        .collect();
    format!(
        "\n\n[TOOLS `{agent}` ACTUALLY RAN (ground truth — trust this over the prose above; if \
         the answer claims an action whose tool is not listed here, it did not happen): {}]",
        parts.join(", ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_sources_pulls_unique_urls_from_tool_previews() {
        let gathered = vec![
            "[SEARCH SOURCES]\n  [S1] Title — https://example.com/a\nbody".to_string(),
            "see (https://example.com/b) and https://example.com/a again".to_string(),
            "no urls here".to_string(),
        ];
        let got = extract_sources(&gathered);
        assert_eq!(got, vec!["https://example.com/a", "https://example.com/b"]);
    }

    #[test]
    fn extract_sources_trims_trailing_punctuation_and_brackets() {
        // A URL wrapped in parens or followed by a period must come out clean.
        let gathered = vec!["The source (https://example.com/x). Next.".to_string()];
        assert_eq!(extract_sources(&gathered), vec!["https://example.com/x"]);
    }

    fn inv(tool: &str, success: bool) -> ToolInvocation {
        ToolInvocation {
            tool: tool.to_string(),
            success,
        }
    }

    #[test]
    fn findings_carry_the_relay_instruction_and_sources() {
        let out = format_subagent_findings(
            "researcher",
            "Reticulum has a 500-byte MTU.",
            &["[S1] RNS docs — https://reticulum.network/manual".to_string()],
            &[inv("search_web", true)],
        );
        // The relay header must be present (cite + relay limits).
        assert!(out.contains("relay these faithfully"));
        assert!(out.contains("researcher"));
        // The answer is included verbatim...
        assert!(out.contains("500-byte MTU"));
        // ...and the source is lifted out for citation.
        assert!(out.contains("https://reticulum.network/manual"));
        assert!(out.contains("SOURCES:"));
    }

    #[test]
    fn findings_omit_the_sources_block_when_there_are_none() {
        let out = format_subagent_findings(
            "knowledge-worker",
            "From the graph: X.",
            &[],
            &[inv("search_notes", true)],
        );
        assert!(out.contains("From the graph: X."));
        assert!(!out.contains("SOURCES:"));
    }

    #[test]
    fn receipt_aggregates_tools_and_counts_outcomes() {
        let out = format_tool_receipt(
            "toolsmith",
            &[
                inv("analyze_own_structure", true),
                inv("analyze_own_structure", true),
                inv("manage_dynamic_tool", true),
                inv("http_request", false),
            ],
        );
        assert!(out.contains("ACTUALLY RAN"));
        assert!(out.contains("analyze_own_structure ✓×2"), "got: {out}");
        assert!(out.contains("manage_dynamic_tool ✓×1"), "got: {out}");
        assert!(out.contains("http_request ✗×1"), "got: {out}");
    }

    #[test]
    fn receipt_flags_a_run_that_called_no_tools() {
        // The toolsmith case: an answer that narrates work while no tool ran.
        let out = format_tool_receipt("toolsmith", &[]);
        assert!(out.contains("none"));
        assert!(out.contains("did NOT happen"), "got: {out}");
    }

    #[test]
    fn tokenize_drops_stopwords_and_short_words() {
        let toks = tokenize("What are the latest Reticulum mesh developments?");
        assert!(toks.contains("latest"));
        assert!(toks.contains("reticulum"));
        assert!(toks.contains("mesh"));
        // Stopwords and short words are filtered.
        assert!(!toks.contains("what"));
        assert!(!toks.contains("are"));
        assert!(!toks.contains("the"));
    }

    #[test]
    fn token_overlap_matches_by_substring() {
        // `research` (goal) must hook `researcher` (agent name) — the exact-token
        // matching this replaced returned zero here, the bug the smoke test found.
        let goal = tokenize("research the latest off-grid mesh networking hardware");
        let researcher = tokenize("researcher current external info web search");
        assert!(
            token_overlap(&goal, &researcher) >= 1,
            "research goal should hook the researcher via substring"
        );
    }

    #[test]
    fn token_overlap_discriminates_research_from_code() {
        let goal = tokenize("research the latest off-grid mesh networking hardware");
        let researcher = tokenize("researcher current external info web search");
        // code-analyst specialty AFTER the reword (no ambiguous "source").
        let coder = tokenize(
            "code-analyst questions about this codebase code structure git history static analysis",
        );
        assert!(
            token_overlap(&goal, &researcher) > token_overlap(&goal, &coder),
            "a research goal must prefer the researcher over the code analyst"
        );
    }

    #[test]
    fn name_weight_prefers_researcher_over_knowledge_worker_for_research() {
        // Both specialties contain "search" (and research ⊃ search), so a raw
        // overlap ties them; weighting the NAME match must break the tie toward
        // the researcher. Mirrors the scoring in select_agent.
        const NAME_WEIGHT: usize = 2;
        let goal = tokenize("research the latest off-grid mesh networking hardware");
        let score = |name: &str, spec: &str| {
            NAME_WEIGHT * token_overlap(&goal, &tokenize(name))
                + token_overlap(&goal, &tokenize(spec))
        };
        let researcher = score(
            "researcher",
            "current external info web search knowledge graph",
        );
        let kworker = score(
            "knowledge-worker",
            "what the brain already knows search synthesise stored notes knowledge graph",
        );
        assert!(
            researcher > kworker,
            "research goal should prefer researcher ({researcher}) over knowledge-worker ({kworker})"
        );
    }

    #[test]
    fn generalist_default_prefers_researcher() {
        let roster = vec![
            ("code-analyst".to_string(), "codebase".to_string()),
            ("researcher".to_string(), "web".to_string()),
        ];
        assert_eq!(generalist_default(&roster).as_deref(), Some("researcher"));
        // Falls back to the first spawnable when there is no researcher.
        let roster = vec![("code-analyst".to_string(), "codebase".to_string())];
        assert_eq!(generalist_default(&roster).as_deref(), Some("code-analyst"));
        assert_eq!(generalist_default(&[]), None);
    }
}
