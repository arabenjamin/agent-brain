//! Retry caps, nudges and the tool-trace receipt shared by every provider loop.

/// How many times to re-send a request that came back as an **empty completion**
/// — the provider reported generating tokens but the stream carried no text and
/// no tool call.
///
/// This is a provider failure wearing the costume of a finished turn, and the
/// distinction is the same one [`agent_brain_llm::shared_llm`] draws with
/// `classify_unavailable`: *the provider could not answer* is retryable, *the
/// provider answered and declined* is not. Treating it as an answer is what
/// produced the silent dropped turn: the loop exited having emitted nothing,
/// the user's message was already banked, and the only trace was a WARN.
///
/// Measured 2026-08-25 against `gemma4:31b-cloud` on ollama.com — the same
/// prompt, twelve times, streaming three chunks and 230–290 `eval_count`
/// tokens every run:
///
/// ```text
/// #1  eval_tok=230  content=0  tool_calls=1
/// #2  eval_tok=268  content=0  tool_calls=0  <-- EMPTY
/// #3  eval_tok=259  content=0  tool_calls=0  <-- EMPTY
/// #4  eval_tok=292  content=0  tool_calls=0  <-- EMPTY
/// #5..#12                      tool_calls=1
/// ```
///
/// Every run spends its tokens emitting a tool call; ~75% of the time Ollama's
/// server-side template parser extracts it into `tool_calls`, and ~25% of the
/// time it fails to parse it but strips it from `content` anyway, leaving both
/// fields empty with `finish_reason: "stop"`. The native `/api/chat` endpoint
/// behaves identically (`content`, `thinking`, and `tool_calls` all empty), so
/// this is not a parsing bug on our side — there is genuinely nothing in the
/// stream to read.
///
/// At a 25% rate one retry takes the user-visible failure to ~6% and two to
/// ~1.5%. Retries re-send the *identical* message list and therefore consume a
/// slot from [`MAX_TOOL_ITERATIONS`]; with a cap of 2 that is affordable.
pub(super) const MAX_EMPTY_COMPLETION_RETRIES: usize = 2;

/// Shown when every retry above also came back empty.
///
/// It has to say that nothing was carried out. An empty completion is
/// indistinguishable, from the user's seat, from a turn where the assistant
/// quietly did the work — and the turn that exposed this was a request to go
/// and change something.
pub(super) const EMPTY_COMPLETION_MESSAGE: &str = "The model provider returned an empty response — it reported generating tokens but sent \
     no text and no tool call — and did so again on every retry. Nothing was saved and no \
     action you asked for was carried out. Please retry.";

/// Appended for one last pass when a turn has used every tool iteration and
/// still not written an answer.
///
/// Running out of tool rounds used to end the turn with nothing: the `for` loop
/// simply fell through, sent `Done`, and the dropped-turn detector reported
/// *"produced no response, and reported no error"* — true, but not the reason,
/// and useless to a user who watched ten searches go by. Measured 2026-08-25 on
/// `gpt-oss:120b-cloud`, which is fast and reliable per call but loops: given
/// this prompt it re-searched `Tech Dependency Synthesis` at four different
/// `limit` values, spent all ten rounds, and answered nothing — **4 of 8 turns**.
///
/// The wrap-up round is offered **no tools at all**, which is what makes it
/// work: a model that keeps choosing to search cannot choose to search again,
/// and the gathered results are already in its context. Nudging with the tools
/// still attached just buys an eleventh search.
pub(super) const FINAL_ROUND_NUDGE: &str = "You have used all available tool calls for this turn. Do not \
     request any more. Write the final answer now, using only what you have already gathered \
     above. If the information is incomplete, say what you found, say plainly what is still \
     missing, and stop.";

/// Reported when even the wrap-up round produced nothing.
pub(super) const NO_ANSWER_AFTER_TOOLS_MESSAGE: &str = "The assistant used every available tool call for this turn and still did not produce an \
     answer. Any tool calls above did run, so work may have been done, but nothing was written \
     back. Please retry — and check anything that looks like it should have been created.";

/// A factual record of the tool calls a turn actually made — the ground truth a
/// final answer must be built from.
///
/// The chat models compose the reply from tool results plus their own prose, and
/// that prose can narrate actions that never ran. On 2026-09-08 a turn asked to
/// "find and use the GDELT API" reported it had probed the API (blocked by "the
/// sandbox's outbound HTTP restrictions") and was drafting a Python client —
/// while the only tools it ran were `search_notes` and `search_web`: no
/// `http_request`, no `execute_code`, no `ingest_api`. The worker's fabricated
/// prose was then handed to the synthesis model as "initial analysis" and
/// polished into the answer the user saw. Handing the composer this list — and
/// stating plainly that anything absent did not happen — anchors the answer to
/// what the tools did, not what the model wishes it had done. This is the same
/// receipt idea `AgentFindings.tool_trace` gives sub-agents, applied to the main
/// chat loop, which had no equivalent.
pub(super) fn tool_trace_receipt(trace: &[(String, bool)]) -> String {
    if trace.is_empty() {
        return "GROUND TRUTH — no tools ran this turn. Do not describe any action \
                (searching, fetching a URL, running code, creating, registering, or \
                ingesting anything) as having happened; answer only from prior knowledge \
                and say so."
            .to_string();
    }
    let mut lines = String::from(
        "GROUND TRUTH — the ONLY tools that ran this turn, in order. Build the answer \
         strictly from these. Do NOT claim any action that is not listed: if `http_request` \
         is absent you fetched no URL, if `execute_code` is absent you ran no code, if \
         `ingest_api`/`define_api_context` are absent you registered no API. Treat any such \
         claim (including in any 'initial analysis') as something that did NOT happen:\n",
    );
    for (name, success) in trace {
        let mark = if *success { "ok" } else { "FAILED" };
        lines.push_str(&format!("  - {name} [{mark}]\n"));
    }
    lines
}

//
// Gated behind SUBAGENT_ENABLED (default OFF) so it cannot perturb chat until it
// has been measured. When on, the orchestrator gains one pseudo-tool, `delegate`,
// which is INTERCEPTED in the chat loop (not dispatched to ToolHandler) and runs
// a nested, bounded tool-loop under a worker profile whose findings fold back
// into the turn. Conservative Phase 0 policy: explicit calls only, read-only
// sub-agents, a curated roster, serial, depth-capped at 1.

/// Maximum number of non-system messages kept in the OllamaCloud/Ollama message history.
/// Older messages are dropped (keeping system + user + last N) to prevent context overflow.
pub(super) const MAX_HISTORY_MESSAGES: usize = 10;
