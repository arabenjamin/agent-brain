//! Clustered memory consolidation (see `CONSOLIDATION_SOURCE_PREDICATE`).

use super::*;

/// Which notes consolidation may merge, as a Cypher predicate on `n`. Shared
/// with the scheduler's "many notes due" trigger, which must count exactly the
/// notes consolidation can consume — a note counted but refused stays due
/// forever and keeps re-triggering consolidations (it had drifted: the trigger
/// counted five types consolidation refused).
///
/// Only **semantic** notes, and not housekeeping ones. Episodic notes are now
/// almost all event logs (`scheduler_dispatch`, `claim_evidence`, chat turns,
/// todo reviews), and folding them in is how a chat answer's "127 notes today"
/// became long-term knowledge. `resume` is personal data and is never merged.
/// A `source_context` that is a JSON blob is a writer bug, not a topic.
///
/// Semantic-only also keeps out every type that must never be merged:
/// `claim` / `source_record` / `unsourced_synthesis` (a summary would strip
/// their epistemic label and launder them into settled knowledge — 71
/// consolidated notes absorbed uncited gap notes before that was filtered),
/// and LLM abstractions (`consolidated`, `reflection`, `inference`), which
/// re-fed drift further from their sources each cycle.
pub(crate) const CONSOLIDATION_SOURCE_PREDICATE: &str = "\
    coalesce(n.note_type, 'semantic') = 'semantic' \
    AND NOT coalesce(n.source_context, '') IN ['adversarial_review', 'codebase_self_analysis', \
        'todo_snapshot', 'brain_api_sync', 'resume', 'chat_session', 'scheduler_dispatch', \
        'overdue_todo_review'] \
    AND NOT coalesce(n.source_context, '') STARTS WITH '{' \
    AND NOT (n)-[:PART_OF]->() \
    AND NOT (n)-[:SUMMARIZED_BY]->()";

/// A neighbour joins a seed's cluster only at or above this cosine similarity.
/// Measured on bge-m3 (2026-10-01): shared `## Gap synthesis:` boilerplate puts
/// hundreds of unrelated notes above 0.75; at 0.82 a seed's cluster stays on
/// its subject (US–Canada tariffs: trade volume, retaliation, lumber, tourism).
pub(super) const CLUSTER_MIN_SIMILARITY: f64 = 0.82;

/// A consolidation needs the seed plus at least this many neighbours.
pub(super) const CLUSTER_MIN_NEIGHBOURS: usize = 2;

/// The most notes one consolidation merges.
pub(super) const CLUSTER_MAX_NOTES: usize = 8;

/// Seeds tried per call before giving up for this cycle.
pub(super) const CLUSTER_MAX_SEEDS: usize = 6;

impl KnowledgeService {
    /// Consolidate a set of notes on a topic into a single summary note via LLM.
    pub async fn consolidate_memories(
        &self,
        topic: &str,
        limit: usize,
    ) -> Result<(String, usize, String)> {
        let llm = self.llm.as_ref().ok_or_else(|| {
            anyhow::anyhow!("LLM is required for memory consolidation but is not configured")
        })?;

        // 1. Fetch top-N raw notes, excluding already-consolidated and reflection notes.
        // Consolidated notes must not be re-used as consolidation sources: they are abstract
        // LLM output, not observations, and re-feeding them produces an abstraction feedback
        // loop that drifts further from reality with each cycle.
        let embedding = {
            let llm = self.llm.as_ref().ok_or_else(|| {
                anyhow::anyhow!("LLM required for consolidation embedding but is not configured")
            })?;
            llm.embed(topic)
                .await
                .map_err(|e| anyhow::anyhow!("Failed to embed consolidation topic: {}", e))?
        };

        // Cluster first, then summarise. The old selection took the top-K due
        // notes by similarity to a fixed generic topic ("recent experiences and
        // knowledge") and summarised them as one record, so every consolidation
        // was a grab-bag (2026-10-01 review: a Hugging Face attack, birds of prey
        // and a film scene in one "knowledge" paragraph). Now: take the due
        // eligible note nearest the topic as a SEED, gather its close neighbours
        // from all eligible notes, and consolidate only a real cluster. A seed
        // with too few neighbours is deferred +30d so the next seed gets a turn
        // rather than the same loner being retried every cycle.
        let embed_dim = embedding.len() as i64;
        let max_notes = limit.clamp(CLUSTER_MIN_NEIGHBOURS + 1, CLUSTER_MAX_NOTES);
        let seed_cypher = format!(
            "MATCH (n:Note) \
             WHERE {CONSOLIDATION_SOURCE_PREDICATE} \
               AND n.next_review_at IS NOT NULL AND n.next_review_at <= datetime() \
               AND n.embedding IS NOT NULL AND size(n.embedding) = $dim \
             WITH n, vector.similarity.cosine(n.embedding, $embedding) AS score \
             ORDER BY score DESC LIMIT $seeds \
             RETURN n.id AS id"
        );
        let seed_rows = self
            .neo4j
            .execute(
                neo4rs::query(&seed_cypher)
                    .param("embedding", embedding)
                    .param("dim", embed_dim)
                    .param("seeds", CLUSTER_MAX_SEEDS as i64),
            )
            .await?;
        let seeds: Vec<String> = seed_rows
            .into_iter()
            .filter_map(|r| r.get::<String>("id").ok())
            .collect();

        let cluster_cypher = format!(
            "MATCH (s:Note {{id: $seed}}) \
             MATCH (n:Note) \
             WHERE n = s OR ({CONSOLIDATION_SOURCE_PREDICATE} \
               AND n.embedding IS NOT NULL AND size(n.embedding) = size(s.embedding)) \
             WITH s, n, CASE WHEN n = s THEN 2.0 \
                  ELSE vector.similarity.cosine(n.embedding, s.embedding) END AS sim \
             WHERE sim >= $min_sim \
             RETURN n.id AS id, n.content AS content, sim \
             ORDER BY sim DESC LIMIT $max"
        );
        let mut notes: Vec<(String, String)> = Vec::new();
        let mut loners: Vec<String> = Vec::new();
        for seed in &seeds {
            let rows = self
                .neo4j
                .execute(
                    neo4rs::query(&cluster_cypher)
                        .param("seed", seed.clone())
                        .param("min_sim", CLUSTER_MIN_SIMILARITY)
                        .param("max", max_notes as i64),
                )
                .await?;
            let mut cluster: Vec<(String, String)> = Vec::new();
            for row in rows {
                if let (Ok(id), Ok(content)) =
                    (row.get::<String>("id"), row.get::<String>("content"))
                    && !cluster.iter().any(|(_, c)| *c == content)
                {
                    cluster.push((id, content));
                }
            }
            if cluster.len() > CLUSTER_MIN_NEIGHBOURS {
                notes = cluster;
                break;
            }
            loners.push(seed.clone());
        }

        // A loner has been looked at and has nothing to merge with yet: push its
        // review out so it stops being the seed every cycle.
        if !loners.is_empty() {
            let bump = "MATCH (n:Note) WHERE n.id IN $ids \
                        SET n.next_review_at = datetime() + duration({days: 30})";
            if let Err(e) = self
                .neo4j
                .run(neo4rs::query(bump).param("ids", loners.clone()))
                .await
            {
                tracing::warn!("Failed to defer unclustered consolidation seeds: {e}");
            }
        }

        if notes.is_empty() {
            // "Nothing to merge" is a normal steady state, not a failure: return a
            // no-op success so chain callers (the bedtime chain) carry on.
            return Ok((
                String::new(),
                0,
                format!(
                    "No cluster to consolidate on topic '{}': {} seed(s) checked, none had \
                     {} or more neighbours at similarity >= {CLUSTER_MIN_SIMILARITY}; they \
                     were deferred 30 days.",
                    topic,
                    loners.len(),
                    CLUSTER_MIN_NEIGHBOURS
                ),
            ));
        }

        let source_ids: Vec<String> = notes.iter().map(|(id, _)| id.clone()).collect();
        let source_count = notes.len();

        // 2. Build consolidation prompt
        let notes_block = notes
            .iter()
            .enumerate()
            .map(|(i, (_, c))| format!("[Memory {}]\n{}", i + 1, c))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let prompt = format!(
            "You are a memory consolidation system. Respond in English only.\n\
             The following {} memory records were grouped because they are about the \
             same subject as [Memory 1]. Write a factual, plain-prose summary of what \
             they say about that subject (context: '{}'). If a record turns out not to \
             be about the subject of [Memory 1], leave it out entirely. \
             Report only what is directly stated in the source records: what happened, \
             what was built or changed, what errors occurred, what was learned. \
             Do NOT add recommendations, analogies, frameworks, or analysis that is not in \
             the source material. Do NOT use headers, bullet points, emoji, or section labels. \
             Do NOT echo the [Memory N] input labels. Write 2-4 concise paragraphs.\n\n{}",
            source_count, topic, notes_block
        );

        // 4. Auto-snapshot before LLM call (best-effort, never fails the operation).
        if self.auto_snapshot
            && let Some(svc) = &self.snapshot_svc
        {
            match svc.take_snapshot(Some("pre_consolidate")).await {
                Ok((path, _)) => {
                    info!(path = %path.display(), "Pre-consolidation snapshot taken")
                }
                Err(e) => warn!("Pre-consolidation snapshot failed (continuing): {}", e),
            }
        }

        // 5. Generate consolidated content
        let consolidated_content = llm
            .generate(&prompt, None)
            .await
            .map_err(|e| anyhow::anyhow!("LLM generation failed: {}", e))?;

        // 6. Persist via store_note_raw (skip chunking + entity extraction for consolidated notes)
        let (consolidated_id, _) = self
            .store_note_raw(
                &consolidated_content,
                Some("consolidated"),
                None,
                None,
                Some(ProvenanceFlag::SynthesisInference),
            )
            .await?;

        // 7. Create SUMMARIZED_BY relationships from source notes to consolidated note
        if !source_ids.is_empty() {
            let link_cypher = r#"
            MATCH (src:Note) WHERE src.id IN $source_ids
            MATCH (dst:Note {id: $consolidated_id})
            MERGE (src)-[:SUMMARIZED_BY]->(dst)
            "#;

            let link_query = neo4rs::query(link_cypher)
                .param("source_ids", source_ids.clone())
                .param("consolidated_id", consolidated_id.clone());

            self.neo4j.run(link_query).await?;

            // 8. Reset spaced-rep schedule on source notes so they no longer appear "overdue"
            //    and don't trigger another consolidation cycle immediately.
            let reset_cypher = "MATCH (n:Note) WHERE n.id IN $ids \
                SET n.next_review_at = datetime() + duration({days: 30}), \
                    n.review_interval_days = 30";
            if let Err(e) = self
                .neo4j
                .run(neo4rs::query(reset_cypher).param("ids", source_ids))
                .await
            {
                tracing::warn!("Failed to reset next_review_at on consolidated source notes: {e}");
            }
        }

        let preview: String = consolidated_content.chars().take(200).collect();

        Ok((consolidated_id, source_count, preview))
    }
}
