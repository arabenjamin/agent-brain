//! Cross-cutting chat tests (provider loops against mock servers, turns, the fallback ladder).

use super::*;

// -- tool_result_budget ---------------------------------------------------

fn tool_of(name: &str, desc_len: usize) -> agent_brain_protocol::ToolDefinition {
    agent_brain_protocol::ToolDefinition {
        name: name.to_string(),
        description: "x".repeat(desc_len),
        input_schema: serde_json::json!({ "type": "object", "properties": {} }),
    }
}

/// A lean sub-agent allowlist must earn more than the old blanket 2000.
/// This is the whole point: `knowledge-worker` (12 tools, ~13K of schema)
/// was handed the cap sized for `general` (46 tools, ~33K), so a complete
/// `recall_recent` result arrived clipped and was relayed as "not the
/// complete list".
#[test]
fn a_lean_allowlist_earns_the_full_tool_result_budget() {
    let lean: Vec<_> = (0..12).map(|i| tool_of(&format!("t{i}"), 500)).collect();
    assert_eq!(
        tool_result_budget(&lean, Some(16_384)),
        STREAM_TOOL_RESULT_CEILING
    );
}

/// ...and a large one is still squeezed back to the floor, so the profile
/// the 2000 was chosen for is unaffected.
#[test]
fn a_large_allowlist_is_squeezed_back_to_the_floor() {
    let heavy: Vec<_> = (0..46).map(|i| tool_of(&format!("t{i}"), 700)).collect();
    assert_eq!(
        tool_result_budget(&heavy, Some(16_384)),
        CLOUD_TOOL_RESULT_CHARS
    );
}

/// The budget is clamped at both ends — never tighter than the constant it
/// replaced, never looser than the non-streaming path.
#[test]
fn tool_result_budget_stays_within_both_caps() {
    for n in [0usize, 1, 5, 20, 60, 200] {
        let tools: Vec<_> = (0..n).map(|i| tool_of(&format!("t{i}"), 400)).collect();
        let b = tool_result_budget(&tools, Some(16_384));
        assert!(
            (CLOUD_TOOL_RESULT_CHARS..=STREAM_TOOL_RESULT_CEILING).contains(&b),
            "n={n} gave {b}"
        );
    }
}

/// The ration must follow the MODEL, not a constant. A 1M-token orchestrator
/// and a 16384-token local worker were handed the identical allowance, which
/// is indefensible in both directions — it starves the big one and flatters
/// the small one. This is the half that answers "why should a sub-agent be
/// rationed like a small local model when the orchestrator has a million
/// tokens".
#[test]
fn a_bigger_context_window_earns_a_bigger_tool_result() {
    let tools: Vec<_> = (0..12).map(|i| tool_of(&format!("t{i}"), 500)).collect();
    let small = tool_result_budget(&tools, Some(16_384));
    let large = tool_result_budget(&tools, Some(1_000_000));
    assert!(large > small, "1M window gave {large}, 16K gave {small}");
    assert!(large <= LARGE_CONTEXT_RESULT_CEILING);
}

/// An uncatalogued model must behave exactly as the old constant did, so an
/// unknown model is never a surprise regression.
#[test]
fn an_unknown_context_window_falls_back_to_the_old_allowance() {
    let tools: Vec<_> = (0..12).map(|i| tool_of(&format!("t{i}"), 500)).collect();
    assert_eq!(
        tool_result_budget(&tools, None),
        tool_result_budget(&tools, Some(ASSUMED_CONTEXT_TOKENS))
    );
}

/// Monotonic: adding tools can only shrink the room left for a result.
#[test]
fn more_schema_never_buys_a_bigger_tool_result() {
    let small: Vec<_> = (0..8).map(|i| tool_of(&format!("t{i}"), 600)).collect();
    let big: Vec<_> = (0..30).map(|i| tool_of(&format!("t{i}"), 600)).collect();
    assert!(tool_result_budget(&big, Some(16_384)) <= tool_result_budget(&small, Some(16_384)));
}

#[test]
fn base_prompt_is_used_when_no_profile_applies() {
    let p = build_system_prompt(None, &[]);
    assert!(p.contains("You are agent-brain"));
    assert!(!p.contains("LIVE SELF-STATE"));
}

/// Spawnability is opt-in: a profile without `spawnable: true` must not be
/// spawnable. This is the safety default that keeps the human entry points
/// (`general`, `orchestrator`) from being delegated to — the recursion the
/// depth cap exists to prevent.
#[test]
fn profiles_are_not_spawnable_by_default() {
    use crate::services::context_builder::ContextProfile;
    let p: ContextProfile =
        serde_yaml::from_str("name: general\ndescription: the default chat agent\n").unwrap();
    assert!(
        !p.spawnable,
        "a profile without `spawnable: true` must not be spawnable"
    );
    assert!(p.specialty.is_none());
}

/// The roster uses each worker's `specialty`, falling back to `description`.
#[test]
fn format_roster_uses_specialty_then_falls_back_to_description() {
    use crate::services::context_builder::ContextProfile;
    let with_spec: ContextProfile = serde_yaml::from_str(
        "name: researcher\ndescription: deep research\nspecialty: web + graph search\n",
    )
    .unwrap();
    let no_spec: ContextProfile =
        serde_yaml::from_str("name: knowledge-worker\ndescription: note retrieval\n").unwrap();
    let roster = format_roster(&[with_spec, no_spec]);
    assert!(roster.contains("- researcher: web + graph search"));
    assert!(roster.contains("- knowledge-worker: note retrieval"));
}

/// A sub-agent's tool budget must stay well under the orchestrator's — a
/// turn's worst-case model calls is the product of the two, and a loose
/// sub-agent cap is what let a delegated worker loop `search_notes` to
/// exhaustion and summarise junk (measured 2026-09-06).
#[test]
fn subagent_budget_is_tighter_than_the_orchestrator() {
    // Compile-time invariants: a violation fails the build, not just this test.
    const {
        assert!(
            MAX_SUBAGENT_ITERATIONS < MAX_TOOL_ITERATIONS,
            "sub-agent cap must be < orchestrator cap"
        );
        assert!(MAX_SUBAGENT_ITERATIONS >= 1);
    }
}

/// The delegate tool must expose exactly the two-arg contract the loop
/// interception reads (`agent`, `task`), both required.
#[test]
fn delegate_tool_has_the_expected_contract() {
    let def = delegate_tool_def("  - researcher: web + graph search");
    assert_eq!(def.name, DELEGATE_TOOL);
    let required = def.input_schema["required"].as_array().unwrap();
    assert!(required.iter().any(|v| v == "agent"));
    assert!(required.iter().any(|v| v == "task"));
    let props = &def.input_schema["properties"];
    assert!(props.get("agent").is_some() && props.get("task").is_some());
}

#[test]
fn profile_prompt_is_appended_after_the_base_rules() {
    let p = build_system_prompt(Some("Profile rule: inspect yourself first."), &[]);
    let base = p.find("You are agent-brain").unwrap();
    let profile = p.find("Profile rule").unwrap();
    // Profile guidance must sit after the base so the specific layer wins.
    assert!(profile > base);
}

#[test]
fn empty_profile_prompt_adds_no_layer() {
    assert_eq!(
        build_system_prompt(Some("   "), &[]),
        build_system_prompt(None, &[])
    );
}

#[test]
fn pre_loaded_state_lands_last_and_is_marked_authoritative() {
    let p = build_system_prompt(
        Some("Profile rule."),
        &[
            "## CATALOG\n- gemma4:latest".to_string(),
            "## CHAINS\n- learn".to_string(),
        ],
    );
    assert!(p.contains("LIVE SELF-STATE"));
    assert!(p.contains("gemma4:latest"));
    assert!(p.contains("- learn"));
    // Live state is the final layer, closest to the conversation.
    assert!(p.find("LIVE SELF-STATE").unwrap() > p.find("Profile rule.").unwrap());
}

#[cfg(test)]
mod empty_completion_tests {
    use super::*;
    use crate::clients::chat::test_support::*;

    /// Run `run_openai_loop` against a mock server and collect what the
    /// client saw. Tools and handler are empty — the retry decision depends on
    /// the stream alone.
    async fn drive_cloud_loop(server: &wiremock::MockServer) -> Vec<ChatEvent> {
        let svc = chat_service(vec![]);

        let (tx, mut rx) = mpsc::channel::<ChatEvent>(64);
        svc.run_openai_loop(
            cloud_config(server),
            vec![],
            None,
            test_request(),
            "sys".into(),
            tx,
            false,
            MAX_TOOL_ITERATIONS,
        )
        .await;

        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            seen.push(e);
        }
        seen
    }

    /// The provider burning a call on nothing must not end the turn: re-sending
    /// the same request is what recovers it, ~75% of the time in practice.
    #[tokio::test]
    async fn an_empty_completion_is_retried_and_the_next_answer_is_delivered() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(EMPTY_COMPLETION_SSE))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(ANSWERED_COMPLETION_SSE))
            .expect(1)
            .mount(&server)
            .await;

        let seen = drive_cloud_loop(&server).await;

        let messages: Vec<&String> = seen
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Message { content } => Some(content),
                _ => None,
            })
            .collect();
        assert_eq!(
            messages,
            vec!["Substrates and power."],
            "the retry's answer must reach the client, got: {seen:?}"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
            "a recovered turn must not also report an error, got: {seen:?}"
        );
    }

    /// When every attempt comes back empty the turn has to fail loudly, and say
    /// that nothing was carried out — an empty completion is indistinguishable
    /// from silent success from the user's seat.
    #[tokio::test]
    async fn an_always_empty_provider_reports_an_error_rather_than_going_quiet() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer};

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(EMPTY_COMPLETION_SSE))
            // One initial call plus every retry, and no more: the retry budget
            // must be bounded or a persistently empty provider spins.
            .expect(1 + MAX_EMPTY_COMPLETION_RETRIES as u64)
            .mount(&server)
            .await;

        let seen = drive_cloud_loop(&server).await;

        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Message { .. })),
            "nothing was generated, so nothing may be presented as an answer: {seen:?}"
        );
        let errors: Vec<&String> = seen
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Error { message } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 1, "expected exactly one error, got: {seen:?}");
        assert_eq!(errors[0], EMPTY_COMPLETION_MESSAGE);
        assert!(
            seen.iter().any(|e| matches!(e, ChatEvent::Done)),
            "the stream must still terminate: {seen:?}"
        );
    }
}

// ============================================================================
// The fallback ladder
// ============================================================================

#[cfg(test)]
mod fallback_ladder_tests {
    use super::*;
    use crate::clients::chat::test_support::{
        ANSWERED_COMPLETION_SSE, EMPTY_COMPLETION_SSE, chat_service, cloud_config, sse,
        test_request,
    };
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    /// Run one attempt against `server` and return what reached the sink plus
    /// the verdict the ladder would act on.
    async fn attempt(server: &MockServer) -> (Vec<ChatEvent>, AttemptOutcome) {
        let svc = chat_service(vec![]);
        let (sink, mut sink_rx) = mpsc::channel::<ChatEvent>(128);
        let outcome = svc
            .run_one_attempt(
                cloud_config(server),
                vec![],
                None,
                test_request(),
                "sys".into(),
                true,
                MAX_TOOL_ITERATIONS,
                &sink,
            )
            .await;
        drop(sink);
        let mut seen = Vec::new();
        while let Some(e) = sink_rx.recv().await {
            seen.push(e);
        }
        (seen, outcome)
    }

    /// The whole ladder turns on this: a turn that produced nothing must report
    /// `Unanswered`, and must not have leaked its error to the client — another
    /// model is about to try, and a recovered turn narrated with the previous
    /// failure is worse than one that just works.
    #[tokio::test]
    async fn a_turn_that_produced_nothing_is_unanswered_and_stays_silent() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(EMPTY_COMPLETION_SSE))
            .mount(&server)
            .await;

        let (seen, outcome) = attempt(&server).await;

        match outcome {
            AttemptOutcome::Unanswered { error } => {
                assert_eq!(error.as_deref(), Some(EMPTY_COMPLETION_MESSAGE));
            }
            other => panic!("expected Unanswered, got {other:?}"),
        }
        assert!(
            seen.is_empty(),
            "nothing may reach the client on a turn the ladder will retry: {seen:?}"
        );
    }

    #[tokio::test]
    async fn an_answered_turn_is_delivered_and_reaches_the_client() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(ANSWERED_COMPLETION_SSE))
            .mount(&server)
            .await;

        let (seen, outcome) = attempt(&server).await;

        assert!(
            matches!(outcome, AttemptOutcome::Delivered),
            "got {outcome:?}"
        );
        assert!(
            seen.iter()
                .any(|e| matches!(e, ChatEvent::Message { content } if content == "Substrates and power.")),
            "the answer must reach the client: {seen:?}"
        );
    }

    /// Round 1 streams one call to `tool` with `args_json` (a JSON string, as
    /// the wire carries it); round 2 answers 503.
    async fn tool_then_503(server: &MockServer, tool: &str, args_json: &str) {
        let args = serde_json::to_string(args_json).unwrap();
        let first = format!(
            "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"role\":\"assistant\",\
             \"tool_calls\":[{{\"index\":0,\"id\":\"c1\",\"type\":\"function\",\
             \"function\":{{\"name\":\"{tool}\",\"arguments\":{args}}}}}]}},\
             \"finish_reason\":null}}]}}\n\n\
             data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"tool_calls\"}}]}}\n\n\
             data: {{\"choices\":[],\"usage\":{{\"prompt_tokens\":10,\"completion_tokens\":8}}}}\n\n\
             data: [DONE]\n\n"
        );
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(&first))
            .up_to_n_times(1)
            .mount(server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
            .mount(server)
            .await;
    }

    /// A model that ran only reads and then failed hands its work to the next
    /// rung (2026-09-26: a gemini 503 on the round after a delegate ended the
    /// turn with nothing). The error is held back — the next rung answers.
    #[tokio::test]
    async fn a_failure_after_read_only_tools_hands_the_work_over() {
        let server = MockServer::start().await;
        tool_then_503(&server, "search_notes", r#"{"query":"agency"}"#).await;

        let (seen, outcome) = attempt(&server).await;

        let AttemptOutcome::Interrupted { error, calls } = outcome else {
            panic!("expected a handover: {outcome:?}");
        };
        assert!(error.contains("503"), "{error}");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tool, "search_notes");
        assert_eq!(calls[0].args["query"], "agency");
        assert!(
            calls[0].success.is_some(),
            "the result must be paired: {calls:?}"
        );
        assert!(
            seen.iter().any(|e| matches!(e, ChatEvent::ToolCall { .. })),
            "the tool call must have reached the client: {seen:?}"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
            "a handed-over failure is not shown as the turn's error: {seen:?}"
        );
    }

    /// A model that ran a WRITE owns the turn even when it fails after: a
    /// second model could repeat the write, so the failure is reported in place.
    #[tokio::test]
    async fn a_failure_after_a_write_keeps_the_turn_and_its_error() {
        let server = MockServer::start().await;
        tool_then_503(&server, "store_note", r#"{"content":"x"}"#).await;

        let (seen, outcome) = attempt(&server).await;

        assert!(
            matches!(outcome, AttemptOutcome::Delivered),
            "a turn that wrote must not be handed to another model: {outcome:?}"
        );
        assert!(
            seen.iter()
                .any(|e| matches!(e, ChatEvent::Error { message } if message.contains("503"))),
            "the failure must still be reported: {seen:?}"
        );
    }

    #[test]
    fn read_only_classification_is_conservative() {
        let none = json!({});
        assert!(call_is_read_only("search_notes", &none));
        assert!(call_is_read_only(
            "delegate",
            &json!({"agent":"knowledge-worker"})
        ));
        assert!(call_is_read_only("http_request", &json!({"url":"x"})));
        assert!(!call_is_read_only(
            "http_request",
            &json!({"method":"POST"})
        ));
        assert!(call_is_read_only(
            "mcp_call",
            &json!({"tool":"web_fetch_exa"})
        ));
        assert!(!call_is_read_only(
            "mcp_call",
            &json!({"tool":"apply_to_job"})
        ));
        for write in [
            "store_note",
            "manage_workflow",
            "ingest_api",
            "run_agent",
            "claim",
        ] {
            assert!(!call_is_read_only(write, &none), "{write}");
        }
    }

    #[test]
    fn the_handover_names_each_call_and_its_result() {
        let calls = vec![PriorCall {
            tool: "recall_recent".into(),
            args: json!({"source_context":"news"}),
            success: Some(true),
            result: "12 notes".into(),
        }];
        let block = handover_block(&calls, "Status 503 Service Unavailable");
        assert!(block.starts_with("WORK ALREADY DONE THIS TURN"));
        assert!(block.contains("recall_recent({\"source_context\":\"news\"}) ✓"));
        assert!(block.contains("12 notes"));
        assert!(block.contains("Do NOT call these tools again"));
    }

    /// Regression for the "apply to #N choked" bug. A tool call streamed, then
    /// the model returned an EMPTY completion on every retry — unlike a hard
    /// error after a tool call, this must be handed to the next model, not ended
    /// on an error banner. The tool events still reached the client, but the
    /// empty-completion error is held back so the ladder can try another model.
    #[tokio::test]
    async fn an_empty_completion_after_a_tool_call_falls_back_not_choke() {
        let server = MockServer::start().await;
        // Round 1: a tool call (streams a ToolCall event).
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(concat!(
                "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
                "\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",",
                "\"function\":{\"name\":\"search_notes\",\"arguments\":\"{}\"}}]},",
                "\"finish_reason\":null}]}\n\n",
                "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
                "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":8}}\n\n",
                "data: [DONE]\n\n",
            )))
            .up_to_n_times(1)
            .mount(&server)
            .await;
        // Round 2 and every retry: empty completions (the thinking-model failure).
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(EMPTY_COMPLETION_SSE))
            .mount(&server)
            .await;

        let (seen, outcome) = attempt(&server).await;

        match outcome {
            AttemptOutcome::Unanswered { error } => {
                assert_eq!(error.as_deref(), Some(EMPTY_COMPLETION_MESSAGE));
            }
            other => panic!("empty-after-tool must fall back, got {other:?}"),
        }
        assert!(
            seen.iter().any(|e| matches!(e, ChatEvent::ToolCall { .. })),
            "the tool call still reached the client: {seen:?}"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
            "the empty-completion error is held back for the ladder, not shown: {seen:?}"
        );
    }

    /// `forward_chat_events` re-emits exactly one `Done` for the whole turn. An
    /// attempt that let its loop's `Done` through would close the client's
    /// reader while the ladder was still working.
    #[tokio::test]
    async fn an_attempt_never_forwards_done() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(ANSWERED_COMPLETION_SSE))
            .mount(&server)
            .await;

        let (seen, _) = attempt(&server).await;
        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Done)),
            "Done belongs to the turn, not to one attempt: {seen:?}"
        );
    }

    /// A sub-agent turn (`run_bounded_loop`) must walk the ladder too: a transient
    /// 500 on the primary model degrades to the next rung instead of surfacing the
    /// raw error as the whole delegated answer. This is the fix for the live
    /// `run_agent` 500 that returned `ok:false` on a provider blip.
    #[tokio::test]
    async fn a_subagent_turn_falls_back_past_a_transient_500() {
        // Primary rung: always 500.
        let primary = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("Internal Server Error"))
            .mount(&primary)
            .await;
        // Fallback rung: answers.
        let fallback = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(ANSWERED_COMPLETION_SSE))
            .mount(&fallback)
            .await;

        // A different model: the primary's 500 benches its model in the budget,
        // and each rung is re-judged just before it is tried.
        let svc = chat_service(vec![
            cloud_config(&fallback).with_model("subagent-fallback-model"),
        ]);
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(128);
        // allow_delegate = false, a bounded budget — the sub-agent shape.
        svc.run_bounded_loop(
            cloud_config(&primary),
            vec![],
            None,
            test_request(),
            "sys".into(),
            tx,
            false,
            MAX_TOOL_ITERATIONS,
        )
        .await;

        let mut seen = Vec::new();
        while let Some(e) = rx.recv().await {
            seen.push(e);
        }
        assert!(
            seen.iter().any(
                |e| matches!(e, ChatEvent::Message { content } if content == "Substrates and power.")
            ),
            "the fallback rung's answer must reach the sub-agent's drain: {seen:?}"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
            "a recovered sub-agent turn must not surface the primary's 500: {seen:?}"
        );
    }

    /// When every rung fails, the sub-agent turn reports the concrete error rather
    /// than going silent (so the caller builds an honest `ok:false`).
    #[tokio::test]
    async fn a_subagent_turn_reports_error_when_all_rungs_fail() {
        let primary = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&primary)
            .await;
        let fallback = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(500).set_body_string("boom"))
            .mount(&fallback)
            .await;

        let svc = chat_service(vec![cloud_config(&fallback)]);
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(128);
        svc.run_bounded_loop(
            cloud_config(&primary),
            vec![],
            None,
            test_request(),
            "sys".into(),
            tx,
            false,
            MAX_TOOL_ITERATIONS,
        )
        .await;

        let mut seen = Vec::new();
        while let Some(e) = rx.recv().await {
            seen.push(e);
        }
        assert!(
            seen.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
            "a fully-failed sub-agent turn must report an error, not go silent: {seen:?}"
        );
    }
}

// ============================================================================
// The whole turn
// ============================================================================

#[cfg(test)]
mod turn_tests {
    use super::*;
    use crate::clients::chat::test_support::{
        ANSWERED_COMPLETION_SSE, EMPTY_COMPLETION_SSE, cloud_config, sse, test_request,
    };
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer};

    async fn always(server: &MockServer, body: &str) {
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(body))
            .mount(server)
            .await;
    }

    /// Drive the full `run()` path — the one the HTTP handler calls — and fail
    /// rather than hang if it never terminates.
    async fn run_turn(active: LlmConfig, ladder: Vec<LlmConfig>) -> Vec<ChatEvent> {
        let svc = ChatService::with_context_builder(
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(ToolRegistry::new())),
            Arc::new(RwLock::new(Some(active))),
            Arc::new(RwLock::new(None)),
            None,
            ladder,
        );
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(128);
        tokio::time::timeout(Duration::from_secs(20), svc.run(test_request(), tx))
            .await
            .expect("run() must terminate");

        let mut seen = Vec::new();
        while let Some(e) = rx.recv().await {
            seen.push(e);
        }
        seen
    }

    /// A rung on a provider the registry does not know. The policy refuses it
    /// whatever the environment holds (its billing and data policy are unknown).
    fn unregistered_config(server: &MockServer) -> LlmConfig {
        LlmConfig {
            provider: LlmProviderType::OpenAiCompat,
            provider_name: Some("unregistered-provider".into()),
            base_url: Some(server.uri()),
            api_key: Some("test-key".into()),
            timeout: Duration::from_secs(30),
            ..Default::default()
        }
    }

    /// Privacy before availability: when the policy refuses every rung, the turn
    /// says so and never sends the prompt anywhere.
    #[tokio::test]
    async fn a_turn_the_policy_refuses_everywhere_fails_loudly_and_calls_nothing() {
        let server = MockServer::start().await;
        always(&server, ANSWERED_COMPLETION_SSE).await;

        let seen = run_turn(unregistered_config(&server), vec![]).await;

        assert!(
            seen.iter().any(
                |e| matches!(e, ChatEvent::Error { message } if message.contains("routing policy"))
            ),
            "the refusal must reach the client with its reason: {seen:?}"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Message { .. })),
            "no model answered, so nothing may be presented as an answer: {seen:?}"
        );
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            0,
            "a refused provider must never receive the prompt"
        );
    }

    /// A model chosen for the turn that the catalog does not have ends the
    /// turn with the reason; the turn never answers on another model.
    #[tokio::test]
    async fn a_turn_model_that_cannot_run_ends_the_turn_and_calls_nothing() {
        let server = MockServer::start().await;
        always(&server, ANSWERED_COMPLETION_SSE).await;
        let svc = ChatService::with_context_builder(
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(ToolRegistry::new())),
            Arc::new(RwLock::new(Some(cloud_config(&server)))),
            Arc::new(RwLock::new(None)),
            None,
            vec![],
        );
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(128);
        let request = ChatRequest {
            model: Some("no-such-model".into()),
            ..test_request()
        };
        tokio::time::timeout(Duration::from_secs(20), svc.run(request, tx))
            .await
            .expect("run() must terminate");
        let mut seen = Vec::new();
        while let Some(e) = rx.recv().await {
            seen.push(e);
        }
        assert!(
            seen.iter().any(|e| matches!(e, ChatEvent::Error { message }
                if message.contains("no-such-model") && message.contains("not in the model catalog"))),
            "{seen:?}"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Message { .. })),
            "{seen:?}"
        );
        assert_eq!(
            server.received_requests().await.unwrap_or_default().len(),
            0
        );
    }

    #[tokio::test]
    async fn a_refused_primary_is_skipped_for_the_next_permitted_rung() {
        let refused = MockServer::start().await;
        always(&refused, ANSWERED_COMPLETION_SSE).await;
        let permitted = MockServer::start().await;
        always(&permitted, ANSWERED_COMPLETION_SSE).await;

        let seen = run_turn(
            unregistered_config(&refused),
            vec![cloud_config(&permitted)],
        )
        .await;

        assert!(
            seen.iter()
                .any(|e| matches!(e, ChatEvent::Message { content } if content == "Substrates and power.")),
            "the permitted rung answers the turn: {seen:?}"
        );
        assert_eq!(
            refused.received_requests().await.unwrap_or_default().len(),
            0,
            "the refused primary must never receive the prompt"
        );
    }

    /// Regression: `run()` holds the only remaining sender once the ladder
    /// passes `inner_tx` by reference, so failing to drop it leaves the
    /// forwarding task waiting on a channel that never closes and the turn
    /// hangs forever. Caught in a live rebuild — a chat turn banked its user
    /// message at 17:07:50 and produced nothing for ten minutes.
    #[tokio::test]
    async fn a_turn_terminates_and_emits_exactly_one_done() {
        let server = MockServer::start().await;
        always(&server, ANSWERED_COMPLETION_SSE).await;

        let seen = run_turn(cloud_config(&server), vec![]).await;

        assert_eq!(
            seen.iter().filter(|e| matches!(e, ChatEvent::Done)).count(),
            1,
            "a turn ends exactly once: {seen:?}"
        );
        assert!(
            seen.iter()
                .any(|e| matches!(e, ChatEvent::Message { content } if content == "Substrates and power.")),
            "the answer must reach the client: {seen:?}"
        );
    }

    /// The ladder, end to end: the active model never answers, the next rung
    /// does, and the user gets the answer rather than an error.
    #[tokio::test]
    async fn a_dead_primary_falls_through_to_the_next_rung() {
        let dead = MockServer::start().await;
        always(&dead, EMPTY_COMPLETION_SSE).await;
        let alive = MockServer::start().await;
        always(&alive, ANSWERED_COMPLETION_SSE).await;

        let seen = run_turn(cloud_config(&dead), vec![cloud_config(&alive)]).await;

        assert!(
            seen.iter()
                .any(|e| matches!(e, ChatEvent::Message { content } if content == "Substrates and power.")),
            "the fallback's answer must reach the client: {seen:?}"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
            "a turn the ladder recovered must not also report an error: {seen:?}"
        );
        assert!(
            seen.iter().any(
                |e| matches!(e, ChatEvent::Thinking { content } if content.contains("retrying on"))
            ),
            "a silent model switch is unattributable — it must be announced: {seen:?}"
        );
    }

    /// Every rung empty: one error, and it is the specific one.
    #[tokio::test]
    async fn an_exhausted_ladder_reports_the_failure_once() {
        let a = MockServer::start().await;
        always(&a, EMPTY_COMPLETION_SSE).await;
        let b = MockServer::start().await;
        always(&b, EMPTY_COMPLETION_SSE).await;

        let seen = run_turn(cloud_config(&a), vec![cloud_config(&b)]).await;

        let errors: Vec<&String> = seen
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Error { message } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 1, "expected one error, got: {seen:?}");
        assert_eq!(errors[0], EMPTY_COMPLETION_MESSAGE);
        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Message { .. })),
            "nothing was generated, so nothing may be presented as an answer: {seen:?}"
        );
    }
}

// ============================================================================
// Running out of tool calls
// ============================================================================
