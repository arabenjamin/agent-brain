//! Knowledge Skill - Provides tools for managing notes and memories.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::{info, warn};

use agent_brain_knowledge::KnowledgeStore;
use agent_brain_llm::LlmProvider;
use agent_brain_models::ProvenanceFlag;
use agent_brain_protocol::Skill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};

/// Knowledge Skill implementation.
pub struct KnowledgeSkill {
    svc: Arc<dyn KnowledgeStore>,
    llm: Arc<dyn LlmProvider>,
}

impl KnowledgeSkill {
    /// Create a new knowledge skill.
    pub fn new(svc: Arc<dyn KnowledgeStore>, llm: Arc<dyn LlmProvider>) -> Self {
        Self { svc, llm }
    }

    // ========================================================================
    // Tool Definitions
    // ========================================================================

    fn store_note_def() -> ToolDefinition {
        ToolDefinition {
            name: "store_note".to_string(),
            description: "Store a text note or memory in the knowledge graph. \
                         The note will be embedded for semantic search, automatically \
                         linked to similar notes, and entities extracted when LLM is available. \
                         Long notes (>1500 chars) are semantically chunked into sub-notes."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "Content of the note"
                    },
                    "note_type": {
                        "type": "string",
                        "enum": ["semantic", "episodic", "reflection", "consolidated", "outcome", "inference", "claim", "source_record", "news", "unsourced_synthesis"],
                        "description": "Type of note (default: semantic). Use `source_record` for what an external source asserted (web/video), `claim` for a single checkable proposition, `unsourced_synthesis` for your own uncited reasoning. Do not invent a type — an unknown value is stored but never labelled or recalled correctly."
                    },
                    "source_context": {
                        "type": "string",
                        "description": "Optional source context (e.g. session ID, document URL)"
                    },
                    "event_at": {
                        "type": "string",
                        "description": "Optional ISO-8601 timestamp of the event this note describes"
                    },
                    "provenance": {
                        "type": "string",
                        "enum": ["user_input", "synthesis_inference", "core_training"],
                        "description": "Source authority flag: who/what produced this note. Default: user_input for an outside MCP client, synthesis_inference for the brain's own jobs, chats and agents (which cannot claim user_input)."
                    }
                },
                "required": ["content"]
            }),
        }
    }

    fn search_notes_def() -> ToolDefinition {
        ToolDefinition {
            name: "search_notes".to_string(),
            description: "Search OR enumerate stored notes. TWO MODES — pick deliberately. \
                         (1) SEARCH: provide `query` for hybrid BM25+semantic ranking with optional \
                         graph expansion, or `entity_name` to find notes mentioning a named entity. \
                         This returns a RANKED, TRUNCATED SAMPLE — it cannot count, order, or \
                         return everything. (2) ENUMERATE: OMIT `query` and pass `source_context` \
                         and/or `note_type` to get ALL notes carrying that label, oldest first, with \
                         a `complete` flag telling you whether you were shown the whole set. If you \
                         know the LABEL a set of notes was filed under (a résumé under \
                         source_context 'resume', a report under 'daily_brief'), ENUMERATE — a \
                         `query` will rank a fraction of it and you will wrongly read the gap as \
                         absence. At least one of `query`, `entity_name`, `note_type`, or \
                         `source_context` must be supplied."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "Text search query (BM25 + semantic)"
                    },
                    "entity_name": {
                        "type": "string",
                        "description": "Find notes that mention this named entity (case-insensitive)"
                    },
                    "entity_type": {
                        "type": "string",
                        "description": "Optional entity type filter when using entity_name (e.g. technology, organisation)"
                    },
                    "note_type": {
                        "type": "string",
                        "description": "Filter by note type: semantic, episodic, reflection, consolidated, news, news_raw, outcome. Without a query, ENUMERATES all notes of this type by date."
                    },
                    "source_context": {
                        "type": "string",
                        "description": "The label a set of notes was filed under (e.g. 'resume', 'video_learning', 'daily_brief'). WITHOUT a `query` this ENUMERATES every note carrying that source_context (chunk fragments excluded) and returns a `complete` flag — this is how you retrieve a whole multi-note document. With a `query` it only narrows the ranked search. An enumeration that matches NOTHING returns `available_note_types` / `available_source_contexts` — the labels that actually exist. Retry with one of those; never report absence off a zero here, and never guess a third label without reading that list."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of results (default: 5). Raise it when enumerating — a multi-note document needs a limit above its note count for `complete` to come back true."
                    },
                    "graph_hops": {
                        "type": "integer",
                        "description": "RELATES_TO hops to expand text-search results (default: 2, 0 to disable)"
                    },
                    "entity_expansion": {
                        "type": "boolean",
                        "description": "Also surface notes that share named entities with text-search results (default: false)"
                    }
                }
            }),
        }
    }

    // export_graph_visualization is served by GET /api/graph (REST API)

    fn recall_recent_def() -> ToolDefinition {
        ToolDefinition {
            name: "recall_recent".to_string(),
            description: "Time-window recall: what was created recently, counted by note_type and \
                 source_context, with a most-recent sample. This is the RIGHT tool for \
                 'what did we learn today', 'what happened yesterday', 'what have we been \
                 working on' — any whole-day or whole-period question. Do NOT semantic-search \
                 for a date string (e.g. search_notes '2026-09-07'); a date does not embed and \
                 will miss the day's notes. Counts exclude chunk fragments and are computed in \
                 the brain's LOCAL day; the sample is the newest rows, not the whole window.\n\n\
                 SHOWING vs REASONING — if the user wants to SEE the notes (\"show me all X\", \
                 \"list everything we learned\", \"I want to see...\"), pass `full: true`: every \
                 matching note goes straight to their screen and you get a receipt. That is a \
                 delivery, not a retrieval — do not page it, do not re-query it, and never \
                 call it incomplete. Use the paged form below only when YOU need to reason \
                 over the notes.\n\n\
                 TWO-STEP DRILL-DOWN — this is how you answer 'summarize the videos we watched \
                 today' or any 'what did <source> produce' question: (1) call with no filter to \
                 see the buckets, then (2) call AGAIN passing `source_context` (or `note_type`) \
                 copied verbatim from a row of `by_source_context` / `by_note_type` to get those \
                 notes themselves, with longer snippets. NEVER switch to search_notes to expand a \
                 bucket: it ranks by topic similarity, so searching the label 'video_learning' \
                 returns whatever note is topically nearest — typically an unrelated note from \
                 another month — and reads as 'nothing today'. If step 1 reports a non-zero count \
                 for a bucket, those notes EXIST; a later empty search is the search failing, not \
                 absence. Check `complete` to know whether you were shown every matching note; if it is \
                 false, call again with the SAME filter and `offset` set to the returned \
                 `next_offset`, and keep paging until it is true. Do NOT raise \
                 `sample_limit` to fix an incomplete read — a bigger page does not fit \
                 through the tool-result channel any better, it just moves where the \
                 clipping falls. And if the text you received ends in a TRUNCATED marker, \
                 you did not see all of what was sent: page from where your copy stopped \
                 rather than telling the user the list is unavailable. Filter ONE side at a time — pairing a \
                 source_context with a GUESSED note_type is the usual way to match nothing \
                 (source_context 'video_learning' is stored as note_type 'source_record' and \
                 'claim', never 'episodic'). If a filtered call returns 0 it tells you which \
                 values DO exist in the window, in `available_note_types` / \
                 `available_source_contexts` — retry with one of those. A 0 from a filter means \
                 the FILTER is wrong, not that the day was empty: never report absence from it."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "window": {
                        "type": "string",
                        "description": "today (default), yesterday, or 'Nd' for the last N days (e.g. 7d, 30d)."
                    },
                    "sample_limit": {
                        "type": "integer",
                        "description": "Max sample rows per page (default 10; capped at 100). Raising this does NOT get you more notes if the result is being clipped in transit — page with `offset` instead."
                    },
                    "full": {
                        "type": "boolean",
                        "description": "Set true when the USER asked to SEE the notes themselves — \"show me all X\", \"list everything\", \"I want to see what we learned\". Delivers the COMPLETE matching set straight to the user's screen with whole note content, and gives you only a receipt plus 3 rows. The rows are NOT in your context and do not need to be: the user is already looking at them. Never page or re-query after a full delivery, and never tell the user the list is partial."
                    },
                    "offset": {
                        "type": "integer",
                        "description": "Rows to skip. This is how you enumerate a set bigger than one tool result: call again with the SAME filter and the `next_offset` the previous result gave you, until `complete` is true. Cite `matched_notes`, never the number of rows you happen to hold."
                    },
                    "source_context": {
                        "type": "string",
                        "description": "Drill down: return only notes with this source_context, copied verbatim from a by_source_context row (e.g. 'video_learning', 'fill_knowledge_gap', or the literal '(none)'). Counts and sample are both narrowed."
                    },
                    "note_type": {
                        "type": "string",
                        "description": "Drill down: return only notes of this note_type (e.g. 'source_record', 'claim', 'reflection'). Combines with source_context."
                    }
                }
            }),
        }
    }

    fn prune_old_notes_def() -> ToolDefinition {
        ToolDefinition {
            name: "prune_old_notes".to_string(),
            description:
                "Delete stale notes. Use score_threshold/lambda for adaptive decay scoring \
                         (recommended), or days_stale/min_accesses for simple time-based pruning. \
                         Protected types (consolidated, reflection) are never deleted. \
                         min_retain (default 50) and max_pct (default 0.30) prevent over-pruning."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "days_stale": {
                        "type": "integer",
                        "description": "Legacy: delete notes not accessed in this many days (default: 30)"
                    },
                    "min_accesses": {
                        "type": "integer",
                        "description": "Legacy: delete notes accessed fewer than this many times (default: 2)"
                    },
                    "score_threshold": {
                        "type": "number",
                        "description": "Adaptive decay: delete notes with decay score below this value (default: 0.1)"
                    },
                    "lambda": {
                        "type": "number",
                        "description": "Adaptive decay: exponential decay rate (default: 0.1)"
                    },
                    "dry_run": {
                        "type": "boolean",
                        "description": "If true, return count of stale notes without deleting (default: false)"
                    },
                    "min_retain": {
                        "type": "integer",
                        "description": "Never delete below this total note count (default: 50)"
                    },
                    "max_pct": {
                        "type": "number",
                        "description": "Cap deletions to this fraction of total notes per run (default: 0.30)"
                    }
                }
            }),
        }
    }

    fn consolidate_memories_def() -> ToolDefinition {
        ToolDefinition {
            name: "consolidate_memories".to_string(),
            description: "Use the LLM to synthesize multiple notes on a topic into a single \
                         consolidated summary note. Creates SUMMARIZED_BY edges from source \
                         notes to the consolidated note. Requires LLM."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "description": "Topic to consolidate memories about"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of source notes to include (default: 10)"
                    }
                },
                "required": ["topic"]
            }),
        }
    }

    fn reason_def() -> ToolDefinition {
        ToolDefinition {
            name: "reason".to_string(),
            description: "Multi-mode LLM reasoning tool. \
                         action=\"infer\" (default): derive new inferences from stored knowledge; \
                         action=\"explain\": narrate why a decision occurred, citing sources; \
                         action=\"clarify\": analyse a request for ambiguity and generate questions; \
                         action=\"audit\": check a proposed action against stored values and principles; \
                         action=\"structured\": full structured output with sources, caveats, follow-up questions, \
                         gaps, inferences, and optional adversarial critic pass."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "description": "Mode: infer (default), explain, clarify, audit, or structured"
                    },
                    "question": {
                        "type": "string",
                        "description": "The question/decision/request/action to reason about (required)"
                    },
                    "context": {
                        "type": "string",
                        "description": "Optional raw context text. When provided in infer mode, bypasses RAG and reasons directly over this text — use for live data like search results passed via {{_prev}}. Also used by clarify and audit."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max knowledge notes to retrieve (default: 8 for infer/explain)"
                    },
                    "store_inference": {
                        "type": "boolean",
                        "description": "Persist the result as a Note (default: true, infer mode only)"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Include task-linked reflection notes (explain mode)"
                    },
                    "available_tools": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Tools available to the agent (clarify mode)"
                    },
                    "run_critic": {
                        "type": "boolean",
                        "description": "Run adversarial critic pass to stress-test the answer and adjust confidence (structured mode only, default: false)"
                    },
                    "create_gap_tasks": {
                        "type": "boolean",
                        "description": "Create Task nodes for each identified knowledge gap (structured mode only, requires store_inference=true, default: false)"
                    }
                },
                "required": ["question"]
            }),
        }
    }

    // ========================================================================
    // Tool Handlers
    // ========================================================================

    async fn handle_store_note(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: StoreNoteInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        info!(content_len = input.content.len(), "Storing note");

        // Who is writing decides the default; see `effective_provenance`.
        let provenance = input
            .provenance
            .as_deref()
            .and_then(|s| s.parse::<ProvenanceFlag>().ok());

        let null_result = is_null_result(&input.content, input.note_type.as_deref());
        let unverified = !null_result && is_unverified(&input.content, input.note_type.as_deref());
        // Recorded whatever type the caller asked for: a note that failed
        // verification is labelled so on retrieval, not just retyped.
        let failed_reason = (!null_result)
            .then(|| unverified_reason(&input.content))
            .flatten();
        let note_type = if null_result {
            info!(
                source_context = ?input.source_context,
                "store_note: research found nothing — storing as episodic, not semantic"
            );
            Some("episodic")
        } else if unverified {
            info!(
                source_context = ?input.source_context,
                "store_note: agent answer failed verification — storing as unsourced_synthesis"
            );
            Some("unsourced_synthesis")
        } else {
            input.note_type.as_deref()
        };

        match self
            .svc
            .store_note(
                &input.content,
                note_type,
                input.source_context.as_deref(),
                input.event_at.as_deref(),
                provenance,
            )
            .await
        {
            Ok((id, links_created)) => {
                if let Some(reason) = &failed_reason
                    && let Err(e) = self.svc.mark_verification_failed(&id, reason).await
                {
                    warn!(error = %e, note_id = %id, "store_note: could not flag failed verification");
                }
                // "answer" echoes the stored content so downstream {{_prev}} steps
                // (e.g. notify_user after store_note) receive the original text, not metadata JSON.
                let mut response = json!({
                    "success": true,
                    "id": id,
                    "links_created": links_created,
                    "message": "Note stored successfully",
                    "answer": input.content
                });
                if unverified {
                    response["note_type"] = json!("unsourced_synthesis");
                    response["message"] = json!(
                        "Note stored as `unsourced_synthesis`: it carries an UNVERIFIED marker \
                         (the agent did not do what its caller required), so it is not knowledge."
                    );
                }
                if null_result {
                    response["note_type"] = json!("episodic");
                    response["message"] = json!(
                        "Note stored as `episodic`: it reports that the evidence did not answer \
                         the question, which is a record of a search, not knowledge."
                    );
                }
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Failed to store note: {}", e)),
        }
    }

    async fn handle_search_notes(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SearchNotesInput = match parse_args(arguments) {
            Ok(input) => input,
            Err(e) => return e,
        };

        // entity_name path — direct named-entity lookup. An empty/whitespace
        // `entity_name` is treated as ABSENT (not searched): a model looping
        // `search_notes` with `entity_name: ""` would otherwise match nothing,
        // get noise back, read it as a successful search, and loop — the exact
        // degenerate behaviour that burned a delegated worker's whole budget
        // (measured 2026-09-07). Falling through yields a clear error instead,
        // which tells the model to supply a real query. Mirrors the empty-query
        // guard below.
        if let Some(entity_name) = input
            .entity_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            info!(entity = %entity_name, "Searching notes by entity");
            return match self
                .svc
                .search_by_entity(entity_name, input.entity_type.as_deref(), input.limit)
                .await
            {
                Ok(notes) => ToolCallResult::success_json(json!({
                    "entity_name": entity_name,
                    "count": notes.len(),
                    "notes": notes
                })),
                Err(e) => ToolCallResult::error(format!("Entity search failed: {}", e)),
            };
        }

        // Normalize note_type: an empty/whitespace value is NOT a filter for
        // `note_type = ""` (which matches nothing and reads back as "no results"
        // — the false negative that made a delegated worker report "no
        // information" on data that existed, 2026-09-07). Treat it as absent,
        // exactly like the empty-query and empty-entity_name guards above.
        let note_type = input
            .note_type
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // Same empty-is-absent normalization for source_context. Until now this
        // arg was parsed and silently dropped — the semantic search ranked purely
        // on content, so `source_context:"resume"` still returned unrelated notes.
        let source_context = input
            .source_context
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());

        // Query-less ENUMERATION path: note_type and/or source_context list the
        // matching notes in full, ordered by date, instead of ranking a sample.
        //
        // `source_context` belongs here for the same reason `recall_recent` and
        // `list_endpoints` exist: when the caller already knows the LABEL, a
        // ranked top-K is the wrong tool. A résumé stored as 11 notes under
        // `source_context:"resume"` (10411 chars) was reachable only through
        // similarity search, which truncates and misses — on 2026-09-11 a
        // delegated `job-applicant` spent all 8 of its rounds on repeat searches
        // and still reported the résumé "incomplete", declining to compile an
        // application from a résumé that was entirely present in the graph.
        if input.query.as_deref().is_none_or(str::is_empty) {
            if note_type.is_some() || source_context.is_some() {
                info!(
                    note_type = ?note_type,
                    source_context = ?source_context,
                    "Enumerating notes by label"
                );
                return match self
                    .svc
                    .list_notes_filtered(input.limit, note_type, source_context)
                    .await
                {
                    Ok(notes) => {
                        // `complete` tells the caller whether they were shown the
                        // whole set or just the first `limit` — the same signal
                        // recall_recent carries, and the thing that stops an agent
                        // concluding "incomplete" from a full-but-capped result.
                        let complete = notes.len() < input.limit;
                        let mut out = json!({
                            "count": notes.len(),
                            "note_type": note_type,
                            "source_context": source_context,
                            "complete": complete,
                            "notes": notes
                        });
                        // A zero here is almost always a LABEL that does not
                        // exist, not an empty graph — and the caller guessed the
                        // label, so it cannot correct itself without being shown
                        // the real ones. This is the `recall_recent` escape
                        // hatch, which this sibling path never got: on
                        // 2026-09-11 a delegated worker probed note_type
                        // "agent", then source_context "agent", then note_type
                        // "news", learned nothing from any miss, and reported it
                        // could not determine the answer.
                        if notes.is_empty() {
                            match self.svc.label_buckets().await {
                                Ok((types, sources, total)) => {
                                    let m = out.as_object_mut().expect("object");
                                    m.insert("graph_total_notes".into(), json!(total));
                                    m.insert("available_note_types".into(), json!(types));
                                    m.insert("available_source_contexts".into(), json!(sources));
                                    m.insert("note".into(), json!(format!(
                                        "NO NOTES CARRY THIS LABEL — and this is NOT evidence the graph is empty: {total} notes exist. The label is the likely fault. `available_note_types` and `available_source_contexts` list the values that DO exist — retry with one of those, or drop one side of the filter. Note the two do not pair freely (e.g. source_context 'video_learning' is stored as note_type 'source_record' and 'claim', never 'episodic'). Do NOT report absence off this result."
                                    )));
                                }
                                Err(e) => {
                                    // Advisory only — never turn a successful
                                    // enumeration into an error.
                                    warn!(error = %e, "label_buckets failed (non-fatal)");
                                }
                            }
                        }
                        ToolCallResult::success_json(out)
                    }
                    Err(e) => ToolCallResult::error(format!("note enumeration failed: {}", e)),
                };
            }
            return ToolCallResult::error(
                "Provide a non-empty `query`, `entity_name`, `note_type`, or \
                 `source_context` — empty values are ignored, not searched."
                    .to_string(),
            );
        }

        let query = input.query.as_deref().unwrap();
        info!(query = %query, note_type = ?note_type, "Searching notes");

        let results = if input.entity_expansion {
            self.svc
                .search_notes_with_entity_expansion(
                    query,
                    input.limit,
                    input.graph_hops,
                    note_type,
                    source_context,
                )
                .await
        } else {
            self.svc
                .search_notes(
                    query,
                    input.limit,
                    input.graph_hops,
                    note_type,
                    source_context,
                )
                .await
        };
        match results {
            Ok(results) => ToolCallResult::success_json(json!({
                "count": results.len(),
                "notes": results
            })),
            Err(e) => ToolCallResult::error(format!("Search failed: {}", e)),
        }
    }

    async fn handle_recall_recent(&self, arguments: Option<Value>) -> ToolCallResult {
        use chrono::{Duration, Utc};
        let input: RecallRecentInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };
        // Defaults are chosen by MEASUREMENT against a sub-agent's tool-result
        // budget (`tool_result_budget`, 8000 chars for a lean allowlist), not by
        // taste. A sample row serialises to ~450 chars unfiltered (200-char
        // snippet + id + timestamp + two labels) and ~850 filtered (600-char
        // snippet), so the old 24/50 produced 10562/14000+ — both clipped. The
        // clip is the whole bug: a payload that says `complete: true` and then
        // loses a third of itself in transit gets relayed to the user as "not
        // the complete list".
        //
        // 12 fits either way with headroom. Fewer rows per call is strictly
        // better than more rows that arrive cut, because `offset` makes the
        // rest reachable and truncation does not — and the caller can always
        // ask for a bigger page deliberately.
        // `full` means the audience is the PERSON, so the model-sized page cap
        // does not apply — a page is the wrong shape for "show me everything".
        // The 5000 bound is a runaway guard, not a display budget.
        let full = input.full.unwrap_or(false);
        let sample_limit = if full {
            input.sample_limit.unwrap_or(5000).clamp(1, 5000)
        } else {
            // 10, not 12: claim / source-record rows now carry their epistemic
            // label (~100 chars each), and 12 rows already landed at ~7.2K of
            // an 8K tool-result budget.
            input.sample_limit.unwrap_or(10).clamp(1, 100)
        };

        // Windows are computed in the brain's LOCAL zone (stored timestamps are
        // UTC, so the bounds are converted to UTC before the query) — "today"
        // must mean the local day, not the UTC day, or part of every evening
        // reads as "yesterday". See services/clock.rs.
        let now_local = agent_brain_graph::clock::now_local();
        let local_midnight = agent_brain_graph::clock::local_midnight_utc;
        let now_utc = now_local.with_timezone(&Utc);
        let today = now_local.date_naive();

        let window = input
            .window
            .as_deref()
            .unwrap_or("today")
            .trim()
            .to_lowercase();
        let (since, until) = match window.as_str() {
            "today" => (local_midnight(today), now_utc),
            "yesterday" => (
                local_midnight(today - Duration::days(1)),
                local_midnight(today),
            ),
            other => {
                // "Nd" or a bare integer → the last N days ending now.
                let days: i64 = other
                    .strip_suffix('d')
                    .unwrap_or(other)
                    .trim()
                    .parse()
                    .unwrap_or(1);
                let days = days.clamp(1, 365);
                (now_utc - Duration::days(days), now_utc)
            }
        };

        match self
            .svc
            .recall_window(agent_brain_knowledge::traits::RecallSpec {
                since_iso: &since.to_rfc3339(),
                until_iso: &until.to_rfc3339(),
                sample_limit,
                source_context: input.source_context.as_deref(),
                note_type: input.note_type.as_deref(),
                offset: input.offset.unwrap_or(0),
                full,
            })
            .await
        {
            Ok(mut v) => {
                if let Value::Object(ref mut m) = v {
                    m.insert("window".into(), json!(window));
                }
                if full {
                    Self::split_full_delivery(v)
                } else {
                    Self::lift_bulk_rows(v)
                }
            }
            Err(e) => ToolCallResult::error(format!("recall_recent failed: {e}")),
        }
    }

    /// Move any rows the service staged for the PERSON out of the model-facing
    /// envelope and into the `bulk` channel.
    ///
    /// A filtered recall always stages the complete set (see the unconditional
    /// delivery note in `KnowledgeService::recall_window`). The model still gets
    /// only its page; this just makes sure the rest reaches the screen instead
    /// of the context — and tells the model plainly that it did, so it stops
    /// reporting a delivered set as one it "could only partly retrieve".
    fn lift_bulk_rows(mut v: Value) -> ToolCallResult {
        let rows: Vec<Value> = match v
            .as_object_mut()
            .and_then(|m| m.remove(agent_brain_knowledge::knowledge::BULK_ROWS_KEY))
        {
            Some(Value::Array(a)) => a,
            _ => return ToolCallResult::success_json(v),
        };
        if rows.is_empty() {
            return ToolCallResult::success_json(v);
        }

        let bulk = Self::render_rows(&rows);
        let page = v.get("returned").and_then(Value::as_i64).unwrap_or(0);
        if let Value::Object(ref mut m) = v {
            m.insert("delivered_to_user".into(), json!(rows.len()));
            let prior = m
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            m.insert(
                "note".into(),
                json!(format!(
                    "ALL {} MATCHING NOTES ARE ALREADY ON THE USER'S SCREEN — delivered in \
                     full alongside this result, outside your context. You hold a page of {} \
                     for reasoning. Do NOT tell the user the list is partial, that you could \
                     only retrieve some, or that showing them all is impractical: they are \
                     looking at all {} right now. Answer questions about them; page only if \
                     YOU need more to reason with.\n{prior}",
                    rows.len(),
                    page,
                    rows.len(),
                )),
            );
        }
        ToolCallResult::success_json(v).with_bulk(bulk)
    }

    /// Render note rows as a readable block — one entry per note, whole content.
    fn render_rows(rows: &[Value]) -> String {
        let mut out = String::new();
        for (i, row) in rows.iter().enumerate() {
            let get = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("");
            out.push_str(&format!(
                "\n[{}/{}] {} · {} · {}\n{}\n{}\n",
                i + 1,
                rows.len(),
                get("note_type"),
                get("source_context"),
                get("created_at"),
                get("id"),
                get("snippet"),
            ));
        }
        out
    }

    /// Split a `full: true` recall into its two audiences.
    ///
    /// The person gets every matching note rendered for reading; the model gets
    /// a receipt — counts, a three-row taste, and an instruction not to re-query
    /// in order to enumerate what the user is already looking at.
    ///
    /// This is the point of the bulk channel: the model's context cost becomes
    /// independent of how many rows came back. Before it, "show me all of what
    /// we learned from video learning" made a 4B local worker page a 12-row
    /// window to shuttle 50 rows to a human — extra LLM calls, a truncation
    /// risk at every hop, and less delivered at the end than one query.
    fn split_full_delivery(mut v: Value) -> ToolCallResult {
        let rows: Vec<Value> = match v.get_mut("sample").map(Value::take) {
            Some(Value::Array(a)) => a,
            _ => Vec::new(),
        };
        let total = v.get("matched_notes").and_then(Value::as_i64).unwrap_or(0);

        let mut bulk = String::new();
        for (i, row) in rows.iter().enumerate() {
            let get = |k: &str| row.get(k).and_then(Value::as_str).unwrap_or("");
            bulk.push_str(&format!(
                "\n[{}/{}] {} · {} · {}\n{}\n{}\n",
                i + 1,
                rows.len(),
                get("note_type"),
                get("source_context"),
                get("created_at"),
                get("id"),
                get("snippet"),
            ));
        }

        if let Value::Object(ref mut m) = v {
            m.insert("delivered_to_user".into(), json!(rows.len()));
            m.insert(
                "sample".into(),
                json!(rows.iter().take(3).cloned().collect::<Vec<_>>()),
            );
            m.insert(
                "note".into(),
                json!(format!(
                    "FULL DELIVERY — all {} of {total} matching notes were sent to the USER \
                     directly and are on their screen now. You were given 3 as a taste; the rest \
                     were deliberately NOT loaded into your context. Do NOT page, re-query or \
                     enumerate them, and do NOT say the list is incomplete or that you could \
                     only retrieve part of it — the user can see all of it. State what was \
                     delivered, then offer to answer questions about it.",
                    rows.len()
                )),
            );
        }
        ToolCallResult::success_json(v).with_bulk(bulk)
    }

    async fn handle_prune_old_notes(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: PruneOldNotesInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(
            days_stale = input.days_stale,
            min_accesses = input.min_accesses,
            dry_run = input.dry_run,
            "Pruning stale notes"
        );

        match self
            .svc
            .prune_old_notes(
                input.days_stale,
                input.min_accesses,
                input.score_threshold,
                input.lambda,
                input.dry_run,
                input.min_retain,
                input.max_pct,
            )
            .await
        {
            Ok(count) => {
                let response = if input.dry_run {
                    json!({
                        "count": count,
                        "dry_run": true,
                        "message": format!("Would delete {} stale note(s) (dry run)", count)
                    })
                } else {
                    json!({
                        "count": count,
                        "message": format!("Deleted {} stale note(s)", count)
                    })
                };
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Failed to prune notes: {}", e)),
        }
    }

    async fn handle_consolidate_memories(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ConsolidateMemoriesInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(topic = %input.topic, limit = input.limit, "Consolidating memories");

        match self
            .svc
            .consolidate_memories(&input.topic, input.limit)
            .await
        {
            Ok((id, source_count, preview)) => {
                let response = json!({
                    "id": id,
                    "source_count": source_count,
                    "preview": preview
                });
                ToolCallResult::success_json(response)
            }
            Err(e) => ToolCallResult::error(format!("Consolidation failed: {}", e)),
        }
    }

    async fn handle_reason(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ReasonInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        match input.action.as_deref().unwrap_or("infer") {
            "explain" => {
                info!(decision = %input.question, "Explaining reasoning");
                match self
                    .svc
                    .explain_reasoning(&input.question, input.task_id.as_deref(), input.limit)
                    .await
                {
                    Ok((explanation, sources)) => ToolCallResult::success_json(json!({
                        "explanation": explanation,
                        "knowledge_sources": sources,
                    })),
                    Err(e) => ToolCallResult::error(format!("Explanation failed: {}", e)),
                }
            }
            "clarify" => {
                info!(request = %input.question, "Analyzing request for clarification");
                let mut prompt = format!(
                    "Analyze the following request for ambiguity. Determine if clarification is needed before acting.\n\nREQUEST: {}\n",
                    input.question
                );
                if let Some(ctx) = &input.context {
                    prompt.push_str(&format!("\nCONTEXT: {}\n", ctx));
                }
                if let Some(tools) = &input.available_tools
                    && !tools.is_empty()
                {
                    prompt.push_str(&format!("\nAVAILABLE TOOLS: {}\n", tools.join(", ")));
                }
                prompt.push_str(
                    r#"
Respond with a JSON object only (no markdown, no explanation):
{
  "needs_clarification": true,
  "ambiguities": ["specific ambiguous aspect 1", "..."],
  "clarifying_questions": ["question to ask 1", "..."],
  "assumptions": ["assumption that would be made if proceeding 1", "..."],
  "recommended_approach": "brief description of how to proceed"
}"#,
                );
                match self
                    .llm
                    .generate_json(&prompt, None, &["needs_clarification"], 2)
                    .await
                {
                    Ok(parsed) => ToolCallResult::success_json(parsed),
                    // The LLM was unavailable, or still couldn't produce valid
                    // JSON after self-correcting retries — surface the error
                    // rather than fabricate a clarification result.
                    Err(e) => {
                        ToolCallResult::error(format!("Clarification analysis failed: {}", e))
                    }
                }
            }
            "audit" => {
                info!(action = %input.question, "Auditing action");
                match self
                    .svc
                    .audit_action(&input.question, input.context.as_deref())
                    .await
                {
                    Ok((aligned, confidence, concerns, suggestions, reasoning)) => {
                        ToolCallResult::success_json(json!({
                            "aligned": aligned,
                            "confidence": confidence,
                            "concerns": concerns,
                            "suggestions": suggestions,
                            "reasoning": reasoning,
                        }))
                    }
                    Err(e) => ToolCallResult::error(format!("Audit failed: {}", e)),
                }
            }
            "structured" => {
                info!(question = %input.question, run_critic = input.run_critic, "Structured reasoning");
                match self
                    .svc
                    .reason_structured(
                        &input.question,
                        input.limit,
                        input.store_inference,
                        input.run_critic,
                    )
                    .await
                {
                    Ok(output) => {
                        let sources_json: Vec<Value> = output
                            .sources
                            .iter()
                            .map(|s| json!({ "note_id": s.note_id, "preview": s.preview }))
                            .collect();

                        let mut response = json!({
                            "answer": output.answer,
                            "sources": sources_json,
                            "confidence": output.confidence,
                            "caveats": output.caveats,
                            "follow_up_questions": output.follow_up_questions,
                            "inferences": output.inferences,
                            "gaps": output.gaps,
                            "critic_counter_arguments": output.critic_counter_arguments,
                        });

                        // Emitted only when true, so a normal result is unchanged.
                        // A consumer reading `confidence`/`gaps`/`caveats` has no
                        // other way to tell a graded answer from raw prose wearing
                        // the same field names.
                        if output.structured_output_failed {
                            response["structured_output_failed"] = json!(true);
                            response["caveats"] = json!([
                                "The model did not return parseable structured output; this \
                                 answer is raw prose. `confidence`, `gaps` and `caveats` were \
                                 NOT reported by the model and must not be read as its own \
                                 assessment."
                            ]);
                        }

                        if let Some(nid) = &output.inference_note_id {
                            response["inference_note_id"] = json!(nid);
                        }

                        if input.create_gap_tasks
                            && let Some(ref note_id) = output.inference_note_id
                            && !output.gaps.is_empty()
                        {
                            match self.svc.create_gap_tasks(&output.gaps, note_id).await {
                                Ok(ids) => {
                                    response["gap_task_ids"] = json!(ids);
                                }
                                Err(e) => {
                                    response["gap_task_error"] = json!(e.to_string());
                                }
                            }
                        }

                        ToolCallResult::success_json(response)
                    }
                    Err(e) => ToolCallResult::error(format!("Structured reasoning failed: {}", e)),
                }
            }
            _ => {
                // "infer" (default)
                info!(question = %input.question, limit = input.limit, "Reasoning");

                // When external context is provided, bypass RAG and reason directly over it.
                // This prevents contamination from stale notes when the caller has live data
                // (e.g. freshly-fetched search results in a job chain).
                if let Some(ctx) = &input.context {
                    let prompt = format!(
                        "You are a reasoning engine. Respond in English only. All JSON field values must be in English.\n\
                         Using the provided context, answer the question \
                         clearly. Distinguish what is directly stated vs inferred.\n\
                         Output ONLY valid JSON. The \"answer\" field may contain markdown formatting \
                         (headers, bullets, bold) when the question requests a structured report:\n\
                         {{\"answer\":\"...\",\"inferences\":[\"...\"],\"confidence\":0.0,\"gaps\":[\"...\"]}}\n\n\
                         QUESTION: {}\n\
                         CONTEXT:\n{}",
                        input.question, ctx
                    );
                    return match self.llm.generate(&prompt, None).await {
                        Ok(text_resp) => {
                            let text = text_resp.trim();
                            let mut parsed: Value =
                                agent_brain_llm::llm::first_json_object(text, &["answer"])
                                    .unwrap_or_else(|| {
                                        json!({
                                            "answer": text,
                                            "inferences": [],
                                            "confidence": 0.5,
                                            "gaps": []
                                        })
                                    });
                            // The context path must honor store_inference like the
                            // RAG path does — scheduler chains rely on the inference
                            // note as their durable output artifact.
                            if input.store_inference {
                                let answer = parsed["answer"].as_str().unwrap_or(text).to_string();
                                let inferences: Vec<String> = parsed["inferences"]
                                    .as_array()
                                    .map(|arr| {
                                        arr.iter()
                                            .filter_map(|v| v.as_str().map(String::from))
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                if let Some(id) = self
                                    .svc
                                    .store_context_inference(&input.question, &answer, &inferences)
                                    .await
                                {
                                    parsed["id"] = json!(id);
                                }
                            }
                            ToolCallResult::success_json(parsed)
                        }
                        Err(e) => ToolCallResult::error(format!("Reasoning failed: {}", e)),
                    };
                }

                match self
                    .svc
                    .reason(&input.question, input.limit, input.store_inference)
                    .await
                {
                    Ok((answer, inferences, confidence, gaps, note_id)) => {
                        let mut response = json!({
                            "answer": answer,
                            "inferences": inferences,
                            "confidence": confidence,
                            "gaps": gaps,
                        });
                        if let Some(nid) = note_id {
                            response["id"] = json!(nid);
                        }
                        ToolCallResult::success_json(response)
                    }
                    Err(e) => ToolCallResult::error(format!("Reasoning failed: {}", e)),
                }
            }
        }
    }

    fn adversarial_plan_review_def() -> ToolDefinition {
        ToolDefinition {
            name: "adversarial_plan_review".to_string(),
            description:
                "Stress-test a proposed plan before execution by generating N adversarial \
                         failure scenarios. For each hypothesis the tool scores how well the plan \
                         handles it (1–5) and suggests a mitigation. Returns an overall robustness \
                         score and adjusted plan notes. Results are stored as a semantic note so \
                         future reviews benefit from accumulated failure-pattern knowledge."
                    .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "goal": {
                        "type": "string",
                        "description": "The goal or objective the plan is trying to achieve"
                    },
                    "proposed_plan": {
                        "type": "string",
                        "description": "Description of the proposed plan steps (free text or step list)"
                    },
                    "n_hypotheses": {
                        "type": "integer",
                        "description": "Number of failure hypotheses to generate (default: 3)"
                    },
                    "task_id": {
                        "type": "string",
                        "description": "Optional task ID to link the adversarial review note via REFLECTS_ON"
                    }
                },
                "required": ["goal", "proposed_plan"]
            }),
        }
    }

    async fn handle_adversarial_plan_review(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: AdversarialPlanReviewInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(
            goal = %input.goal,
            n = input.n_hypotheses,
            "Running adversarial plan review"
        );

        // Pull past adversarial analyses from the knowledge base so the LLM can
        // avoid repeating already-known failure patterns and build on prior learning.
        let past_patterns = self
            .svc
            .search_notes("adversarial review plan failure scenario", 3, 0, None, None)
            .await
            .unwrap_or_default();

        let past_context = if past_patterns.is_empty() {
            "None found.".to_string()
        } else {
            past_patterns
                .iter()
                .filter_map(|n| n["content"].as_str())
                .take(3)
                .map(|c| format!("- {}", c.chars().take(300).collect::<String>()))
                .collect::<Vec<_>>()
                .join("\n")
        };

        let prompt = format!(
            "You are an Adversarial Critic. Stress-test the following plan by generating \
             {n} distinct failure scenarios — plausible ways the plan could fail, be derailed, \
             or produce a wrong outcome.\n\n\
             GOAL: {goal}\n\
             PROPOSED PLAN: {plan}\n\n\
             PAST FAILURE PATTERNS (from knowledge base — do not repeat these):\n{past}\n\n\
             For each scenario score how well the current plan handles it \
             (1 = completely unprepared, 5 = fully mitigated).\n\
             Then give an overall_robustness score (1.0–5.0, one decimal) and \
             brief adjusted_plan_notes on how to strengthen the weakest points.\n\n\
             Respond ONLY with valid JSON — no markdown fences, no explanation outside the JSON:\n\
             {{\"hypotheses\":[{{\"scenario\":\"...\",\"robustness_score\":N,\"mitigation\":\"...\"}}],\
             \"overall_robustness\":N.N,\"adjusted_plan_notes\":\"...\"}}",
            n = input.n_hypotheses,
            goal = input.goal,
            plan = input.proposed_plan,
            past = past_context,
        );

        let raw = match self.llm.generate(&prompt, None).await {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("LLM call failed: {}", e)),
        };

        let text = raw.trim();
        let parsed: Value = agent_brain_llm::llm::first_json_object(text, &["overall_robustness"])
            .unwrap_or_else(|| {
                json!({
                    "hypotheses": [],
                    "overall_robustness": 3.0,
                    "adjusted_plan_notes": text
                })
            });

        let overall_robustness = parsed["overall_robustness"].as_f64().unwrap_or(3.0);

        // Persist so future reviews can learn from this one.
        let note_content = format!(
            "Adversarial Plan Review\nGoal: {}\nPlan: {}\nOverall robustness: {:.1}/5\n\n{}",
            input.goal,
            input.proposed_plan.chars().take(200).collect::<String>(),
            overall_robustness,
            serde_json::to_string_pretty(&parsed).unwrap_or_default()
        );
        let stored = self
            .svc
            .store_note(
                &note_content,
                Some("semantic"),
                Some("adversarial_review"),
                None,
                None,
            )
            .await;
        // The schema promises the review is linked to its task; a review that
        // is never linked cannot be found from the task it judged.
        if let (Ok((note_id, _)), Some(task_id)) = (&stored, input.task_id.as_deref()) {
            match self.svc.link_note_to_task(note_id, task_id).await {
                Ok(true) => {}
                Ok(false) => warn!(
                    task_id,
                    "adversarial review: task not found, note left unlinked"
                ),
                Err(e) => {
                    warn!(task_id, error = %e, "adversarial review: linking note to task failed")
                }
            }
        }

        ToolCallResult::success_json(parsed)
    }

    fn synthesize_knowledge_def() -> ToolDefinition {
        ToolDefinition {
            name: "synthesize_knowledge".to_string(),
            description: "Search for recent notes and inferences related to a topic, then use \
                         the LLM to distill durable facts, patterns, and insights into a new \
                         semantic knowledge note. Use this at the end of a reasoning or research \
                         chain to persist what was actually learned."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "topic": {
                        "type": "string",
                        "description": "The topic or goal to synthesize knowledge about"
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of source notes to draw from (default: 10)"
                    }
                },
                "required": ["topic"]
            }),
        }
    }

    async fn handle_synthesize_knowledge(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: SynthesizeKnowledgeInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };

        info!(topic = %input.topic, limit = input.limit, "Synthesizing semantic knowledge");

        match self
            .svc
            .synthesize_knowledge(&input.topic, input.limit)
            .await
        {
            Ok((note_id, preview)) => ToolCallResult::success_json(json!({
                "success": true,
                "note_id": note_id,
                "note_type": "semantic",
                "preview": preview
            })),
            Err(e) => ToolCallResult::error(format!("Knowledge synthesis failed: {}", e)),
        }
    }
}

#[async_trait]
impl Skill for KnowledgeSkill {
    fn name(&self) -> &str {
        "Knowledge Manager"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![
            Self::store_note_def(),
            Self::search_notes_def(),
            Self::recall_recent_def(),
            Self::prune_old_notes_def(),
            Self::consolidate_memories_def(),
            Self::synthesize_knowledge_def(),
            Self::reason_def(),
            Self::adversarial_plan_review_def(),
        ]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "store_note" => Some(self.handle_store_note(arguments).await),
            "search_notes" => Some(self.handle_search_notes(arguments).await),
            "recall_recent" => Some(self.handle_recall_recent(arguments).await),
            "prune_old_notes" => Some(self.handle_prune_old_notes(arguments).await),
            "consolidate_memories" => Some(self.handle_consolidate_memories(arguments).await),
            "synthesize_knowledge" => Some(self.handle_synthesize_knowledge(arguments).await),
            "reason" => Some(self.handle_reason(arguments).await),
            "adversarial_plan_review" => Some(self.handle_adversarial_plan_review(arguments).await),
            _ => None,
        }
    }
}

// ============================================================================
// Input structs
// ============================================================================

/// Marker a research step writes as the first line of its answer when the
/// evidence did not answer the question. Chain prompts ask for it by name.
pub const NO_FINDINGS_MARKER: &str = "NO FINDINGS";

/// The opening of a body paragraph that says the evidence did not cover the
/// question: "The provided context does not contain …". Only short notes are
/// checked, so an answer that lists what is missing at the end is not caught.
static NULL_RESULT_OPENER: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
    regex::Regex::new(
        r"(?i)^(\*\*)?(the provided|based on the provided|regarding|the web search|the search)[^\n]{0,240}\b(do|does) not (contain|provide|offer|include|mention|address)\b",
    )
    .expect("null-result opener regex")
});

/// Longest note the opener fallback applies to; matches
/// `scripts/retype_null_result_notes.cypher`.
const NULL_RESULT_MAX_CHARS: usize = 900;

/// Whether a note that would be stored as knowledge (`semantic`, or no type)
/// only reports that a search came up empty.
///
/// Research and gap chains store their `reason` output as `semantic`, and
/// consolidation merges semantic notes. A note saying "the sources do not
/// contain anything about X" was therefore merged as a finding: 4 of the 8
/// sources of the "what is a knowledge gap" consolidation (2026-10-03) were
/// such notes, and 104 more were waiting. Stored as `episodic`, the note still
/// records that the search ran and leaves consolidation alone.
///
/// Two signals: the `NO FINDINGS` marker on any line (what the prompts ask
/// for), or, for a short note, a first body paragraph that opens by saying the
/// evidence lacks the answer (what local models write when they skip the marker).
fn is_null_result(content: &str, note_type: Option<&str>) -> bool {
    if !matches!(note_type.map(str::trim), None | Some("") | Some("semantic")) {
        return false;
    }
    let marked = content.lines().any(|line| {
        line.trim_start_matches(|c: char| c == '#' || c == '*' || c == '-' || c.is_whitespace())
            .starts_with(NO_FINDINGS_MARKER)
    });
    if marked {
        return true;
    }
    if content.chars().count() >= NULL_RESULT_MAX_CHARS {
        return false;
    }
    // First paragraph after an optional markdown heading line.
    let body = match content.trim_start().strip_prefix('#') {
        Some(_) => content
            .trim_start()
            .split_once('\n')
            .map_or("", |(_, rest)| rest),
        None => content,
    };
    NULL_RESULT_OPENER.is_match(body.trim_start())
}

/// Marker `run_agent` puts on the first line of an answer that failed its
/// caller's `verify` requirements.
pub use agent_brain_chat::agent_runner::UNVERIFIED_MARKER;

/// Whether a note headed for `semantic` (or untyped) carries the UNVERIFIED
/// marker on a line of its own. Such a note is stored as `unsourced_synthesis`.
fn is_unverified(content: &str, note_type: Option<&str>) -> bool {
    matches!(note_type.map(str::trim), None | Some("") | Some("semantic"))
        && content.lines().any(|line| {
            line.trim_start_matches(|c: char| c == '#' || c == '*' || c.is_whitespace())
                .starts_with(UNVERIFIED_MARKER)
        })
}

/// The verifier's reason from an UNVERIFIED marker line: the text after the
/// marker, up to the sentence `run_agent` appends. `None` without a marker.
fn unverified_reason(content: &str) -> Option<String> {
    let line = content.lines().find_map(|line| {
        line.trim_start_matches(|c: char| c == '#' || c == '*' || c.is_whitespace())
            .strip_prefix(UNVERIFIED_MARKER)
    })?;
    let reason = line
        .split(". Treat everything below as unchecked")
        .next()
        .unwrap_or(line)
        .trim()
        .trim_end_matches('.');
    Some(if reason.is_empty() {
        "unspecified".to_string()
    } else {
        reason.to_string()
    })
}

#[derive(Debug, Deserialize)]
struct StoreNoteInput {
    content: String,
    #[serde(default)]
    note_type: Option<String>,
    #[serde(default)]
    source_context: Option<String>,
    #[serde(default)]
    event_at: Option<String>,
    #[serde(default)]
    provenance: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RecallRecentInput {
    #[serde(default)]
    window: Option<String>,
    #[serde(default)]
    sample_limit: Option<usize>,
    // Drill-down filters. Empty strings are "absent", not a constraint for "" —
    // a model that omits a filter by passing "" must not silently match nothing.
    #[serde(
        default,
        deserialize_with = "agent_brain_retrieve::serde_flex::empty_string_as_none"
    )]
    source_context: Option<String>,
    #[serde(
        default,
        deserialize_with = "agent_brain_retrieve::serde_flex::empty_string_as_none"
    )]
    note_type: Option<String>,
    /// Rows to skip — how the caller pages through a matching set larger than
    /// one tool result. Copied from the previous call's `next_offset`.
    #[serde(default)]
    offset: Option<usize>,
    /// Deliver the COMPLETE matching set to the USER instead of a model-sized
    /// page. The rows bypass the model's context entirely; it gets a receipt.
    #[serde(default)]
    full: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct SearchNotesInput {
    // Every search argument here is an optional filter: an empty string is
    // "absent", not a constraint for "". Normalized at the boundary via
    // `empty_string_as_none` so no handler has to remember the guard.
    #[serde(
        default,
        deserialize_with = "agent_brain_retrieve::serde_flex::empty_string_as_none"
    )]
    query: Option<String>,
    #[serde(
        default,
        deserialize_with = "agent_brain_retrieve::serde_flex::empty_string_as_none"
    )]
    entity_name: Option<String>,
    #[serde(
        default,
        deserialize_with = "agent_brain_retrieve::serde_flex::empty_string_as_none"
    )]
    entity_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "agent_brain_retrieve::serde_flex::empty_string_as_none"
    )]
    note_type: Option<String>,
    // Restrict the semantic search to notes with this exact provenance tag (e.g.
    // "resume"). Advertised in the tool schema but previously never captured here,
    // so it was silently ignored — the fix is this field + threading it into the
    // search filter.
    #[serde(
        default,
        deserialize_with = "agent_brain_retrieve::serde_flex::empty_string_as_none"
    )]
    source_context: Option<String>,
    #[serde(default = "agent_brain_protocol::default_limit_5")]
    limit: usize,
    #[serde(default = "agent_brain_protocol::default_graph_hops")]
    graph_hops: usize,
    #[serde(default)]
    entity_expansion: bool,
}

#[derive(Debug, Deserialize)]
struct PruneOldNotesInput {
    #[serde(default = "default_days_stale")]
    days_stale: i64,
    #[serde(default = "default_min_accesses")]
    min_accesses: i64,
    #[serde(default)]
    score_threshold: Option<f64>,
    #[serde(default)]
    lambda: Option<f64>,
    #[serde(default)]
    dry_run: bool,
    #[serde(default = "default_min_retain")]
    min_retain: i64,
    #[serde(default = "default_max_prune_pct")]
    max_pct: f64,
}

#[derive(Debug, Deserialize)]
struct ConsolidateMemoriesInput {
    topic: String,
    #[serde(default = "default_consolidate_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct ReasonInput {
    question: String,
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    context: Option<String>,
    #[serde(default = "default_reason_limit")]
    limit: usize,
    #[serde(
        default = "default_store_inference",
        deserialize_with = "agent_brain_retrieve::serde_flex::deserialize_flex_bool"
    )]
    store_inference: bool,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    available_tools: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "agent_brain_retrieve::serde_flex::deserialize_flex_bool"
    )]
    run_critic: bool,
    #[serde(
        default,
        deserialize_with = "agent_brain_retrieve::serde_flex::deserialize_flex_bool"
    )]
    create_gap_tasks: bool,
}

#[derive(Debug, Deserialize)]
struct SynthesizeKnowledgeInput {
    topic: String,
    #[serde(default = "default_consolidate_limit")]
    limit: usize,
}

#[derive(Debug, Deserialize)]
struct AdversarialPlanReviewInput {
    goal: String,
    proposed_plan: String,
    #[serde(default = "default_n_hypotheses")]
    n_hypotheses: u8,
    #[serde(default)]
    task_id: Option<String>,
}

fn default_n_hypotheses() -> u8 {
    3
}

fn default_days_stale() -> i64 {
    30
}
fn default_min_accesses() -> i64 {
    2
}
fn default_min_retain() -> i64 {
    50
}
fn default_max_prune_pct() -> f64 {
    0.30
}
fn default_consolidate_limit() -> usize {
    10
}
fn default_reason_limit() -> usize {
    8
}
fn default_store_inference() -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use std::sync::Arc;

    fn skill_ok() -> KnowledgeSkill {
        KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::ok("{}")),
        )
    }

    // -- null-result notes ----------------------------------------------------

    #[test]
    fn a_no_findings_marker_makes_a_research_note_episodic() {
        let note =
            "## Research: sodium battery cost\n\nNO FINDINGS — no result gave a $/kWh figure.";
        assert!(is_null_result(note, Some("semantic")));
        assert!(is_null_result(note, None));
        assert!(is_null_result(
            "## Deep dive: x\n\n**NO FINDINGS** nothing",
            None
        ));
    }

    #[test]
    fn a_short_note_opening_with_absence_is_a_null_result() {
        let note = "## Gap synthesis: IRA captivity context\n\nThe provided context does not \
                    contain any specific information detailing the nature of the context.";
        assert!(is_null_result(note, Some("semantic")));
    }

    #[test]
    fn an_answer_that_lists_what_is_missing_afterwards_stays_semantic() {
        let note = "## Gap synthesis: sodium batteries\n\n## ANSWER\n- CATL ships Naxtra cells \
                    (https://example.com).\n\n## STILL UNKNOWN\nThe provided sources do not \
                    contain a price per kWh.";
        assert!(!is_null_result(note, Some("semantic")));
    }

    #[test]
    fn a_long_note_is_not_judged_by_its_opener() {
        let note = format!(
            "## Research: x\n\nThe provided context does not contain a definition, but: {}",
            "fact. ".repeat(200)
        );
        assert!(!is_null_result(&note, Some("semantic")));
    }

    #[test]
    fn other_note_types_are_never_retyped() {
        let note = "NO FINDINGS — nothing turned up.";
        assert!(!is_null_result(note, Some("reflection")));
        assert!(!is_null_result(note, Some("source_record")));
    }

    #[tokio::test]
    async fn store_note_reports_the_episodic_type_for_a_null_result() {
        let skill = skill_ok();
        let args =
            json!({"content": "## Research: x\n\nNO FINDINGS — nothing.", "note_type": "semantic"});
        let r = result_json(skill.execute("store_note", Some(args)).await.unwrap());
        assert_eq!(r["note_type"], "episodic");
    }

    #[tokio::test]
    async fn an_unverified_agent_answer_is_stored_as_unsourced_synthesis() {
        let skill = skill_ok();
        let args = json!({
            "content": "## Deep dive: x\n\nUNVERIFIED — no successful `mcp_call` call this run.\n\n## ANSWER\n- y",
            "note_type": "semantic"
        });
        let r = result_json(skill.execute("store_note", Some(args)).await.unwrap());
        assert_eq!(r["note_type"], "unsourced_synthesis");
        assert!(!is_unverified("UNVERIFIED — x", Some("reflection")));
    }

    #[test]
    fn unverified_reason_is_the_verifiers_words() {
        let content = "## Deep dive: q\n\nUNVERIFIED — the code did not compile. Treat everything below as unchecked.\n\n## ANSWER";
        assert_eq!(
            unverified_reason(content).as_deref(),
            Some("the code did not compile")
        );
        assert_eq!(unverified_reason("## ANSWER\n- fine"), None);
        assert_eq!(
            unverified_reason("UNVERIFIED —").as_deref(),
            Some("unspecified")
        );
    }

    // -- tool registry --------------------------------------------------------

    #[test]
    fn tools_list_has_correct_count() {
        assert_eq!(skill_ok().tools().len(), 8);
    }

    #[test]
    fn execute_unknown_tool_returns_none() {
        let skill = skill_ok();
        let result = tokio::runtime::Runtime::new()
            .unwrap()
            .block_on(skill.execute("nonexistent_tool", None));
        assert!(result.is_none());
    }

    // -- recall_recent --------------------------------------------------------

    #[tokio::test]
    async fn recall_recent_defaults_to_today_and_echoes_window() {
        let skill = skill_ok();
        // No args → window defaults to "today"; the mock echoes since/until.
        let r = result_json(
            skill
                .execute("recall_recent", Some(json!({})))
                .await
                .unwrap(),
        );
        assert_eq!(r["window"], "today");
        assert!(r["since"].as_str().is_some(), "since bound present");
        assert!(r["until"].as_str().is_some(), "until bound present");
        // "since" (local midnight, UTC) must not be after "until" (now, UTC).
        assert!(r["since"].as_str().unwrap() <= r["until"].as_str().unwrap());
    }

    #[tokio::test]
    async fn recall_recent_accepts_n_day_window() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute("recall_recent", Some(json!({"window": "7d"})))
                .await
                .unwrap(),
        );
        assert_eq!(r["window"], "7d");
        assert_eq!(r["total_notes"], 0); // mock store is empty
    }

    // The drill-down half: a bucket reported in `by_source_context` has to be
    // expandable by name. Without this, a caller handed "video_learning: 43" can
    // only reach for search_notes, which ranks by topic and reports absence for a
    // label — the 2026-09-11 "no videos today" false negative on a day with 11.
    #[tokio::test]
    async fn recall_recent_forwards_drill_down_filters() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute(
                    "recall_recent",
                    Some(json!({
                        "source_context": "video_learning",
                        "note_type": "source_record"
                    })),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["filter"]["source_context"], "video_learning");
        assert_eq!(r["filter"]["note_type"], "source_record");
    }

    // An omitted filter passed as "" must be absent, not a constraint for "" —
    // otherwise the drill-down silently matches nothing and looks like absence
    // again, one layer down.
    #[tokio::test]
    async fn recall_recent_treats_empty_filter_as_absent() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute(
                    "recall_recent",
                    Some(json!({"source_context": "", "note_type": "  "})),
                )
                .await
                .unwrap(),
        );
        assert!(r["filter"]["source_context"].is_null());
        assert!(r["filter"]["note_type"].is_null());
    }

    // -- search_notes enumeration ---------------------------------------------

    /// A query-less enumeration that matches nothing must name the labels that
    /// DO exist, exactly as `recall_recent` does. Without it the caller sees a
    /// bare `count: 0` and cannot tell a wrong label from an empty graph — on
    /// 2026-09-11 a delegated worker probed note_type "agent", source_context
    /// "agent", then note_type "news", learned nothing from any miss, burned
    /// all 8 of its rounds and reported it could not determine the answer.
    #[tokio::test]
    async fn empty_enumeration_names_the_labels_that_exist() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute("search_notes", Some(json!({"note_type": "agent"})))
                .await
                .unwrap(),
        );
        assert_eq!(r["count"], 0);
        // The escape hatch: totals plus both label inventories.
        assert_eq!(r["graph_total_notes"], 250);
        assert!(r["available_note_types"].is_array());
        assert!(r["available_source_contexts"].is_array());
        let note = r["note"].as_str().expect("advisory note present");
        assert!(
            note.contains("NOT evidence"),
            "must reject the absence reading"
        );
        assert!(note.contains("250"), "must quote the real total");
    }

    /// The hint is for a zero — a successful enumeration must not be polluted
    /// with it, or every drill-down carries a "you may be wrong" banner.
    #[tokio::test]
    async fn non_empty_enumeration_carries_no_absence_hint() {
        let skill = KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore {
                search_result: Ok(vec![json!({"id": "n1", "content": "a"})]),
                ..Default::default()
            }),
            Arc::new(MockLlm::ok("{}")),
        );
        let r = result_json(
            skill
                .execute("search_notes", Some(json!({"note_type": "source_record"})))
                .await
                .unwrap(),
        );
        assert_eq!(r["count"], 1);
        assert!(r["note"].is_null());
        assert!(r["available_note_types"].is_null());
    }

    // -- recall_recent paging -------------------------------------------------

    /// `offset` must reach the service, or "show me the rest" silently re-reads
    /// page one forever.
    #[tokio::test]
    async fn recall_recent_forwards_offset() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute(
                    "recall_recent",
                    Some(json!({"source_context": "video_learning", "offset": 50})),
                )
                .await
                .unwrap(),
        );
        assert_eq!(r["offset"], 50);
    }

    /// Absent offset is page one, not an error.
    #[tokio::test]
    async fn recall_recent_defaults_offset_to_zero() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute("recall_recent", Some(json!({})))
                .await
                .unwrap(),
        );
        assert_eq!(r["offset"], 0);
    }

    /// The schema has to TELL the model to page rather than inflate the page —
    /// the behavioural half. A worker that raises `sample_limit` against a
    /// clipped channel just moves where the clipping falls and reports the set
    /// as unavailable, which is the failure this whole path exists to stop.
    #[test]
    fn recall_recent_schema_steers_to_paging_not_a_bigger_limit() {
        let def = KnowledgeSkill::recall_recent_def();
        let offset = def.input_schema["properties"]["offset"]["description"]
            .as_str()
            .expect("offset documented");
        assert!(offset.contains("next_offset"));
        let limit = def.input_schema["properties"]["sample_limit"]["description"]
            .as_str()
            .expect("sample_limit documented");
        assert!(limit.contains("offset"), "must redirect to paging");
        assert!(def.description.contains("next_offset"));
        assert!(def.description.contains("TRUNCATED"));
    }

    // -- full delivery (the bulk channel) -------------------------------------

    /// `full: true` must reach the service, or "show me everything" silently
    /// returns a model-sized page instead of a delivery.
    #[tokio::test]
    async fn recall_recent_forwards_full() {
        let skill = skill_ok();
        let r = result_json(
            skill
                .execute("recall_recent", Some(json!({"full": true})))
                .await
                .unwrap(),
        );
        assert_eq!(r["full"], true);
    }

    /// The split is the whole feature: the user's copy carries every row, the
    /// model's copy carries a receipt and three. A model that receives all 50
    /// has gained nothing over the old paging and has paid for it in context.
    #[test]
    fn full_delivery_sends_rows_to_the_user_and_a_receipt_to_the_model() {
        let rows: Vec<Value> = (0..50)
            .map(|i| {
                json!({
                    "id": format!("n{i}"),
                    "note_type": "claim",
                    "source_context": "video_learning",
                    "created_at": "2026-09-12T00:00:00Z",
                    "snippet": format!("row {i} content"),
                })
            })
            .collect();
        let v = json!({ "matched_notes": 50, "sample": rows });

        let out = KnowledgeSkill::split_full_delivery(v);
        let bulk = out.bulk.clone().expect("bulk channel populated");
        let model = out.model_text();

        // Every row reached the person.
        assert!(bulk.contains("row 0 content"));
        assert!(bulk.contains("row 49 content"));
        assert!(bulk.contains("[50/50]"));

        // The model got the count and a taste — not the rows.
        let m: Value = serde_json::from_str(&model).expect("model copy is json");
        assert_eq!(m["delivered_to_user"], 50);
        assert_eq!(m["sample"].as_array().map(Vec::len), Some(3));
        assert!(
            !model.contains("row 49 content"),
            "row 49 leaked into context"
        );

        // And it is told not to undo the delivery by re-querying or hedging.
        let note = m["note"].as_str().unwrap_or_default();
        assert!(note.contains("FULL DELIVERY"));
        assert!(note.contains("Do NOT page"));
        assert!(note.contains("incomplete"));
    }

    /// Context cost must not grow with the result. This is the property the
    /// whole bulk channel exists to provide, so assert it directly rather than
    /// trusting the shape.
    #[test]
    fn full_delivery_model_cost_is_flat_in_row_count() {
        let build = |n: usize| {
            let rows: Vec<Value> = (0..n)
                .map(|i| {
                    json!({
                        "id": format!("n{i}"),
                        "note_type": "claim",
                        "source_context": "video_learning",
                        "created_at": "2026-09-12T00:00:00Z",
                        "snippet": "x".repeat(400),
                    })
                })
                .collect();
            KnowledgeSkill::split_full_delivery(json!({ "matched_notes": n, "sample": rows }))
        };
        let small = build(5).model_text().len();
        let huge = build(500).model_text().len();
        // 100x the rows must not meaningfully move the model's payload.
        assert!(
            huge < small * 2,
            "model payload grew with row count: {small} -> {huge}"
        );
        // ...while the user's copy did grow, because that is the point.
        assert!(build(500).bulk.unwrap().len() > build(5).bulk.unwrap().len() * 10);
    }

    #[test]
    fn full_flag_is_documented_as_a_delivery_not_a_page() {
        let def = KnowledgeSkill::recall_recent_def();
        let full = def.input_schema["properties"]["full"]["description"]
            .as_str()
            .expect("full documented");
        assert!(full.contains("show me all") || full.contains("show me"));
        assert!(full.contains("NOT in your context") || full.contains("not in your context"));
        assert!(def.description.contains("full: true"));
    }

    // -- store_note -----------------------------------------------------------

    #[tokio::test]
    async fn store_note_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"content": "hello world"});
        let r = result_json(skill.execute("store_note", Some(args)).await.unwrap());
        assert_eq!(r["id"], "note-id-1");
        assert_eq!(r["links_created"], 2);
    }

    #[tokio::test]
    async fn store_note_missing_content_returns_error() {
        let skill = skill_ok();
        let r = skill
            .execute("store_note", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[tokio::test]
    async fn store_note_propagates_store_error() {
        let store = MockKnowledgeStore {
            store_result: Err("db down".into()),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"content": "test"});
        let msg = result_error(skill.execute("store_note", Some(args)).await.unwrap());
        assert!(msg.contains("db down"));
    }

    // -- search_notes ---------------------------------------------------------

    #[tokio::test]
    async fn search_notes_success() {
        let store = MockKnowledgeStore {
            search_result: Ok(vec![serde_json::json!({"id": "n1", "content": "c1"})]),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"query": "something"});
        let r = result_json(skill.execute("search_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 1);
    }

    #[tokio::test]
    async fn search_notes_with_entity_expansion() {
        let store = MockKnowledgeStore {
            search_result: Ok(vec![serde_json::json!({"id": "n1"})]),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"query": "rust", "entity_expansion": true});
        let r = result_json(skill.execute("search_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 1);
    }

    #[tokio::test]
    async fn search_notes_empty_query_returns_error() {
        let skill = skill_ok();
        let r = skill
            .execute("search_notes", Some(serde_json::json!({"query": ""})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    #[tokio::test]
    async fn search_notes_no_query_or_entity_returns_error() {
        let skill = skill_ok();
        let r = skill
            .execute("search_notes", Some(serde_json::json!({})))
            .await
            .unwrap();
        assert_eq!(r.is_error, Some(true));
    }

    // -- search_notes entity_name path ----------------------------------------

    #[tokio::test]
    async fn search_notes_by_entity_name_success() {
        let store = MockKnowledgeStore {
            search_result: Ok(vec![serde_json::json!({"id": "n2"})]),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"entity_name": "Rust"});
        let r = result_json(skill.execute("search_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 1);
        assert_eq!(r["entity_name"], "Rust");
    }

    /// An empty/whitespace `entity_name` must be treated as absent, not searched
    /// — otherwise a model looping `search_notes` with `entity_name: ""` gets
    /// noise back and keeps looping (measured 2026-09-07 burning a sub-agent's
    /// budget). With nothing else supplied it must error, pointing the model at
    /// a real query rather than returning a successful empty search.
    #[tokio::test]
    async fn search_notes_empty_entity_name_is_not_searched() {
        let skill = skill_ok();
        for v in ["", "   "] {
            let r = skill
                .execute("search_notes", Some(serde_json::json!({"entity_name": v})))
                .await
                .unwrap();
            assert_eq!(
                r.is_error,
                Some(true),
                "entity_name {v:?} should error, not search"
            );
        }
    }

    /// But an empty `entity_name` alongside a real `query` falls through to the
    /// query search rather than erroring — the empty entity is simply ignored.
    #[tokio::test]
    async fn empty_entity_name_with_a_real_query_falls_through_to_query() {
        let store = MockKnowledgeStore {
            search_result: Ok(vec![serde_json::json!({"id": "n3"})]),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"entity_name": "", "query": "reticulum"});
        let r = result_json(skill.execute("search_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 1);
    }

    /// An empty/whitespace `note_type` must be treated as absent, not a filter
    /// for `note_type = ""`. Regression: a delegated worker passed note_type:""
    /// alongside a query, the real store filtered for type "" and returned
    /// nothing, and the worker reported "no information" on data that existed
    /// (2026-09-07). With no query either, an empty note_type leaves nothing to
    /// search, so it must error rather than list notes of type "".
    #[tokio::test]
    async fn empty_note_type_is_not_treated_as_a_filter() {
        let skill = skill_ok();
        for v in ["", "   "] {
            let r = skill
                .execute("search_notes", Some(serde_json::json!({"note_type": v})))
                .await
                .unwrap();
            assert_eq!(
                r.is_error,
                Some(true),
                "note_type {v:?} must be ignored, not listed as a type"
            );
        }
    }

    // -- prune_old_notes ------------------------------------------------------

    #[tokio::test]
    async fn prune_old_notes_dry_run() {
        let skill = skill_ok();
        let args = serde_json::json!({"dry_run": true});
        let r = result_json(skill.execute("prune_old_notes", Some(args)).await.unwrap());
        assert_eq!(r["dry_run"], true);
        assert_eq!(r["count"], 3);
    }

    #[tokio::test]
    async fn prune_old_notes_actual() {
        let skill = skill_ok();
        let args = serde_json::json!({"days_stale": 14});
        let r = result_json(skill.execute("prune_old_notes", Some(args)).await.unwrap());
        assert_eq!(r["count"], 3);
        assert!(r.get("dry_run").is_none());
    }

    // -- consolidate_memories -------------------------------------------------

    #[tokio::test]
    async fn consolidate_memories_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"topic": "rust async"});
        let r = result_json(
            skill
                .execute("consolidate_memories", Some(args))
                .await
                .unwrap(),
        );
        assert_eq!(r["id"], "consolidated-id");
        assert_eq!(r["source_count"], 5);
    }

    #[tokio::test]
    async fn consolidate_memories_propagates_error() {
        let store = MockKnowledgeStore {
            consolidate_result: Err("consolidation failed".into()),
            ..Default::default()
        };
        let skill = KnowledgeSkill::new(Arc::new(store), Arc::new(MockLlm::ok("{}")));
        let args = serde_json::json!({"topic": "test"});
        let msg = result_error(
            skill
                .execute("consolidate_memories", Some(args))
                .await
                .unwrap(),
        );
        assert!(msg.contains("consolidation failed"));
    }

    // -- synthesize_knowledge -------------------------------------------------

    #[tokio::test]
    async fn synthesize_knowledge_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"topic": "Rust async"});
        let r = result_json(
            skill
                .execute("synthesize_knowledge", Some(args))
                .await
                .unwrap(),
        );
        assert_eq!(r["note_id"], "synth-id");
        assert_eq!(r["success"], true);
    }

    // -- reason ---------------------------------------------------------------

    #[tokio::test]
    async fn reason_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"question": "What is async?"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["answer"], "answer text");
        assert!((r["confidence"].as_f64().unwrap() - 0.85).abs() < 1e-9);
    }

    // -- reason action=audit --------------------------------------------------

    #[tokio::test]
    async fn reason_audit_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"action": "audit", "question": "deploy to prod"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["aligned"], true);
    }

    // -- reason action=explain ------------------------------------------------

    #[tokio::test]
    async fn reason_explain_success() {
        let skill = skill_ok();
        let args = serde_json::json!({"action": "explain", "question": "chose Rust"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["explanation"], "explanation text");
    }

    // -- reason action=clarify ------------------------------------------------

    #[tokio::test]
    async fn reason_clarify_parses_llm_json() {
        let llm_resp = r#"{"needs_clarification": true, "ambiguities": ["scope"], "clarifying_questions": ["Which env?"], "assumptions": [], "recommended_approach": "ask first"}"#;
        let skill = KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::ok(llm_resp)),
        );
        let args = serde_json::json!({"action": "clarify", "question": "deploy the thing"});
        let r = result_json(skill.execute("reason", Some(args)).await.unwrap());
        assert_eq!(r["needs_clarification"], true);
    }

    #[tokio::test]
    async fn reason_clarify_llm_error_returns_error() {
        let skill = KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::err("llm unavailable")),
        );
        let args = serde_json::json!({"action": "clarify", "question": "do something"});
        let msg = result_error(skill.execute("reason", Some(args)).await.unwrap());
        assert!(msg.contains("llm unavailable"));
    }
}
