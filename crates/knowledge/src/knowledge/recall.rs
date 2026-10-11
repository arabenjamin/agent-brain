//! Deterministic enumeration: note listings, label buckets, and local-day recall windows.

use super::*;

impl KnowledgeService {
    /// Enumerate notes, optionally narrowed by `note_type` and/or `source_context`.
    ///
    /// `source_context` enumeration exists because similarity search cannot do it:
    /// a résumé filed as 11 notes under `source_context:"resume"` (10411 chars)
    /// was only reachable via `search_notes`, which ranks by topic and truncates,
    /// so a delegated `job-applicant` burned all 8 of its rounds on repeat
    /// searches and still reported the résumé "incomplete" — then declined to
    /// compile an application from it (2026-09-11). Same shape as `recall_recent`
    /// and `list_endpoints`: when the caller knows the LABEL, give them an
    /// enumeration, not a ranked sample.
    /// Whole-graph label inventory: every `note_type` and `source_context` that
    /// actually exists, plus the total note count (chunk fragments excluded).
    ///
    /// The unbounded sibling of [`window_buckets`], and it exists for the same
    /// reason. `search_notes`' query-less enumeration path answers a `note_type`
    /// / `source_context` the caller has to *guess*, and a wrong guess returned
    /// a bare `count: 0` — indistinguishable from "the graph holds none of
    /// these". Observed 2026-09-11: a delegated `researcher` asked "how many
    /// agents does the brain have", probed `note_type:"agent"`, then
    /// `source_context:"agent"`, then `note_type:"news"`, learned nothing from
    /// any of the misses, burned all 8 rounds and reported it could not
    /// determine the answer. A caller cannot guess a label it has never been
    /// shown.
    pub async fn label_buckets(&self) -> Result<(Vec<Value>, Vec<Value>, i64)> {
        // `NOT (n)-[:PART_OF]->()` everywhere, so these counts line up with what
        // the enumeration itself returns rather than double-counting chunks.
        let type_rows = self
            .neo4j
            .execute(neo4rs::query(
                "MATCH (n:Note) WHERE NOT (n)-[:PART_OF]->() \
                 RETURN COALESCE(n.note_type, 'semantic') AS k, count(*) AS c ORDER BY c DESC",
            ))
            .await?;
        let mut by_type = Vec::new();
        let mut total: i64 = 0;
        for row in type_rows {
            let k = row.get::<String>("k").unwrap_or_default();
            let c = row.get::<i64>("c").unwrap_or(0);
            total += c;
            by_type.push(json!({ "note_type": k, "count": c }));
        }

        let src_rows = self
            .neo4j
            .execute(neo4rs::query(
                "MATCH (n:Note) WHERE NOT (n)-[:PART_OF]->() \
                 WITH COALESCE(n.source_context, '(none)') AS k, count(*) AS c \
                 RETURN k, c ORDER BY c DESC LIMIT 30",
            ))
            .await?;
        let by_source = src_rows
            .iter()
            .map(|row| {
                json!({
                    "source_context": row.get::<String>("k").unwrap_or_default(),
                    "count": row.get::<i64>("c").unwrap_or(0),
                })
            })
            .collect();

        Ok((by_type, by_source, total))
    }

    pub async fn list_notes_filtered(
        &self,
        limit: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> Result<Vec<Value>> {
        // Chunk fragments inherit their parent's source_context, so an
        // unfiltered enumeration would return the same text several times over
        // and eat the caller's limit — exclude them, as the prior-run Cypher
        // rule in CLAUDE.md requires.
        if source_context.is_some() {
            let mut q = String::from(
                "MATCH (n:Note) WHERE n.source_context = $source_context \
                 AND NOT (n)-[:PART_OF]->() ",
            );
            if note_type.is_some() {
                q.push_str("AND n.note_type = $note_type ");
            }
            q.push_str(
                "RETURN n.id AS id, n.content AS content, n.note_type AS note_type, \
                 n.access_count AS access_count, toString(n.created_at) AS created_at, \
                 n.source_context AS source_context, n.provenance AS provenance \
                 ORDER BY n.created_at ASC LIMIT $limit",
            );
            let mut query = neo4rs::query(&q)
                .param("source_context", source_context.unwrap_or_default())
                .param("limit", limit as i64);
            if let Some(nt) = note_type {
                query = query.param("note_type", nt);
            }
            return self.rows_to_notes(query).await;
        }

        let query = if let Some(nt) = note_type {
            neo4rs::query(
                r#"
            MATCH (n:Note)
            WHERE n.note_type = $note_type
            RETURN n.id AS id, n.content AS content, n.note_type AS note_type,
                   n.access_count AS access_count, toString(n.created_at) AS created_at,
                   n.source_context AS source_context, n.provenance AS provenance
            ORDER BY n.created_at DESC LIMIT $limit
            "#,
            )
            .param("note_type", nt)
            .param("limit", limit as i64)
        } else {
            neo4rs::query(
                r#"
            MATCH (n:Note)
            RETURN n.id AS id, n.content AS content, n.note_type AS note_type,
                   n.access_count AS access_count, toString(n.created_at) AS created_at,
                   n.source_context AS source_context, n.provenance AS provenance
            ORDER BY n.created_at DESC LIMIT $limit
            "#,
            )
            .param("limit", limit as i64)
        };

        self.rows_to_notes(query).await
    }

    /// Shared row -> JSON mapping for the `list_notes*` enumerations.
    pub(super) async fn rows_to_notes(&self, query: neo4rs::Query) -> Result<Vec<Value>> {
        let rows = self.neo4j.execute(query).await?;
        let mut results = Vec::new();
        for row in rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let content = row.get::<String>("content").unwrap_or_default();
            let note_type = row
                .get::<String>("note_type")
                .unwrap_or_else(|_| "semantic".to_string());
            let access_count = row.get::<i64>("access_count").unwrap_or(0);
            let created_at = row.get::<String>("created_at").unwrap_or_default();
            let source_context = row.get::<String>("source_context").unwrap_or_default();
            let provenance = row
                .get::<String>("provenance")
                .unwrap_or_else(|_| "unrecorded".to_string());
            results.push(serde_json::json!({
                "id": id,
                "content": content,
                "note_type": note_type,
                "access_count": access_count,
                "created_at": created_at,
                "source_context": source_context,
                "provenance": provenance
            }));
        }
        Ok(results)
    }

    /// The two bucket aggregations over a window, optionally narrowed to one
    /// `source_context` / `note_type`. Returns `(by_note_type, by_source_context,
    /// total)`.
    ///
    /// Split out so a filtered call that matches nothing can re-run it
    /// UNFILTERED and report which values do exist — see `recall_window`.
    pub(super) async fn window_buckets(
        &self,
        since_iso: &str,
        until_iso: &str,
        source_context: Option<&str>,
        note_type: Option<&str>,
    ) -> Result<(Vec<Value>, Vec<Value>, i64)> {
        let window = Self::window_predicate(source_context, note_type);
        let bind = |q: neo4rs::Query| {
            let mut q = q.param("since", since_iso).param("until", until_iso);
            if let Some(sc) = source_context {
                q = q.param("source_context", sc);
            }
            if let Some(nt) = note_type {
                q = q.param("note_type", nt);
            }
            q
        };

        // By note_type (+ overall total via a sum below).
        let by_type_rows = self
            .neo4j
            .execute(bind(neo4rs::query(&format!(
                "{window}
                 RETURN COALESCE(n.note_type, 'semantic') AS k, count(*) AS c
                 ORDER BY c DESC"
            ))))
            .await?;
        let mut by_type = Vec::new();
        let mut total: i64 = 0;
        for row in by_type_rows {
            let k = row.get::<String>("k").unwrap_or_default();
            let c = row.get::<i64>("c").unwrap_or(0);
            total += c;
            by_type.push(json!({ "note_type": k, "count": c }));
        }

        // By source_context (what produced the window's notes — video_learning,
        // fill_knowledge_gap, news, brain_api_sync, …). Top 20.
        let by_src_rows = self
            .neo4j
            .execute(bind(neo4rs::query(&format!(
                "{window}
                 WITH COALESCE(n.source_context, '(none)') AS k, count(*) AS c
                 RETURN k, c ORDER BY c DESC LIMIT 20"
            ))))
            .await?;
        let by_source_context: Vec<Value> = by_src_rows
            .into_iter()
            .map(|row| {
                json!({
                    "source_context": row.get::<String>("k").unwrap_or_default(),
                    "count": row.get::<i64>("c").unwrap_or(0),
                })
            })
            .collect();

        Ok((by_type, by_source_context, total))
    }

    /// Window predicate shared by every aggregation and the sample, so a filter
    /// can never apply to some of them and not others.
    ///
    /// `datetime($x)` compares native-to-native; comparing a temporal to a bare
    /// string yields null (a silent row drop), hence the explicit casts — see
    /// project-docs/schema.md. Filters are matched against the same COALESCE'd
    /// values the counts are grouped by, so a name copied out of
    /// `by_source_context` (including the literal "(none)" bucket) round-trips
    /// and selects exactly those rows.
    pub(super) fn window_predicate(
        source_context: Option<&str>,
        note_type: Option<&str>,
    ) -> String {
        let mut p = String::from(
            "MATCH (n:Note)
             WHERE n.created_at >= datetime($since) AND n.created_at < datetime($until)
               AND NOT (n)-[:PART_OF]->()",
        );
        if source_context.is_some() {
            p.push_str(
                "\n               AND COALESCE(n.source_context, '(none)') = $source_context",
            );
        }
        if note_type.is_some() {
            p.push_str("\n               AND COALESCE(n.note_type, 'semantic') = $note_type");
        }
        p
    }

    /// Deterministic time-window recall. See the `KnowledgeStore::recall_window`
    /// doc for intent. Runs read-only aggregations against notes created in
    /// `[since_iso, until_iso)` (UTC), excluding chunk notes so counts are of
    /// distinct items. Never embeds or searches — this is the aggregate a
    /// whole-day question needs, and a top-K semantic search cannot produce.
    ///
    /// `source_context` / `note_type` narrow every aggregation AND the sample, so
    /// a bucket this tool reported can be enumerated by asking again with the
    /// bucket's own name. Without that, a caller handed
    /// `source_context: video_learning, count: 43` had no way to see those 43 —
    /// `search_notes` needs a `query` and then ranks by similarity, which for a
    /// label like "video_learning" returns whatever is topically nearest (a video
    /// *about* learning, from another month) and reads as absence. The tool that
    /// surfaces a count must be able to show the rows behind it.
    ///
    /// ★ A filter that matches NOTHING must not look like an empty window. The
    /// first cut of the drill-down shipped without this and reproduced the bug it
    /// was written to fix: asked for today's videos, the agent guessed
    /// `note_type: "episodic"` alongside `source_context: "video_learning"` —
    /// a reasonable guess, and wrong, because those notes are `source_record` —
    /// got `total: 0`, and told the user no videos had been watched. So a
    /// zero-match filtered call re-runs the aggregations UNFILTERED and returns
    /// the values that DO exist, with a note saying the filter is the likely
    /// fault. Same shape as `list_endpoints` pointing a zero result at
    /// `list_apis`: a deterministic tool's empty answer has to carry the way out
    /// of it, because the caller cannot guess a label it has never been shown.
    /// (2026-09-11)
    /// Hard ceiling on rows sent to a person in one delivery. A runaway guard,
    /// not a display budget — nothing downstream truncates the bulk channel.
    const BULK_DELIVERY_MAX: i64 = 5000;

    /// `full` delivers the COMPLETE matching set with whole note content, for
    /// a person to read — not a model-sized page of snippets.
    pub async fn recall_window(&self, spec: crate::traits::RecallSpec<'_>) -> Result<Value> {
        let crate::traits::RecallSpec {
            since_iso,
            until_iso,
            sample_limit,
            source_context,
            note_type,
            offset,
            full,
        } = spec;
        let filtered = source_context.is_some() || note_type.is_some();
        let (by_type, by_source_context, total) = self
            .window_buckets(since_iso, until_iso, source_context, note_type)
            .await?;

        let window = Self::window_predicate(source_context, note_type);
        // A filtered call is an enumeration rather than a characterisation, so it
        // gets a longer snippet — enough of each note to summarise it without a
        // second lookup.
        // A page for a model gets a snippet; a delivery to a person gets the
        // note. 8000 is a per-note sanity bound, not a display budget — nothing
        // downstream truncates the bulk channel.
        let snippet_len = if full {
            8000
        } else if filtered {
            600
        } else {
            200
        };
        let mut sample_query = neo4rs::query(&format!(
            "{window}
             RETURN COALESCE(n.note_type, 'semantic') AS note_type,
                    COALESCE(n.source_context, '(none)') AS source_context,
                    n.id AS id,
                    toString(n.created_at) AS created_at,
                    left(n.content, {snippet_len}) AS snippet
             ORDER BY n.created_at DESC SKIP $offset LIMIT $limit"
        ))
        .param("since", since_iso)
        .param("until", until_iso)
        .param("offset", offset as i64)
        .param("limit", sample_limit as i64);
        if let Some(sc) = source_context {
            sample_query = sample_query.param("source_context", sc);
        }
        if let Some(nt) = note_type {
            sample_query = sample_query.param("note_type", nt);
        }
        let to_rows = |rows: Vec<neo4rs::Row>| -> Vec<Value> {
            rows.into_iter()
                .map(|row| {
                    json!({
                        "note_type": row.get::<String>("note_type").unwrap_or_default(),
                        "source_context": row.get::<String>("source_context").unwrap_or_default(),
                        "id": row.get::<String>("id").unwrap_or_default(),
                        "created_at": row.get::<String>("created_at").unwrap_or_default(),
                        "snippet": row.get::<String>("snippet").unwrap_or_default(),
                    })
                })
                .collect()
        };
        // Claims, source records and unsourced syntheses carry their epistemic
        // label here exactly as in search: on 2026-09-29 "what did we learn
        // today?" was answered from this sample, unlabelled, and a video's
        // claim ("the launch of America.gov … billions of saved hours") was
        // relayed as something the brain had learned.
        let samples: Vec<Value> = self
            .label_sample_rows(to_rows(self.neo4j.execute(sample_query).await?))
            .await;

        // ── Unconditional delivery of a FILTERED set ────────────────────────
        //
        // A filtered recall means the caller already named what they want, so
        // the complete set is fetched and handed to the PERSON every time,
        // regardless of what the model asked for or noticed. It costs the model
        // nothing — it still gets only its page — and it costs the reader
        // nothing, since the UI keeps it collapsed.
        //
        // It is unconditional because every conditional version failed. A
        // system-prompt rule ("if the user wants to SEE them, pass full:true")
        // and then an in-band hint at the exact decision point were both
        // ignored by the 4B worker, which paged a 12-row window and reported it
        // had "only been able to retrieve the first 12" — and, on the next
        // attempt, that showing all 50 "would be impractical", deciding on the
        // user's behalf not to show them. The intent never survived the trip
        // anyway: the front door paraphrased "I want to see all of what we
        // learned" into "Search all stored notes for information related to…",
        // so the worker could not have known it was a delivery request.
        //
        // Guessing intent from phrasing would just move the guess. Not making
        // it is better: the rows are always THERE, and nobody has to infer that
        // the person who asked about 50 notes might want to look at them.
        let bulk_rows: Vec<Value> = if filtered && !full && (samples.len() as i64) < total {
            let mut q = neo4rs::query(&format!(
                "{window}
                 RETURN COALESCE(n.note_type, 'semantic') AS note_type,
                        COALESCE(n.source_context, '(none)') AS source_context,
                        n.id AS id,
                        toString(n.created_at) AS created_at,
                        left(n.content, 8000) AS snippet
                 ORDER BY n.created_at DESC LIMIT $limit"
            ))
            .param("since", since_iso)
            .param("until", until_iso)
            .param("limit", Self::BULK_DELIVERY_MAX);
            if let Some(sc) = source_context {
                q = q.param("source_context", sc);
            }
            if let Some(nt) = note_type {
                q = q.param("note_type", nt);
            }
            // Never fatal: a failed bulk fetch degrades to the page, which is
            // exactly the old behaviour.
            match self.neo4j.execute(q).await {
                Ok(rows) => self.label_sample_rows(to_rows(rows)).await,
                Err(e) => {
                    tracing::warn!(error = %e, "bulk delivery fetch failed (non-fatal)");
                    Vec::new()
                }
            }
        } else {
            Vec::new()
        };

        // Whether the sample IS the full matching set. A filtered caller is
        // enumerating, and "did I see everything?" is the difference between a
        // correct summary and a false negative, so state it rather than leaving
        // it to be inferred from two numbers.
        //
        // `offset` is part of this: without it, "show me ALL the video-learning
        // notes" had no answer at any setting. Raising sample_limit past what
        // the caller's channel carries does not enumerate more, it just moves
        // where the truncation falls — so the rest has to be reachable by a
        // SECOND call, not a bigger one. See the `next_offset` note below.
        let seen = offset + samples.len();
        let complete = seen as i64 >= total;
        let next_offset = if complete { None } else { Some(seen) };

        let mut out = json!({
            "since": since_iso,
            "until": until_iso,
            "total_notes": total,
            "matched_notes": total,
            "returned": samples.len(),
            "offset": offset,
            "seen_through": seen,
            "next_offset": next_offset,
            "complete": complete,
            "filter": json!({
                "source_context": source_context,
                "note_type": note_type,
            }),
            "by_note_type": by_type,
            "by_source_context": by_source_context,
            "sample": samples,
        });

        // Carried out-of-band under a reserved key. The skill lifts it into the
        // ToolCallResult's `bulk` channel and removes it here, so it never
        // reaches a model's context — see `KnowledgeSkill::lift_bulk_rows`.
        if !bulk_rows.is_empty() {
            out.as_object_mut()
                .expect("object")
                .insert(BULK_ROWS_KEY.into(), json!(bulk_rows));
        }

        let note: String = if filtered && total == 0 {
            // The escape hatch. Re-run unfiltered so the caller is handed the
            // values that actually exist instead of having to guess again.
            let (avail_type, avail_src, window_total) = self
                .window_buckets(since_iso, until_iso, None, None)
                .await?;
            let m = out.as_object_mut().expect("object");
            m.insert("window_total_notes".into(), json!(window_total));
            m.insert("available_note_types".into(), json!(avail_type));
            m.insert("available_source_contexts".into(), json!(avail_src));
            // The most common single mistake, caught deterministically instead
            // of left for the caller to notice: a source_context passed in the
            // note_type slot (or the reverse). Observed 2026-09-12 —
            // `note_type: "video_learning", source_context: "video_learning"`
            // matched nothing, and the worker went on to report that no
            // video-learning notes existed while holding a response whose
            // `available_source_contexts` said `video_learning: 50`. The escape
            // hatch had the answer; naming the swap is what makes it actionable.
            let value_of = |rows: &[Value], key: &str| -> Vec<String> {
                rows.iter()
                    .filter_map(|r| r.get(key).and_then(Value::as_str))
                    .map(str::to_string)
                    .collect()
            };
            let known_types = value_of(&avail_type, "note_type");
            let known_sources = value_of(&avail_src, "source_context");
            let mut swap: Option<String> = None;
            if let Some(nt) = note_type
                && !known_types.iter().any(|k| k == nt)
                && known_sources.iter().any(|k| k == nt)
            {
                swap = Some(format!(
                    "'{nt}' IS NOT A note_type — it is a source_context, and it has notes. \
                     Retry as recall_recent(source_context: \"{nt}\") with NO note_type."
                ));
            } else if let Some(sc) = source_context
                && !known_sources.iter().any(|k| k == sc)
                && known_types.iter().any(|k| k == sc)
            {
                swap = Some(format!(
                    "'{sc}' IS NOT A source_context — it is a note_type. Retry as \
                     recall_recent(note_type: \"{sc}\") with NO source_context."
                ));
            }
            if let Some(ref msg) = swap {
                out.as_object_mut()
                    .expect("object")
                    .insert("filter_error".into(), json!(msg));
            }
            format!(
                "{}NO NOTES MATCHED THIS FILTER — and this is NOT evidence the window is empty: \
                 {window_total} notes exist in it. The filter is the likely fault (a note_type \
                 or source_context that does not pair, e.g. source_context 'video_learning' is \
                 stored as note_type 'source_record' and 'claim', never 'episodic'). \
                 `available_note_types` and `available_source_contexts` list the values that DO \
                 exist here — retry with one of those, or drop one side of the filter. Do NOT \
                 report absence to the user off this result, and do NOT fall back to \
                 search_notes, which ranks by topic and cannot establish absence either.",
                swap.map(|m| format!("{m} ")).unwrap_or_default()
            )
        } else if filtered && complete {
            format!(
                "COMPLETE: all {total} matching notes are here (rows {offset}-{seen}) — \
                 summarise directly. If your copy ends in a TRUNCATED marker it was clipped in \
                 transit: call again with offset set past what you received, do not report the \
                 list as unavailable. Counts exclude chunk fragments.",
                seen = seen,
            )
        } else if filtered {
            // Two different next moves, and which one is right depends on WHO
            // wanted the notes. Naming both here, at the moment of the choice,
            // is the deterministic half — a system-prompt rule alone did not
            // reach a 4B worker, which paged a 12-row window and then told the
            // user it had "only been able to retrieve the first 12".
            format!(
                "PARTIAL: {total} notes match this filter and you have rows {offset}-{seen}. \
                 TWO WAYS ON, pick by who wants them:\n\
                 · The USER asked to SEE the notes → call again with the SAME filter and \
                 `full: true`. That delivers ALL {total} straight to their screen, costs you \
                 no context, and is ONE call. Then say what was delivered — never that you \
                 could only retrieve part of it.\n\
                 · YOU need to reason over them → page with offset={next} until complete=true.\n\
                 Either way do NOT just raise sample_limit: a bigger page does not fit through \
                 the tool-result channel, it only moves where the clipping falls. Cite {total}, \
                 not the number of rows you happen to hold. Counts exclude chunk fragments.",
                seen = seen,
                next = next_offset.unwrap_or(seen),
            )
        } else {
            "Counts exclude chunk fragments; they are distinct notes created in the window. \
             The sample is the most-recent rows, not the whole window — cite it as a sample of \
             the total. To SEE the notes behind any row in by_source_context or by_note_type, \
             call recall_recent again with that source_context / note_type — do NOT semantic-search \
             for the bucket name, which ranks by topic and will miss them. Pass ONE side of the \
             filter first: pairing a source_context with a guessed note_type is the common way to \
             match nothing."
                .to_string()
        };
        out.as_object_mut()
            .expect("object")
            .insert("note".into(), json!(note));

        Ok(out)
    }
}
