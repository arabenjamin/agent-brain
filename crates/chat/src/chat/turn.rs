//! One chat turn: profile/model resolution, the fallback ladder, and one provider attempt.

use super::*;

/// Keep the ladder rungs the routing policy admits for this work, in order, and
/// say why each refused rung was refused. Shared by the top-level chat ladder and
/// the sub-agent ladder so both walk only permitted models.
pub(super) fn admit_rungs(
    candidates: Vec<LlmConfig>,
    ctx: RouteContext,
) -> (Vec<LlmConfig>, Vec<String>) {
    let policy = RoutingPolicy::from_env();
    let mut kept = Vec::new();
    let mut refused = Vec::new();
    for c in candidates {
        match policy.admit(&c, ctx) {
            Ok(()) => kept.push(c),
            Err(reason) => {
                warn!(
                    model = %c.model,
                    provider = %c.provider_label(),
                    purpose = %ctx.purpose,
                    data_class = %ctx.data_class,
                    reason = %reason,
                    "routing policy refused a chat rung"
                );
                refused.push(format!("{} ({}): {reason}", c.model, c.provider_label()));
            }
        }
    }
    (kept, refused)
}

/// Re-judge a rung just before it is tried. [`admit_rungs`] judges the ladder
/// once, when the turn starts; a rung benched since (an earlier rung's 429
/// said the whole account is out of quota) must be skipped rather than spend
/// a call on a known failure.
pub(super) fn still_admitted(cfg: &LlmConfig, ctx: RouteContext) -> bool {
    match RoutingPolicy::from_env().admit(cfg, ctx) {
        Ok(()) => true,
        Err(reason) => {
            warn!(
                model = %cfg.model,
                provider = %cfg.provider_label(),
                reason = %reason,
                "ladder rung refused since the turn began — skipping it"
            );
            false
        }
    }
}

/// What one model's attempt at a turn amounted to.
///
/// The distinction the fallback ladder turns on is not "did it error" but **did
/// anything reach the client**. A model that streamed a token, a tool call, or
/// a message owns the turn even if it failed afterwards: re-running the turn on
/// another model would re-execute its tool calls and emit a second answer after
/// the first. Only a turn that produced *nothing* is safe to hand onward, and
/// that is also the only turn worth handing onward.
#[derive(Debug)]
pub(super) enum AttemptOutcome {
    /// Something reached the client. The turn is this model's, for better or worse.
    Delivered,
    /// Nothing reached the client, so another model may still answer this turn.
    /// `error` is whatever the loop tried to report, held back so the ladder can
    /// decide whether the user ever sees it.
    Unanswered { error: Option<String> },
    /// The model ran tools — every one of them read-only — and then failed
    /// before answering. The next rung continues from `calls` instead of the
    /// turn dying: re-running reads is harmless, and handing their results over
    /// means it need not. A turn that ran any write stays `Delivered`, so a
    /// write is never repeated by a second model.
    Interrupted {
        error: String,
        calls: Vec<PriorCall>,
    },
}

impl ChatService {
    /// Run one turn on one model and report whether the client saw anything.
    ///
    /// The provider loops are unchanged and still stream straight through: this
    /// relays their events to `sink` as they arrive, so a working model's tokens
    /// are not buffered waiting for a verdict. Two events are treated specially.
    /// `Done` is swallowed — `forward_chat_events` re-emits exactly one at the
    /// very end, so a loop that ends early cannot close the stream on a turn the
    /// ladder intends to continue. `Error` is **held back** until the relay knows
    /// whether anything else got through: forwarded at the end if the model did
    /// deliver (its explanation belongs with its output), and returned to the
    /// caller unsent if it did not, so a recovered turn is not narrated with the
    /// failure that preceded it.
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_one_attempt(
        &self,
        cfg: LlmConfig,
        tools: Vec<agent_brain_protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        system_prompt: String,
        allow_delegate: bool,
        max_iterations: usize,
        sink: &mpsc::Sender<ChatEvent>,
    ) -> AttemptOutcome {
        let (attempt_tx, mut attempt_rx) = mpsc::channel::<ChatEvent>(128);

        let provider_loop = async {
            match cfg.provider {
                LlmProviderType::Anthropic => {
                    self.run_anthropic_loop(
                        cfg,
                        tools,
                        handler,
                        request,
                        system_prompt,
                        attempt_tx,
                        allow_delegate,
                        max_iterations,
                    )
                    .await
                }
                LlmProviderType::Ollama => {
                    self.run_ollama_tool_loop(
                        cfg,
                        tools,
                        handler,
                        request,
                        system_prompt,
                        attempt_tx,
                        allow_delegate,
                        max_iterations,
                    )
                    .await
                }
                LlmProviderType::OllamaCloud
                | LlmProviderType::LlamaCpp
                | LlmProviderType::OpenAiCompat
                | LlmProviderType::Gemini => {
                    // All speak the OpenAI-compatible chat/completions wire
                    // (Gemini at its own path), so one tool-call-accumulating
                    // adapter drives them. Gemini used a <tool_call> text
                    // protocol until 2026-10-11 (CORE_AND_PLUGINS_PLAN 6a).
                    // `allow_delegate` is true only for the top-level orchestrator
                    // turn; a sub-agent rung passes false (depth cap 1).
                    self.run_openai_loop(
                        cfg,
                        tools,
                        handler,
                        request,
                        system_prompt,
                        attempt_tx,
                        allow_delegate,
                        max_iterations,
                    )
                    .await
                }
            }
        };

        let relay = async {
            // "Answered" means a real assistant answer reached the client — a
            // non-empty `Message` (or streamed answer `Token`s). Tool calls, tool
            // results, and `Thinking` are PROGRESS, not an answer. A turn that ran
            // tools and then went empty mid-loop has streamed events but produced
            // no answer, and must be reported `Unanswered` so the caller walks the
            // fallback ladder to the next model rather than ending the turn on an
            // error banner. (This is the "apply to #N choked" bug: gpt-oss went
            // empty right after `search_notes`, the relay counted the tool events
            // as "delivered", and the ladder never engaged. Progress events still
            // stream to the client; the ladder's "retrying on X" note explains the
            // re-run on the next model.)
            let mut answered = false;
            let mut streamed_progress = false;
            let mut error: Option<String> = None;
            // This model's own tool calls, for a handover if it fails. A
            // sub-agent's calls arrive prefixed `agent:tool` and are skipped:
            // the `delegate` result already carries what the sub-agent found.
            let mut calls: Vec<PriorCall> = Vec::new();
            while let Some(event) = attempt_rx.recv().await {
                match &event {
                    ChatEvent::ToolCall { tool, args } if !tool.contains(':') => {
                        calls.push(PriorCall {
                            tool: tool.clone(),
                            args: args.clone(),
                            success: None,
                            result: String::new(),
                        });
                    }
                    ChatEvent::ToolResult {
                        tool,
                        success,
                        preview,
                        model_saw_chars,
                    } if !tool.contains(':') => {
                        if let Some(c) = calls
                            .iter_mut()
                            .rev()
                            .find(|c| c.tool == *tool && c.success.is_none())
                        {
                            let saw = model_saw_chars.unwrap_or(usize::MAX);
                            c.success = Some(*success);
                            c.result = preview
                                .chars()
                                .take(saw.min(HANDOVER_RESULT_CHARS))
                                .collect();
                        }
                    }
                    _ => {}
                }
                match &event {
                    ChatEvent::Done => continue,
                    ChatEvent::Error { message } => {
                        // Keep the first: it is the one that describes what
                        // actually went wrong, before any cleanup reporting. Note
                        // an EMPTY COMPLETION no longer arrives here — the loops
                        // emit only `Done` for that, precisely so it is
                        // distinguishable from a hard error below.
                        if error.is_none() {
                            error = Some(message.clone());
                        }
                        continue;
                    }
                    ChatEvent::Message { content } if !content.trim().is_empty() => {
                        answered = true;
                    }
                    ChatEvent::Token { .. } => {
                        answered = true;
                    }
                    // Tool calls, tool results, and Thinking are PROGRESS, not an
                    // answer.
                    _ => streamed_progress = true,
                }
                let _ = sink.send(event).await;
            }

            let empty_completion = error.as_deref() == Some(EMPTY_COMPLETION_MESSAGE);
            if answered {
                // A real answer went out; forward any trailing/cleanup error too.
                if let Some(message) = error {
                    let _ = sink.send(ChatEvent::Error { message }).await;
                }
                AttemptOutcome::Delivered
            } else if let Some(message) = error.clone().filter(|_| {
                !empty_completion
                    && streamed_progress
                    && !calls.is_empty()
                    && calls.iter().all(|c| call_is_read_only(&c.tool, &c.args))
            }) {
                // A HARD error after read-only tools: hand the results to the
                // next rung instead of ending the turn (the 2026-09-26 case —
                // a gemini 503 on the round after a delegate).
                AttemptOutcome::Interrupted {
                    error: message,
                    calls,
                }
            } else if error.is_some() && !empty_completion && streamed_progress && !calls.is_empty()
            {
                // A HARD error after a WRITE ran: this model owns the turn.
                // Handing it onward could repeat the write under a second
                // model, so report the failure in place.
                if let Some(message) = error {
                    let _ = sink.send(ChatEvent::Error { message }).await;
                }
                AttemptOutcome::Delivered
            } else {
                // Fall back to the next ladder rung, error held back for the
                // candidate loop to surface only if every rung is spent:
                //  - an EMPTY COMPLETION (the model produced nothing), even after
                //    running read-only tools first — the "apply to #N choked" fix:
                //    gpt-oss went empty after search_notes and the ladder never
                //    engaged because the tool events looked like a delivered turn;
                //  - a hard error with NO progress (a dead primary), as before.
                AttemptOutcome::Unanswered { error }
            }
        };

        // The loop owns `attempt_tx`; the relay ends when that drop closes the
        // channel, so these must run concurrently rather than in sequence.
        let (_, outcome) = tokio::join!(provider_loop, relay);
        outcome
    }

    /// Record one chat LLM call in the usage ledger (tool_name = "chat").
    ///
    /// `error` is the failure's text when the call failed: it is classified into
    /// `error_kind` (`rate_limited`, `server_error`, …) so the budget can cool the
    /// model down and the next turn skips it. Chat used to record every failure
    /// with a NULL kind, so a model answering 503 was re-tried on every turn.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn record_llm_call(
        &self,
        provider: &str,
        model: &str,
        success: bool,
        duration_ms: i64,
        tokens_in: Option<i64>,
        tokens_out: Option<i64>,
        error: Option<&str>,
    ) {
        let label = usage_tool_label();
        let kind = (!success).then(|| {
            error
                .map(agent_brain_llm::shared_llm::failure_kind_of)
                .unwrap_or("other")
        });
        agent_brain_llm::budget::record_call(
            self.telemetry.as_ref(),
            provider,
            model,
            Some(label.as_str()),
            success,
            Some(duration_ms),
            tokens_in,
            tokens_out,
            kind,
            error,
        );
    }

    /// Resolve a context profile's `model_preference` (+ optional
    /// `provider_hint`) to a full [`LlmConfig`] for this turn.
    ///
    /// The provider and endpoint come from the model catalog (and, through
    /// [`config_for_catalog_entry`], the environment) so a profile pointing at
    /// `gemini-2.5-flash` picks up the Gemini key without the checked-in YAML
    /// ever carrying a credential. Resolution is: exact match on the catalog key
    /// or model string, then a single substring match so legacy aliases
    /// (`sonnet`, `haiku`) still resolve to their canonical model, then the
    /// profile's `provider_hint` for a model not in the catalog.
    ///
    /// Returns `None` — meaning "keep the chat default" — when the preference is
    /// blank or cannot be resolved. A profile is never allowed to fail a turn by
    /// naming a model that does not exist; it degrades to the default, and the
    /// fallback ladder still covers a resolved-but-unreachable model.
    /// The model's context window in tokens, from the DuckDB catalog, or `None`
    /// when it is not catalogued.
    ///
    /// This is what makes the tool-result budget follow the MODEL rather than a
    /// constant: a 1M-token orchestrator and a 16384-token local worker were
    /// being handed the identical ration, which is indefensible in either
    /// direction — it starves the big one and flatters the small one.
    pub(super) fn context_window_tokens(&self, model: &str) -> Option<usize> {
        let models = self.telemetry.as_ref()?.list_models().ok()?;
        models
            .iter()
            .find(|m| m["model"].as_str() == Some(model) || m["name"].as_str() == Some(model))
            .and_then(|m| m["context_window"].as_i64())
            .filter(|w| *w > 0)
            .map(|w| w as usize)
    }

    pub(super) async fn resolve_profile_model(
        &self,
        base: &LlmConfig,
        model_pref: &str,
        provider_hint: Option<&str>,
    ) -> Option<LlmConfig> {
        let model_pref = model_pref.trim();
        if model_pref.is_empty() {
            return None;
        }

        let resolved: Option<(String, String)> = self.telemetry.as_ref().and_then(|tc| {
            let models = tc.list_models().ok()?;
            let want = model_pref.to_lowercase();
            let exact = models.iter().find(|m| {
                m["model"].as_str() == Some(model_pref) || m["name"].as_str() == Some(model_pref)
            });
            let matched = exact.or_else(|| {
                models.iter().find(|m| {
                    let name = m["name"].as_str().unwrap_or("").to_lowercase();
                    let model = m["model"].as_str().unwrap_or("").to_lowercase();
                    name.contains(&want) || model.contains(&want)
                })
            })?;
            Some((
                matched["provider"].as_str().unwrap_or("").to_string(),
                matched["model"].as_str().unwrap_or(model_pref).to_string(),
            ))
        });

        let (provider, model) = match resolved {
            // A known model: the catalog is authoritative for its provider, so
            // `provider_hint` is not consulted (it is routinely stale, e.g. a
            // profile naming `haiku` with a leftover `provider_hint: ollama`).
            Some(pm) => pm,
            None => match provider_hint.map(str::trim).filter(|s| !s.is_empty()) {
                Some(ph) => (ph.to_string(), model_pref.to_string()),
                None => {
                    warn!(
                        model_preference = model_pref,
                        "chat: profile model_preference not in catalog and no provider_hint — \
                         using the default chat model for this turn"
                    );
                    return None;
                }
            },
        };

        Some(agent_brain_llm::model_router::config_for_catalog_entry(
            base, &provider, &model,
        ))
    }

    /// Run the agentic loop for a chat request, emitting events on `tx`.
    ///
    /// When `request.session_id` is set the user message and the final
    /// assistant response are persisted to Neo4j working memory automatically.
    pub async fn run(&self, request: ChatRequest, tx: mpsc::Sender<ChatEvent>) {
        // Deployment-level default for the worker/voice split. Without this,
        // synthesis is reachable only by a caller that sets it per request —
        // which the UI does not do, so the mechanism sat unreachable in
        // production while the worker model both gathered and spoke.
        let mut request = request;
        if request.synthesis_provider.is_none()
            && let Ok(p) = std::env::var("CHAT_SYNTHESIS_PROVIDER")
            && !p.trim().is_empty()
        {
            request.synthesis_provider = Some(p.trim().to_string());
            if request.synthesis_model.is_none()
                && let Ok(m) = std::env::var("CHAT_SYNTHESIS_MODEL")
                && !m.trim().is_empty()
            {
                request.synthesis_model = Some(m.trim().to_string());
            }
        }
        let request = request;

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

        // The fallback profile when neither the request nor auto-assign picks one.
        // Env-driven (CHAT_DEFAULT_PROFILE) so the front door can be swapped
        // without a rebuild.
        let default_profile = agent_brain_work::context_builder::default_profile_name();

        // Resolve the effective profile name: use the one from the request, or
        // auto-assign based on the message content when none is provided.
        let resolved_profile: Option<String> = if has_explicit_tools {
            None
        } else if request.context_profile.is_some() {
            request.context_profile.clone()
        } else if let Some(ref cb) = cb_opt {
            // A session a workflow opened for replies belongs to the profile
            // that declares its prefix (`session_prefixes`). auto_assign sees
            // ONLY the reply text, so a terse reply ("#1", "the Harvey one")
            // scored 0 and fell back to `general`, which lacked the tools the
            // reply needed, and the flow silently failed. An explicit
            // `context_profile` (handled above) still wins.
            let pinned = match session_id.as_deref() {
                Some(s) => cb.profile_for_session(s).await,
                None => None,
            };
            if pinned.is_some() {
                pinned
            } else if std::env::var("CHAT_DEFAULT_PROFILE")
                .ok()
                .is_some_and(|v| !v.trim().is_empty())
            {
                // An explicit CHAT_DEFAULT_PROFILE means the deployment has a
                // designated front door, and the front door IS the router: it
                // reads the live spawnable roster and picks a worker with
                // `delegate`. Running auto_assign here would pre-empt that with
                // a keyword match and swap in a *persona* instead — observed
                // 2026-09-11, when "What did we learn today?" scored
                // `knowledge-worker` and answered from its own `recall_recent`,
                // never reaching the orchestrator at all. Keyword routing and
                // roster delegation are two routers; running both means the
                // dumber one wins, because it runs first.
                Some(default_profile.clone())
            } else {
                Some(cb.auto_assign(&request.message).await)
            }
        } else {
            None
        };

        // A profile may pin a model (`model_preference` + optional
        // `provider_hint`); resolved here and applied as this turn's active
        // model below. `None` = inherit the chat default.
        let mut profile_model: Option<LlmConfig> = None;
        // The profile's own step-down models (`fallback_models`), resolved.
        let mut profile_fallbacks: Vec<LlmConfig> = Vec::new();
        // The profile's data class — private unless it says public — decides
        // which providers the routing policy may send this turn to.
        let mut turn_class = DataClass::Private;

        // The front door's router spec, when its profile declares one.
        let mut profile_routing = None;
        let (tools, profile_system_prompt, profile_notes) = if has_explicit_tools {
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
                profile_routing = bundle.profile.routing.clone();
                turn_class = bundle.profile.data_class.unwrap_or_default();
                // Honour the profile's model_preference for this turn, using the
                // current chat config as the base so timeouts, temperature, and
                // the always-local embedding settings carry over.
                if let Some(pref) = bundle
                    .profile
                    .model_preference
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                {
                    let base = config.clone().unwrap_or_default();
                    profile_model = self
                        .resolve_profile_model(&base, pref, bundle.profile.provider_hint.as_deref())
                        .await;
                }
                for name in bundle
                    .profile
                    .fallback_models
                    .iter()
                    .map(|n| n.trim())
                    .filter(|n| !n.is_empty())
                {
                    let base = config.clone().unwrap_or_default();
                    match self.resolve_profile_model(&base, name, None).await {
                        Some(c) => profile_fallbacks.push(c),
                        None => warn!(
                            profile = %bundle.profile.name,
                            model = %name,
                            "profile fallback_models entry is not in the catalog — skipped"
                        ),
                    }
                }
                (filtered, prompt, notes)
            } else {
                (all_tools, None, Vec::new())
            }
        } else {
            (all_tools, None, Vec::new())
        };

        // Give the orchestrator the `delegate` pseudo-tool when sub-agent
        // delegation is enabled. Only when the caller did not pin an explicit
        // tool allowlist — an explicit allowlist is a deliberate scope the
        // orchestrator role should not silently widen. Sub-agents never receive
        // it (their toolsets are built without it in `run_subagent`). The roster
        // of workers is built live from the spawnable profiles; with none, the
        // tool would be useless, so it is not added.
        let mut tools = tools;
        let mut spawnable = Vec::new();
        if subagent_enabled() && !has_explicit_tools {
            spawnable = match self.context_builder.read().await.clone() {
                Some(cb) => cb.spawnable_profiles().await,
                None => Vec::new(),
            };
            if !spawnable.is_empty() {
                tools.push(delegate_tool_def(&format_roster(&spawnable)));
            }
        }

        // A resolved profile model overrides the chat default for this turn. The
        // fallback ladder still applies on top of it (an unreachable profile
        // model — e.g. an Anthropic pin with no key — falls down the ladder).
        // The routing policy judges the pin for this turn's data class before it
        // may replace the default. A refused pin is reported in the stream and the
        // turn keeps the chat default, which the ladder walk judges in turn.
        let route_ctx = RouteContext::new(Purpose::Chat, turn_class);
        // A model chosen for this turn replaces the profile's pin, and is never
        // quietly swapped: when it cannot run, the turn says why and stops.
        if let Some(chosen) = request
            .model
            .as_deref()
            .map(str::trim)
            .filter(|m| !m.is_empty())
        {
            let rows = self
                .telemetry
                .as_ref()
                .map(agent_brain_llm::model_router::catalog_models)
                .unwrap_or_default();
            let base = config.clone().unwrap_or_default();
            match agent_brain_llm::model_router::pinned_config(&rows, chosen, &base, route_ctx) {
                Ok(cfg) => profile_model = Some(cfg),
                Err(reason) => {
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: format!(
                                "The model chosen for this turn cannot run: {reason}."
                            ),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            }
        }
        let mut pin_refusal: Option<String> = None;
        let profile_model = match profile_model {
            Some(m) => match RoutingPolicy::from_env().admit(&m, route_ctx) {
                Ok(()) => Some(m),
                Err(reason) => {
                    warn!(
                        model = %m.model,
                        provider = %m.provider_label(),
                        data_class = %turn_class,
                        reason = %reason,
                        "chat: routing policy refused the profile's model_preference — \
                         using the chat default"
                    );
                    pin_refusal = Some(format!(
                        "⚙ profile model {} refused by routing policy — {reason}; using the chat default",
                        m.model
                    ));
                    None
                }
            },
            None => None,
        };
        // A refused pin means the profile's own step-downs are next in line,
        // ahead of the chat default.
        let pin_was_refused = pin_refusal.is_some();
        let config = match profile_model {
            Some(m) => Some(m),
            None => config,
        };

        // Compose once here so every provider loop runs the same prompt.
        let mut system_prompt =
            build_system_prompt(profile_system_prompt.as_deref(), &profile_notes);

        // A front door that declares `routing:` gets the turn's route before it
        // runs (GRAPH_CONTEXT_PLAN Phase 3). It can only delegate when the
        // roster exists, so no roster means no routing.
        let route = match &profile_routing {
            Some(spec) if super::route::enabled() && !spawnable.is_empty() => {
                let prev = previous_turn(&request.history);
                let topics = agent_brain_graph::topics::Topics::global();
                super::route::route(spec, &spawnable, &topics.defs, prev, &user_message).await
            }
            _ => None,
        };
        // A behavior route is run here, by code (Phase B): its rows become the
        // turn's context and the front door only phrases them. Any other route,
        // or a behavior that fails, becomes a hint; a failed behavior's hint is
        // the stored agent.
        let mut behavior_line: Option<String> = None;
        // The route as logged, kept for the turn's `turn:` row at the end.
        let mut route_log: Option<Value> = None;
        if let (Some(r), Some(spec)) = (&route, &profile_routing) {
            let mut ran: Option<(Value, Value)> = None;
            if let Some(b) = super::route::behavior(spec, &r.plan[0]) {
                let handler = self.tool_handler.read().await.clone();
                if let Some(h) = handler {
                    let t0 = std::time::Instant::now();
                    let turn =
                        super::route::turn_text(previous_turn(&request.history), &user_message);
                    match super::route::run_behavior(b, &h, &turn).await {
                        Ok((args, result)) => {
                            system_prompt.push_str(&super::route::context_block(&args, &result));
                            behavior_line = Some(format!(
                                "⚙ behavior: lookup {args} → {} rows{} ({} ms)",
                                result["count"],
                                if result["wide"].is_string() {
                                    ", widened"
                                } else {
                                    ""
                                },
                                t0.elapsed().as_millis()
                            ));
                            ran = Some((args, result));
                            // The behavior is the turn's retrieval: the front door
                            // phrases its rows and cannot hand the turn to an agent
                            // (its prompt says to always delegate; it did, on the
                            // first live run, and ignored the rows).
                            tools.retain(|t| t.name != "delegate");
                        }
                        Err(e) => {
                            tracing::warn!(behavior = %b.name, error = %e, "chat: the behavior failed; the turn goes to the stored agent");
                        }
                    }
                }
            }
            if ran.is_none() {
                let plan = if super::route::behavior(spec, &r.plan[0]).is_some() {
                    vec![spec.stored.clone()]
                } else {
                    r.plan.clone()
                };
                system_prompt.push_str(&super::route::hint(&plan, &r.named));
            }
            tracing::info!(plan = ?r.plan, picked = %r.picked, checked = ?r.checked, why = %r.why, ms = r.ms, behavior = ?ran.as_ref().map(|(a, _)| a), "chat: routed the turn");
            let logged = json!({"plan": r.plan, "picked": r.picked, "checked": r.checked, "why": r.why, "named": r.named,
                                "behavior": ran.as_ref().map(|(a, res)| json!({"call": a, "count": res["count"], "wide": res["wide"], "empty": res["empty"]}))});
            route_log = Some(logged.clone());
            if let Some(t) = &self.telemetry {
                let _ = t.log_interaction(
                    &format!(
                        "route: {}",
                        json!({"message": user_message, "session_id": session_id})
                    ),
                    &logged.to_string(),
                    None,
                    true,
                    r.ms,
                    "router",
                );
            }
        }

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
        let (inner_tx, inner_rx) = mpsc::channel::<ChatEvent>(128);
        let (result_tx, mut result_rx) = mpsc::channel::<String>(1);

        // Forwarding task: relay every event to the caller; capture final text.
        //
        // It also owns the only detector for a *dropped turn* — a turn where the
        // user's message was banked, a provider loop returned, and nothing was
        // ever written back. `tx` is moved in here and dropped when this task
        // ends, so this is the last point at which anything can still be said to
        // the client; the code after `.await` on the loops below cannot reach it.
        let dropped_turn_session = session_id.clone();
        let dropped_turn_snippet: String = user_message.chars().take(120).collect();
        let citations: Option<Arc<dyn CitationChecker>> = self
            .tool_handler
            .read()
            .await
            .clone()
            .map(|h| Arc::new(GraphCitationChecker(h)) as Arc<dyn CitationChecker>);
        let trace: super::events::TurnTrace = Default::default();
        let turn_started = std::time::Instant::now();
        tokio::spawn(forward_chat_events(
            inner_rx,
            tx,
            result_tx,
            dropped_turn_session,
            dropped_turn_snippet,
            citations,
            Some(trace.clone()),
        ));

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
                        "⚙ provider={} | model={} | profile={} | tools={} | prompt={} | preload={} | mode={}",
                        provider_str,
                        config
                            .as_ref()
                            .map(|c| c.model.as_str())
                            .unwrap_or("unknown"),
                        resolved_profile.as_deref().unwrap_or(&default_profile),
                        tools.len(),
                        if profile_system_prompt.is_some() {
                            "profile"
                        } else {
                            "base"
                        },
                        profile_notes.len(),
                        mode,
                    ),
                })
                .await;
            if let Some(r) = &route {
                let _ = inner_tx
                    .send(ChatEvent::Thinking {
                        content: format!(
                            "⚙ route: {} ({}, {} ms)",
                            r.plan.join(" → "),
                            r.why,
                            r.ms
                        ),
                    })
                    .await;
            }
            if let Some(line) = &behavior_line {
                let _ = inner_tx
                    .send(ChatEvent::Thinking {
                        content: line.clone(),
                    })
                    .await;
            }
        }

        match config {
            None => {
                let _ = inner_tx
                    .send(ChatEvent::Error {
                        message: "No LLM provider configured. Set CHAT_DEFAULT_PROFILE or a provider in models.yaml.".into(),
                    })
                    .await;
                let _ = inner_tx.send(ChatEvent::Done).await;
            }
            Some(active) => {
                // The active model first, then the ladder. Each attempt runs only
                // if every attempt before it streamed nothing at all.
                // A rung is identified by where it is *called*, not just by the
                // model name: the same model on a different endpoint is a
                // different rung (a local mirror of a cloud model is the case
                // that matters), and deduping on the name alone would silently
                // drop it.
                let rung_id = |c: &LlmConfig| {
                    (
                        c.provider,
                        c.base_url.clone().unwrap_or_default(),
                        c.model.clone(),
                    )
                };
                let mut candidates = if pin_was_refused {
                    let mut c = profile_fallbacks.clone();
                    c.push(active);
                    c
                } else {
                    let mut c = vec![active];
                    c.extend(profile_fallbacks.iter().cloned());
                    c
                };
                let mut seen = Vec::new();
                candidates.retain(|c| {
                    let id = rung_id(c);
                    let fresh = !seen.contains(&id);
                    if fresh {
                        seen.push(id);
                    }
                    fresh
                });
                for cfg in &self.fallback_ladder {
                    if !candidates.iter().any(|c| rung_id(c) == rung_id(cfg)) {
                        candidates.push(cfg.clone());
                    }
                }

                if let Some(note) = pin_refusal.take() {
                    let _ = inner_tx.send(ChatEvent::Thinking { content: note }).await;
                }
                // Only rungs the routing policy admits for this turn are walked.
                // With none left the turn fails loudly with every reason — it
                // never falls through to a provider the policy refused.
                let (candidates, refused) = admit_rungs(candidates, route_ctx);
                if candidates.is_empty() {
                    let _ = inner_tx
                        .send(ChatEvent::Error {
                            message: format!(
                                "No model may answer this {turn_class} chat turn under the routing policy: {}",
                                refused.join("; ")
                            ),
                        })
                        .await;
                    let _ = inner_tx.send(ChatEvent::Done).await;
                }

                let last = candidates.len().saturating_sub(1);
                let mut last_error: Option<String> = None;
                // Read-only work a failed rung did, handed to the next one.
                let mut prior_calls: Vec<PriorCall> = Vec::new();

                for (i, cfg) in candidates.into_iter().enumerate() {
                    // The last rung is always tried, so the turn still ends
                    // through the every-rung-spent report below.
                    if i > 0 && i != last && !still_admitted(&cfg, route_ctx) {
                        continue;
                    }
                    if i > 0 && !prior_calls.is_empty() {
                        warn!(
                            model = %cfg.model,
                            provider = %cfg.provider,
                            rung = i,
                            handed_over = prior_calls.len(),
                            previous_error = last_error.as_deref().unwrap_or("(none reported)"),
                            "Chat model failed mid-turn — handing its tool results to the next model"
                        );
                        let _ = inner_tx
                            .send(ChatEvent::Thinking {
                                content: format!(
                                    "⚙ the previous model failed mid-turn — continuing on {} ({}) with the {} tool result(s) it already gathered",
                                    cfg.model,
                                    cfg.provider,
                                    prior_calls.len()
                                ),
                            })
                            .await;
                    } else if i > 0 {
                        warn!(
                            model = %cfg.model,
                            provider = %cfg.provider,
                            rung = i,
                            previous_error = last_error.as_deref().unwrap_or("(none reported)"),
                            "Chat turn produced nothing — falling back to the next model"
                        );
                        // Say so in the stream. A turn that quietly changes model
                        // is a turn whose answer cannot be attributed later, and
                        // the ladder is exactly the state a reader needs when the
                        // reply is slower or worse than usual.
                        let _ = inner_tx
                            .send(ChatEvent::Thinking {
                                content: format!(
                                    "⚙ no response from the previous model — retrying on {} ({})",
                                    cfg.model, cfg.provider
                                ),
                            })
                            .await;
                    }

                    let model = cfg.model.clone();
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
                    // Scoped per attempt: every tool, synthesis pass and sub-agent
                    // this turn runs is judged as this turn's work.
                    let outcome = ROUTE_CONTEXT
                        .scope(
                            route_ctx,
                            self.run_one_attempt(
                                cfg,
                                tools.clone(),
                                handler.clone(),
                                request.clone(),
                                attempt_prompt,
                                true,
                                MAX_TOOL_ITERATIONS,
                                &inner_tx,
                            ),
                        )
                        .await;
                    let error = match outcome {
                        AttemptOutcome::Delivered => break,
                        AttemptOutcome::Unanswered { error } => error,
                        AttemptOutcome::Interrupted { error, calls } => {
                            prior_calls.extend(calls);
                            Some(error)
                        }
                    };
                    last_error = error.or(last_error);
                    if i == last {
                        // Every rung is spent. Report the concrete failure rather
                        // than letting the dropped-turn detector emit its generic one.
                        warn!(
                            model = %model,
                            rungs_tried = i + 1,
                            "Every chat model produced nothing for this turn"
                        );
                        let _ = inner_tx
                            .send(ChatEvent::Error {
                                message: last_error
                                    .clone()
                                    .unwrap_or_else(|| EMPTY_COMPLETION_MESSAGE.to_string()),
                            })
                            .await;
                        let _ = inner_tx.send(ChatEvent::Done).await;
                    }
                }
            }
        }
        // Closing `inner_rx` is what lets the forwarding task finish and hand
        // back the assistant text below, so this drop is load-bearing, not
        // tidiness. It used to happen implicitly because each match arm *moved*
        // `inner_tx` into a provider loop; the ladder passes it by reference so
        // one turn can run several attempts, which left `run()` holding the last
        // sender and `result_rx.recv()` waiting on a channel that would never
        // close. The symptom was a chat turn that hung indefinitely — the exact
        // failure this whole change set exists to remove.
        drop(inner_tx);

        // Wait for the forwarder to return the captured assistant text.
        let final_text = result_rx.recv().await.unwrap_or_default();

        // One row per turn for route mining (GRAPH_CONTEXT_PLAN Phase B): what
        // the router chose, every tool the turn ran, and whether it answered.
        // A fallback turn whose agent ran the same calls every time is a
        // behavior waiting to be written.
        if let Some(t) = &self.telemetry {
            let calls = trace.lock().map(|c| c.clone()).unwrap_or_default();
            let agents: Vec<String> = calls
                .iter()
                .filter(|c| c["tool"] == "delegate")
                .filter_map(|c| c["args"]["agent"].as_str().map(String::from))
                .collect();
            let _ = t.log_interaction(
                &format!(
                    "turn: {}",
                    json!({"message": user_message, "session_id": session_id})
                ),
                &json!({
                    "profile": resolved_profile.as_deref().unwrap_or(&default_profile),
                    "route": route_log,
                    "agents": agents,
                    "calls": calls,
                    "answered": !final_text.trim().is_empty(),
                    "reply_chars": final_text.chars().count(),
                })
                .to_string(),
                None,
                !final_text.trim().is_empty(),
                turn_started.elapsed().as_millis() as u64,
                "chat",
            );
        }

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
            let profile = resolved_profile.as_deref().unwrap_or(&default_profile);
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
}

/// The last exchange before this message: the user's turn and the reply to
/// it, for a router reading a follow-up ("yes please", "check all three").
fn previous_turn(history: &[super::ChatHistoryMessage]) -> Option<(&str, &str)> {
    let a = history.iter().rposition(|m| m.role == "assistant")?;
    let q = history[..a].iter().rposition(|m| m.role == "user")?;
    Some((history[q].content.as_str(), history[a].content.as_str()))
}
