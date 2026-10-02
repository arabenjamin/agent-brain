//! Tool-result budgets, truncation, and the position-zero markers prepended to results.

use super::*;

/// Build the copy a PERSON sees: the model-facing text plus any bulk output the
/// model was deliberately not given.
///
/// The bulk is fenced with a header naming the split, so a reader is never left
/// wondering why the assistant's answer is thinner than the data above it.
pub(super) fn user_facing_result(model_text: &str, bulk: Option<&str>) -> String {
    match bulk {
        Some(b) if !b.is_empty() => format!(
            "{model_text}\n\n             ───── FULL RESULT (delivered to you, not loaded into the model) ─────\n             {b}"
        ),
        _ => model_text.to_string(),
    }
}

/// Maximum characters of a tool result fed back to the LLM.
/// Prevents context-window overflow (OllamaCloud/Ollama models often have 4K–32K token limits).
/// The display preview uses the same cap so the UI stays consistent.
pub(super) const MAX_TOOL_RESULT_CHARS: usize = 6000;

/// Floor for the OllamaCloud/Ollama streaming loops' tool-result cap — what a
/// profile with a very large allowlist gets. Tool schemas are resent every round;
/// combined with cumulative tool results the context grows quickly and causes
/// 500s from smaller cloud models.
pub(super) const CLOUD_TOOL_RESULT_CHARS: usize = 2000;

/// Fraction of a model's context window this loop is willing to spend on tool
/// schemas plus ONE tool result. The rest is the system prompt, the running
/// message history, and room to answer.
pub(super) const ROUND_CONTEXT_FRACTION: f64 = 0.35;

/// Chars per token, roughly, for English prose and JSON. Used only to turn a
/// catalog `context_window` (tokens) into the char budget everything else here
/// is measured in.
pub(super) const CHARS_PER_TOKEN: usize = 4;

/// Context window assumed when the catalog does not name one. The old
/// `ROUND_CONTEXT_CHARS = 24_000` constant in disguise, kept so an unknown model
/// behaves exactly as before rather than surprising anyone.
pub(super) const ASSUMED_CONTEXT_TOKENS: usize = 17_000;

/// Ceiling for the streaming loops' tool-result cap.
///
/// Deliberately ABOVE [`MAX_TOOL_RESULT_CHARS`], which is the non-streaming
/// preview cap and a different concern that was being conflated with this one.
/// Set from measurement: `knowledge-worker` (11-12 tools, ~11K chars of schema)
/// peaks at 8080 `tokens_in` against E4B's 16384, so ~8K chars (~2K tokens) of
/// tool result still leaves roughly a third of the window free.
pub(super) const STREAM_TOOL_RESULT_CEILING: usize = 8_000;

/// Upper bound once a genuinely large context window is in play. A 1M-token
/// orchestrator has no business being held to a 16K model's ration, but an
/// unbounded result is still a bad idea: a single tool call should not be able
/// to evict the conversation it is supposed to serve.
pub(super) const LARGE_CONTEXT_RESULT_CEILING: usize = 120_000;

/// How many characters of tool result to feed back, given the schemas resent
/// alongside it every round AND the window of the model that has to hold both.
///
/// Two things were wrong with the single blanket cap this replaced. It was 2000
/// for everyone — a number sized for `general` (46 tools, ~33K chars of schema)
/// and then applied unchanged to a `knowledge-worker` sub-agent whose 12 schemas
/// are ~13K. The cost is not a crash; it is a **tool that lies about itself**:
/// on 2026-09-12 `recall_recent` returned `complete: true`, `returned: 41`, and
/// "These are ALL the notes matching the filter — summarise from it directly" in
/// a 6254-char payload of which the worker saw 2000, then relayed the truncation
/// honestly, so the user was told the graph could not produce what it already had.
///
/// The second thing was that the fix was **model-blind**: a fixed 24_000-char
/// allowance regardless of whether the loop was driving Gemma-4-E4B at 16384
/// tokens or a 1M-token cloud model. Asked why a sub-agent should be rationed
/// like a small local model when the orchestrator has a million tokens, there
/// was no answer — the code never looked. It does now: the allowance is a
/// fraction of the model's own catalog `context_window`, so the ration follows
/// the model rather than the codebase's oldest assumption.
pub(super) fn tool_result_budget(
    tools: &[agent_brain_protocol::ToolDefinition],
    context_tokens: Option<usize>,
) -> usize {
    let schema_chars: usize = tools
        .iter()
        .map(|t| t.name.len() + t.description.len() + t.input_schema.to_string().len())
        .sum();
    let window_chars = context_tokens.unwrap_or(ASSUMED_CONTEXT_TOKENS) * CHARS_PER_TOKEN;
    let allowance = (window_chars as f64 * ROUND_CONTEXT_FRACTION) as usize;
    // A big window raises the ceiling as well as the allowance — otherwise the
    // extra room is computed and then thrown away by the clamp.
    let ceiling = if window_chars > ASSUMED_CONTEXT_TOKENS * CHARS_PER_TOKEN * 2 {
        LARGE_CONTEXT_RESULT_CEILING
    } else {
        STREAM_TOOL_RESULT_CEILING
    };
    allowance
        .saturating_sub(schema_chars)
        .clamp(CLOUD_TOOL_RESULT_CHARS, ceiling)
}

/// Cap a tool result for the model, appending an explicit marker when content
/// was dropped.
///
/// Silent truncation is worse than a short answer: the model cannot distinguish
/// "the list ends here" from "the list was cut here" and states the former as
/// fact. Observed 2026-08-10 — `manage_scheduled_task(action=list)` returned all
/// 16 schedules, the tail was silently cut, and the model reported the three
/// schedules that had been cut off as not existing at all. The marker turns an
/// invisible loss into something the model can react to by narrowing its query.
pub(super) fn truncate_tool_result(text: &str, limit: usize) -> String {
    // chars().count() is O(n) but tool results are bounded and this runs once
    // per tool call, not per token.
    let total = text.chars().count();
    if total <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!(
        "{kept}\n\n[TRUNCATED: showing the first {limit} of {total} characters. \
         This result is INCOMPLETE — do not conclude that something is absent \
         because it does not appear above. Narrow the query (filters, limits, \
         or a more specific query string) and call the tool again.]"
    )
}

/// Tools whose structured output can declare what the call failed to establish.
/// Keyed by tool name, not by action — `reason` covers `infer`, `structured`,
/// `explain`, and the rest, and they share the `gaps`/`caveats` shape.
pub(super) const LIMIT_DECLARING_TOOLS: &[&str] = &["reason"];

/// Below this, `confidence` is worth naming on its own. Strictly less than, so
/// the 0.5 that `reason` emits as its parse-failure default does not fire the
/// marker by itself — a genuinely uncertain answer nearly always also populates
/// `gaps` or `caveats`, which do.
pub(super) const LOW_CONFIDENCE: f64 = 0.5;

/// Keep the marker bounded: it is prepended to a result that is itself capped.
pub(super) const MAX_MARKER_ITEMS: usize = 3;

pub(super) const MAX_MARKER_ITEM_CHARS: usize = 200;

/// Hoist whatever a tool result needs the model to *act* on to the front of
/// what the model reads: `reason`'s declared limitations, `search_web`'s source
/// URLs.
///
/// In both cases the information was never missing — it was ignored, and in
/// both cases a prose rule in `contexts/general.yaml` failed to change that.
/// Observed 2026-08-23: `reason` reported in five separate fields that it could
/// not establish an integration, and the reply presented a confident
/// architecture relaying none of them. Observed 2026-08-24: `search_web`
/// returned directly relevant sources and the reply cited no URL at all, with
/// a CITATION RULE already in the prompt telling it to.
///
/// This is the `truncate_tool_result` lesson generalised. A signal buried
/// mid-payload is not a signal the model acts on; a loud marker at position
/// zero is. Prepended rather than appended so it survives truncation, which
/// keeps the head — which for search results also means the URLs most likely
/// to be cut are the ones now guaranteed to be present.
///
/// Markers are mutually exclusive by tool, so the first match wins.
pub(super) fn annotate_tool_result(tool_name: &str, text: &str) -> String {
    let marker =
        reason_limits_marker(tool_name, text).or_else(|| search_sources_marker(tool_name, text));
    match marker {
        Some(marker) => format!("{marker}\n\n{text}"),
        None => text.to_string(),
    }
}

/// Tools that return retrieved sources the reply is expected to cite.
pub(super) const SOURCE_LISTING_TOOLS: &[&str] = &["search_web"];

/// Cap on sources listed in the marker. `search_web` returns at most 20.
pub(super) const MAX_MARKER_SOURCES: usize = 12;

pub(super) const MAX_MARKER_TITLE_CHARS: usize = 90;

/// Lift the URLs out of a `search_web` payload into a numbered, citable list at
/// the front of the result.
///
/// The CITATION RULE in `contexts/general.yaml` has asked for this in prose
/// since it was written, and it does not hold: measured 2026-08-24, two
/// `search_web` calls returned good sources — including
/// `github.com/FreeTAKTeam/Reticulum_Meshtastic_Integration`, directly on
/// point — and the reply cited no URL at all. This is the same shape as the
/// `reason` limits problem: the information is present in the payload (field
/// `link` of each of ten results) and the model does not carry it into prose.
///
/// So the marker does more than restate the rule — it makes citing *cheap*.
/// Short `[S1]`-style handles paired with their URLs mean the model copies a
/// token rather than re-extracting a URL from JSON it has already scrolled
/// past. Listing them at the head also means the URLs survive truncation:
/// under `CLOUD_TOOL_RESULT_CHARS` a long result previously lost its tail
/// entries' links entirely, so the sources most likely to be cut were
/// uncitable by construction.
pub(super) fn search_sources_marker(tool_name: &str, text: &str) -> Option<String> {
    if !SOURCE_LISTING_TOOLS.contains(&tool_name) {
        return None;
    }
    let parsed: Value = serde_json::from_str(text.trim()).ok()?;
    let items = parsed.as_array()?;

    let sources: Vec<(String, String)> = items
        .iter()
        .filter_map(|item| {
            // Engines disagree on the field name: SearXNG/SerpApi/Google CSE
            // are normalised to `link`, Brave still emits `url`.
            let url = item
                .get("link")
                .or_else(|| item.get("url"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|u| !u.is_empty())?;
            let title = item
                .get("title")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .unwrap_or("untitled");
            let title = if title.chars().count() > MAX_MARKER_TITLE_CHARS {
                let head: String = title.chars().take(MAX_MARKER_TITLE_CHARS).collect();
                format!("{head}…")
            } else {
                title.to_string()
            };
            Some((title, url.to_string()))
        })
        .take(MAX_MARKER_SOURCES)
        .collect();

    if sources.is_empty() {
        return None;
    }

    let mut marker = format!(
        "[SEARCH SOURCES — {} retrieved. Every claim you take from these results \
         must carry its source as a markdown link, using the URLs below. Naming a \
         source without its link, or telling the user to go and search for it, \
         discards the retrieval and leaves nothing they can check.",
        sources.len()
    );
    for (i, (title, url)) in sources.iter().enumerate() {
        marker.push_str(&format!("\n  [S{}] {title} — {url}", i + 1));
    }
    marker.push(']');
    Some(marker)
}

/// Build the limits marker, or `None` when the tool declared no limits.
///
/// Returns `None` for any non-JSON body so a plain-text or errored result is
/// passed through untouched — a marker on a result we could not parse would be
/// a claim we cannot support.
pub(super) fn reason_limits_marker(tool_name: &str, text: &str) -> Option<String> {
    if !LIMIT_DECLARING_TOOLS.contains(&tool_name) {
        return None;
    }
    let parsed: Value = serde_json::from_str(text.trim()).ok()?;

    let gaps = capped_items(&parsed, "gaps");
    let caveats = capped_items(&parsed, "caveats");
    let critiques = capped_items(&parsed, "critic_counter_arguments");
    let confidence = parsed.get("confidence").and_then(Value::as_f64);
    let low_confidence = confidence.is_some_and(|c| c < LOW_CONFIDENCE);

    // A fallback result is the case where the other four signals are least
    // trustworthy and most likely to be empty — the model never answered the
    // question, so it declared no gaps and no caveats. Checked before the
    // early return: without this the emptiness suppresses the marker entirely,
    // which is exactly how an unstructured `reason` answer reached a chat reply
    // on 2026-08-24 and was written up as a graded finding.
    let structured_failed = parsed
        .get("structured_output_failed")
        .and_then(Value::as_bool)
        .unwrap_or(false);

    if gaps.is_empty()
        && caveats.is_empty()
        && critiques.is_empty()
        && !low_confidence
        && !structured_failed
    {
        return None;
    }

    if structured_failed {
        return Some(String::from(
            "[REASON — THE TOOL DID NOT PRODUCE A STRUCTURED ANSWER. The model \
             failed to return parseable output, so what follows is raw prose, \
             not a graded result. Its `confidence`, `gaps` and `caveats` fields \
             are placeholders the tool filled in — NOT the model's own \
             assessment — so do not cite them or describe this answer as \
             high-confidence, verified, or established. Say in your reply that \
             this came back unstructured, or re-run the tool.]",
        ));
    }

    let mut marker = String::from(
        "[REASON — LIMITS THE TOOL DECLARED ABOUT ITS OWN ANSWER. \
         These are things it reported it could NOT establish. Do not present \
         them as settled, and do not fill them in from general knowledge while \
         implying the tool supported it. Either state the limitation in your \
         reply, or resolve it with another tool call first — then say which \
         part came from where.",
    );
    push_marker_section(&mut marker, "NOT ESTABLISHED", &gaps);
    push_marker_section(&mut marker, "CAVEATS", &caveats);
    push_marker_section(&mut marker, "THE TOOL'S OWN CRITIQUE", &critiques);
    if let Some(c) = confidence.filter(|_| low_confidence) {
        marker.push_str(&format!("\n  CONFIDENCE: {c} — low."));
    }
    marker.push(']');
    Some(marker)
}

pub(super) fn push_marker_section(marker: &mut String, heading: &str, items: &[String]) {
    if items.is_empty() {
        return;
    }
    marker.push_str(&format!("\n  {heading}:"));
    for item in items {
        marker.push_str(&format!("\n    - {item}"));
    }
}

/// Read a string array, dropping blanks and capping both count and length so a
/// verbose reasoning step cannot crowd out the result it is annotating.
pub(super) fn capped_items(parsed: &Value, key: &str) -> Vec<String> {
    parsed
        .get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .take(MAX_MARKER_ITEMS)
                .map(|s| {
                    if s.chars().count() > MAX_MARKER_ITEM_CHARS {
                        let head: String = s.chars().take(MAX_MARKER_ITEM_CHARS).collect();
                        format!("{head}…")
                    } else {
                        s.to_string()
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Annotate a tool result with any limits it declared, then cap it for the
/// model. Every call site goes through this so a new one cannot pick up the
/// truncation marker while silently dropping the limits marker.
pub(super) fn prepare_tool_result(tool_name: &str, text: &str, limit: usize) -> String {
    truncate_tool_result(&annotate_tool_result(tool_name, text), limit)
}

#[cfg(test)]
mod truncation_tests {
    use super::*;

    #[test]
    fn short_results_pass_through_untouched() {
        let text = r#"{"count":2,"scheduled_tasks":[]}"#;
        assert_eq!(truncate_tool_result(text, 6000), text);
    }

    #[test]
    fn oversized_results_are_marked_as_incomplete() {
        let text = "x".repeat(3000);
        let out = truncate_tool_result(&text, 2000);
        assert!(out.starts_with(&"x".repeat(2000)));
        assert!(out.contains("[TRUNCATED:"));
        // The model must be told the result is partial, or it reports the
        // missing tail as nonexistent — which is exactly what happened to the
        // three long-cadence schedules on 2026-08-10.
        assert!(out.contains("INCOMPLETE"));
        assert!(out.contains("3000"));
    }

    #[test]
    fn a_result_exactly_at_the_limit_is_not_marked() {
        let text = "y".repeat(2000);
        let out = truncate_tool_result(&text, 2000);
        assert_eq!(out, text);
        assert!(!out.contains("TRUNCATED"));
    }

    #[test]
    fn multibyte_content_is_cut_on_char_boundaries() {
        // Naive byte slicing panics here; tool results carry em-dashes and
        // emoji routinely (note content, news briefs).
        let text = "🧠é—".repeat(50);
        let out = truncate_tool_result(&text, 10);
        assert!(out.starts_with("🧠é—🧠"));
        assert!(out.contains("[TRUNCATED:"));
    }
}

#[cfg(test)]
mod limits_marker_tests {
    use super::*;

    /// Trimmed from the real `reason` result of session 42d8ff9b on
    /// 2026-08-23 — the one whose five limitation signals were all discarded.
    fn nebula_result() -> String {
        serde_json::json!({
            "answer": "Nebula is positioned as a fully open-source, peer-to-peer mesh VPN. \
                       The provided knowledge does not specify how Nebula integrates with \
                       Meshtastic or Reticulum.",
            "caveats": [
                "The knowledge confirms Nebula's technical advantages over Tailscale but \
                 does not provide information on its operational synergy with Meshtastic \
                 or Reticulum."
            ],
            "confidence": 0.5,
            "critic_counter_arguments": [
                "The answer fails to provide any technical details regarding the \
                 integration of Nebula with Meshtastic or Reticulum, which were core \
                 components of the original question."
            ],
            "gaps": [
                "The specific mechanisms of integration between Nebula, Meshtastic, and \
                 Reticulum are unknown."
            ],
            "inferences": []
        })
        .to_string()
    }

    #[test]
    fn the_nebula_result_is_marked_and_the_marker_leads() {
        let out = prepare_tool_result("reason", &nebula_result(), MAX_TOOL_RESULT_CHARS);
        assert!(
            out.starts_with("[REASON — LIMITS"),
            "marker must lead so truncation cannot drop it: {out}"
        );
        assert!(out.contains("NOT ESTABLISHED"));
        assert!(out.contains("specific mechanisms of integration"));
        assert!(out.contains("CAVEATS"));
        assert!(out.contains("THE TOOL'S OWN CRITIQUE"));
        // 0.5 is not strictly below the threshold, so it is not called out on
        // its own — gaps and caveats already carry this result.
        assert!(!out.contains("CONFIDENCE:"));
        // The original payload must still be there in full.
        assert!(out.contains("fully open-source, peer-to-peer mesh VPN"));
    }

    #[test]
    fn the_marker_survives_a_tight_truncation() {
        let mut payload: serde_json::Value =
            serde_json::from_str(&nebula_result()).expect("fixture parses");
        payload["answer"] = serde_json::json!("z".repeat(20_000));
        let out = prepare_tool_result("reason", &payload.to_string(), 2000);
        assert!(out.starts_with("[REASON — LIMITS"));
        assert!(out.contains("[TRUNCATED:"));
    }

    /// The 2026-08-24 shape: `reason` fell back to raw prose, so every field the
    /// marker normally keys on came back empty and the early return suppressed
    /// the marker entirely. The fallback flag has to fire on its own.
    #[test]
    fn a_structured_output_failure_is_marked_even_with_every_field_empty() {
        let fallback = serde_json::json!({
            "answer": "The user has provided a series of inputs covering multiple domains.",
            "inferences": [],
            "confidence": 0.0,
            "gaps": [],
            "caveats": [],
            "follow_up_questions": [],
            "structured_output_failed": true
        })
        .to_string();

        let out = prepare_tool_result("reason", &fallback, MAX_TOOL_RESULT_CHARS);
        assert!(
            out.starts_with("[REASON — THE TOOL DID NOT PRODUCE A STRUCTURED ANSWER."),
            "the fallback marker must lead: {out}"
        );
        assert!(out.contains("placeholders"));
        assert!(out.contains("The user has provided a series of inputs"));
    }

    /// The fallback marker must not be attached to a real graded answer just
    /// because confidence happens to be low.
    #[test]
    fn a_graded_low_confidence_result_keeps_the_limits_marker() {
        let graded = serde_json::json!({
            "answer": "Partially supported.",
            "gaps": ["no release date for the benchmark"],
            "caveats": [],
            "confidence": 0.3
        })
        .to_string();
        let out = prepare_tool_result("reason", &graded, MAX_TOOL_RESULT_CHARS);
        assert!(out.starts_with("[REASON — LIMITS"), "{out}");
        assert!(out.contains("CONFIDENCE: 0.3 — low."));
    }

    /// Drive `forward_chat_events` over a scripted provider-loop output and
    /// return what the client saw, plus the captured assistant text.
    async fn drive_forwarder(events: Vec<ChatEvent>) -> (Vec<ChatEvent>, String) {
        let (inner_tx, inner_rx) = mpsc::channel::<ChatEvent>(64);
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(64);
        let (result_tx, mut result_rx) = mpsc::channel::<String>(1);

        for e in events {
            inner_tx.send(e).await.expect("scripted send");
        }
        drop(inner_tx);

        forward_chat_events(
            inner_rx,
            tx,
            result_tx,
            Some("s-1".into()),
            "create the task".into(),
            None,
        )
        .await;

        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            seen.push(e);
        }
        let final_text = result_rx.try_recv().unwrap_or_default();
        (seen, final_text)
    }

    /// A turn that drains normally is NOT an abandonment, however it ended —
    /// the dropped-turn warning already covers "ended with nothing to say", and
    /// double-reporting it would make the abandonment signal useless.
    #[test]
    fn a_drained_turn_is_not_reported_as_abandoned() {
        let mut g = TurnAbandonGuard::new(Some("sess-1"), "hello");
        assert!(g.should_warn(), "armed before the stream drains");
        g.mark_completed();
        assert!(!g.should_warn(), "a drained turn must not warn");
    }

    /// A turn dropped before its stream drained — the cancellation path, where
    /// none of `forward_chat_events`'s tail runs — must still be reported.
    #[test]
    fn a_cancelled_turn_is_reported_as_abandoned() {
        let g = TurnAbandonGuard::new(None, "summarize the videos you watched today");
        assert!(g.should_warn(), "a cancelled turn must warn on drop");
        assert_eq!(g.session_id, "-", "missing session id renders as '-'");
    }

    /// The 2026-08-24 failure: the loop returned having emitted nothing at all.
    #[tokio::test]
    async fn a_turn_with_no_message_and_no_error_reports_a_dropped_turn() {
        let (seen, final_text) = drive_forwarder(vec![ChatEvent::Done]).await;

        assert!(final_text.is_empty());
        assert!(
            matches!(seen.first(), Some(ChatEvent::Error { message }) if message.contains("no response")),
            "a dropped turn must surface an error, got: {seen:?}"
        );
        assert!(
            matches!(seen.last(), Some(ChatEvent::Done)),
            "the error must precede done, or the client never renders it: {seen:?}"
        );
        assert_eq!(
            seen.iter().filter(|e| matches!(e, ChatEvent::Done)).count(),
            1,
            "exactly one done"
        );
    }

    /// A turn that already failed loudly keeps its own explanation.
    #[tokio::test]
    async fn a_turn_that_reported_an_error_is_not_double_reported() {
        let (seen, _) = drive_forwarder(vec![
            ChatEvent::Error {
                message: "No LLM provider configured.".into(),
            },
            ChatEvent::Done,
        ])
        .await;

        let errors: Vec<_> = seen
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Error { message } => Some(message.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(errors, vec!["No LLM provider configured."]);
    }

    /// A normal turn must be untouched by any of the above.
    #[tokio::test]
    async fn a_normal_turn_passes_through_unchanged() {
        let (seen, final_text) = drive_forwarder(vec![
            ChatEvent::Token {
                content: "PO".into(),
            },
            ChatEvent::Message {
                content: "PONG".into(),
            },
            ChatEvent::Done,
        ])
        .await;

        assert_eq!(final_text, "PONG");
        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
            "no spurious error: {seen:?}"
        );
        assert!(matches!(seen.last(), Some(ChatEvent::Done)));
        assert_eq!(
            seen.iter().filter(|e| matches!(e, ChatEvent::Done)).count(),
            1
        );
    }

    #[test]
    fn a_clean_result_gets_no_marker() {
        let clean = serde_json::json!({
            "answer": "Nebula lighthouses accept DNS names in static_host_map.",
            "gaps": [],
            "caveats": [],
            "confidence": 0.9
        })
        .to_string();
        assert_eq!(prepare_tool_result("reason", &clean, 6000), clean);
    }

    #[test]
    fn low_confidence_alone_is_enough() {
        let shaky = serde_json::json!({"answer": "Probably.", "confidence": 0.2}).to_string();
        let out = prepare_tool_result("reason", &shaky, 6000);
        assert!(out.contains("CONFIDENCE: 0.2 — low."));
    }

    /// Only `reason` declares limits this way. A `search_web` payload that
    /// happens to carry a "gaps" key must not be rewritten.
    #[test]
    fn other_tools_are_untouched() {
        let payload = serde_json::json!({"gaps": ["something"], "confidence": 0.1}).to_string();
        assert_eq!(prepare_tool_result("search_web", &payload, 6000), payload);
    }

    /// A marker on a body we could not parse would be a claim we cannot
    /// support — and errored tool results are plain text.
    #[test]
    fn non_json_results_pass_through() {
        let text = "Reasoning failed: Provider error: HTTP request failed";
        assert_eq!(prepare_tool_result("reason", text, 6000), text);
    }

    #[test]
    fn verbose_limits_cannot_crowd_out_the_result() {
        let noisy = serde_json::json!({
            "answer": "ok",
            "gaps": ["g".repeat(5000), "second", "third", "fourth", "fifth"],
        })
        .to_string();
        let out = prepare_tool_result("reason", &noisy, 100_000);
        // Assert on the marker alone — the untouched payload below it still
        // carries every gap, which is the point: the marker is a summary, not
        // a replacement.
        let marker = out.split("\n\n").next().expect("marker is the first block");
        assert!(marker.contains('…'), "long items are elided: {marker}");
        assert!(
            !marker.contains("fourth"),
            "marker keeps at most {MAX_MARKER_ITEMS} items: {marker}"
        );
        assert!(marker.contains("second") && marker.contains("third"));
        assert!(
            marker.len() < 1500,
            "marker stays bounded: {}",
            marker.len()
        );
        assert!(out.contains("fourth"), "the raw result is left intact");
    }
}

#[cfg(test)]
mod search_sources_tests {
    use super::*;

    /// The shape SearXNG / SerpApi / Google CSE are normalised to. Trimmed from
    /// the real `search_web` result of 2026-08-24 whose sources went uncited.
    fn serp_results() -> String {
        serde_json::json!([
            {
                "link": "https://www.defined.net/compare/nebula-vs-tailscale/",
                "snippet": "Managed Nebula and Tailscale are both overlay networking tools…",
                "title": "Managed Nebula vs Tailscale - Defined Networking"
            },
            {
                "link": "https://github.com/FreeTAKTeam/Reticulum_Meshtastic_Integration",
                "snippet": "Seamless Integration of Meshtastic and Reticulum via RCH…",
                "title": "GitHub - FreeTAKTeam/Reticulum_Meshtastic_Integration"
            }
        ])
        .to_string()
    }

    #[test]
    fn sources_are_listed_with_citable_handles() {
        let out = prepare_tool_result("search_web", &serp_results(), MAX_TOOL_RESULT_CHARS);
        assert!(out.starts_with("[SEARCH SOURCES — 2 retrieved."), "{out}");
        assert!(out.contains(
            "[S1] Managed Nebula vs Tailscale - Defined Networking — \
             https://www.defined.net/compare/nebula-vs-tailscale/"
        ));
        assert!(out.contains("[S2] GitHub - FreeTAKTeam/Reticulum_Meshtastic_Integration"));
        assert!(out.contains("https://github.com/FreeTAKTeam/Reticulum_Meshtastic_Integration"));
        // The raw payload must survive underneath — snippets are the substance.
        assert!(out.contains("Seamless Integration of Meshtastic"));
    }

    /// Brave emits `url`/`description` rather than `link`/`snippet`; the
    /// post-filter in skills/search.rs already accepts both and so must this,
    /// or Brave results silently lose their citations.
    #[test]
    fn brave_url_field_is_accepted() {
        let brave = serde_json::json!([
            {"title": "Nebula docs", "url": "https://nebula.defined.net/docs/", "description": "d"}
        ])
        .to_string();
        let out = prepare_tool_result("search_web", &brave, 6000);
        assert!(out.contains("[S1] Nebula docs — https://nebula.defined.net/docs/"));
    }

    /// The whole point is that the links outlive a tight cap.
    #[test]
    fn urls_survive_truncation_of_the_body() {
        let items: Vec<_> = (0..12)
            .map(|i| {
                serde_json::json!({
                    "title": format!("Result {i}"),
                    "link": format!("https://example.com/{i}"),
                    "snippet": "z".repeat(2000),
                })
            })
            .collect();
        let payload = serde_json::Value::Array(items).to_string();
        let out = prepare_tool_result("search_web", &payload, 2000);
        assert!(out.contains("https://example.com/0"));
        assert!(
            out.contains("https://example.com/11"),
            "the last source's URL must survive even though its snippet is cut"
        );
        assert!(out.contains("[TRUNCATED:"));
    }

    #[test]
    fn source_count_is_capped() {
        let items: Vec<_> = (0..30)
            .map(|i| serde_json::json!({"title": "t", "link": format!("https://e.com/{i}")}))
            .collect();
        let payload = serde_json::Value::Array(items).to_string();
        let out = prepare_tool_result("search_web", &payload, 100_000);
        let marker = out.split("\n\n").next().unwrap();
        assert!(marker.contains(&format!("[S{MAX_MARKER_SOURCES}]")));
        assert!(!marker.contains(&format!("[S{}]", MAX_MARKER_SOURCES + 1)));
    }

    /// "Nobody has anything on this query" is a legitimate outcome and must not
    /// grow a marker telling the model to cite sources it does not have.
    #[test]
    fn an_empty_result_set_gets_no_marker() {
        assert_eq!(prepare_tool_result("search_web", "[]", 6000), "[]");
    }

    #[test]
    fn results_without_links_get_no_marker() {
        let payload = serde_json::json!([{"title": "t", "snippet": "s"}]).to_string();
        assert_eq!(prepare_tool_result("search_web", &payload, 6000), payload);
    }

    #[test]
    fn other_tools_are_untouched() {
        let payload = serde_json::json!([{"link": "https://example.com"}]).to_string();
        assert_eq!(prepare_tool_result("search_notes", &payload, 6000), payload);
    }

    #[test]
    fn a_search_error_string_passes_through() {
        let text = "SerpApi failed: 429 Too Many Requests";
        assert_eq!(prepare_tool_result("search_web", text, 6000), text);
    }
}

// ============================================================================
// Empty completions
// ============================================================================

#[cfg(test)]
mod tool_budget_tests {
    use super::*;
    use crate::clients::chat::test_support::{cloud_config, sse, test_request};
    use wiremock::matchers::{body_string_contains, method, path};
    use wiremock::{Mock, MockServer};

    /// One round that asks for a tool call — the model can emit this forever.
    const TOOL_CALL_SSE: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
        "\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"type\":\"function\",",
        "\"function\":{\"name\":\"search_notes\",\"arguments\":\"{}\"}}]},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":8}}\n\n",
        "data: [DONE]\n\n",
    );

    const WRAP_UP_SSE: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
        "\"content\":\"Here is what I found.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5}}\n\n",
        "data: [DONE]\n\n",
    );

    async fn drive(server: &MockServer) -> Vec<ChatEvent> {
        let svc = crate::clients::chat::test_support::chat_service(vec![]);
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(256);
        svc.run_ollama_cloud_loop(
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

    /// The 2026-08-25 regression: `gpt-oss:120b-cloud` spent all ten rounds
    /// re-searching and never wrote an answer, and the turn ended silently —
    /// 4 of 8 turns. The wrap-up round is matched on the nudge text, so this
    /// also asserts the nudge is actually sent rather than merely intended.
    #[tokio::test]
    async fn a_model_that_only_calls_tools_is_asked_to_answer_without_them() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .and(body_string_contains(
                "You have used all available tool calls",
            ))
            .respond_with(sse(WRAP_UP_SSE))
            .expect(1)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(TOOL_CALL_SSE))
            .expect(MAX_TOOL_ITERATIONS as u64)
            .mount(&server)
            .await;

        let seen = drive(&server).await;

        assert!(
            seen.iter()
                .any(|e| matches!(e, ChatEvent::Message { content } if content == "Here is what I found.")),
            "the wrap-up round's answer must reach the client: {seen:?}"
        );
        assert!(
            !seen.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
            "a turn that recovered in the wrap-up round is not a failure: {seen:?}"
        );
    }

    /// And when even that produces nothing, say what happened — the tool calls
    /// ran, so the user has to be told work may have occurred.
    #[tokio::test]
    async fn a_turn_that_never_answers_says_the_tool_budget_ran_out() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(sse(TOOL_CALL_SSE))
            .mount(&server)
            .await;

        let seen = drive(&server).await;

        let errors: Vec<&String> = seen
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Error { message } => Some(message),
                _ => None,
            })
            .collect();
        assert_eq!(errors.len(), 1, "expected one error, got: {seen:?}");
        assert_eq!(errors[0], NO_ANSWER_AFTER_TOOLS_MESSAGE);
        // The generic dropped-turn line would say "reported no error", which is
        // both wrong and useless once this fires.
        assert!(!errors[0].contains("reported no error"));
    }
}
