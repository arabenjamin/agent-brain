//! `search_notes` and `recall_recent`.

use super::KnowledgeSkill;
use agent_brain_protocol::{ToolCallResult, ToolDefinition, parse_args};
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

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

impl KnowledgeSkill {
    pub(super) fn search_notes_def() -> ToolDefinition {
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

    pub(super) fn recall_recent_def() -> ToolDefinition {
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

    pub(super) async fn handle_search_notes(&self, arguments: Option<Value>) -> ToolCallResult {
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

    pub(super) async fn handle_recall_recent(&self, arguments: Option<Value>) -> ToolCallResult {
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
