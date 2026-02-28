//! Knowledge Service - Manages Notes, Projects, and Embeddings.

use std::collections::HashMap;

use anyhow::Result;
use tracing::{debug, info, warn};
use uuid::Uuid;
use chrono::Utc;
use serde_json::Value;

use crate::repository::Neo4jClient;
use crate::services::LlmClient;

/// Content length above which we attempt to chunk into sub-notes.
const CHUNK_THRESHOLD_CHARS: usize = 1500;

/// Service for managing general knowledge (RAG).
pub struct KnowledgeService {
    pub(crate) neo4j: Neo4jClient,
    pub(crate) llm: Option<LlmClient>,
}

impl KnowledgeService {
    /// Create a new knowledge service.
    pub fn new(neo4j: Neo4jClient, llm: Option<LlmClient>) -> Self {
        Self { neo4j, llm }
    }

    // =========================================================================
    // Private helpers
    // =========================================================================

    /// Core CREATE logic: embed content, persist the Note node, return (id, embedding).
    /// Does NOT call link_similar_notes or extract_entities — the caller handles that.
    async fn store_note_raw(
        &self,
        content: &str,
        note_type: Option<&str>,
        source_context: Option<&str>,
        event_at: Option<&str>,
    ) -> Result<(String, Option<Vec<f32>>)> {
        let note_id = Uuid::new_v4().to_string();
        let timestamp = Utc::now().to_rfc3339();
        let nt = note_type.unwrap_or("semantic");

        let embedding = if let Some(llm) = &self.llm {
            debug!("Generating embedding for note…");
            match llm.embeddings(content).await {
                Ok(emb) => Some(emb),
                Err(e) => {
                    info!("Failed to generate embedding: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let cypher = if embedding.is_some() {
            r#"
            CREATE (n:Note {
                id: $id,
                content: $content,
                note_type: $note_type,
                created_at: datetime($timestamp),
                last_accessed_at: datetime($timestamp),
                access_count: 0,
                next_review_at: datetime($timestamp) + duration({days: 1}),
                review_interval_days: 1,
                embedding: $embedding
            })
            "#
        } else {
            r#"
            CREATE (n:Note {
                id: $id,
                content: $content,
                note_type: $note_type,
                created_at: datetime($timestamp),
                last_accessed_at: datetime($timestamp),
                access_count: 0,
                next_review_at: datetime($timestamp) + duration({days: 1}),
                review_interval_days: 1
            })
            "#
        };

        let mut query = neo4rs::query(cypher)
            .param("id", note_id.clone())
            .param("content", content)
            .param("note_type", nt)
            .param("timestamp", timestamp);

        if let Some(ref emb) = embedding {
            query = query.param("embedding", emb.clone());
        }

        self.neo4j.run(query).await?;

        // Optional fields written in separate SET queries to keep the CREATE clean.
        if let Some(sc) = source_context {
            let q = neo4rs::query(
                "MATCH (n:Note {id: $id}) SET n.source_context = $sc",
            )
            .param("id", note_id.clone())
            .param("sc", sc);
            let _ = self.neo4j.run(q).await;
        }

        if let Some(ea) = event_at {
            let q = neo4rs::query(
                "MATCH (n:Note {id: $id}) SET n.event_at = $ea",
            )
            .param("id", note_id.clone())
            .param("ea", ea);
            let _ = self.neo4j.run(q).await;
        }

        Ok((note_id, embedding))
    }

    /// Auto-link a note to similar existing notes via RELATES_TO edges.
    async fn link_similar_notes(&self, note_id: &str, embedding: &[f32]) -> Result<usize> {
        let cypher = r#"
        CALL db.index.vector.queryNodes('note_embeddings', 10, $embedding)
        YIELD node AS other, score
        WHERE other.id <> $note_id AND score >= 0.75
        MATCH (n:Note {id: $note_id})
        MERGE (n)-[r:RELATES_TO]->(other)
        ON CREATE SET r.similarity = score
        RETURN count(r) AS created
        "#;

        let query = neo4rs::query(cypher)
            .param("embedding", embedding.to_vec())
            .param("note_id", note_id);

        let rows = self.neo4j.execute(query).await?;
        let created = rows
            .first()
            .and_then(|row| row.get::<i64>("created").ok())
            .unwrap_or(0) as usize;

        Ok(created)
    }

    /// Ask the LLM to split long content into self-contained sub-concepts.
    async fn maybe_chunk_content(
        &self,
        content: &str,
        llm: &LlmClient,
    ) -> Result<Option<Vec<String>>> {
        let prompt = format!(
            "Split the following text into 2-5 self-contained sub-concepts separated by '---'. \
             Each chunk must be independently meaningful. Output chunks only:\n\n{}",
            content
        );

        let response = llm.generate(&prompt).await?;
        let chunks: Vec<String> = response
            .text
            .split("---")
            .map(|s| s.trim().to_string())
            .filter(|s| s.len() >= 100)
            .collect();

        if chunks.len() <= 1 {
            Ok(None)
        } else {
            Ok(Some(chunks))
        }
    }

    /// Reciprocal rank fusion of vector and BM25 result lists.
    /// Returns `(id, content, rrf_score)` triples so callers can apply further re-ranking.
    fn rrf_merge(
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
    /// Final score = 0.7 * rrf_score + 0.3 * freshness_score (capped at 1.0).
    async fn apply_freshness_boost(
        &self,
        hits: Vec<(String, String, f64)>,
    ) -> Vec<(String, String)> {
        if hits.is_empty() {
            return Vec::new();
        }

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

        let freshness: HashMap<String, (i64, i64)> = self.neo4j
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
        let max_rrf = hits.iter().map(|(_, _, s)| *s).fold(0.0_f64, f64::max).max(1e-9);

        let mut boosted: Vec<(String, String, f64)> = hits.into_iter().map(|(id, content, rrf)| {
            let (ac, days) = freshness.get(&id).copied().unwrap_or((0, 30));
            let access_score = ((ac as f64 + 1.0).ln() / (10.0_f64).ln()).min(1.0);
            let recency_score = (-days as f64 / 30.0).exp();
            let freshness_score = (access_score * 0.5 + recency_score * 0.5).min(1.0);
            let final_score = (rrf / max_rrf) * 0.7 + freshness_score * 0.3;
            (id, content, final_score)
        }).collect();

        boosted.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        boosted.into_iter().map(|(id, content, _)| (id, content)).collect()
    }

    /// Traverse RELATES_TO edges up to `hop_limit` hops from `primary_ids`,
    /// returning (id, content, path_score) for neighbours not already in `primary_ids`.
    async fn expand_with_graph_context(
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

    /// Extract named entities from note content and persist them to the graph.
    async fn extract_entities(&self, note_id: &str, content: &str, llm: &LlmClient) -> Result<usize> {
        let prompt = format!(
            "Extract named entities (APIs, technologies, organisations, concepts, people). \
             Output only JSON: [{{\"name\":\"rust\",\"type\":\"technology\"}},...] or []\n\nTEXT:\n{}",
            content
        );

        let response = match llm.generate(&prompt).await {
            Ok(r) => r,
            Err(e) => {
                warn!("Entity extraction LLM call failed: {}", e);
                return Ok(0);
            }
        };

        let text = response.text.trim();
        let json_start = text.find('[').unwrap_or(0);
        let json_end = text.rfind(']').map(|i| i + 1).unwrap_or(text.len());
        let json_str = &text[json_start..json_end];

        let entities: Vec<Value> = match serde_json::from_str(json_str) {
            Ok(v) => v,
            Err(e) => {
                warn!("Failed to parse entity JSON: {}", e);
                return Ok(0);
            }
        };

        let mut count = 0usize;
        let timestamp = Utc::now().to_rfc3339();

        for entity in &entities {
            let name = match entity.get("name").and_then(|v| v.as_str()) {
                Some(n) if !n.trim().is_empty() => n.trim().to_lowercase(),
                _ => continue,
            };
            let entity_type = entity
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();
            let entity_id = Uuid::new_v4().to_string();

            let cypher = r#"
            MERGE (e:Entity {name: $name})
            ON CREATE SET e.id = $entity_id, e.entity_type = $entity_type, e.created_at = datetime($ts)
            WITH e
            MATCH (n:Note {id: $note_id})
            MERGE (n)-[r:MENTIONS]->(e)
            ON CREATE SET r.count = 1
            ON MATCH SET r.count = r.count + 1
            "#;

            match self.neo4j.run(
                neo4rs::query(cypher)
                    .param("name", name.clone())
                    .param("entity_id", entity_id)
                    .param("entity_type", entity_type)
                    .param("ts", timestamp.clone())
                    .param("note_id", note_id),
            ).await {
                Ok(_) => count += 1,
                Err(e) => warn!("Failed to store entity '{}': {}", name, e),
            }
        }

        Ok(count)
    }

    // =========================================================================
    // Public API
    // =========================================================================

    /// Store a note with optional metadata. Chunks long content automatically.
    pub async fn store_note(
        &self,
        content: &str,
        note_type: Option<&str>,
        source_context: Option<&str>,
        event_at: Option<&str>,
    ) -> Result<(String, usize)> {
        // Attempt semantic chunking for long content (skip for consolidated/reflection types).
        let skip_chunk = matches!(note_type, Some("consolidated") | Some("reflection"));

        if !skip_chunk && content.len() > CHUNK_THRESHOLD_CHARS {
            if let Some(llm) = &self.llm {
                match self.maybe_chunk_content(content, llm).await {
                    Ok(Some(chunks)) if chunks.len() > 1 => {
                        info!(chunks = chunks.len(), "Chunking long note into sub-notes");

                        // Create parent note
                        let (parent_id, parent_emb) =
                            self.store_note_raw(content, note_type, source_context, event_at).await?;

                        let mut total_links = if let Some(ref emb) = parent_emb {
                            self.link_similar_notes(&parent_id, emb).await.unwrap_or(0)
                        } else {
                            0
                        };

                        // Create child chunk notes
                        for chunk in &chunks {
                            let (child_id, child_emb) =
                                self.store_note_raw(chunk, note_type, source_context, event_at).await?;

                            // Link child to parent
                            let link_q = neo4rs::query(
                                "MATCH (child:Note {id: $child_id}), (parent:Note {id: $parent_id}) \
                                 MERGE (child)-[:PART_OF]->(parent)",
                            )
                            .param("child_id", child_id.clone())
                            .param("parent_id", parent_id.clone());
                            let _ = self.neo4j.run(link_q).await;

                            if let Some(ref emb) = child_emb {
                                total_links +=
                                    self.link_similar_notes(&child_id, emb).await.unwrap_or(0);
                            }
                        }

                        // Entity extraction on the full parent content
                        if let Some(llm) = &self.llm {
                            let _ = self.extract_entities(&parent_id, content, llm).await;
                        }

                        return Ok((parent_id, total_links));
                    }
                    Err(e) => warn!("Chunking failed, storing as single note: {}", e),
                    _ => {}
                }
            }
        }

        // Normal (non-chunked) path
        let (note_id, embedding) =
            self.store_note_raw(content, note_type, source_context, event_at).await?;

        let links_created = if let Some(ref emb) = embedding {
            self.link_similar_notes(&note_id, emb).await.unwrap_or_else(|e| {
                warn!("Failed to link similar notes: {}", e);
                0
            })
        } else {
            0
        };

        if let Some(llm) = &self.llm {
            let _ = self.extract_entities(&note_id, content, llm).await;
        }

        Ok((note_id, links_created))
    }

    /// Find notes related to a given note via RELATES_TO edges.
    pub async fn find_related_notes(&self, note_id: &str) -> Result<Vec<(String, f64)>> {
        let cypher = r#"
        MATCH (n:Note {id: $note_id})-[r:RELATES_TO]->(m:Note)
        RETURN m.content AS content, r.similarity AS score
        ORDER BY r.similarity DESC
        LIMIT 10
        "#;

        let query = neo4rs::query(cypher).param("note_id", note_id);
        let rows = self.neo4j.execute(query).await?;

        let mut results = Vec::new();
        for row in rows {
            let content = row.get::<String>("content").unwrap_or_default();
            let score = row.get::<f64>("score").unwrap_or(0.0);
            results.push((content, score));
        }

        Ok(results)
    }

    /// Search notes using hybrid BM25 + vector RRF, with optional graph expansion.
    /// Updates spaced-repetition fields on accessed notes.
    pub async fn search_notes(
        &self,
        query_text: &str,
        limit: usize,
        graph_hops: usize,
    ) -> Result<Vec<serde_json::Value>> {
        let fetch_limit = (limit * 3).max(10);

        // 1. Vector search (if LLM available)
        let mut vec_hits: Vec<(String, String)> = Vec::new();
        if let Some(llm) = &self.llm {
            if let Ok(query_embedding) = llm.embeddings(query_text).await {
                let cypher = r#"
                CALL db.index.vector.queryNodes('note_embeddings', $limit, $embedding)
                YIELD node, score
                RETURN node.id AS id, node.content AS content
                "#;

                let q = neo4rs::query(cypher)
                    .param("embedding", query_embedding)
                    .param("limit", fetch_limit as i64);

                if let Ok(rows) = self.neo4j.execute(q).await {
                    for row in rows {
                        if let (Ok(id), Ok(content)) = (
                            row.get::<String>("id"),
                            row.get::<String>("content"),
                        ) {
                            vec_hits.push((id, content));
                        }
                    }
                }
            }
        }

        // 2. BM25 full-text search
        let mut bm25_hits: Vec<(String, String)> = Vec::new();
        {
            let cypher = r#"
            CALL db.index.fulltext.queryNodes('note_content_fulltext', $query, {limit: $limit})
            YIELD node, score
            RETURN node.id AS id, node.content AS content
            "#;

            let q = neo4rs::query(cypher)
                .param("query", query_text)
                .param("limit", fetch_limit as i64);

            if let Ok(rows) = self.neo4j.execute(q).await {
                for row in rows {
                    if let (Ok(id), Ok(content)) = (
                        row.get::<String>("id"),
                        row.get::<String>("content"),
                    ) {
                        bm25_hits.push((id, content));
                    }
                }
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
                .param("limit", limit as i64);

            self.neo4j.execute(q).await?
                .into_iter()
                .filter_map(|row| {
                    if let (Ok(id), Ok(content)) = (
                        row.get::<String>("id"),
                        row.get::<String>("content"),
                    ) {
                        Some((id, content))
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            let rrf_ranked = Self::rrf_merge(vec_hits, bm25_hits, 60.0, limit);
            self.apply_freshness_boost(rrf_ranked).await
        };

        // 3.5 Hybrid Retrieval: Resolve child chunks to parents (Small-to-Big)
        if !merged.is_empty() {
            let hit_ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
            let parent_cypher = r#"
            MATCH (n:Note) WHERE n.id IN $hit_ids
            OPTIONAL MATCH (n)-[:PART_OF]->(p:Note)
            RETURN n.id AS hit_id, n.content AS hit_content, p.id AS parent_id, p.content AS parent_content
            "#;

            if let Ok(rows) = self.neo4j.execute(neo4rs::query(parent_cypher).param("hit_ids", hit_ids)).await {
                let mut resolved = Vec::new();
                let mut seen_ids = std::collections::HashSet::new();

                for row in rows {
                    let parent_id = row.get::<String>("parent_id").ok();
                    let parent_content = row.get::<String>("parent_content").ok();
                    
                    if let (Some(pid), Some(pcontent)) = (parent_id, parent_content) {
                        if !seen_ids.contains(&pid) {
                            resolved.push((pid.clone(), pcontent));
                            seen_ids.insert(pid);
                        }
                    } else if let (Ok(hid), Ok(hcontent)) = (row.get::<String>("hit_id"), row.get::<String>("hit_content")) {
                        if !seen_ids.contains(&hid) {
                            resolved.push((hid.clone(), hcontent));
                            seen_ids.insert(hid);
                        }
                    }
                }
                merged = resolved;
                merged.truncate(limit);
            }
        }

        // 4. Graph expansion
        if graph_hops > 0 && !merged.is_empty() {
            let primary_ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
            match self.expand_with_graph_context(&primary_ids, graph_hops, 5).await {
                Ok(neighbours) => {
                    let existing_ids: std::collections::HashSet<String> =
                        primary_ids.iter().cloned().collect();
                    for (nb_id, nb_content, _score) in neighbours {
                        if !existing_ids.contains(&nb_id) {
                            merged.push((nb_id, nb_content));
                        }
                    }
                    merged.truncate(limit);
                }
                Err(e) => warn!("Graph expansion failed: {}", e),
            }
        }

        // 5. Access tracking with spaced-repetition update
        let hit_ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
        if !hit_ids.is_empty() {
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
            let _ = self.neo4j.run(
                neo4rs::query(update_cypher)
                    .param("ids", hit_ids)
                    .param("now", now),
            ).await;
        }

        Ok(merged.into_iter().map(|(id, content)| serde_json::json!({ "id": id, "content": content })).collect())
    }

    /// Return notes whose spaced-repetition review is due.
    pub async fn review_due_notes(&self, limit: usize) -> Result<Vec<Value>> {
        let cypher = r#"
        MATCH (n:Note)
        WHERE n.next_review_at <= datetime()
          AND NOT COALESCE(n.note_type, 'semantic') IN ['consolidated']
        RETURN n.id AS id, n.content AS content, n.note_type AS note_type,
               toString(n.next_review_at) AS next_review_at, n.access_count AS access_count
        ORDER BY n.next_review_at ASC LIMIT $limit
        "#;

        let query = neo4rs::query(cypher).param("limit", limit as i64);
        let rows = self.neo4j.execute(query).await?;

        let mut results = Vec::new();
        for row in rows {
            let id = row.get::<String>("id").unwrap_or_default();
            let content = row.get::<String>("content").unwrap_or_default();
            let note_type = row.get::<String>("note_type").unwrap_or_else(|_| "semantic".to_string());
            let next_review = row.get::<String>("next_review_at").unwrap_or_default();
            let access_count = row.get::<i64>("access_count").unwrap_or(0);
            results.push(serde_json::json!({
                "id": id,
                "content": content,
                "note_type": note_type,
                "next_review_at": next_review,
                "access_count": access_count
            }));
        }

        Ok(results)
    }

    /// Delete stale notes. When score_threshold/lambda provided, uses adaptive decay scoring.
    /// When dry_run=true, returns count without deleting.
    pub async fn prune_old_notes(
        &self,
        days_stale: i64,
        min_accesses: i64,
        score_threshold: Option<f64>,
        lambda: Option<f64>,
        dry_run: bool,
    ) -> Result<usize> {
        if score_threshold.is_some() || lambda.is_some() {
            let threshold = score_threshold.unwrap_or(0.1);
            let lam = lambda.unwrap_or(0.1);

            if dry_run {
                let cypher = r#"
                MATCH (n:Note)
                WHERE NOT COALESCE(n.note_type, 'semantic') IN ['consolidated', 'reflection']
                OPTIONAL MATCH (other:Note)-[:RELATES_TO]->(n)
                WITH n, count(other) AS in_degree,
                     duration.between(n.last_accessed_at, datetime()).days AS days_idle
                WITH n,
                     toFloat(COALESCE(n.access_count, 0) + 1) / (1.0 + toFloat(in_degree))
                         * exp(-$lambda * toFloat(days_idle)) AS decay_score
                WHERE decay_score < $threshold
                RETURN count(n) AS total
                "#;

                let rows = self.neo4j.execute(
                    neo4rs::query(cypher)
                        .param("lambda", lam)
                        .param("threshold", threshold),
                ).await?;

                let total = rows.first()
                    .and_then(|r| r.get::<i64>("total").ok())
                    .unwrap_or(0) as usize;
                return Ok(total);
            }

            let cypher = r#"
            MATCH (n:Note)
            WHERE NOT COALESCE(n.note_type, 'semantic') IN ['consolidated', 'reflection']
            OPTIONAL MATCH (other:Note)-[:RELATES_TO]->(n)
            WITH n, count(other) AS in_degree,
                 duration.between(n.last_accessed_at, datetime()).days AS days_idle
            WITH n,
                 toFloat(COALESCE(n.access_count, 0) + 1) / (1.0 + toFloat(in_degree))
                     * exp(-$lambda * toFloat(days_idle)) AS decay_score
            WHERE decay_score < $threshold
            WITH collect(n) AS stale
            FOREACH (n IN stale | DETACH DELETE n)
            RETURN size(stale) AS total
            "#;

            let rows = self.neo4j.execute(
                neo4rs::query(cypher)
                    .param("lambda", lam)
                    .param("threshold", threshold),
            ).await?;

            let total = rows.first()
                .and_then(|r| r.get::<i64>("total").ok())
                .unwrap_or(0) as usize;
            return Ok(total);
        }

        // Legacy path: days_stale / min_accesses
        if dry_run {
            let cypher = r#"
            MATCH (n:Note)
            WHERE n.last_accessed_at < datetime() - duration({days: $days})
              AND n.access_count < $min_accesses
            RETURN count(n) AS total
            "#;
            let rows = self.neo4j.execute(
                neo4rs::query(cypher)
                    .param("days", days_stale)
                    .param("min_accesses", min_accesses),
            ).await?;
            let total = rows.first()
                .and_then(|r| r.get::<i64>("total").ok())
                .unwrap_or(0) as usize;
            return Ok(total);
        }

        let cypher = r#"
        MATCH (n:Note)
        WHERE n.last_accessed_at < datetime() - duration({days: $days})
          AND n.access_count < $min_accesses
        WITH collect(n) AS stale
        FOREACH (n IN stale | DETACH DELETE n)
        RETURN size(stale) AS total
        "#;

        let rows = self.neo4j.execute(
            neo4rs::query(cypher)
                .param("days", days_stale)
                .param("min_accesses", min_accesses),
        ).await?;

        let total = rows.first()
            .and_then(|r| r.get::<i64>("total").ok())
            .unwrap_or(0) as usize;

        Ok(total)
    }

    /// Find notes that mention a given entity name.
    pub async fn search_by_entity(
        &self,
        entity_name: &str,
        entity_type: Option<&str>,
        limit: usize,
    ) -> Result<Vec<Value>> {
        let cypher = r#"
        MATCH (e:Entity)
        WHERE toLower(e.name) CONTAINS toLower($entity_name)
          AND ($entity_type IS NULL OR e.entity_type = $entity_type)
        MATCH (n:Note)-[r:MENTIONS]->(e)
        RETURN n.id AS note_id, n.content AS content,
               e.name AS entity, e.entity_type AS entity_type, r.count AS mention_count
        ORDER BY r.count DESC LIMIT $limit
        "#;

        // neo4rs doesn't support passing Optional NULL cleanly; handle it via separate queries.
        let rows = if let Some(et) = entity_type {
            self.neo4j.execute(
                neo4rs::query(cypher)
                    .param("entity_name", entity_name)
                    .param("entity_type", et)
                    .param("limit", limit as i64),
            ).await?
        } else {
            // Use a variant without the entity_type filter
            let cypher_no_type = r#"
            MATCH (e:Entity)
            WHERE toLower(e.name) CONTAINS toLower($entity_name)
            MATCH (n:Note)-[r:MENTIONS]->(e)
            RETURN n.id AS note_id, n.content AS content,
                   e.name AS entity, e.entity_type AS entity_type, r.count AS mention_count
            ORDER BY r.count DESC LIMIT $limit
            "#;
            self.neo4j.execute(
                neo4rs::query(cypher_no_type)
                    .param("entity_name", entity_name)
                    .param("limit", limit as i64),
            ).await?
        };

        let mut results = Vec::new();
        for row in rows {
            results.push(serde_json::json!({
                "note_id": row.get::<String>("note_id").unwrap_or_default(),
                "content": row.get::<String>("content").unwrap_or_default(),
                "entity": row.get::<String>("entity").unwrap_or_default(),
                "entity_type": row.get::<String>("entity_type").unwrap_or_default(),
                "mention_count": row.get::<i64>("mention_count").unwrap_or(0)
            }));
        }

        Ok(results)
    }

    /// Search notes returning (id, content) pairs — used internally for reasoning tools.
    async fn search_notes_with_ids(
        &self,
        query_text: &str,
        limit: usize,
        graph_hops: usize,
    ) -> Result<Vec<(String, String)>> {
        let fetch_limit = (limit * 3).max(10);

        let mut vec_hits: Vec<(String, String)> = Vec::new();
        if let Some(llm) = &self.llm {
            if let Ok(query_embedding) = llm.embeddings(query_text).await {
                let cypher = r#"
                CALL db.index.vector.queryNodes('note_embeddings', $limit, $embedding)
                YIELD node, score
                RETURN node.id AS id, node.content AS content
                "#;
                let q = neo4rs::query(cypher)
                    .param("embedding", query_embedding)
                    .param("limit", fetch_limit as i64);
                if let Ok(rows) = self.neo4j.execute(q).await {
                    for row in rows {
                        if let (Ok(id), Ok(content)) = (row.get::<String>("id"), row.get::<String>("content")) {
                            vec_hits.push((id, content));
                        }
                    }
                }
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
                .param("query", query_text)
                .param("limit", fetch_limit as i64);
            if let Ok(rows) = self.neo4j.execute(q).await {
                for row in rows {
                    if let (Ok(id), Ok(content)) = (row.get::<String>("id"), row.get::<String>("content")) {
                        bm25_hits.push((id, content));
                    }
                }
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
            self.neo4j.execute(q).await?
                .into_iter()
                .filter_map(|row| {
                    if let (Ok(id), Ok(content)) = (row.get::<String>("id"), row.get::<String>("content")) {
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
            if let Ok(neighbours) = self.expand_with_graph_context(&primary_ids, graph_hops, 5).await {
                let existing_ids: std::collections::HashSet<String> = primary_ids.iter().cloned().collect();
                for (nb_id, nb_content, _score) in neighbours {
                    if !existing_ids.contains(&nb_id) {
                        merged.push((nb_id, nb_content));
                    }
                }
                merged.truncate(limit);
            }
        }

        Ok(merged)
    }

    // =========================================================================
    // Cognitive layer — reason, audit_action, explain_reasoning
    // =========================================================================

    /// Gather relevant notes and derive new inferences via LLM.
    /// Returns (answer, inferences, confidence, gaps, optional_inference_note_id).
    pub async fn reason(
        &self,
        question: &str,
        limit: usize,
        store_inference: bool,
    ) -> Result<(String, Vec<String>, f64, Vec<String>, Option<String>)> {
        let llm = self.llm.as_ref().ok_or_else(|| {
            anyhow::anyhow!("LLM is required for reasoning but is not configured")
        })?;

        let notes = self.search_notes_with_ids(question, limit, 1).await.unwrap_or_default();

        let notes_block = notes
            .iter()
            .enumerate()
            .map(|(i, (_, content))| format!("Note {}:\n{}", i + 1, content))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let prompt = format!(
            "You are a reasoning engine. Given the following retrieved knowledge, answer the question \
             by logical inference. Clearly distinguish what is known vs inferred.\n\
             Output ONLY valid JSON (no markdown, no code fences): \
             {{\"answer\":\"...\",\"inferences\":[\"...\"],\"confidence\":0.0,\"gaps\":[\"...\"]}}\n\n\
             QUESTION: {}\n\
             KNOWLEDGE:\n{}",
            question, notes_block
        );

        let response = llm.generate(&prompt).await
            .map_err(|e| anyhow::anyhow!("LLM reasoning failed: {}", e))?;

        let text = response.text.trim();
        let json_start = text.find('{').unwrap_or(0);
        let json_end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
        let json_str = &text[json_start..json_end];

        let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap_or_else(|_| {
            serde_json::json!({
                "answer": text,
                "inferences": [],
                "confidence": 0.5,
                "gaps": []
            })
        });

        let answer = parsed.get("answer").and_then(|v| v.as_str()).unwrap_or(text).to_string();
        let inferences: Vec<String> = parsed.get("inferences")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let confidence = parsed.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.5);
        let gaps: Vec<String> = parsed.get("gaps")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();

        // Store inference note and create DERIVED_FROM edges
        let inference_note_id = if store_inference && !inferences.is_empty() {
            let inference_content = format!(
                "Q: {}\n\nAnswer: {}\n\nInferences:\n{}",
                question, answer,
                inferences.iter().enumerate().map(|(i, inf)| format!("{}. {}", i + 1, inf)).collect::<Vec<_>>().join("\n")
            );
            match self.store_note_raw(&inference_content, Some("inference"), Some(question), None).await {
                Ok((inf_id, _)) => {
                    // Create DERIVED_FROM edges to source notes
                    let source_ids: Vec<String> = notes.iter().map(|(id, _)| id.clone()).collect();
                    if !source_ids.is_empty() {
                        let link_cypher = r#"
                        MATCH (inf:Note {id: $inf_id})
                        MATCH (src:Note) WHERE src.id IN $source_ids
                        MERGE (inf)-[:DERIVED_FROM]->(src)
                        "#;
                        let _ = self.neo4j.run(
                            neo4rs::query(link_cypher)
                                .param("inf_id", inf_id.clone())
                                .param("source_ids", source_ids),
                        ).await;
                    }
                    Some(inf_id)
                }
                Err(e) => {
                    warn!("Failed to store inference note: {}", e);
                    None
                }
            }
        } else {
            None
        };

        Ok((answer, inferences, confidence, gaps, inference_note_id))
    }

    /// Check a proposed action against stored values and principles.
    /// Returns (aligned, confidence, concerns, suggestions, reasoning).
    pub async fn audit_action(
        &self,
        action: &str,
        context: Option<&str>,
    ) -> Result<(bool, f64, Vec<String>, Vec<String>, String)> {
        let llm = self.llm.as_ref().ok_or_else(|| {
            anyhow::anyhow!("LLM is required for audit but is not configured")
        })?;

        let principles_notes = self
            .search_notes_with_ids("ethical principles values guidelines safety alignment", 5, 0)
            .await
            .unwrap_or_default();

        let action_notes = self
            .search_notes_with_ids(action, 3, 0)
            .await
            .unwrap_or_default();

        let principles_block = principles_notes
            .iter()
            .enumerate()
            .map(|(i, (_, c))| format!("Principle {}:\n{}", i + 1, c))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let action_notes_block = action_notes
            .iter()
            .enumerate()
            .map(|(i, (_, c))| format!("Context {}:\n{}", i + 1, c))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let prompt = format!(
            "You are a values alignment auditor. Evaluate the proposed action against the principles.\n\
             Output ONLY valid JSON (no markdown): \
             {{\"aligned\":true,\"confidence\":0.0,\"concerns\":[\"...\"],\"suggestions\":[\"...\"],\"reasoning\":\"...\"}}\n\n\
             PROPOSED ACTION: {}\n\
             CONTEXT: {}\n\
             RELEVANT PRINCIPLES:\n{}\n\
             ACTION CONTEXT:\n{}",
            action,
            context.unwrap_or("(none)"),
            if principles_block.is_empty() { "(no stored principles found)" } else { &principles_block },
            if action_notes_block.is_empty() { "(no relevant context found)" } else { &action_notes_block }
        );

        let response = llm.generate(&prompt).await
            .map_err(|e| anyhow::anyhow!("LLM audit failed: {}", e))?;

        let text = response.text.trim();
        let json_start = text.find('{').unwrap_or(0);
        let json_end = text.rfind('}').map(|i| i + 1).unwrap_or(text.len());
        let json_str = &text[json_start..json_end];

        let parsed: serde_json::Value = serde_json::from_str(json_str).unwrap_or_else(|_| {
            serde_json::json!({
                "aligned": true,
                "confidence": 0.5,
                "concerns": [],
                "suggestions": [],
                "reasoning": text
            })
        });

        let aligned = parsed.get("aligned").and_then(|v| v.as_bool()).unwrap_or(true);
        let confidence = parsed.get("confidence").and_then(|v| v.as_f64()).unwrap_or(0.5);
        let concerns: Vec<String> = parsed.get("concerns")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let suggestions: Vec<String> = parsed.get("suggestions")
            .and_then(|v| v.as_array())
            .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
            .unwrap_or_default();
        let reasoning = parsed.get("reasoning").and_then(|v| v.as_str()).unwrap_or(text).to_string();

        Ok((aligned, confidence, concerns, suggestions, reasoning))
    }

    /// Narrate a human-readable explanation of why a decision was made.
    /// Returns (explanation, knowledge_sources: [{note_id, preview}]).
    pub async fn explain_reasoning(
        &self,
        decision: &str,
        task_id: Option<&str>,
        limit: usize,
    ) -> Result<(String, Vec<serde_json::Value>)> {
        let llm = self.llm.as_ref().ok_or_else(|| {
            anyhow::anyhow!("LLM is required for explanation but is not configured")
        })?;

        // Fetch knowledge that drove the decision
        let knowledge_notes = self
            .search_notes_with_ids(decision, limit / 2 + 1, 2)
            .await
            .unwrap_or_default();

        // Optionally fetch task + reflection notes
        let task_context = if let Some(tid) = task_id {
            let cypher = r#"
            MATCH (n:Note)-[:REFLECTS_ON]->(t:Task {id: $task_id})
            RETURN n.id AS id, n.content AS content
            LIMIT 5
            "#;
            let q = neo4rs::query(cypher).param("task_id", tid);
            match self.neo4j.execute(q).await {
                Ok(rows) => rows
                    .into_iter()
                    .filter_map(|row| {
                        if let (Ok(id), Ok(content)) = (row.get::<String>("id"), row.get::<String>("content")) {
                            Some(format!("[Task Note {}]: {}", id, content))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n"),
                Err(_) => String::new(),
            }
        } else {
            String::new()
        };

        let notes_block = knowledge_notes
            .iter()
            .enumerate()
            .map(|(i, (_, c))| format!("Knowledge {}:\n{}", i + 1, c))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let prompt = format!(
            "You are an explainability engine. Using the context below, explain in plain language \
             why the following decision was taken. Be specific about what information drove it.\n\n\
             DECISION: {}\n\
             TASK CONTEXT:\n{}\n\
             RELEVANT KNOWLEDGE:\n{}",
            decision,
            if task_context.is_empty() { "(none)" } else { &task_context },
            if notes_block.is_empty() { "(no stored knowledge found)" } else { &notes_block }
        );

        let response = llm.generate(&prompt).await
            .map_err(|e| anyhow::anyhow!("LLM explanation failed: {}", e))?;

        let sources: Vec<serde_json::Value> = knowledge_notes
            .iter()
            .map(|(id, content)| {
                let preview: String = content.chars().take(120).collect();
                serde_json::json!({ "note_id": id, "preview": preview })
            })
            .collect();

        Ok((response.text, sources))
    }

    /// Consolidate a set of notes on a topic into a single summary note via LLM.
    pub async fn consolidate_memories(&self, topic: &str, limit: usize) -> Result<(String, usize, String)> {
        let llm = self.llm.as_ref().ok_or_else(|| {
            anyhow::anyhow!("LLM is required for memory consolidation but is not configured")
        })?;

        // 1. Get embedding for the topic
        let topic_embedding = llm.embeddings(topic).await
            .map_err(|e| anyhow::anyhow!("Failed to embed topic: {}", e))?;

        // 2. Find top-N similar notes
        let search_cypher = r#"
        CALL db.index.vector.queryNodes('note_embeddings', $limit, $embedding)
        YIELD node, score
        RETURN node.id AS id, node.content AS content
        "#;

        let search_query = neo4rs::query(search_cypher)
            .param("embedding", topic_embedding)
            .param("limit", limit as i64);

        let rows = self.neo4j.execute(search_query).await?;
        let mut source_ids: Vec<String> = Vec::new();
        let mut note_contents: Vec<String> = Vec::new();

        for row in rows {
            if let (Ok(id), Ok(content)) = (row.get::<String>("id"), row.get::<String>("content")) {
                source_ids.push(id);
                note_contents.push(content);
            }
        }

        if note_contents.is_empty() {
            return Err(anyhow::anyhow!("No notes found related to topic '{}'", topic));
        }

        let source_count = note_contents.len();

        // 3. Build consolidation prompt
        let notes_block = note_contents
            .iter()
            .enumerate()
            .map(|(i, c)| format!("Note {}:\n{}", i + 1, c))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let prompt = format!(
            "You are a memory consolidation system. Synthesize the following notes about '{}' \
             into a single, structured summary that captures all key facts without redundancy:\n\n{}",
            topic, notes_block
        );

        // 4. Generate consolidated content
        let llm_response = llm.generate(&prompt).await
            .map_err(|e| anyhow::anyhow!("LLM generation failed: {}", e))?;
        let consolidated_content = llm_response.text;

        // 5. Persist via store_note_raw (skip chunking + entity extraction for consolidated notes)
        let (consolidated_id, _) =
            self.store_note_raw(&consolidated_content, Some("consolidated"), None, None).await?;

        // 6. Create SUMMARIZED_BY relationships from source notes to consolidated note
        if !source_ids.is_empty() {
            let link_cypher = r#"
            MATCH (src:Note) WHERE src.id IN $source_ids
            MATCH (dst:Note {id: $consolidated_id})
            MERGE (src)-[:SUMMARIZED_BY]->(dst)
            "#;

            let link_query = neo4rs::query(link_cypher)
                .param("source_ids", source_ids)
                .param("consolidated_id", consolidated_id.clone());

            self.neo4j.run(link_query).await?;
        }

        let preview: String = consolidated_content.chars().take(200).collect();

        Ok((consolidated_id, source_count, preview))
    }
}
