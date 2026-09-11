//! Knowledge Skill - Provides tools for managing notes and memories.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::info;

use crate::services::traits::{KnowledgeStore, LlmProvider};
use crate::skills::Skill;
use agent_brain_models::ProvenanceFlag;
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
                    "tags": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional tags for categorization (stored but not indexed)"
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
                        "description": "Source authority flag: who/what produced this note (default: user_input)"
                    }
                },
                "required": ["content"]
            }),
        }
    }

    fn search_notes_def() -> ToolDefinition {
        ToolDefinition {
            name: "search_notes".to_string(),
            description: "Search stored notes. Provide `query` for hybrid BM25+semantic search with \
                         optional graph expansion. Provide `entity_name` instead to find notes that \
                         mention a specific named entity (case-insensitive). Use `note_type` to filter \
                         by type (semantic, episodic, reflection, consolidated, news, news_raw, outcome) \
                         — when set without a `query`, lists all notes of that type ordered by date. \
                         At least one of `query`, `entity_name`, or `note_type` must be supplied."
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
                        "description": "Filter by note type: semantic, episodic, reflection, consolidated, news, news_raw, outcome. Without a query, lists all notes of this type by date."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "Max number of results (default: 5)"
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
                 TWO-STEP DRILL-DOWN — this is how you answer 'summarize the videos we watched \
                 today' or any 'what did <source> produce' question: (1) call with no filter to \
                 see the buckets, then (2) call AGAIN passing `source_context` (or `note_type`) \
                 copied verbatim from a row of `by_source_context` / `by_note_type` to get those \
                 notes themselves, with longer snippets. NEVER switch to search_notes to expand a \
                 bucket: it ranks by topic similarity, so searching the label 'video_learning' \
                 returns whatever note is topically nearest — typically an unrelated note from \
                 another month — and reads as 'nothing today'. If step 1 reports a non-zero count \
                 for a bucket, those notes EXIST; a later empty search is the search failing, not \
                 absence. Check `complete` to know whether you were shown every matching note, \
                 and raise `sample_limit` if not. Filter ONE side at a time — pairing a \
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
                        "description": "Max sample rows to return (default 24, or 50 when a filter is set; capped at 100)."
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

        let provenance = input
            .provenance
            .as_deref()
            .and_then(|s| s.parse::<ProvenanceFlag>().ok())
            .unwrap_or(ProvenanceFlag::UserInput);

        match self
            .svc
            .store_note(
                &input.content,
                input.note_type.as_deref(),
                input.source_context.as_deref(),
                input.event_at.as_deref(),
                Some(provenance),
            )
            .await
        {
            Ok((id, links_created)) => {
                // "answer" echoes the stored content so downstream {{_prev}} steps
                // (e.g. notify_user after store_note) receive the original text, not metadata JSON.
                let response = json!({
                    "success": true,
                    "id": id,
                    "links_created": links_created,
                    "message": "Note stored successfully",
                    "answer": input.content
                });
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

        // note_type-only path: list all notes of that type ordered by date
        if input.query.as_deref().is_none_or(str::is_empty) {
            if let Some(nt) = note_type {
                info!(note_type = %nt, "Listing notes by type");
                return match self.svc.list_notes(input.limit, Some(nt)).await {
                    Ok(notes) => ToolCallResult::success_json(json!({
                        "count": notes.len(),
                        "note_type": nt,
                        "notes": notes
                    })),
                    Err(e) => ToolCallResult::error(format!("list_notes failed: {}", e)),
                };
            }
            return ToolCallResult::error(
                "Provide a non-empty `query`, `entity_name`, or `note_type` — empty values \
                 are ignored, not searched."
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
        use chrono::{Duration, TimeZone, Utc};
        let input: RecallRecentInput = match parse_args(arguments) {
            Ok(v) => v,
            Err(e) => return e,
        };
        // A filtered call is an enumeration, not a characterisation, so it gets a
        // larger default — the point is to see the matching notes, and a default
        // that truncates them re-creates the "I found nothing" failure one step on.
        let filtered = input.source_context.is_some() || input.note_type.is_some();
        let sample_limit = input
            .sample_limit
            .unwrap_or(if filtered { 50 } else { 24 })
            .clamp(1, 100);

        // Windows are computed in the brain's LOCAL zone (stored timestamps are
        // UTC, so the bounds are converted to UTC before the query) — "today"
        // must mean the local day, not the UTC day, or part of every evening
        // reads as "yesterday". See services/clock.rs.
        let now_local = crate::services::clock::now_local();
        let local_midnight = |d: chrono::NaiveDate| {
            now_local
                .timezone()
                .from_local_datetime(&d.and_hms_opt(0, 0, 0).unwrap())
                .single()
                .map(|dt| dt.with_timezone(&Utc))
                .unwrap_or_else(Utc::now)
        };
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
            .recall_window(
                &since.to_rfc3339(),
                &until.to_rfc3339(),
                sample_limit,
                input.source_context.as_deref(),
                input.note_type.as_deref(),
            )
            .await
        {
            Ok(mut v) => {
                if let Value::Object(ref mut m) = v {
                    m.insert("window".into(), json!(window));
                }
                ToolCallResult::success_json(v)
            }
            Err(e) => ToolCallResult::error(format!("recall_recent failed: {e}")),
        }
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
                            let json_start = text.find('{').unwrap_or(0);
                            let json_end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
                            let mut parsed: Value = serde_json::from_str(
                                &text[json_start..json_end],
                            )
                            .unwrap_or_else(|_| {
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
        let json_start = text.find('{').unwrap_or(0);
        let json_end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
        let parsed: Value =
            serde_json::from_str(&text[json_start..json_end]).unwrap_or_else(|_| {
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
        let _ = self
            .svc
            .store_note(
                &note_content,
                Some("semantic"),
                Some("adversarial_review"),
                None,
                None,
            )
            .await;

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

#[derive(Debug, Deserialize)]
struct StoreNoteInput {
    content: String,
    #[serde(default)]
    #[allow(dead_code)]
    tags: Vec<String>,
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
        deserialize_with = "crate::skills::serde_flex::empty_string_as_none"
    )]
    source_context: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::skills::serde_flex::empty_string_as_none"
    )]
    note_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct SearchNotesInput {
    // Every search argument here is an optional filter: an empty string is
    // "absent", not a constraint for "". Normalized at the boundary via
    // `empty_string_as_none` so no handler has to remember the guard.
    #[serde(
        default,
        deserialize_with = "crate::skills::serde_flex::empty_string_as_none"
    )]
    query: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::skills::serde_flex::empty_string_as_none"
    )]
    entity_name: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::skills::serde_flex::empty_string_as_none"
    )]
    entity_type: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::skills::serde_flex::empty_string_as_none"
    )]
    note_type: Option<String>,
    // Restrict the semantic search to notes with this exact provenance tag (e.g.
    // "resume"). Advertised in the tool schema but previously never captured here,
    // so it was silently ignored — the fix is this field + threading it into the
    // search filter.
    #[serde(
        default,
        deserialize_with = "crate::skills::serde_flex::empty_string_as_none"
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
        deserialize_with = "crate::skills::serde_flex::deserialize_flex_bool"
    )]
    store_inference: bool,
    #[serde(default)]
    task_id: Option<String>,
    #[serde(default)]
    available_tools: Option<Vec<String>>,
    #[serde(
        default,
        deserialize_with = "crate::skills::serde_flex::deserialize_flex_bool"
    )]
    run_critic: bool,
    #[serde(
        default,
        deserialize_with = "crate::skills::serde_flex::deserialize_flex_bool"
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
    #[allow(dead_code)]
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
    use crate::skills::test_helpers::*;
    use std::sync::Arc;

    fn skill_ok() -> KnowledgeSkill {
        KnowledgeSkill::new(
            Arc::new(MockKnowledgeStore::default()),
            Arc::new(MockLlm::ok("{}")),
        )
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
