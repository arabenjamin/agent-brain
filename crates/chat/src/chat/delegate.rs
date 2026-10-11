//! The `delegate` pseudo-tool and the sub-agent loop engine (`SubagentLoopRunner`).

use super::*;

/// The pseudo-tool name the orchestrator calls to delegate a sub-task. Never a
/// registered MCP tool — it runs a nested LLM loop, which ToolHandler cannot.
pub const DELEGATE_TOOL: &str = "delegate";

/// Whether in-chat sub-agent delegation is enabled (default OFF for the spike).
pub(super) fn subagent_enabled() -> bool {
    std::env::var("SUBAGENT_ENABLED")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

/// Build the `delegate` roster text from the spawnable profiles — one
/// `- name: specialty` line each (falling back to `description`). Data-driven:
/// a worker joins the roster by setting `spawnable: true` in its YAML, no code
/// change. Used both to fill the tool description and to tell the orchestrator
/// which agents exist.
pub(super) fn format_roster(
    profiles: &[agent_brain_work::context_builder::ContextProfile],
) -> String {
    profiles
        .iter()
        .map(|p| {
            let blurb = p
                .specialty
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(p.description.as_str());
            format!("  - {}: {}", p.name, blurb)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The `delegate` tool definition shown to the orchestrator, with the live
/// roster of spawnable workers injected.
pub(super) fn delegate_tool_def(roster: &str) -> agent_brain_protocol::ToolDefinition {
    agent_brain_protocol::ToolDefinition {
        name: DELEGATE_TOOL.to_string(),
        description: format!(
            "Delegate a self-contained lookup or tool-heavy sub-task to a worker sub-agent and \
             get its findings back in THIS turn. Prefer this over doing multi-step tool work \
             yourself. The sub-agent does NOT see this conversation — put everything it needs \
             into `task`. It returns findings you must then relay to the user, citing any \
             sources it gives. Available agents:\n{roster}"
        ),
        input_schema: json!({
            "type": "object",
            "properties": {
                "agent": {
                    "type": "string",
                    "description": "Which sub-agent to use — one of the names listed above."
                },
                "task": {
                    "type": "string",
                    "description": "A complete, self-contained instruction for the sub-agent (it has no other context)."
                }
            },
            "required": ["agent", "task"]
        }),
    }
}

/// `ChatService` is the loop engine an [`AgentRunner`] drives: `run_bounded_loop`
/// is the same provider dispatch the orchestrator and sub-agent paths already
/// used, and `resolve_agent_model` forwards to the catalog-aware resolver. This
/// is deliberately a trait rather than a move of the loops — see
/// `services/agent_runner.rs` for why the machinery stays here.
#[async_trait::async_trait]
impl SubagentLoopRunner for ChatService {
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
    ) -> LadderReport {
        // Walk [cfg] + the chat fallback ladder, exactly as the top-level chat
        // path does, so a transient provider failure on a sub-agent turn (a 500,
        // a rate-limit, a dropped connection) degrades to the next rung instead of
        // surfacing the raw error as the sub-agent's whole answer. Before this, a
        // single ollama.com 500 mid-loop returned `ok:false` and the delegated
        // task failed on a blip. The ladder is unmetered-cloud → local, so it is
        // consistent with the sub-agent unmetered clamp (no metered/rate-limited
        // rung is introduced by falling back).
        let rung_id = |c: &LlmConfig| {
            (
                c.provider,
                c.base_url.clone().unwrap_or_default(),
                c.model.clone(),
            )
        };
        let mut candidates = vec![cfg];
        for c in &self.fallback_ladder {
            if !candidates.iter().any(|x| rung_id(x) == rung_id(c)) {
                candidates.push(c.clone());
            }
        }
        // The sub-agent's route context (scoped by AgentRunner::run) decides which
        // rungs it may use; a refused rung is skipped, never tried.
        let (candidates, refused) = admit_rungs(candidates, RouteContext::current());
        let mut report = LadderReport {
            attempts: Vec::new(),
            refused,
        };
        if candidates.is_empty() {
            let _ = tx
                .send(ChatEvent::Error {
                    message: format!(
                        "No model may run this sub-agent under the routing policy: {}",
                        report.refused.join("; ")
                    ),
                })
                .await;
            return report;
        }

        let mut last_error: Option<String> = None;
        let mut prior_calls: Vec<PriorCall> = Vec::new();
        for (i, cand) in candidates.into_iter().enumerate() {
            if i > 0 && !still_admitted(&cand, RouteContext::current()) {
                report.refused.push(format!(
                    "{}: refused after an earlier rung failed",
                    cand.model
                ));
                continue;
            }
            let (model, provider) = (cand.model.clone(), cand.provider_label());
            if i > 0 {
                warn!(
                    model = %cand.model,
                    provider = %cand.provider,
                    rung = i,
                    handed_over = prior_calls.len(),
                    previous_error = last_error.as_deref().unwrap_or("(none reported)"),
                    "sub-agent turn produced nothing — falling back to the next model"
                );
                let _ = tx
                    .send(ChatEvent::Thinking {
                        content: format!(
                            "⚙ sub-agent model unavailable — retrying on {} ({})",
                            cand.model, cand.provider
                        ),
                    })
                    .await;
            }
            let attempt_prompt = if prior_calls.is_empty() {
                system_prompt.clone()
            } else {
                format!(
                    "{system_prompt}\n\n{}",
                    handover_block(
                        &prior_calls,
                        last_error.as_deref().unwrap_or("unknown error")
                    )
                )
            };
            match self
                .run_one_attempt(
                    cand,
                    tools.clone(),
                    handler.clone(),
                    request.clone(),
                    attempt_prompt,
                    allow_delegate,
                    max_iterations,
                    &tx,
                )
                .await
            {
                // The rung produced real output; the drain reads it off `tx` and
                // the channel closing on return ends the turn.
                AttemptOutcome::Delivered => {
                    report.attempts.push(RungAttempt {
                        model,
                        provider,
                        error: None,
                    });
                    return report;
                }
                AttemptOutcome::Unanswered { error } => {
                    report.attempts.push(RungAttempt {
                        model,
                        provider,
                        error: Some(
                            error
                                .clone()
                                .unwrap_or_else(|| "produced no answer".to_string()),
                        ),
                    });
                    last_error = error.or(last_error);
                }
                AttemptOutcome::Interrupted { error, calls } => {
                    report.attempts.push(RungAttempt {
                        model,
                        provider,
                        error: Some(error.clone()),
                    });
                    prior_calls.extend(calls);
                    last_error = Some(error);
                }
            }
        }

        // Every rung was spent — report the concrete failure so the caller sees a
        // real error (and builds an honest `ok:false`), not silence.
        if let Some(message) = last_error {
            let _ = tx.send(ChatEvent::Error { message }).await;
        }
        report
    }

    async fn resolve_agent_model(
        &self,
        base: &LlmConfig,
        model_pref: &str,
        provider_hint: Option<&str>,
    ) -> Option<LlmConfig> {
        self.resolve_profile_model(base, model_pref, provider_hint)
            .await
    }
}

impl ChatService {
    /// Run a worker sub-agent synchronously and return `(success, retrieved,
    /// findings)` to fold back into the orchestrator's turn as the `delegate`
    /// tool result. `retrieved` is true when the worker actually ran ≥1
    /// successful tool — the orchestrator uses it to ground its own write guard
    /// (a delegate is a retrieval event the guard cannot otherwise see).
    ///
    /// The sub-agent runs under a curated, `spawnable` profile with a READ-ONLY
    /// toolset and NO `delegate` of its own (depth cap 1). Its progress (tool
    /// calls/results) is forwarded to `main_tx` so the user sees it working; its
    /// final prose is captured and returned, never emitted as the turn's answer
    /// — the orchestrator composes the reply from it. See
    /// project-docs/SUBAGENT_ORCHESTRATION_PLAN.md.
    pub(super) async fn run_subagent(
        &self,
        agent: &str,
        task: &str,
        main_tx: &mpsc::Sender<ChatEvent>,
    ) -> (bool, bool, String) {
        // Thin streaming wrapper over the shared [`AgentRunner`]. Chat sub-agents
        // are read-only (`allow_writes: false`, one writer per turn) and bounded
        // to [`MAX_SUBAGENT_ITERATIONS`]; passing the live chat sink streams the
        // worker's activity to the user. All the orchestration (spawnability
        // check, model resolution, toolset build, drain) now lives in
        // `services/agent_runner.rs`; this maps the structured findings back to
        // the `(ok, retrieved, findings)` tuple `dispatch_tool_call` expects.
        let f = self
            .agent_runner
            .run(
                agent,
                task,
                AgentBudget {
                    max_iterations: MAX_SUBAGENT_ITERATIONS,
                    allow_writes: false,
                    allow_explicit_only: false,
                },
                Some(main_tx.clone()),
            )
            .await;
        (f.ok, f.retrieved, f.findings)
    }

    // ========================================================================
    // Synthesis step — called after the tool-use loop in research mode
    // ========================================================================
}
