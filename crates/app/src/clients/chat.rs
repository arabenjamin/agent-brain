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

/// How many times to re-send a request that came back as an **empty completion**
/// — the provider reported generating tokens but the stream carried no text and
/// no tool call.
///
/// This is a provider failure wearing the costume of a finished turn, and the
/// distinction is the same one [`crate::services::shared_llm`] draws with
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
const MAX_EMPTY_COMPLETION_RETRIES: usize = 2;

/// Shown when every retry above also came back empty.
///
/// It has to say that nothing was carried out. An empty completion is
/// indistinguishable, from the user's seat, from a turn where the assistant
/// quietly did the work — and the turn that exposed this was a request to go
/// and change something.
const EMPTY_COMPLETION_MESSAGE: &str = "The model provider returned an empty response — it reported generating tokens but sent \
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
const FINAL_ROUND_NUDGE: &str = "You have used all available tool calls for this turn. Do not \
     request any more. Write the final answer now, using only what you have already gathered \
     above. If the information is incomplete, say what you found, say plainly what is still \
     missing, and stop.";

/// Reported when even the wrap-up round produced nothing.
const NO_ANSWER_AFTER_TOOLS_MESSAGE: &str = "The assistant used every available tool call for this turn and still did not produce an \
     answer. Any tool calls above did run, so work may have been done, but nothing was written \
     back. Please retry — and check anything that looks like it should have been created.";

// ============================================================================
// Turn write guard — grounding and duplicate suppression for chat-authored writes
// ============================================================================

/// Tools that constitute *retrieval*: after one of these runs, the turn has
/// consulted something outside the model's own weights.
///
/// The list is deliberately narrow. `read_file`/`get_file_tree` are retrieval in
/// the literal sense but say nothing about the *world*, and the failure this
/// guards against is a note about an external technology written from prior
/// alone. Graph reads count because "what do we already know" is a legitimate
/// grounding for a `semantic` note that consolidates stored knowledge.
const RETRIEVAL_TOOLS: &[&str] = &[
    "search_web",
    "search_notes",
    "neo4j_query",
    "fetch_transcript",
    "ingest_media",
    "list_channel_videos",
    "http_request",
    "get_search_usage",
    "execute_code",
];

/// Tools whose effects persist beyond the turn. An identical call to one of
/// these twice in a single turn is never intentional — see
/// [`TurnWriteGuard::screen`].
const WRITE_TOOLS: &[&str] = &[
    "store_note",
    "create_task",
    "write_workspace_file",
    "notify_user",
    "claim",
    "manage_scheduled_task",
];

/// Appended to a `store_note` result when the guard downgraded its type, so the
/// model is told what was actually stored rather than silently getting its way.
const DOWNGRADE_NOTICE: &str = "\n\n[GUARD — NOTE TYPE DOWNGRADED TO `unsourced_synthesis`: \
     this note was written as `semantic` (knowledge the brain established) but no retrieval \
     tool ran in this turn, so nothing outside the model's own prior backs it. It is stored \
     and retrievable, but labelled as unsourced on the way out. Say so in your answer. To \
     store it as `semantic`, search first and write it from what you found.]";

/// Returned in place of a re-executed duplicate write. See [`TurnWriteGuard::screen`].
const DUPLICATE_NOTICE: &str = "[GUARD — DUPLICATE WRITE SUPPRESSED: this exact call was \
     already executed earlier in this turn and was not run again. The earlier call succeeded; \
     do not repeat it. Move on, or write the final answer.]";

/// Per-turn state for [`TurnWriteGuard::screen`].
///
/// Two failures observed on 2026-08-31, both from one chat turn, both silent:
///
/// 1. `gpt-oss:120b-cloud` was asked for technical deep-dives on three mesh
///    technologies. It ran **zero** searches (confirmed against the
///    `search_usage` ledger) and wrote all three from prior — including one for
///    "Metastatic", a typo of *Meshtastic* that had been sitting in a Todo since
///    08-11. It invented a Rust crate, an API, a handshake, and a citation, and
///    stored it as `note_type: semantic`, the one type `label_claims`
///    deliberately does *not* mark on retrieval. Fluent invention filed as
///    established knowledge is the worst cell in the table.
/// 2. The same turn wrote 15 notes for 3 topics — 7 near-identical `metastatic`
///    notes, 4 `reticulum`, 4 `tailscale` — as the tool loop re-issued
///    `store_note` on successive iterations.
///
/// Neither is a model-quality problem that a better prompt fixes: the first is a
/// missing precondition and the second is a missing idempotence check. The guard
/// is deliberately **non-blocking** — a downgraded note is still stored and a
/// duplicate still reports success — because a chat turn that refuses to write
/// is worse than one that writes with an honest label, and because this runs on
/// every turn where a false positive would otherwise cost the user real work.
#[derive(Debug, Default)]
struct TurnWriteGuard {
    /// Set once any tool in [`RETRIEVAL_TOOLS`] has run this turn.
    retrieval_ran: bool,
    /// Fingerprints of write calls already executed this turn.
    seen_writes: std::collections::HashSet<u64>,
}

/// Note types exempt from the grounding downgrade.
///
/// Only `semantic` claims to be *knowledge the brain established*; every other
/// type either already carries its provenance (`claim`, `source_record`,
/// `inference`, `unsourced_synthesis`) or is a record of the turn itself
/// (`episodic`, `reflection`, `outcome`), which is legitimately unsourced.
/// `None` is guarded because `store_note` defaults it to `semantic`.
fn note_type_needs_grounding(note_type: Option<&str>) -> bool {
    matches!(note_type.map(str::trim), None | Some("") | Some("semantic"))
}

/// Stable fingerprint of a write call, used for duplicate suppression.
fn write_fingerprint(tool: &str, args: &Value) -> u64 {
    use std::hash::{Hash as _, Hasher as _};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    tool.hash(&mut hasher);
    // Serialising a `Value` is order-stable: serde_json preserves object key
    // order, and both calls come from the same model emitting the same JSON.
    serde_json::to_string(args)
        .unwrap_or_default()
        .hash(&mut hasher);
    hasher.finish()
}

/// What the guard decided about one tool call.
#[derive(Debug, PartialEq)]
enum Screened {
    /// Execute as normal.
    Pass,
    /// Execute, but with these args, and append this notice to the result.
    Rewrite { args: Value, notice: &'static str },
    /// Do not execute; return this text as the result instead.
    Suppress { notice: &'static str },
}

impl TurnWriteGuard {
    /// Record that `tool` ran, then decide what to do with the *next* call.
    ///
    /// Called once per tool call, before dispatch. Ordering inside is
    /// load-bearing: the duplicate check runs before the downgrade so a repeated
    /// call is suppressed on its original fingerprint rather than on the
    /// rewritten one, which would let the same note through twice — once as
    /// `semantic` and once as `unsourced_synthesis`.
    fn screen(&mut self, tool: &str, args: &Value) -> Screened {
        if WRITE_TOOLS.contains(&tool) {
            let fp = write_fingerprint(tool, args);
            if !self.seen_writes.insert(fp) {
                return Screened::Suppress {
                    notice: DUPLICATE_NOTICE,
                };
            }
        }

        if tool == "store_note" && !self.retrieval_ran {
            let note_type = args.get("note_type").and_then(Value::as_str);
            if note_type_needs_grounding(note_type) {
                let mut rewritten = args.clone();
                if let Some(obj) = rewritten.as_object_mut() {
                    obj.insert("note_type".into(), json!("unsourced_synthesis"));
                    return Screened::Rewrite {
                        args: rewritten,
                        notice: DOWNGRADE_NOTICE,
                    };
                }
            }
        }

        Screened::Pass
    }

    /// Mark retrieval as having happened. Called *after* dispatch so a tool
    /// cannot ground the very call that invoked it.
    fn observe(&mut self, tool: &str, success: bool) {
        if success && RETRIEVAL_TOOLS.contains(&tool) {
            self.retrieval_ran = true;
        }
    }
}

/// Execute one tool call through the turn guard, returning `(success, text)`.
///
/// **All four provider loops dispatch through here.** Wiring a guard into one
/// loop and leaving the other three is the "labelled in one path, not the
/// other" failure this codebase has re-learned repeatedly — most recently when
/// claim labelling covered one retrieval path and the unlabelled copy of the
/// same assertion reached the context window anyway.
///
/// A suppressed duplicate reports `success = true`: the identical earlier call
/// did succeed, and reporting failure would invite the model to retry the very
/// write being suppressed.
async fn execute_guarded(
    handler: &Option<ToolHandler>,
    guard: &mut TurnWriteGuard,
    tool_name: &str,
    tool_args: &Value,
) -> (bool, String) {
    let (args_to_send, notice) = match guard.screen(tool_name, tool_args) {
        Screened::Suppress { notice } => {
            warn!(tool = %tool_name, "Duplicate write suppressed within chat turn");
            return (true, notice.to_string());
        }
        Screened::Rewrite { args, notice } => {
            warn!(
                tool = %tool_name,
                "Ungrounded semantic note downgraded to unsourced_synthesis — no retrieval ran this turn"
            );
            (args, Some(notice))
        }
        Screened::Pass => (tool_args.clone(), None),
    };

    let Some(h) = handler else {
        return (false, "No tool handler available".to_string());
    };

    let args = if args_to_send.is_object() {
        Some(args_to_send)
    } else {
        None
    };
    let result = h.execute(tool_name, args).await;
    let is_err = result.is_error.unwrap_or(false);
    let mut text = result
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

    guard.observe(tool_name, !is_err);

    // Only annotate a write that actually happened — appending the downgrade
    // notice to an error would describe a note that was never stored.
    if let Some(n) = notice
        && !is_err
    {
        text.push_str(n);
    }

    (!is_err, text)
}

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
TIME: right now it is {NOW}. That is local wall-clock time, and it is read \
fresh at the start of every turn — treat it as authoritative over anything you \
remember about the date. Resolve \"today\", \"tonight\", \"this week\", and \
\"recent\" against it. When searching for recent content, include the current \
date in your queries (e.g. \"daily news brief {DATE}\"). Timestamps stored in \
the graph are UTC, which is {UTC_NOW} right now — convert before quoting one to \
a person, and note that a UTC timestamp can carry a different calendar date \
than the local one above.\n\
{PATHS_SECTION}\
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

/// Cap a tool result for the model, appending an explicit marker when content
/// was dropped.
///
/// Silent truncation is worse than a short answer: the model cannot distinguish
/// "the list ends here" from "the list was cut here" and states the former as
/// fact. Observed 2026-08-10 — `manage_scheduled_task(action=list)` returned all
/// 16 schedules, the tail was silently cut, and the model reported the three
/// schedules that had been cut off as not existing at all. The marker turns an
/// invisible loss into something the model can react to by narrowing its query.
fn truncate_tool_result(text: &str, limit: usize) -> String {
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
const LIMIT_DECLARING_TOOLS: &[&str] = &["reason"];

/// Below this, `confidence` is worth naming on its own. Strictly less than, so
/// the 0.5 that `reason` emits as its parse-failure default does not fire the
/// marker by itself — a genuinely uncertain answer nearly always also populates
/// `gaps` or `caveats`, which do.
const LOW_CONFIDENCE: f64 = 0.5;

/// Keep the marker bounded: it is prepended to a result that is itself capped.
const MAX_MARKER_ITEMS: usize = 3;
const MAX_MARKER_ITEM_CHARS: usize = 200;

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
fn annotate_tool_result(tool_name: &str, text: &str) -> String {
    let marker =
        reason_limits_marker(tool_name, text).or_else(|| search_sources_marker(tool_name, text));
    match marker {
        Some(marker) => format!("{marker}\n\n{text}"),
        None => text.to_string(),
    }
}

/// Tools that return retrieved sources the reply is expected to cite.
const SOURCE_LISTING_TOOLS: &[&str] = &["search_web"];

/// Cap on sources listed in the marker. `search_web` returns at most 20.
const MAX_MARKER_SOURCES: usize = 12;
const MAX_MARKER_TITLE_CHARS: usize = 90;

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
fn search_sources_marker(tool_name: &str, text: &str) -> Option<String> {
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
fn reason_limits_marker(tool_name: &str, text: &str) -> Option<String> {
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

fn push_marker_section(marker: &mut String, heading: &str, items: &[String]) {
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
fn capped_items(parsed: &Value, key: &str) -> Vec<String> {
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
fn prepare_tool_result(tool_name: &str, text: &str, limit: usize) -> String {
    truncate_tool_result(&annotate_tool_result(tool_name, text), limit)
}

/// Relay every event from a provider loop to the caller, capture the final
/// assistant text, and guarantee the turn ends with exactly one `Done`.
///
/// This owns the only detector for a **dropped turn** — a turn where the user's
/// message was banked, the provider loop returned, and nothing was ever written
/// back. Observed 2026-08-24: a user asked the brain to create a scheduled task,
/// the message was persisted to working memory at 18:49:30, and then nothing —
/// no reply, no tool call, no error event, and zero ERROR-level log lines all
/// day. The empty string was dropped by an `!is_empty()` guard on the persist
/// path that had no else branch, so the failure had no representation anywhere
/// and the user was left believing the work was underway.
///
/// `tx` is moved in here and dropped when this returns, so this is the last
/// point at which anything can still be said to the client — code after the
/// provider loops in `run()` cannot reach it.
async fn forward_chat_events(
    mut inner_rx: mpsc::Receiver<ChatEvent>,
    tx: mpsc::Sender<ChatEvent>,
    result_tx: mpsc::Sender<String>,
    session_id: Option<String>,
    user_snippet: String,
) {
    let mut final_text = String::new();
    let mut saw_error = false;
    while let Some(event) = inner_rx.recv().await {
        match &event {
            ChatEvent::Message { content } => final_text = content.clone(),
            ChatEvent::Error { .. } => saw_error = true,
            // Held back and re-emitted below. The client closes its reader on
            // `done`, so an error reported after it would never be displayed —
            // the turn has to be marked failed *before* it is marked finished.
            ChatEvent::Done => continue,
            _ => {}
        }
        let _ = tx.send(event).await;
    }

    // `saw_error` keeps this from double-reporting a turn that already failed
    // loudly (e.g. no provider configured): that turn has an explanation, and a
    // second, vaguer error would only obscure it.
    if final_text.trim().is_empty() && !saw_error {
        warn!(
            session_id = session_id.as_deref().unwrap_or("-"),
            user_message = %user_snippet,
            "Chat turn produced no assistant message and no error — dropped turn"
        );
        let _ = tx
            .send(ChatEvent::Error {
                message: "The assistant produced no response for this turn, and reported no \
                          error explaining why. Nothing was saved. Please retry — and do not \
                          assume any action you asked for was carried out."
                    .into(),
            })
            .await;
    }

    // Always terminate the stream, including when a provider loop returned
    // without sending one.
    let _ = tx.send(ChatEvent::Done).await;
    let _ = result_tx.send(final_text).await;
}

/// Compose the effective system prompt for one chat turn.
///
/// Layered so the most specific guidance sits closest to the conversation:
/// the shared base rules, then the active context profile's `system_prompt`,
/// then whatever its `pre_load_query` returned. The profile layers are dropped
/// when no profile applies (explicit tool allowlists, or no context builder).
fn build_system_prompt(profile_prompt: Option<&str>, pre_loaded: &[String]) -> String {
    let mut prompt = build_base_system_prompt();

    if let Some(p) = profile_prompt.map(str::trim).filter(|p| !p.is_empty()) {
        prompt.push_str("\n\n");
        prompt.push_str(p);
    }

    if !pre_loaded.is_empty() {
        prompt.push_str(
            "\n\n# LIVE SELF-STATE — read from your own graph just now. \
             This is authoritative: prefer it over any recollection, and do not \
             contradict it.\n\n",
        );
        prompt.push_str(&pre_loaded.join("\n\n"));
    }

    prompt
}

/// Compose the shared base prompt for one turn.
///
/// Called per turn (from `run`), not cached, so the clock in the prompt stays
/// correct across a long session and across a local midnight.
fn build_base_system_prompt() -> String {
    let date = crate::services::clock::today();
    let now = crate::services::clock::now_stamp();
    let utc_now = chrono::Utc::now().format("%Y-%m-%d %H:%M UTC").to_string();
    let codebase_dir = std::env::var("CODEBASE_DIR").unwrap_or_default();
    let workspace_dir = std::env::var("WORKSPACE_DIR").unwrap_or_default();
    let paths_section = match (codebase_dir.is_empty(), workspace_dir.is_empty()) {
        (false, false) => format!(
            "Your codebase root is `{codebase_dir}` (read-only via codebase tools). \
             Your writable workspace is `{workspace_dir}` — use write_workspace_file to create files there.\n"
        ),
        (false, true) => {
            format!("Your codebase root is `{codebase_dir}` (read-only via codebase tools).\n")
        }
        (true, false) => format!(
            "Your writable workspace is `{workspace_dir}` — use write_workspace_file to create files there.\n"
        ),
        (true, true) => String::new(),
    };
    DEFAULT_SYSTEM_PROMPT_TEMPLATE
        .replace("{DATE}", &date)
        .replace("{NOW}", &now)
        .replace("{UTC_NOW}", &utc_now)
        .replace("{PATHS_SECTION}", &paths_section)
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

/// What one model's attempt at a turn amounted to.
///
/// The distinction the fallback ladder turns on is not "did it error" but **did
/// anything reach the client**. A model that streamed a token, a tool call, or
/// a message owns the turn even if it failed afterwards: re-running the turn on
/// another model would re-execute its tool calls and emit a second answer after
/// the first. Only a turn that produced *nothing* is safe to hand onward, and
/// that is also the only turn worth handing onward.
#[derive(Debug)]
enum AttemptOutcome {
    /// Something reached the client. The turn is this model's, for better or worse.
    Delivered,
    /// Nothing reached the client, so another model may still answer this turn.
    /// `error` is whatever the loop tried to report, held back so the ladder can
    /// decide whether the user ever sees it.
    Unanswered { error: Option<String> },
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
    /// Usage ledger. Chat is the main cloud consumer — every LLM call made by
    /// the chat loops must land in `model_usage` or quota accounting is fiction.
    telemetry: Option<crate::repository::TelemetryClient>,
    /// Models to try, in order, when a turn produces nothing the user can see.
    /// Resolved from `models.yaml`'s `chat_fallback_ladder` at startup; empty
    /// means the active model is the only one that will ever be tried.
    fallback_ladder: Vec<LlmConfig>,
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
            telemetry: None,
            fallback_ladder: Vec::new(),
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
        Arc::new(Self {
            tool_handler,
            tool_registry,
            llm_config,
            context_builder,
            telemetry,
            fallback_ladder,
        })
    }

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
    async fn run_one_attempt(
        &self,
        cfg: LlmConfig,
        tools: Vec<agent_brain_protocol::ToolDefinition>,
        handler: Option<ToolHandler>,
        request: ChatRequest,
        system_prompt: String,
        sink: &mpsc::Sender<ChatEvent>,
    ) -> AttemptOutcome {
        let (attempt_tx, mut attempt_rx) = mpsc::channel::<ChatEvent>(128);

        let provider_loop = async {
            match cfg.provider {
                LlmProviderType::Anthropic => {
                    self.run_anthropic_loop(cfg, tools, handler, request, system_prompt, attempt_tx)
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
                    )
                    .await
                }
                LlmProviderType::OllamaCloud => {
                    self.run_ollama_cloud_loop(
                        cfg,
                        tools,
                        handler,
                        request,
                        system_prompt,
                        attempt_tx,
                    )
                    .await
                }
                _ => {
                    self.run_text_loop(cfg, tools, handler, request, system_prompt, attempt_tx)
                        .await
                }
            }
        };

        let relay = async {
            let mut delivered = false;
            let mut error: Option<String> = None;
            while let Some(event) = attempt_rx.recv().await {
                match &event {
                    ChatEvent::Done => continue,
                    ChatEvent::Error { message } => {
                        // Keep the first: it is the one that describes what
                        // actually went wrong, before any cleanup reporting.
                        if error.is_none() {
                            error = Some(message.clone());
                        }
                        continue;
                    }
                    _ => delivered = true,
                }
                let _ = sink.send(event).await;
            }

            if delivered {
                if let Some(message) = error {
                    let _ = sink.send(ChatEvent::Error { message }).await;
                }
                AttemptOutcome::Delivered
            } else {
                AttemptOutcome::Unanswered { error }
            }
        };

        // The loop owns `attempt_tx`; the relay ends when that drop closes the
        // channel, so these must run concurrently rather than in sequence.
        let (_, outcome) = tokio::join!(provider_loop, relay);
        outcome
    }

    /// Record one chat LLM call in the usage ledger (tool_name = "chat").
    fn record_llm_call(
        &self,
        model: &str,
        success: bool,
        duration_ms: i64,
        tokens_in: Option<i64>,
        tokens_out: Option<i64>,
    ) {
        if let Some(ref tc) = self.telemetry {
            let _ = tc.record_model_usage(
                model,
                Some("chat"),
                success,
                Some(duration_ms),
                tokens_in,
                tokens_out,
                None,
            );
        }
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
                (filtered, prompt, notes)
            } else {
                (all_tools, None, Vec::new())
            }
        } else {
            (all_tools, None, Vec::new())
        };

        // Compose once here so every provider loop runs the same prompt.
        let system_prompt = build_system_prompt(profile_system_prompt.as_deref(), &profile_notes);

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
        tokio::spawn(forward_chat_events(
            inner_rx,
            tx,
            result_tx,
            dropped_turn_session,
            dropped_turn_snippet,
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
                        resolved_profile.as_deref().unwrap_or("general"),
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
        }

        match config {
            None => {
                let _ = inner_tx
                    .send(ChatEvent::Error {
                        message: "No LLM provider configured. Use `use_model` to set one.".into(),
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
                let mut candidates = vec![active];
                for cfg in &self.fallback_ladder {
                    if !candidates.iter().any(|c| rung_id(c) == rung_id(cfg)) {
                        candidates.push(cfg.clone());
                    }
                }

                let last = candidates.len().saturating_sub(1);
                let mut last_error: Option<String> = None;

                for (i, cfg) in candidates.into_iter().enumerate() {
                    if i > 0 {
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
                    match self
                        .run_one_attempt(
                            cfg,
                            tools.clone(),
                            handler.clone(),
                            request.clone(),
                            system_prompt.clone(),
                            &inner_tx,
                        )
                        .await
                    {
                        AttemptOutcome::Delivered => break,
                        AttemptOutcome::Unanswered { error } => {
                            last_error = error.or(last_error);
                            if i == last {
                                // Every rung is spent. Report the concrete
                                // failure rather than letting the dropped-turn
                                // detector emit its generic one.
                                warn!(
                                    model = %model,
                                    rungs_tried = i + 1,
                                    "Every chat model produced nothing for this turn"
                                );
                                let _ = inner_tx
                                    .send(ChatEvent::Error {
                                        message: last_error.clone().unwrap_or_else(|| {
                                            EMPTY_COMPLETION_MESSAGE.to_string()
                                        }),
                                    })
                                    .await;
                                let _ = inner_tx.send(ChatEvent::Done).await;
                            }
                        }
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
        system_prompt: String,
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

        let mut empty_completions = 0usize;

        // One guard per turn: grounding state and duplicate fingerprints are
        // per-turn, and the ladder may run this loop once per rung.
        let mut write_guard = TurnWriteGuard::default();

        for _iteration in 0..MAX_TOOL_ITERATIONS {
            let body = json!({
                "model": model,
                "max_tokens": 4096,
                "system": system_prompt,
                "tools": anthropic_tools,
                "messages": messages,
            });

            let call_start = std::time::Instant::now();
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
                    self.record_llm_call(
                        &model,
                        false,
                        call_start.elapsed().as_millis() as i64,
                        None,
                        None,
                    );
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
                let rate_limited = msg.contains("rate") || msg.contains("429");
                if let Some(ref tc) = self.telemetry {
                    let _ = tc.record_model_usage(
                        &model,
                        Some("chat"),
                        false,
                        Some(call_start.elapsed().as_millis() as i64),
                        None,
                        None,
                        rate_limited.then_some("rate_limited"),
                    );
                }
                let _ = tx.send(ChatEvent::Error { message: msg }).await;
                let _ = tx.send(ChatEvent::Done).await;
                return;
            }

            self.record_llm_call(
                &model,
                true,
                call_start.elapsed().as_millis() as i64,
                resp_json["usage"]["input_tokens"].as_i64(),
                resp_json["usage"]["output_tokens"].as_i64(),
            );

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
                // Nothing at all came back: no text and no tool use. Same failure
                // as the other loops — retry rather than exit silently.
                // See MAX_EMPTY_COMPLETION_RETRIES.
                if final_text.is_empty() && tool_use_blocks.is_empty() {
                    if empty_completions < MAX_EMPTY_COMPLETION_RETRIES {
                        empty_completions += 1;
                        warn!(
                            model = %model,
                            stop_reason = %stop_reason,
                            attempt = empty_completions,
                            "Anthropic returned an empty completion — retrying"
                        );
                        continue;
                    }
                    warn!(
                        model = %model,
                        attempts = empty_completions + 1,
                        "Anthropic returned an empty completion on every attempt — giving up"
                    );
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: EMPTY_COMPLETION_MESSAGE.into(),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }

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

                let (success, result_text) =
                    execute_guarded(&handler, &mut write_guard, &tool_name, &tool_input).await;

                let preview: String =
                    prepare_tool_result(&tool_name, &result_text, MAX_TOOL_RESULT_CHARS);
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
        system_prompt: String,
        tx: mpsc::Sender<ChatEvent>,
    ) {
        let do_synthesis = request.synthesis_provider.is_some();
        let mut gathered: Vec<String> = Vec::new();
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
        let mut messages: Vec<Value> = vec![json!({ "role": "system", "content": system_prompt })];
        for h in &request.history {
            messages.push(json!({ "role": h.role, "content": h.content }));
        }
        messages.push(json!({ "role": "user", "content": request.message }));

        let client = reqwest::Client::new();
        let mut weak_model_answer = String::new();
        let mut empty_completions = 0usize;
        let mut tool_rounds = 0usize;
        let mut answered = false;
        let mut final_round = false;

        // One guard per turn: grounding state and duplicate fingerprints are
        // per-turn, and the ladder may run this loop once per rung.
        let mut write_guard = TurnWriteGuard::default();

        for _ in 0..=(MAX_TOOL_ITERATIONS + MAX_EMPTY_COMPLETION_RETRIES + 1) {
            // The wrap-up round: no tools offered, so a model that keeps
            // choosing to search has to answer instead. See FINAL_ROUND_NUDGE.
            if !final_round && tool_rounds >= MAX_TOOL_ITERATIONS {
                final_round = true;
                warn!(
                    model = %model,
                    rounds = tool_rounds,
                    "Tool budget exhausted without an answer — asking for a final answer with no tools"
                );
                messages.push(json!({ "role": "user", "content": FINAL_ROUND_NUDGE }));
            }

            let mut body = json!({
                "model": model,
                "messages": messages,
                "stream": true,
                "options": {
                    "temperature": config.temperature,
                }
            });
            if !final_round {
                body["tools"] = Value::Array(ollama_tools.clone());
            }

            let mut req = client
                .post(format!("{}/api/chat", base_url))
                .header("content-type", "application/json")
                .timeout(config.timeout)
                .json(&body);
            if let Some(ref key) = config.api_key {
                req = req.header("Authorization", format!("Bearer {}", key));
            }
            let call_start = std::time::Instant::now();
            let response = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    self.record_llm_call(
                        &model,
                        false,
                        call_start.elapsed().as_millis() as i64,
                        None,
                        None,
                    );
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
            // Usage from the final (done) chunk, for the model_usage ledger.
            let mut usage_tokens: (Option<i64>, Option<i64>) = (None, None);
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
                        usage_tokens = (
                            chunk_json["prompt_eval_count"].as_i64(),
                            chunk_json["eval_count"].as_i64(),
                        );
                        break 'stream;
                    }
                }
            }

            self.record_llm_call(
                &model,
                true,
                call_start.elapsed().as_millis() as i64,
                usage_tokens.0,
                usage_tokens.1,
            );

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
                // Nothing at all came back: no text and no tool call, despite the
                // provider counting generated tokens. That is a failed call, not a
                // finished turn — re-send the identical request rather than exiting
                // silently. See MAX_EMPTY_COMPLETION_RETRIES.
                if content.is_empty() {
                    if empty_completions < MAX_EMPTY_COMPLETION_RETRIES {
                        empty_completions += 1;
                        warn!(
                            model = %model,
                            eval_count = ?usage_tokens.1,
                            attempt = empty_completions,
                            "Ollama returned an empty completion — retrying"
                        );
                        continue;
                    }
                    warn!(
                        model = %model,
                        attempts = empty_completions + 1,
                        "Ollama returned an empty completion on every attempt — giving up"
                    );
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: EMPTY_COMPLETION_MESSAGE.into(),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }

                // No tool calls — weak model has a final answer.
                answered = true;
                if do_synthesis {
                    // Surface weak model's answer as a thinking event so the
                    // user can see what was researched before synthesis.
                    weak_model_answer = content.clone();
                    let _ = tx.send(ChatEvent::Thinking { content }).await;
                } else {
                    let _ = tx.send(ChatEvent::Message { content }).await;
                }
                break;
            }

            // A tool call is what the budget counts; empty-completion retries
            // above deliberately do not.
            tool_rounds += 1;

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

                let (success, result_text) =
                    execute_guarded(&handler, &mut write_guard, &tool_name, &tool_args).await;

                let preview: String =
                    prepare_tool_result(&tool_name, &result_text, MAX_TOOL_RESULT_CHARS);
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: tool_name.clone(),
                        success,
                        preview: preview.clone(),
                    })
                    .await;

                if do_synthesis && success {
                    gathered.push(format!("### {tool_name}\n{preview}"));
                }

                // Append the tool result as a tool message (truncated to avoid context overflow).
                messages.push(json!({
                    "role": "tool",
                    "content": preview,
                }));
            }
        }

        // Research mode: synthesize gathered findings with a stronger model.
        if do_synthesis {
            self.run_synthesis(&request, &gathered, &weak_model_answer, tx.clone())
                .await;
        } else if !answered {
            // See the same guard in run_ollama_cloud_loop: the tool calls above
            // ran, so silence here would hide work that may have happened.
            warn!(
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
        system_prompt: String,
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
        let mut messages: Vec<Value> = vec![json!({ "role": "system", "content": system_prompt })];
        for h in &request.history {
            messages.push(json!({ "role": h.role, "content": h.content }));
        }
        messages.push(json!({ "role": "user", "content": request.message }));

        let client = reqwest::Client::new();
        let mut empty_completions = 0usize;

        // Rounds that actually spent a tool call, which is what the budget is
        // for. Empty-completion retries deliberately do not count against it:
        // a provider failure is not the model using up its allowance.
        let mut tool_rounds = 0usize;
        let mut answered = false;
        let mut final_round = false;

        // Worker/voice split: when synthesis is configured, this loop is the
        // *worker*. Its own prose is surfaced as thinking rather than as the
        // answer, and a second model composes the reply from the tool results.
        let do_synthesis = request.synthesis_provider.is_some();
        let mut weak_model_answer = String::new();
        let mut gathered: Vec<String> = Vec::new();

        // The upper bound only guarantees termination — the loop exits on an
        // answer, on a hard failure, or after the wrap-up round below.
        // One guard per turn: grounding state and duplicate fingerprints are
        // per-turn, and the ladder may run this loop once per rung.
        let mut write_guard = TurnWriteGuard::default();

        for _ in 0..=(MAX_TOOL_ITERATIONS + MAX_EMPTY_COMPLETION_RETRIES + 1) {
            // One extra pass beyond the tool budget: the wrap-up round. See
            // FINAL_ROUND_NUDGE — it is offered no tools, so a looping model
            // has to answer from what it has instead of searching again.
            if !final_round && tool_rounds >= MAX_TOOL_ITERATIONS {
                final_round = true;
                warn!(
                    model = %model,
                    rounds = tool_rounds,
                    "Tool budget exhausted without an answer — asking for a final answer with no tools"
                );
                messages.push(json!({ "role": "user", "content": FINAL_ROUND_NUDGE }));
            }

            let mut body = json!({
                "model": model,
                "messages": messages,
                "stream": true,
                // Ask OpenAI-compat servers to include a usage block in the
                // final stream chunk — without it token accounting is NULL.
                "stream_options": { "include_usage": true },
                "temperature": config.temperature,
            });
            if !final_round {
                body["tools"] = Value::Array(oai_tools.clone());
            }

            let mut req = client
                .post(&url)
                .header("content-type", "application/json")
                .timeout(config.timeout)
                .json(&body);
            if let Some(ref key) = config.api_key {
                req = req.header("Authorization", format!("Bearer {}", key));
            }

            let call_start = std::time::Instant::now();
            let response = match req.send().await {
                Ok(r) => r,
                Err(e) => {
                    self.record_llm_call(
                        &model,
                        false,
                        call_start.elapsed().as_millis() as i64,
                        None,
                        None,
                    );
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
                // 429s are the quota-pressure signal — mark them in the ledger.
                if let Some(ref tc) = self.telemetry {
                    let error_kind = if status.as_u16() == 429 {
                        Some("rate_limited")
                    } else {
                        None
                    };
                    let _ = tc.record_model_usage(
                        &model,
                        Some("chat"),
                        false,
                        Some(call_start.elapsed().as_millis() as i64),
                        None,
                        None,
                        error_kind,
                    );
                }
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
            // Usage block (OpenAI-compat servers send it in the final chunk).
            let mut usage_tokens: (Option<i64>, Option<i64>) = (None, None);
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

                    // Capture usage when the server includes it (final chunk).
                    if chunk_json.get("usage").is_some() {
                        usage_tokens = (
                            chunk_json["usage"]["prompt_tokens"].as_i64(),
                            chunk_json["usage"]["completion_tokens"].as_i64(),
                        );
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

                    // Do NOT break on finish_reason: the usage chunk arrives
                    // AFTER it (choices is empty there), followed by [DONE].
                    // Breaking early was silently discarding token counts.
                    let finish = chunk_json["choices"][0]["finish_reason"]
                        .as_str()
                        .unwrap_or("");
                    if (finish == "stop" || finish == "tool_calls") && usage_tokens.0.is_some() {
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

            self.record_llm_call(
                &model,
                true,
                call_start.elapsed().as_millis() as i64,
                usage_tokens.0,
                usage_tokens.1,
            );

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
                // Nothing at all came back: no text and no tool call, despite the
                // usage block counting generated tokens. That is a failed call, not
                // a finished turn — re-send the identical request rather than
                // exiting silently. See MAX_EMPTY_COMPLETION_RETRIES.
                if full_content.is_empty() {
                    if empty_completions < MAX_EMPTY_COMPLETION_RETRIES {
                        empty_completions += 1;
                        warn!(
                            model = %model,
                            completion_tokens = ?usage_tokens.1,
                            attempt = empty_completions,
                            "OllamaCloud returned an empty completion — retrying"
                        );
                        continue;
                    }
                    warn!(
                        model = %model,
                        attempts = empty_completions + 1,
                        "OllamaCloud returned an empty completion on every attempt — giving up"
                    );
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: EMPTY_COMPLETION_MESSAGE.into(),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }

                // No tool calls — final answer.
                answered = true;
                if do_synthesis {
                    // Surface the worker's answer as thinking so the user can
                    // see what was gathered before the voice model speaks.
                    weak_model_answer = full_content.clone();
                    let _ = tx
                        .send(ChatEvent::Thinking {
                            content: full_content,
                        })
                        .await;
                } else {
                    let _ = tx
                        .send(ChatEvent::Message {
                            content: full_content,
                        })
                        .await;
                }
                break;
            }

            // A tool call is what the budget counts. The wrap-up round offers no
            // tools, so reaching here on it means the model smuggled one through
            // the text channel (`parse_xml_tool_calls`); let it run, but do not
            // let it buy another round.
            tool_rounds += 1;

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

                let (success, result_text) =
                    execute_guarded(&handler, &mut write_guard, tool_name, tool_args).await;

                let preview: String =
                    prepare_tool_result(tool_name, &result_text, MAX_TOOL_RESULT_CHARS);
                let _ = tx
                    .send(ChatEvent::ToolResult {
                        tool: tool_name.clone(),
                        success,
                        preview: preview.clone(),
                    })
                    .await;

                if do_synthesis && success {
                    gathered.push(format!("### {tool_name}\n{preview}"));
                }

                // OpenAI requires tool results as role="tool" with tool_call_id.
                // Use CLOUD_TOOL_RESULT_CHARS (smaller cap) because tool schemas are resent
                // every round; combined context grows quickly and causes 500s.
                let cloud_preview: String =
                    prepare_tool_result(tool_name, &result_text, CLOUD_TOOL_RESULT_CHARS);
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

        // Synthesis runs whether or not the worker produced closing prose: the
        // tool results are what the voice model composes from, and a worker that
        // looped until its budget ran out is exactly the case where its own
        // summary is least worth relaying. This also means a turn that would
        // otherwise report NO_ANSWER_AFTER_TOOLS_MESSAGE still gets an answer.
        if do_synthesis {
            self.run_synthesis(&request, &gathered, &weak_model_answer, tx.clone())
                .await;
        } else if !answered {
            // Falling out of the loop having answered nothing is a real outcome
            // and needs a real explanation: the tool calls above did run, so the
            // user has to be told work may have happened even though nothing was
            // written back.
            warn!(
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

    // ========================================================================
    // Synthesis step — called after the tool-use loop in research mode
    // ========================================================================

    /// Compose the turn's reply from what the worker gathered.
    ///
    /// `gathered` is passed explicitly rather than scraped back out of the
    /// message history because `run_ollama_cloud_loop` trims that history to
    /// `MAX_HISTORY_MESSAGES` to keep the context from overflowing. Reading
    /// tool results from it would silently lose the earliest ones on exactly
    /// the long research turns synthesis exists to serve.
    async fn run_synthesis(
        &self,
        request: &ChatRequest,
        gathered: &[String],
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

        let (provider, default_model, api_key, base_url) = match provider_str.as_str() {
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
                    None,
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
                    None,
                )
            }
            // Ollama and Ollama Cloud are the $0 rungs, and the reason this
            // arm exists: the split is only useful if the *voice* model can be
            // one we already run. `config_for_catalog_entry` is the same
            // primitive capability routing and the fallback ladder use to turn
            // a catalog name into a callable config — endpoint and credentials
            // come from the environment, never from a checked-in file.
            "ollama" | "ollama-cloud" | "ollamacloud" => {
                let model = request
                    .synthesis_model
                    .clone()
                    .or_else(|| std::env::var("CHAT_SYNTHESIS_MODEL").ok())
                    .filter(|m| !m.is_empty())
                    .unwrap_or_else(|| "gemma4:31b-cloud".to_string());
                let cfg = crate::services::model_router::config_for_catalog_entry(
                    &LlmConfig::default(),
                    &provider_str,
                    &model,
                );
                (
                    cfg.provider,
                    model,
                    cfg.api_key.clone(),
                    cfg.base_url.clone(),
                )
            }
            other => {
                let _ = tx
                    .send(ChatEvent::Error {
                        message: format!(
                            "Unknown synthesis provider: {other}. \
                             Use 'ollama', 'ollama-cloud', 'gemini', or 'anthropic'."
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
            base_url,
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

        let tool_results: Vec<&str> = gathered
            .iter()
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .collect();

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
        let call_start = std::time::Instant::now();
        let chat_result = llm.chat(&messages).await;
        self.record_llm_call(
            &model,
            chat_result.is_ok(),
            call_start.elapsed().as_millis() as i64,
            chat_result
                .as_ref()
                .ok()
                .and_then(|r| r.tokens_in)
                .map(i64::from),
            chat_result
                .as_ref()
                .ok()
                .and_then(|r| r.tokens_out)
                .map(i64::from),
        );
        match chat_result {
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
        system_prompt: String,
        tx: mpsc::Sender<ChatEvent>,
    ) {
        let model = config.model.clone();
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
            system_prompt, tools_str
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

        let mut empty_completions = 0usize;

        // One guard per turn: grounding state and duplicate fingerprints are
        // per-turn, and the ladder may run this loop once per rung.
        let mut write_guard = TurnWriteGuard::default();

        for _iteration in 0..MAX_TOOL_ITERATIONS {
            let call_start = std::time::Instant::now();
            let chat_result = llm.chat(&messages).await;
            self.record_llm_call(
                &model,
                chat_result.is_ok(),
                call_start.elapsed().as_millis() as i64,
                chat_result
                    .as_ref()
                    .ok()
                    .and_then(|r| r.tokens_in)
                    .map(i64::from),
                chat_result
                    .as_ref()
                    .ok()
                    .and_then(|r| r.tokens_out)
                    .map(i64::from),
            );
            let response = match chat_result {
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

                let (success, result_text) =
                    execute_guarded(&handler, &mut write_guard, &tool_name, &tool_args).await;

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
                // Nothing at all came back: no text and no tool call. Same failure
                // as the two streaming loops — retry rather than exit silently.
                // See MAX_EMPTY_COMPLETION_RETRIES.
                if text.is_empty() {
                    if empty_completions < MAX_EMPTY_COMPLETION_RETRIES {
                        empty_completions += 1;
                        warn!(
                            model = %model,
                            attempt = empty_completions,
                            "LLM returned an empty completion — retrying"
                        );
                        continue;
                    }
                    warn!(
                        model = %model,
                        attempts = empty_completions + 1,
                        "LLM returned an empty completion on every attempt — giving up"
                    );
                    let _ = tx
                        .send(ChatEvent::Error {
                            message: EMPTY_COMPLETION_MESSAGE.into(),
                        })
                        .await;
                    let _ = tx.send(ChatEvent::Done).await;
                    return;
                }

                // No tool call — this is the final response.
                debug!(text = %text, "Chat: final text response");
                let _ = tx.send(ChatEvent::Message { content: text }).await;
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

// ============================================================================
// Tests
// ============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base_prompt_is_used_when_no_profile_applies() {
        let p = build_system_prompt(None, &[]);
        assert!(p.contains("You are agent-brain"));
        assert!(!p.contains("LIVE SELF-STATE"));
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
        )
        .await;

        let mut seen = Vec::new();
        while let Ok(e) = rx.try_recv() {
            seen.push(e);
        }
        let final_text = result_rx.try_recv().unwrap_or_default();
        (seen, final_text)
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
mod empty_completion_tests {
    use super::*;

    /// Byte-for-byte the shape ollama.com streamed for the 2026-08-25 dropped
    /// turn: an empty `content` delta, `finish_reason: "stop"`, and a usage
    /// block counting 249 generated tokens that never appeared anywhere.
    pub(super) const EMPTY_COMPLETION_SSE: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",\"content\":\"\"},",
        "\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":12100,\"completion_tokens\":249}}\n\n",
        "data: [DONE]\n\n",
    );

    pub(super) const ANSWERED_COMPLETION_SSE: &str = concat!(
        "data: {\"choices\":[{\"index\":0,\"delta\":{\"role\":\"assistant\",",
        "\"content\":\"Substrates and power.\"},\"finish_reason\":null}]}\n\n",
        "data: {\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}]}\n\n",
        "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":4}}\n\n",
        "data: [DONE]\n\n",
    );

    pub(super) fn sse(body: &str) -> wiremock::ResponseTemplate {
        wiremock::ResponseTemplate::new(200).set_body_raw(body.as_bytes(), "text/event-stream")
    }

    /// A `ChatService` with empty registries — these tests exercise the stream,
    /// not tool dispatch.
    pub(super) fn chat_service(fallback_ladder: Vec<LlmConfig>) -> Arc<ChatService> {
        ChatService::with_context_builder(
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(ToolRegistry::new())),
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(None)),
            None,
            fallback_ladder,
        )
    }

    pub(super) fn cloud_config(server: &wiremock::MockServer) -> LlmConfig {
        LlmConfig {
            provider: LlmProviderType::OllamaCloud,
            base_url: Some(server.uri()),
            timeout: Duration::from_secs(30),
            ..Default::default()
        }
    }

    pub(super) fn test_request() -> ChatRequest {
        ChatRequest {
            message: "map the secondary supply chains".into(),
            history: vec![],
            session_id: None,
            tools: None,
            context_profile: None,
            synthesis_provider: None,
            synthesis_model: None,
        }
    }

    /// Run `run_ollama_cloud_loop` against a mock server and collect what the
    /// client saw. Tools and handler are empty — the retry decision depends on
    /// the stream alone.
    async fn drive_cloud_loop(server: &wiremock::MockServer) -> Vec<ChatEvent> {
        let svc = chat_service(vec![]);

        let (tx, mut rx) = mpsc::channel::<ChatEvent>(64);
        svc.run_ollama_cloud_loop(
            cloud_config(server),
            vec![],
            None,
            test_request(),
            "sys".into(),
            tx,
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
    use super::empty_completion_tests::{
        ANSWERED_COMPLETION_SSE, EMPTY_COMPLETION_SSE, chat_service, cloud_config, sse,
        test_request,
    };
    use super::*;
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

    /// A model that streamed something owns the turn even when it fails after.
    /// Handing it onward would re-execute its tool calls and stream a second
    /// answer underneath the first, so the failure is reported in place.
    #[tokio::test]
    async fn a_turn_that_streamed_before_failing_keeps_the_turn_and_its_error() {
        let server = MockServer::start().await;
        // Round 1 emits a tool call, which streams ToolCall/ToolResult events.
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
        // Round 2 dies.
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(ResponseTemplate::new(503).set_body_string("upstream down"))
            .mount(&server)
            .await;

        let (seen, outcome) = attempt(&server).await;

        assert!(
            matches!(outcome, AttemptOutcome::Delivered),
            "a turn that streamed must not be handed to another model: {outcome:?}"
        );
        assert!(
            seen.iter().any(|e| matches!(e, ChatEvent::ToolCall { .. })),
            "the tool call must have reached the client: {seen:?}"
        );
        // Held back during the attempt, but not swallowed — it is this model's
        // turn, so its explanation is the one the user needs.
        assert!(
            seen.iter()
                .any(|e| matches!(e, ChatEvent::Error { message } if message.contains("503"))),
            "the failure must still be reported: {seen:?}"
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
}

// ============================================================================
// The whole turn
// ============================================================================

#[cfg(test)]
mod turn_tests {
    use super::empty_completion_tests::{
        ANSWERED_COMPLETION_SSE, EMPTY_COMPLETION_SSE, cloud_config, sse, test_request,
    };
    use super::*;
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

#[cfg(test)]
mod tool_budget_tests {
    use super::empty_completion_tests::{cloud_config, sse, test_request};
    use super::*;
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
        let svc = super::empty_completion_tests::chat_service(vec![]);
        let (tx, mut rx) = mpsc::channel::<ChatEvent>(256);
        svc.run_ollama_cloud_loop(
            cloud_config(server),
            vec![],
            None,
            test_request(),
            "sys".into(),
            tx,
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

#[cfg(test)]
mod write_guard_tests {
    use super::*;

    fn note(note_type: Option<&str>, content: &str) -> Value {
        match note_type {
            Some(t) => json!({ "content": content, "note_type": t }),
            None => json!({ "content": content }),
        }
    }

    // --- grounding ---------------------------------------------------------

    #[test]
    fn ungrounded_semantic_note_is_downgraded() {
        let mut g = TurnWriteGuard::default();
        let args = note(Some("semantic"), "Metastatic is a Rust mesh library.");
        match g.screen("store_note", &args) {
            Screened::Rewrite { args, notice } => {
                assert_eq!(args["note_type"], "unsourced_synthesis");
                // Content must survive untouched — the type is the only repair.
                assert_eq!(args["content"], "Metastatic is a Rust mesh library.");
                assert!(notice.contains("unsourced_synthesis"));
            }
            other => panic!("expected downgrade, got {other:?}"),
        }
    }

    #[test]
    fn missing_note_type_is_downgraded_because_it_defaults_to_semantic() {
        let mut g = TurnWriteGuard::default();
        assert!(matches!(
            g.screen("store_note", &note(None, "x")),
            Screened::Rewrite { .. }
        ));
    }

    #[test]
    fn retrieval_earlier_in_the_turn_grounds_a_semantic_note() {
        let mut g = TurnWriteGuard::default();
        g.observe("search_web", true);
        assert_eq!(
            g.screen("store_note", &note(Some("semantic"), "x")),
            Screened::Pass
        );
    }

    #[test]
    fn a_failed_retrieval_does_not_ground_anything() {
        // The 2026-08-18 SearXNG outage returned well-formed empty results for
        // three days. A retrieval that did not retrieve must not license a
        // note claiming to be established knowledge.
        let mut g = TurnWriteGuard::default();
        g.observe("search_web", false);
        assert!(matches!(
            g.screen("store_note", &note(Some("semantic"), "x")),
            Screened::Rewrite { .. }
        ));
    }

    #[test]
    fn non_semantic_types_are_left_alone() {
        // Every other type either carries its own provenance or is a record of
        // the turn itself. Notably `episodic`: run() writes one per chat turn.
        for t in [
            "episodic",
            "reflection",
            "source_record",
            "claim",
            "inference",
            "outcome",
            "unsourced_synthesis",
        ] {
            let mut g = TurnWriteGuard::default();
            assert_eq!(
                g.screen("store_note", &note(Some(t), "x")),
                Screened::Pass,
                "type {t} should not be downgraded"
            );
        }
    }

    #[test]
    fn only_store_note_is_grounded() {
        let mut g = TurnWriteGuard::default();
        assert_eq!(
            g.screen("create_task", &json!({ "goal": "do a thing" })),
            Screened::Pass
        );
    }

    // --- duplicate suppression --------------------------------------------

    #[test]
    fn an_identical_write_runs_once() {
        let mut g = TurnWriteGuard::default();
        g.observe("search_web", true);
        let args = note(Some("semantic"), "same body");
        assert_eq!(g.screen("store_note", &args), Screened::Pass);
        assert!(matches!(
            g.screen("store_note", &args),
            Screened::Suppress { .. }
        ));
    }

    #[test]
    fn differing_content_is_not_a_duplicate() {
        let mut g = TurnWriteGuard::default();
        g.observe("search_notes", true);
        assert_eq!(g.screen("store_note", &note(None, "a")), Screened::Pass);
        assert_eq!(g.screen("store_note", &note(None, "b")), Screened::Pass);
    }

    #[test]
    fn reads_are_never_deduped() {
        // Searching the same phrase twice is wasteful, not incorrect, and
        // suppressing it would hand the model a fabricated tool result.
        let mut g = TurnWriteGuard::default();
        let args = json!({ "query": "reticulum" });
        assert_eq!(g.screen("search_web", &args), Screened::Pass);
        assert_eq!(g.screen("search_web", &args), Screened::Pass);
    }

    #[test]
    fn dedup_runs_before_downgrade_so_one_note_cannot_land_under_two_types() {
        // Ordering guard: if the downgrade ran first, the second call would
        // fingerprint the *rewritten* args, miss the original, and store the
        // same content twice — once semantic, once unsourced_synthesis.
        let mut g = TurnWriteGuard::default();
        let args = note(Some("semantic"), "body");
        assert!(matches!(
            g.screen("store_note", &args),
            Screened::Rewrite { .. }
        ));
        assert!(matches!(
            g.screen("store_note", &args),
            Screened::Suppress { .. }
        ));
    }

    #[test]
    fn the_regression_case_writes_three_notes_not_fifteen() {
        // 2026-08-31: one turn wrote 7 metastatic + 4 reticulum + 4 tailscale
        // notes, all as `semantic`, with zero searches in the whole turn.
        let mut g = TurnWriteGuard::default();
        let topics = [("metastatic", 7), ("reticulum", 4), ("tailscale", 4)];
        let mut executed = 0;
        let mut downgraded = 0;
        for (topic, repeats) in topics {
            for _ in 0..repeats {
                match g.screen("store_note", &note(Some("semantic"), topic)) {
                    Screened::Rewrite { .. } => {
                        executed += 1;
                        downgraded += 1;
                    }
                    Screened::Pass => executed += 1,
                    Screened::Suppress { .. } => {}
                }
            }
        }
        assert_eq!(executed, 3, "one write per distinct note");
        assert_eq!(downgraded, 3, "all three ungrounded");
    }
}
