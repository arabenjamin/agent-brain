//! The one tool-use round loop every provider runs.
//!
//! A provider supplies only its wire format ([`ProviderTurn`]): make one call,
//! append the assistant turn, append tool results. Everything a turn promises
//! the user lives here once — the write guard, the tool-result budget and
//! markers, the ground-truth receipt, empty-completion retries, malformed-call
//! correction, the tools-off wrap-up round, the worker/voice split, and the
//! no-answer report.
//!
//! These used to be four copies, and they drifted: on 2026-10-02 the text loop
//! (Gemini) sent tool results raw with no budget or markers, the Anthropic and
//! text loops had no wrap-up round and ignored the voice split, the Anthropic
//! loop sent no receipt, and the host-Ollama loop streamed a worker's tokens
//! even when a voice model would answer — the double-answer bug fixed in the
//! OpenAI loop alone.

use super::*;

/// One tool call the model asked for, in provider-neutral form. `id` is the
/// provider's call id where it has one (OpenAI, Anthropic), else empty.
pub(super) struct ToolCallReq {
    pub(super) id: String,
    pub(super) name: String,
    pub(super) args: Value,
}

/// A `<tool_call>` the text protocol could not parse.
pub(super) struct MalformedCall {
    pub(super) tool: Option<String>,
    pub(super) error: String,
}

/// What one model call returned.
#[derive(Default)]
pub(super) struct RoundReply {
    /// The prose the model wrote this round — the answer when there are no
    /// calls, the reasoning that accompanied them when there are.
    pub(super) text: String,
    pub(super) calls: Vec<ToolCallReq>,
    pub(super) malformed: Option<MalformedCall>,
}

/// One executed call, as the provider must feed it back to the model.
pub(super) struct ToolOutcome {
    pub(super) id: String,
    pub(super) success: bool,
    /// Budgeted and annotated (`prepare_tool_result`); never carries `bulk`.
    pub(super) model_text: String,
}

/// A provider's wire format. Implementations keep their own message history.
#[async_trait::async_trait]
pub(super) trait ProviderTurn: Send {
    /// Provider name for logs and error messages (e.g. `OllamaCloud`).
    fn label(&self) -> &'static str;
    fn model(&self) -> &str;

    /// One call over the current history. `offer_tools` is false on the
    /// wrap-up round. When `stream_to` is set, prose goes out as `Token`s as it
    /// arrives; the loop still sends the full `Message` afterward. An `Err` is
    /// the message the user sees; the call is already recorded in the ledger.
    async fn call(
        &mut self,
        svc: &ChatService,
        offer_tools: bool,
        stream_to: Option<&mpsc::Sender<ChatEvent>>,
    ) -> Result<RoundReply, String>;

    fn push_user(&mut self, text: String);
    /// Record the model's turn — its calls, or the malformed text it wrote.
    fn push_assistant(&mut self, reply: &RoundReply);
    fn push_tool_results(&mut self, results: Vec<ToolOutcome>);
    /// Everything the model has been given so far (system prompt, history,
    /// this turn's context), as text: what its cited links are checked
    /// against. Default: nothing, so every link would be marked.
    fn transcript(&self) -> String {
        String::new()
    }
    /// Bound history growth between rounds. Default: keep everything.
    fn trim_history(&mut self) {}
    /// Registry name of the provider this turn bills, for the per-call spend
    /// check. `None` skips it (the default: local and free loops).
    fn provider_name(&self) -> Option<String> {
        None
    }
}

/// Why a paid provider must not take another call right now, or `None`.
///
/// The routing policy admits a model once, when a turn or agent run starts,
/// and nothing re-checked the spend cap between rounds: on 2026-10-05 a single
/// Claude Sonnet sub-agent run spent $2.03 against a $1.00 daily cap, 16 calls
/// on one admission. Checked before every call, a run can now overshoot by at
/// most the one call that crosses the cap.
fn paid_cap_refusal(provider: &str, model: &str) -> Option<String> {
    use agent_brain_llm::provider_registry::{self, Billing};
    let entry = provider_registry::get(provider)?;
    if entry.billing != Billing::Paid {
        return None;
    }
    agent_brain_llm::budget::verdict(provider, model, &entry).err()
}

impl ChatService {
    #[allow(clippy::too_many_arguments)]
    pub(super) async fn run_round_loop<P: ProviderTurn>(
        &self,
        mut provider: P,
        tools: &[agent_brain_protocol::ToolDefinition],
        handler: Option<ToolHandler>,
        request: &ChatRequest,
        tx: mpsc::Sender<ChatEvent>,
        // Whether a `delegate` tool call is intercepted and run as a sub-agent.
        // True only for the top-level orchestrator loop; a sub-agent's own loop
        // passes false so it cannot delegate further (depth cap 1).
        allow_delegate: bool,
        max_iterations: usize,
    ) {
        let label = provider.label();
        let model = provider.model().to_string();

        // Sized against THIS loop's actual schema pressure and model window,
        // not a blanket constant — see `tool_result_budget`. At INFO on purpose:
        // it is the number that explains why a complete tool result can still
        // arrive clipped. Without it, the only symptom is a worker hedging an
        // answer it actually had.
        let result_budget = tool_result_budget(tools, self.context_window_tokens(&model));
        tracing::info!(
            tools = tools.len(),
            result_budget,
            "tool-result budget for this loop"
        );

        // Worker/voice split: when synthesis is configured, this loop is the
        // *worker*. Its prose is surfaced as thinking rather than as the answer
        // (and never streamed as tokens, or the user gets two answers), and a
        // second model composes the reply from the tool results.
        let do_synthesis = request.synthesis_provider.is_some();
        let mut weak_model_answer = String::new();
        let mut gathered: Vec<String> = Vec::new();
        // Every tool call this turn actually made (name, success) — the
        // ground-truth receipt the final answer is anchored to.
        let mut tool_trace: Vec<(String, bool)> = Vec::new();
        // One guard per turn: grounding state and duplicate fingerprints are
        // per-turn, and the ladder may run this loop once per rung.
        let mut write_guard = TurnWriteGuard::default();
        // What the model was given, for checking the links it cites: its
        // messages now, plus each tool result as it reads it (history trimming
        // drops old results, but the model already saw them).
        let mut evidence = provider.transcript();

        let mut empty_completions = 0usize;
        let mut malformed_calls = 0usize;
        // Rounds that actually spent a tool call, which is what the budget is
        // for. Retries deliberately do not count: a provider failure or a
        // malformed call is not the model using up its allowance.
        let mut tool_rounds = 0usize;
        let mut answered = false;
        let mut final_round = false;

        // The bound only guarantees termination — the loop exits on an answer,
        // a hard failure, or after the wrap-up round.
        let bound =
            max_iterations + MAX_EMPTY_COMPLETION_RETRIES + MAX_MALFORMED_TOOL_CALL_RETRIES + 1;
        for _ in 0..=bound {
            // One extra pass beyond the tool budget: the wrap-up round. It is
            // offered no tools, so a looping model has to answer from what it
            // has instead of searching again. See FINAL_ROUND_NUDGE.
            if !final_round && tool_rounds >= max_iterations {
                final_round = true;
                warn!(
                    provider = label,
                    model = %model,
                    rounds = tool_rounds,
                    "Tool budget exhausted without an answer — asking for a final answer with no tools"
                );
                provider.push_user(format!(
                    "{}\n\n{}",
                    FINAL_ROUND_NUDGE,
                    tool_trace_receipt(&tool_trace)
                ));
            }

            if let Some(reason) = provider
                .provider_name()
                .and_then(|p| paid_cap_refusal(&p, &model))
            {
                warn!(provider = label, model = %model, reason = %reason,
                      "Spend cap reached mid-turn — refusing the next paid call");
                let _ = tx
                    .send(ChatEvent::Error {
                        message: format!("{model} stopped before its next call: {reason}"),
                    })
                    .await;
                let _ = tx.send(ChatEvent::Done).await;
                return;
            }

            let stream_to = (!do_synthesis).then_some(&tx);
            let reply = match provider.call(self, !final_round, stream_to).await {
                Ok(r) => r,
                Err(message) => {
                    let _ = tx.send(ChatEvent::Error { message }).await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }
            };

            // A `<tool_call>` that did not parse is not prose: the model gets
            // one correction round, and a second failure ends the turn with a
            // visible line saying the call did NOT run — prose promising the
            // action must not stand on its own.
            if let Some(bad) = &reply.malformed {
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: bad
                            .tool
                            .clone()
                            .unwrap_or_else(|| "(unparsed tool call)".into()),
                        success: false,
                        preview: format!("Malformed tool call — not executed: {}", bad.error),
                        model_saw_chars: None,
                    })
                    .await;
                if malformed_calls < MAX_MALFORMED_TOOL_CALL_RETRIES {
                    malformed_calls += 1;
                    warn!(provider = label, model = %model, tool = ?bad.tool, error = %bad.error,
                          "Malformed tool call — asking the model to re-emit it");
                    provider.push_assistant(&reply);
                    provider.push_user(format!(
                        "{}\n\n{}",
                        malformed_tool_call_correction(bad.tool.as_deref(), &bad.error),
                        tool_trace_receipt(&tool_trace)
                    ));
                    continue;
                }
                warn!(provider = label, model = %model, tool = ?bad.tool, error = %bad.error,
                      "Malformed tool call again — reporting it to the user");
                let notice = malformed_tool_call_notice(bad.tool.as_deref(), &bad.error);
                let prose = reply.text.trim();
                let content = if prose.is_empty() {
                    notice
                } else {
                    format!("{prose}\n\n{notice}")
                };
                let _ = tx.send(ChatEvent::Message { content }).await;
                answered = true;
                break;
            }

            if reply.calls.is_empty() {
                // Nothing at all came back: no text and no tool call. That is a
                // failed call, not a finished turn — re-send the identical
                // request rather than exiting silently.
                if reply.text.is_empty() {
                    if empty_completions < MAX_EMPTY_COMPLETION_RETRIES {
                        empty_completions += 1;
                        warn!(
                            provider = label,
                            model = %model,
                            attempt = empty_completions,
                            "Provider returned an empty completion — retrying"
                        );
                        continue;
                    }
                    warn!(
                        provider = label,
                        model = %model,
                        attempts = empty_completions + 1,
                        "Provider returned an empty completion on every attempt — giving up"
                    );
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: EMPTY_COMPLETION_MESSAGE.into(),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }

                // No tool calls — the final answer.
                answered = true;
                if do_synthesis {
                    // Shown as thinking so the user sees what was gathered
                    // before the voice model speaks.
                    weak_model_answer = reply.text.clone();
                    let _ = tx
                        .send(ChatEvent::Thinking {
                            content: reply.text,
                        })
                        .await;
                } else {
                    let checked = agent_brain_knowledge::knowledge::exact::check_links(
                        &reply.text,
                        &evidence,
                    );
                    if checked.changed() {
                        warn!(provider = label, model = %model, relinked = checked.relinked.len(),
                              not_in_sources = ?checked.not_in_sources,
                              "chat: answer links checked against what the model was given");
                    }
                    let _ = tx
                        .send(ChatEvent::Message {
                            content: checked.text,
                        })
                        .await;
                }
                break;
            }

            // A tool call is what the budget counts. The wrap-up round offers no
            // tools, so reaching here on it means the model smuggled one through
            // the text channel; let it run, but do not let it buy another round.
            tool_rounds += 1;

            if !reply.text.trim().is_empty() {
                let _ = tx
                    .send(ChatEvent::Thinking {
                        content: reply.text.clone(),
                    })
                    .await;
            }
            provider.push_assistant(&reply);

            let mut outcomes = Vec::with_capacity(reply.calls.len());
            for call in &reply.calls {
                let _ = tx
                    .send(ChatEvent::ToolCall {
                        tool: call.name.clone(),
                        args: call.args.clone(),
                    })
                    .await;

                let (success, result_text, bulk) = self
                    .dispatch_tool_call(
                        allow_delegate,
                        &handler,
                        &mut write_guard,
                        &call.name,
                        &call.args,
                        &tx,
                    )
                    .await;

                // THE HUMAN GETS THE WHOLE THING. The model's ration and the
                // user's view are different concerns and must not share a cap:
                // capping the view at what a 4B model can read hid rows the
                // person asked to see. Honesty about the model's narrower view
                // is carried by `model_saw_chars`, not by degrading the view.
                let preview = user_facing_result(
                    &annotate_tool_result(&call.name, &result_text),
                    bulk.as_deref(),
                );
                // The model's ration — schema- and window-aware, and NEVER
                // including `bulk`, which is why a bulk result's context cost
                // stays flat however many rows it carries.
                let model_text = prepare_tool_result(&call.name, &result_text, result_budget);
                let model_saw = (model_text.chars().count() < preview.chars().count())
                    .then(|| model_text.chars().count());
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: call.name.clone(),
                        success,
                        preview,
                        model_saw_chars: model_saw,
                    })
                    .await;

                evidence.push('\n');
                evidence.push_str(&model_text);
                tool_trace.push((call.name.clone(), success));
                if do_synthesis && success {
                    gathered.push(format!("### {}\n{model_text}", call.name));
                }
                outcomes.push(ToolOutcome {
                    id: call.id.clone(),
                    success,
                    model_text,
                });
            }
            provider.push_tool_results(outcomes);
            provider.trim_history();
        }

        // Synthesis runs whether or not the worker produced closing prose: the
        // tool results are what the voice model composes from, and a worker that
        // looped until its budget ran out is exactly the case where its own
        // summary is least worth relaying.
        if do_synthesis {
            self.run_synthesis(
                request,
                &gathered,
                &weak_model_answer,
                &tool_trace,
                tx.clone(),
            )
            .await;
        } else if !answered {
            // Falling out of the loop having answered nothing is a real outcome
            // and needs a real explanation: the tool calls above did run, so the
            // user has to be told work may have happened.
            warn!(
                provider = label,
                model = %model,
                tool_rounds,
                "Chat loop ended without an answer after the wrap-up round"
            );
            let _ = tx
                .send(ChatEvent::Error {
                    message: NO_ANSWER_AFTER_TOOLS_MESSAGE.into(),
                })
                .await;
        }

        let _ = tx.send(ChatEvent::Done).await;
    }
}

#[cfg(test)]
mod round_loop_tests {
    use super::*;
    use crate::chat::test_support::{chat_service, test_request};
    use std::sync::Mutex;

    /// What the loop asked of the provider, in order.
    #[derive(Default)]
    struct Log {
        /// (`offer_tools`, whether tokens were to be streamed) per call.
        calls: Vec<(bool, bool)>,
        users: Vec<String>,
        tool_results: Vec<String>,
    }

    /// A provider that replays scripted replies — the loop's contract without
    /// any wire format. Past the script it answers "done".
    struct FakeTurn {
        script: Vec<RoundReply>,
        log: Arc<Mutex<Log>>,
        /// What the model was given (`transcript`).
        given: String,
    }

    #[async_trait::async_trait]
    impl ProviderTurn for FakeTurn {
        fn label(&self) -> &'static str {
            "Fake"
        }
        fn model(&self) -> &str {
            "fake-model"
        }
        async fn call(
            &mut self,
            _svc: &ChatService,
            offer_tools: bool,
            stream_to: Option<&mpsc::Sender<ChatEvent>>,
        ) -> Result<RoundReply, String> {
            self.log
                .lock()
                .unwrap()
                .calls
                .push((offer_tools, stream_to.is_some()));
            Ok(if self.script.is_empty() {
                text("done")
            } else {
                self.script.remove(0)
            })
        }
        fn push_user(&mut self, text: String) {
            self.log.lock().unwrap().users.push(text);
        }
        fn push_assistant(&mut self, _reply: &RoundReply) {}
        fn transcript(&self) -> String {
            self.given.clone()
        }
        fn push_tool_results(&mut self, results: Vec<ToolOutcome>) {
            let mut log = self.log.lock().unwrap();
            log.tool_results
                .extend(results.into_iter().map(|r| r.model_text));
        }
    }

    fn text(t: &str) -> RoundReply {
        RoundReply {
            text: t.into(),
            ..Default::default()
        }
    }

    fn call(name: &str) -> RoundReply {
        RoundReply {
            calls: vec![ToolCallReq {
                id: "c1".into(),
                name: name.into(),
                args: json!({}),
            }],
            ..Default::default()
        }
    }

    async fn drive(
        script: Vec<RoundReply>,
        request: ChatRequest,
        max_iterations: usize,
    ) -> (Vec<ChatEvent>, Arc<Mutex<Log>>) {
        drive_given(script, request, max_iterations, "").await
    }

    async fn drive_given(
        script: Vec<RoundReply>,
        request: ChatRequest,
        max_iterations: usize,
        given: &str,
    ) -> (Vec<ChatEvent>, Arc<Mutex<Log>>) {
        let svc = chat_service(vec![]);
        let log = Arc::new(Mutex::new(Log::default()));
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(256);
        let turn = FakeTurn {
            script,
            log: log.clone(),
            given: given.to_string(),
        };
        svc.run_round_loop(turn, &[], None, &request, tx, false, max_iterations)
            .await;
        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            seen.push(e);
        }
        (seen, log)
    }

    fn messages(seen: &[ChatEvent]) -> Vec<String> {
        seen.iter()
            .filter_map(|e| match e {
                ChatEvent::Message { content } => Some(content.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every provider gets the wrap-up round: once the tool budget is spent
    /// the next call offers no tools and carries the nudge plus the receipt.
    /// The Anthropic and text loops had neither before the loops were unified.
    #[tokio::test]
    async fn an_exhausted_tool_budget_gets_a_tools_off_wrap_up_round_with_the_receipt() {
        let (seen, log) = drive(
            vec![call("search_notes"), call("search_notes")],
            test_request(),
            2,
        )
        .await;
        let log = log.lock().unwrap();
        assert_eq!(
            log.calls.iter().map(|c| c.0).collect::<Vec<_>>(),
            vec![true, true, false],
            "third call must offer no tools"
        );
        let nudge = log.users.last().expect("a wrap-up nudge was pushed");
        assert!(nudge.starts_with(FINAL_ROUND_NUDGE), "{nudge}");
        assert!(
            nudge.contains("search_notes"),
            "receipt names what ran: {nudge}"
        );
        assert_eq!(messages(&seen), vec!["done".to_string()]);
    }

    /// A loop that runs out without answering says so — it does not end on a
    /// bare Done (the Anthropic and text loops did).
    #[tokio::test]
    async fn a_loop_that_never_answers_reports_it() {
        let (seen, _) = drive(
            (0..20).map(|_| call("search_notes")).collect(),
            test_request(),
            1,
        )
        .await;
        assert!(messages(&seen).is_empty());
        assert!(
            seen.iter().any(|e| matches!(e, ChatEvent::Error { message }
                if message == NO_ANSWER_AFTER_TOOLS_MESSAGE)),
            "{seen:?}"
        );
        assert!(matches!(seen.last(), Some(ChatEvent::Done)));
    }

    /// Empty completions are retried without spending the tool budget.
    #[tokio::test]
    async fn empty_completions_do_not_spend_tool_rounds() {
        let (seen, log) = drive(
            vec![text(""), text(""), call("search_notes"), text("answer")],
            test_request(),
            1,
        )
        .await;
        assert_eq!(messages(&seen), vec!["answer".to_string()]);
        // Both retries, the tool round, then the wrap-up round.
        assert_eq!(log.lock().unwrap().calls.len(), 4);
    }

    /// The answer sent is the answer with its links checked against what the
    /// model was given (`transcript`, then each tool result). What the check
    /// does is `exact::tests`; here, that the loop applies it to that evidence.
    #[tokio::test]
    async fn the_answer_sent_is_link_checked_against_what_the_model_was_given() {
        let given = r#"[{"role":"system","content":"CONTEXT [note 3c90ba42-9eed-4b2c-97bd-bd38fdc40155] see https://example.org/real"}]"#;
        let answer = "Age checks (https://3c90ba42-9eed-4b2c-97bd-bd38fdc40155), \
                      [real](https://example.org/real), [made up](https://example.org/invented)";
        let (seen, _) = drive_given(vec![text(answer)], test_request(), 3, given).await;
        let expected = agent_brain_knowledge::knowledge::exact::check_links(answer, given).text;
        assert_eq!(messages(&seen), vec![expected]);
    }

    /// Tool results go back to the model, one outcome per call.
    #[tokio::test]
    async fn tool_results_are_fed_back_to_the_provider() {
        let (_, log) = drive(vec![call("search_notes")], test_request(), 3).await;
        assert_eq!(log.lock().unwrap().tool_results.len(), 1);
    }

    /// With a voice model composing the reply, the worker's prose is never
    /// streamed as tokens — the double-answer bug, now fixed for every
    /// provider rather than only the OpenAI-compatible one.
    #[tokio::test]
    async fn a_worker_under_a_voice_model_never_streams_tokens() {
        let mut request = test_request();
        // Unknown synthesis provider: the voice pass fails fast, offline.
        request.synthesis_provider = Some("no-such-voice".into());
        let (seen, log) = drive(vec![text("worker answer")], request, 3).await;
        assert!(log.lock().unwrap().calls.iter().all(|c| !c.1));
        assert!(
            !seen
                .iter()
                .any(|e| matches!(e, ChatEvent::Message { content } if content == "worker answer")),
            "the worker's prose is thinking, not the answer: {seen:?}"
        );
    }
}
