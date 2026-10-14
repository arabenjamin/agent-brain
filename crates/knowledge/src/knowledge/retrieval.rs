//! Hybrid retrieval: vector + BM25, RRF merge, freshness boost, graph expansion.

use super::*;

/// Escape Lucene query-syntax special characters so arbitrary query text is treated as
/// literal terms by `db.index.fulltext.queryNodes`. Without this, characters like `:`,
/// `/`, `[`, or an unbalanced `"` (common in a graph full of URLs) make the fulltext
/// call throw and hybrid search silently degrades to the slow CONTAINS fallback.
pub fn sanitize_lucene_query(q: &str) -> String {
    // The Lucene classic-parser reserved set, plus `&`/`|` handled individually so the
    // `&&`/`||` operators are neutralised too.
    const SPECIAL: &[char] = &[
        '\\', '+', '-', '!', '(', ')', ':', '^', '[', ']', '"', '{', '}', '~', '*', '?', '|', '&',
        '/',
    ];
    let mut out = String::with_capacity(q.len() + 8);
    for c in q.chars() {
        if SPECIAL.contains(&c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// Trace key present when the query could not be embedded, so the search
/// was keyword-only. Not a note id.
pub const TRACE_NO_VECTOR: &str = "__no_vector__";

/// Why one note is in a search's results (`search_notes_explained`).
/// Ranks are 1-based positions in each stage's own list.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct MatchTrace {
    /// Final position in the returned list.
    pub rank: usize,
    pub vector_rank: Option<usize>,
    /// Cosine similarity from the vector index.
    pub vector_score: Option<f64>,
    pub bm25_rank: Option<usize>,
    /// Lucene relevance score from the fulltext index.
    pub bm25_score: Option<f64>,
    /// The match was in this chunk of the note (small-to-big resolution).
    pub via_chunk: Option<String>,
    /// Not matched itself but added: `graph`, `entity`, or `substring_fallback`.
    pub added_by: Option<&'static str>,
    /// How many of the query's exact spans (money, dates, codes, quoted text)
    /// the note holds as written; these rank first.
    pub exact: Option<usize>,
}

impl MatchTrace {
    /// A chunk matched; credit its parent, keeping the parent's own better ranks.
    fn credit_parent(t: &mut HashMap<String, MatchTrace>, chunk: &str, parent: &str) {
        let Some(c) = t.get(chunk).cloned() else {
            return;
        };
        let p = t.entry(parent.to_string()).or_default();
        fn best(a: Option<usize>, b: Option<usize>) -> Option<usize> {
            match (a, b) {
                (Some(x), Some(y)) => Some(x.min(y)),
                (x, None) => x,
                (None, y) => y,
            }
        }
        if best(p.vector_rank, c.vector_rank) != p.vector_rank {
            p.vector_rank = c.vector_rank;
            p.vector_score = c.vector_score;
        }
        if best(p.bm25_rank, c.bm25_rank) != p.bm25_rank {
            p.bm25_rank = c.bm25_rank;
            p.bm25_score = c.bm25_score;
        }
        if p.via_chunk.is_none() {
            p.via_chunk = Some(chunk.to_string());
        }
    }
}

impl KnowledgeService {
    /// Reciprocal rank fusion of vector and BM25 result lists.
    /// Returns `(id, content, rrf_score)` triples so callers can apply further re-ranking.
    pub(super) fn rrf_merge(
        vec_hits: Vec<(String, String)>,
        bm25_hits: Vec<(String, String)>,
        k: f64,
        limit: usize,
    ) -> Vec<(String, String, f64)> {
        let mut scores: HashMap<String, f64> = HashMap::new();
        let mut contents: HashMap<String, String> = HashMap::new();

        for (rank, (id, content)) in vec_hits.iter().enumerate() {
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
            contents.insert(id.clone(), content.clone());
        }
        for (rank, (id, content)) in bm25_hits.iter().enumerate() {
            *scores.entry(id.clone()).or_insert(0.0) += 1.0 / (k + rank as f64 + 1.0);
            contents.insert(id.clone(), content.clone());
        }

        let mut ranked: Vec<(String, f64)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);

        ranked
            .into_iter()
            .filter_map(|(id, score)| contents.remove(&id).map(|c| (id, c, score)))
            .collect()
    }

    /// Apply freshness boost to a ranked list.
    /// Boosts notes with higher access counts and more recent access.
    /// Final score = `w * rrf_score + (1 - w) * freshness_score` (capped at 1.0),
    /// where the recency half of `freshness_score` decays as `exp(-days / tau)`.
    ///
    /// `w` and `tau` were hardcoded at `0.7` and `30` — intuition constants that
    /// nothing had ever measured. They are now read from the environment so the
    /// retrieval eval harness can sweep them without a rebuild; **unset leaves
    /// the original values, so production behaviour is byte-identical**:
    /// - `RETRIEVAL_RRF_WEIGHT` (default `0.7`, clamped to `[0,1]`) — `1.0`
    ///   disables the freshness boost entirely (pure RRF).
    /// - `RETRIEVAL_RECENCY_TAU_DAYS` (default `30`, must be `> 0`) — larger
    ///   values decay old notes more gently, so an old-but-relevant note is less
    ///   likely to be buried (the documented self-assessment miss).
    pub(super) async fn apply_freshness_boost(
        &self,
        hits: Vec<(String, String, f64)>,
    ) -> Vec<(String, String)> {
        if hits.is_empty() {
            return Vec::new();
        }

        let rrf_weight = std::env::var("RETRIEVAL_RRF_WEIGHT")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|w| (0.0..=1.0).contains(w))
            .unwrap_or(0.7);
        let recency_tau = std::env::var("RETRIEVAL_RECENCY_TAU_DAYS")
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .filter(|t| *t > 0.0)
            .unwrap_or(30.0);

        let ids: Vec<String> = hits.iter().map(|(id, _, _)| id.clone()).collect();
        let cypher = r#"
        MATCH (n:Note) WHERE n.id IN $ids
        RETURN n.id AS id,
               COALESCE(n.access_count, 0) AS access_count,
               COALESCE(
                   duration.between(datetime(n.last_accessed_at), datetime()).days,
                   30
               ) AS days_since_access
        "#;

        let freshness: HashMap<String, (i64, i64)> = self
            .neo4j
            .execute(neo4rs::query(cypher).param("ids", ids))
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|row| {
                let id = row.get::<String>("id").ok()?;
                let ac = row.get::<i64>("access_count").unwrap_or(0);
                let days = row.get::<i64>("days_since_access").unwrap_or(30);
                Some((id, (ac, days)))
            })
            .collect();

        // Normalise RRF scores to [0,1].
        let max_rrf = hits
            .iter()
            .map(|(_, _, s)| *s)
            .fold(0.0_f64, f64::max)
            .max(1e-9);

        let mut boosted: Vec<(String, String, f64)> = hits
            .into_iter()
            .map(|(id, content, rrf)| {
                let (ac, days) = freshness.get(&id).copied().unwrap_or((0, 30));
                let access_score = ((ac as f64 + 1.0).ln() / (10.0_f64).ln()).min(1.0);
                let recency_score = (-days as f64 / recency_tau).exp();
                let freshness_score = (access_score * 0.5 + recency_score * 0.5).min(1.0);
                let final_score =
                    (rrf / max_rrf) * rrf_weight + freshness_score * (1.0 - rrf_weight);
                (id, content, final_score)
            })
            .collect();

        boosted.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        boosted
            .into_iter()
            .map(|(id, content, _)| (id, content))
            .collect()
    }

    /// Traverse RELATES_TO edges up to `hop_limit` hops from `primary_ids`,
    /// returning (id, content, path_score) for neighbours not already in `primary_ids`.
    pub(super) async fn expand_with_graph_context(
        &self,
        primary_ids: &[String],
        hop_limit: usize,
        per_hop_limit: i64,
    ) -> Result<Vec<(String, String, f64)>> {
        if primary_ids.is_empty() || hop_limit == 0 {
            return Ok(Vec::new());
        }

        let hops = hop_limit.min(3);
        let cypher = format!(
            r#"
            MATCH (n:Note) WHERE n.id IN $primary_ids
            MATCH (n)-[r:RELATES_TO*1..{hops}]->(nb:Note)
            WHERE NOT nb.id IN $primary_ids
            WITH nb.id AS id, nb.content AS content,
                 min(reduce(s = 1.0, rel IN r | s * COALESCE(rel.similarity, 1.0))) AS path_score
            ORDER BY path_score DESC LIMIT $per_hop_limit
            RETURN id, content, path_score
            "#,
            hops = hops
        );

        let query = neo4rs::query(&cypher)
            .param("primary_ids", primary_ids.to_vec())
            .param("per_hop_limit", per_hop_limit);

        let rows = self.neo4j.execute(query).await?;
        let mut results = Vec::new();
        for row in rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let content = row.get::<String>("content").unwrap_or_default();
            let score = row.get::<f64>("path_score").unwrap_or(0.0);
            if !id.is_empty() {
                results.push((id, content, score));
            }
        }

        Ok(results)
    }

    /// Search notes using hybrid BM25 + vector RRF, with optional graph expansion.
    /// Updates spaced-repetition fields on accessed notes.
    /// Pass `entity_expansion = true` to bridge through shared Entity nodes after
    /// the primary results are collected (adds notes that mention the same entities).
    pub async fn search_notes(
        &self,
        query_text: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        self.search_notes_inner(
            query_text,
            limit,
            graph_hops,
            false,
            note_type,
            source_context,
            true,
            None,
        )
        .await
    }

    /// Like `search_notes` but with entity-bridging enabled.
    pub async fn search_notes_with_entity_expansion(
        &self,
        query_text: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        self.search_notes_inner(
            query_text,
            limit,
            graph_hops,
            true,
            note_type,
            source_context,
            true,
            None,
        )
        .await
    }

    /// Like `search_notes` but **non-perturbing**: it does not bump
    /// `access_count`/`last_accessed_at` or advance the spaced-repetition
    /// schedule on the notes it returns.
    ///
    /// This exists for the retrieval eval harness (`services::retrieval_eval`).
    /// A normal search writes the freshness signal that `apply_freshness_boost`
    /// later reads, so an eval loop that called `search_notes` would move the
    /// very ranking input it is trying to measure — every re-run would score a
    /// slightly different graph. Measurement must not have side effects.
    pub async fn search_notes_readonly(
        &self,
        query_text: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> Result<Vec<serde_json::Value>> {
        self.search_notes_inner(
            query_text,
            limit,
            graph_hops,
            false,
            note_type,
            source_context,
            false,
            None,
        )
        .await
    }

    /// Non-perturbing search that also says why each hit is there — the HBI's
    /// semantic search. Same pipeline, same ranking as `search_notes_readonly`;
    /// the trace is recorded alongside and changes nothing.
    pub async fn search_notes_explained(
        &self,
        query_text: &str,
        limit: usize,
        graph_hops: usize,
        note_type: Option<&str>,
    ) -> Result<(Vec<serde_json::Value>, HashMap<String, MatchTrace>)> {
        let mut trace = HashMap::new();
        let hits = self
            .search_notes_inner(
                query_text,
                limit,
                graph_hops,
                false,
                note_type,
                None,
                false,
                Some(&mut trace),
            )
            .await?;
        let keep: std::collections::HashSet<&str> =
            hits.iter().filter_map(|h| h["id"].as_str()).collect();
        trace.retain(|id, _| keep.contains(id.as_str()));
        Ok((hits, trace))
    }

    #[allow(clippy::too_many_arguments)]
    pub(super) async fn search_notes_inner(
        &self,
        query_text: &str,
        limit: usize,
        graph_hops: usize,
        entity_expansion: bool,
        note_type: Option<&str>,
        source_context: Option<&str>,
        track_access: bool,
        mut trace: Option<&mut HashMap<String, MatchTrace>>,
    ) -> Result<Vec<serde_json::Value>> {
        let fetch_limit = (limit * 3).max(10);
        // Carry the full candidate pool (`fetch_limit`) through every re-ranking
        // and expansion stage; cut down to the caller's `limit` only at the very
        // end (after the note_type filter, at the single `merged.truncate(limit)`
        // below).
        //
        // This was previously `limit` unless a note_type filter was active, which
        // truncated the RRF merge to the top-k *before* `apply_freshness_boost`
        // (and chunk→parent resolution) ever ran — so re-ranking could only
        // reorder within the top-k and never lift a rank-15 note into the top-10.
        // The retrieval eval harness measured that as a hard ceiling: recall@10
        // was flat across every freshness weight (even freshness fully off),
        // because the re-rank had nothing below rank-k available to promote,
        // while recall@100 was 1.0 — the target notes were all retrievable, just
        // ranked 26–100. Re-ranking now sees all ~fetch_limit candidates.
        let pool_limit = fetch_limit;

        // 1. Vector search (if LLM available)
        //
        // A failed embed degrades this to BM25-only, which is correct — but the
        // error used to be discarded by the let-chain with no log at all, so a
        // silently half-blind hybrid search looked identical to a healthy one.
        // That invisibility is why the codebase carried a wrong folk diagnosis
        // ("certain strings return NaN") for months: nothing ever showed which
        // inputs actually failed. Probing the endpoint found the real causes —
        // over-length and empty input — both now prevented in
        // `prepare_embed_input`.
        let mut vec_hits: Vec<(String, String)> = Vec::new();
        let query_embedding = match &self.llm {
            Some(llm) => match llm.embed(query_text).await {
                Ok(e) => Some(e),
                Err(e) => {
                    warn!(
                        error = %e,
                        chars = query_text.len(),
                        "Embedding failed — this search is BM25-only (semantic recall degraded)"
                    );
                    None
                }
            },
            None => None,
        };
        if let Some(query_embedding) = query_embedding {
            let cypher = r#"
                CALL db.index.vector.queryNodes('note_embeddings', $limit, $embedding)
                YIELD node, score
                RETURN node.id AS id, node.content AS content, score
                "#;

            let q = neo4rs::query(cypher)
                .param("embedding", query_embedding)
                .param("limit", fetch_limit as i64);

            match self.neo4j.execute(q).await {
                Ok(rows) => {
                    for row in rows {
                        if let (Ok(id), Ok(content)) =
                            (row.get::<String>("id"), row.get::<String>("content"))
                        {
                            if let Some(t) = trace.as_deref_mut() {
                                let e = t.entry(id.clone()).or_default();
                                e.vector_rank = Some(vec_hits.len() + 1);
                                e.vector_score = row.get::<f64>("score").ok();
                            }
                            vec_hits.push((id, content));
                        }
                    }
                }
                Err(e) => warn!("Vector search failed (continuing with BM25): {}", e),
            }
        } else if let Some(t) = trace.as_deref_mut() {
            t.insert(TRACE_NO_VECTOR.to_string(), MatchTrace::default());
        }

        // 2. BM25 full-text search
        let mut bm25_hits: Vec<(String, String)> = Vec::new();
        {
            let cypher = r#"
            CALL db.index.fulltext.queryNodes('note_content_fulltext', $query, {limit: $limit})
            YIELD node, score
            RETURN node.id AS id, node.content AS content, score
            "#;

            // Sanitize: raw query text with Lucene special chars (URLs, colons, brackets)
            // would throw and silently drop BM25 from the hybrid merge.
            let q = neo4rs::query(cypher)
                .param("query", sanitize_lucene_query(query_text))
                .param("limit", fetch_limit as i64);

            match self.neo4j.execute(q).await {
                Ok(rows) => {
                    for row in rows {
                        if let (Ok(id), Ok(content)) =
                            (row.get::<String>("id"), row.get::<String>("content"))
                        {
                            if let Some(t) = trace.as_deref_mut() {
                                let e = t.entry(id.clone()).or_default();
                                e.bm25_rank = Some(bm25_hits.len() + 1);
                                e.bm25_score = row.get::<f64>("score").ok();
                            }
                            bm25_hits.push((id, content));
                        }
                    }
                }
                Err(e) => warn!("BM25 full-text search failed (continuing): {}", e),
            }
        }

        // 3. Merge or fall back to keyword search, then apply freshness boost.
        let mut merged: Vec<(String, String)> = if vec_hits.is_empty() && bm25_hits.is_empty() {
            // Fallback: CONTAINS keyword search (no freshness boost needed — already fast-path)
            let cypher = r#"
            MATCH (n:Note)
            WHERE toLower(n.content) CONTAINS toLower($query)
            RETURN n.id AS id, n.content AS content
            LIMIT $limit
            "#;

            let q = neo4rs::query(cypher)
                .param("query", query_text)
                .param("limit", pool_limit as i64);

            let found: Vec<(String, String)> = self
                .neo4j
                .execute(q)
                .await?
                .into_iter()
                .filter_map(|row| {
                    if let (Ok(id), Ok(content)) =
                        (row.get::<String>("id"), row.get::<String>("content"))
                    {
                        Some((id, content))
                    } else {
                        None
                    }
                })
                .collect();
            if let Some(t) = trace.as_deref_mut() {
                for (id, _) in &found {
                    t.entry(id.clone()).or_default().added_by = Some("substring_fallback");
                }
            }
            found
        } else {
            let rrf_ranked = Self::rrf_merge(vec_hits, bm25_hits, 60.0, pool_limit);
            self.apply_freshness_boost(rrf_ranked).await
        };

        // 3.25 Exact spans: a string the query names as written (a dollar
        // amount, a date, a code, quoted text) is found as a phrase, and the
        // notes holding it rank first. Before chunk resolution, so a chunk
        // that holds it brings its parent.
        let spans = super::exact::ExactPatterns::global().spans(query_text);
        if !spans.is_empty() {
            let mut exact = Vec::with_capacity(spans.len());
            for s in &spans {
                exact.push(self.exact_hits(&s.text, pool_limit).await);
            }
            debug!(
                spans = ?spans.iter().map(|s| format!("{}:{}", s.pattern, s.text)).collect::<Vec<_>>(),
                hits = exact.iter().map(Vec::len).sum::<usize>(),
                "retrieval: exact spans"
            );
            let (m, held) = super::exact::exact_first(merged, exact);
            merged = m;
            if let Some(t) = trace.as_deref_mut() {
                for (id, n) in held {
                    t.entry(id).or_default().exact = Some(n);
                }
            }
            merged.truncate(pool_limit);
        }

        // 3.5 Hybrid Retrieval: Resolve child chunks to parents (Small-to-Big)
        if !merged.is_empty() {
            let hit_ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
            let parent_cypher = r#"
            MATCH (n:Note) WHERE n.id IN $hit_ids
            OPTIONAL MATCH (n)-[:PART_OF]->(p:Note)
            RETURN n.id AS hit_id, n.content AS hit_content, p.id AS parent_id, p.content AS parent_content
            "#;

            if let Ok(rows) = self
                .neo4j
                .execute(neo4rs::query(parent_cypher).param("hit_ids", hit_ids))
                .await
            {
                // Build a lookup of hit_id -> its parent (if any). The query rows
                // come back in Neo4j's arbitrary order, so they must NOT be
                // iterated as the result order — doing so discarded the RRF +
                // freshness ranking entirely and rebuilt the list in storage
                // order. That was invisible while the pool going in equalled the
                // output `limit` (the same members survived a reorder), but once
                // the re-rank pool was widened to `fetch_limit` it truncated to
                // an arbitrary top-k. It was also silently depressing MRR: the
                // top-k order handed to the caller was storage order, not rank.
                let mut parent_of: std::collections::HashMap<String, (String, String)> =
                    std::collections::HashMap::new();
                for row in rows {
                    let Ok(hid) = row.get::<String>("hit_id") else {
                        continue;
                    };
                    if let (Ok(pid), Ok(pcontent)) = (
                        row.get::<String>("parent_id"),
                        row.get::<String>("parent_content"),
                    ) {
                        parent_of.insert(hid, (pid, pcontent));
                    }
                }

                // Walk `merged` in ranked order, replacing each chunk with its
                // parent, and dedup (a parent shared by two chunks collapses to
                // one, keeping the higher-ranked position).
                let mut resolved = Vec::new();
                let mut seen_ids = std::collections::HashSet::new();
                for (id, content) in std::mem::take(&mut merged) {
                    let (rid, rcontent) = match parent_of.get(&id) {
                        Some((pid, pcontent)) => {
                            if let Some(t) = trace.as_deref_mut() {
                                MatchTrace::credit_parent(t, &id, pid);
                            }
                            (pid.clone(), pcontent.clone())
                        }
                        None => (id, content),
                    };
                    if seen_ids.insert(rid.clone()) {
                        resolved.push((rid, rcontent));
                    }
                }
                merged = resolved;
                merged.truncate(pool_limit);
            }
        }

        // 4. Graph expansion
        if graph_hops > 0 && !merged.is_empty() {
            let primary_ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
            match self
                .expand_with_graph_context(&primary_ids, graph_hops, 5)
                .await
            {
                Ok(neighbours) => {
                    let existing_ids: std::collections::HashSet<String> =
                        primary_ids.iter().cloned().collect();
                    for (nb_id, nb_content, _score) in neighbours {
                        if !existing_ids.contains(&nb_id) {
                            if let Some(t) = trace.as_deref_mut() {
                                t.entry(nb_id.clone()).or_default().added_by = Some("graph");
                            }
                            merged.push((nb_id, nb_content));
                        }
                    }
                    merged.truncate(pool_limit);
                }
                Err(e) => warn!("Graph expansion failed: {}", e),
            }
        }

        // 4.5 Entity-bridge expansion: find other notes that mention the same entities.
        if entity_expansion && !merged.is_empty() {
            let primary_ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
            let entity_cypher = r#"
            MATCH (n:Note)-[:MENTIONS]->(e:Entity)<-[:MENTIONS]-(m:Note)
            WHERE n.id IN $primary_ids AND NOT m.id IN $primary_ids
            RETURN DISTINCT m.id AS id, m.content AS content
            LIMIT $limit
            "#;
            let extra_limit = (limit / 2).max(3) as i64;
            if let Ok(rows) = self
                .neo4j
                .execute(
                    neo4rs::query(entity_cypher)
                        .param("primary_ids", primary_ids.clone())
                        .param("limit", extra_limit),
                )
                .await
            {
                let existing: std::collections::HashSet<String> = primary_ids.into_iter().collect();
                for row in rows {
                    if let (Ok(id), Ok(content)) =
                        (row.get::<String>("id"), row.get::<String>("content"))
                        && !existing.contains(&id)
                    {
                        if let Some(t) = trace.as_deref_mut() {
                            t.entry(id.clone()).or_default().added_by = Some("entity");
                        }
                        merged.push((id, content));
                    }
                }
                merged.truncate(pool_limit);
            }
        }

        // 4.75 Type filter: keep only notes of the requested type, then cut the
        // pool down to the caller's limit. Runs before access tracking so
        // spaced-repetition stats are only bumped on notes actually returned.
        if let Some(nt) = note_type {
            let ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
            if !ids.is_empty() {
                let q = neo4rs::query(
                    "MATCH (n:Note) WHERE n.id IN $ids \
                       AND COALESCE(n.note_type, 'semantic') = $nt \
                     RETURN n.id AS id",
                )
                .param("ids", ids)
                .param("nt", nt);
                match self.neo4j.execute(q).await {
                    Ok(rows) => {
                        let keep: std::collections::HashSet<String> = rows
                            .into_iter()
                            .filter_map(|r| r.get::<String>("id").ok())
                            .collect();
                        merged.retain(|(id, _)| keep.contains(id));
                    }
                    Err(e) => warn!("note_type filter query failed: {}", e),
                }
            }
        }

        // 4.76 Source-context filter: same shape as the note_type filter. Vector +
        // BM25 both rank purely on content, so a `source_context` restriction (e.g.
        // the job-applicant profile grounding on `source_context:"resume"`) is only
        // honoured if applied here — without it the semantic search silently
        // returned unrelated notes (a Paxos knowledge note under a resume query).
        // No COALESCE default: filtering by source_context means ONLY that source.
        if let Some(sc) = source_context.map(str::trim).filter(|s| !s.is_empty()) {
            let ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
            if !ids.is_empty() {
                let q = neo4rs::query(
                    "MATCH (n:Note) WHERE n.id IN $ids \
                       AND n.source_context = $sc \
                     RETURN n.id AS id",
                )
                .param("ids", ids)
                .param("sc", sc);
                match self.neo4j.execute(q).await {
                    Ok(rows) => {
                        let keep: std::collections::HashSet<String> = rows
                            .into_iter()
                            .filter_map(|r| r.get::<String>("id").ok())
                            .collect();
                        merged.retain(|(id, _)| keep.contains(id));
                    }
                    Err(e) => warn!("source_context filter query failed: {}", e),
                }
            }
        }
        // 4.77 Agent exclusion: an agent whose profile sets `retrieval_exclude`
        // never sees those notes (see `RetrievalExclude`). Fails open: if the
        // lookup errors, nothing is dropped.
        if let Some(ex) = current_retrieval_exclude() {
            let ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
            if !ids.is_empty() {
                let q = neo4rs::query(
                    "MATCH (n:Note) WHERE n.id IN $ids \
                       AND (COALESCE(n.note_type, 'semantic') IN $types \
                            OR COALESCE(n.source_context, '') IN $sources) \
                     RETURN n.id AS id",
                )
                .param("ids", ids)
                .param("types", ex.note_types.clone())
                .param("sources", ex.source_contexts.clone());
                match self.neo4j.execute(q).await {
                    Ok(rows) => {
                        let drop: std::collections::HashSet<String> = rows
                            .into_iter()
                            .filter_map(|r| r.get::<String>("id").ok())
                            .collect();
                        if !drop.is_empty() {
                            info!(
                                excluded = drop.len(),
                                "retrieval: dropped notes the running agent may not see"
                            );
                        }
                        merged.retain(|(id, _)| !drop.contains(id));
                    }
                    Err(e) => warn!("retrieval exclusion query failed: {}", e),
                }
            }
        }
        merged.truncate(limit);
        if let Some(t) = trace {
            for (i, (id, _)) in merged.iter().enumerate() {
                t.entry(id.clone()).or_default().rank = i + 1;
            }
        }

        // Label claims before they leave retrieval. This is the second of two
        // retrieval paths (the other is search_notes_with_ids, feeding reason's
        // internal RAG); both must label, because a claim that reaches a context
        // window unlabelled is indistinguishable from an established fact.
        let merged = self.label_claims(merged).await;

        // 5. Access tracking with spaced-repetition update.
        // Skipped when `track_access` is false (the eval harness reads without
        // perturbing the freshness signal — see `search_notes_readonly`).
        let hit_ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
        if track_access && !hit_ids.is_empty() {
            let now = Utc::now().to_rfc3339();
            let update_cypher = r#"
            MATCH (n:Note) WHERE n.id IN $ids
            WITH n,
                 CASE WHEN n.review_interval_days IS NULL THEN 1
                      ELSE toInteger(n.review_interval_days * 2) END AS new_interval
            SET n.access_count = n.access_count + 1,
                n.last_accessed_at = datetime($now),
                n.review_interval_days = new_interval,
                n.next_review_at = datetime($now) + duration({days: new_interval})
            "#;
            let _ = self
                .neo4j
                .run(
                    neo4rs::query(update_cypher)
                        .param("ids", hit_ids)
                        .param("now", now),
                )
                .await;
        }

        Ok(merged
            .into_iter()
            .map(|(id, content)| serde_json::json!({ "id": id, "content": content }))
            .collect())
    }

    /// Search notes returning (id, content) pairs — used internally for reasoning tools.
    pub(super) async fn search_notes_with_ids(
        &self,
        query_text: &str,
        limit: usize,
        graph_hops: usize,
    ) -> Result<Vec<(String, String)>> {
        let fetch_limit = (limit * 3).max(10);

        let mut vec_hits: Vec<(String, String)> = Vec::new();
        if let Some(llm) = &self.llm
            && let Ok(query_embedding) = llm.embed(query_text).await
        {
            let cypher = r#"
                CALL db.index.vector.queryNodes('note_embeddings', $limit, $embedding)
                YIELD node, score
                RETURN node.id AS id, node.content AS content
                "#;
            let q = neo4rs::query(cypher)
                .param("embedding", query_embedding)
                .param("limit", fetch_limit as i64);
            match self.neo4j.execute(q).await {
                Ok(rows) => {
                    for row in rows {
                        if let (Ok(id), Ok(content)) =
                            (row.get::<String>("id"), row.get::<String>("content"))
                        {
                            vec_hits.push((id, content));
                        }
                    }
                }
                Err(e) => warn!("Vector search failed (continuing with BM25): {}", e),
            }
        }

        let mut bm25_hits: Vec<(String, String)> = Vec::new();
        {
            let cypher = r#"
            CALL db.index.fulltext.queryNodes('note_content_fulltext', $query, {limit: $limit})
            YIELD node, score
            RETURN node.id AS id, node.content AS content
            "#;
            let q = neo4rs::query(cypher)
                .param("query", sanitize_lucene_query(query_text))
                .param("limit", fetch_limit as i64);
            match self.neo4j.execute(q).await {
                Ok(rows) => {
                    for row in rows {
                        if let (Ok(id), Ok(content)) =
                            (row.get::<String>("id"), row.get::<String>("content"))
                        {
                            bm25_hits.push((id, content));
                        }
                    }
                }
                Err(e) => warn!("BM25 full-text search failed (continuing): {}", e),
            }
        }

        let mut merged: Vec<(String, String)> = if vec_hits.is_empty() && bm25_hits.is_empty() {
            let cypher = r#"
            MATCH (n:Note)
            WHERE toLower(n.content) CONTAINS toLower($query)
            RETURN n.id AS id, n.content AS content
            LIMIT $limit
            "#;
            let q = neo4rs::query(cypher)
                .param("query", query_text)
                .param("limit", limit as i64);
            self.neo4j
                .execute(q)
                .await?
                .into_iter()
                .filter_map(|row| {
                    if let (Ok(id), Ok(content)) =
                        (row.get::<String>("id"), row.get::<String>("content"))
                    {
                        Some((id, content))
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            Self::rrf_merge(vec_hits, bm25_hits, 60.0, limit)
                .into_iter()
                .map(|(id, content, _score)| (id, content))
                .collect()
        };

        if graph_hops > 0 && !merged.is_empty() {
            let primary_ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
            if let Ok(neighbours) = self
                .expand_with_graph_context(&primary_ids, graph_hops, 5)
                .await
            {
                let existing_ids: std::collections::HashSet<String> =
                    primary_ids.iter().cloned().collect();
                for (nb_id, nb_content, _score) in neighbours {
                    if !existing_ids.contains(&nb_id) {
                        merged.push((nb_id, nb_content));
                    }
                }
                merged.truncate(limit);
            }
        }

        let merged = self.label_claims(merged).await;
        Ok(merged)
    }
}

#[cfg(test)]
mod lucene_tests {
    use super::{KnowledgeService, sanitize_lucene_query};

    #[test]
    fn a_matched_chunk_credits_its_parent_without_hiding_the_parents_own_ranks() {
        use super::MatchTrace;
        use std::collections::HashMap;
        let mut t: HashMap<String, MatchTrace> = HashMap::new();
        t.insert(
            "chunk".into(),
            MatchTrace {
                vector_rank: Some(2),
                vector_score: Some(0.8),
                bm25_rank: Some(9),
                ..Default::default()
            },
        );
        // The parent matched BM25 better on its own, vector not at all.
        t.insert(
            "parent".into(),
            MatchTrace {
                bm25_rank: Some(3),
                bm25_score: Some(5.0),
                ..Default::default()
            },
        );
        MatchTrace::credit_parent(&mut t, "chunk", "parent");
        let p = &t["parent"];
        assert_eq!(
            (p.vector_rank, p.vector_score),
            (Some(2), Some(0.8)),
            "takes the chunk's vector match"
        );
        assert_eq!(
            (p.bm25_rank, p.bm25_score),
            (Some(3), Some(5.0)),
            "keeps its own better BM25"
        );
        assert_eq!(p.via_chunk.as_deref(), Some("chunk"));
    }

    #[test]
    fn escapes_url_and_colon() {
        let out = sanitize_lucene_query("https://example.com/path: [test]");
        assert_eq!(out, "https\\:\\/\\/example.com\\/path\\: \\[test\\]");
    }

    #[test]
    fn escapes_boolean_operators_and_quotes() {
        assert_eq!(sanitize_lucene_query("a && b"), "a \\&\\& b");
        assert_eq!(sanitize_lucene_query("say \"hi\""), "say \\\"hi\\\"");
    }

    #[test]
    fn plain_text_unchanged() {
        assert_eq!(
            sanitize_lucene_query("rust async runtime"),
            "rust async runtime"
        );
    }

    /// The filter has to live in ONE predicate shared by every aggregation and
    /// the sample. If a filter reached the counts but not the sample (or vice
    /// versa) the tool would report a count it then failed to enumerate — the
    /// exact shape of the 2026-09-11 false negative, rebuilt inside the fix.
    #[test]
    fn window_predicate_applies_each_filter_to_the_shared_predicate() {
        let none = KnowledgeService::window_predicate(None, None);
        assert!(!none.contains("$source_context"));
        assert!(!none.contains("$note_type"));
        assert!(
            none.contains("NOT (n)-[:PART_OF]->()"),
            "chunks always excluded"
        );

        let by_src = KnowledgeService::window_predicate(Some("video_learning"), None);
        assert!(by_src.contains("COALESCE(n.source_context, '(none)') = $source_context"));
        assert!(!by_src.contains("$note_type"));

        let both =
            KnowledgeService::window_predicate(Some("video_learning"), Some("source_record"));
        assert!(both.contains("$source_context") && both.contains("$note_type"));

        // The COALESCE'd form is load-bearing: it lets a name copied out of a
        // by_source_context row — including the literal "(none)" bucket — round
        // trip back as a filter instead of silently matching nothing.
        assert!(
            both.contains("COALESCE(n.note_type, 'semantic') = $note_type"),
            "note_type filter must match the value the counts are grouped by"
        );
    }
}
