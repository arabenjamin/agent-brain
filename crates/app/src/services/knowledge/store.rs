//! Writing notes: store/update/delete, chunking, embedding backfill, similarity links.

use super::*;

/// Content length above which we attempt to chunk into sub-notes.
pub(super) const CHUNK_THRESHOLD_CHARS: usize = 1500;

impl KnowledgeService {
    /// Core CREATE logic: embed content, persist the Note node, return (id, embedding).
    /// Does NOT call link_similar_notes or extract_entities — the caller handles that.
    pub(super) async fn store_note_raw(
        &self,
        content: &str,
        note_type: Option<&str>,
        source_context: Option<&str>,
        event_at: Option<&str>,
        provenance: Option<ProvenanceFlag>,
    ) -> Result<(String, Option<Vec<f32>>)> {
        let note_id = Uuid::new_v4().to_string();
        let timestamp = Utc::now().to_rfc3339();
        let nt = note_type.unwrap_or("semantic");

        // Operational/event note types carry no durable knowledge worth reviewing.
        // Excluding them prevents the spaced-rep loop from treating every job
        // completion note as a memory worth consolidating.
        let needs_review = !matches!(nt, "outcome" | "news" | "news_raw");

        let embedding = if let Some(llm) = &self.llm {
            debug!("Generating embedding for note…");
            match llm.embed(content).await {
                Ok(emb) => Some(emb),
                Err(e) => {
                    info!("Failed to generate embedding: {}", e);
                    None
                }
            }
        } else {
            None
        };

        let cypher = match (embedding.is_some(), needs_review) {
            (true, true) => {
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
            }
            (true, false) => {
                r#"
            CREATE (n:Note {
                id: $id,
                content: $content,
                note_type: $note_type,
                created_at: datetime($timestamp),
                last_accessed_at: datetime($timestamp),
                access_count: 0,
                embedding: $embedding
            })
            "#
            }
            (false, true) => {
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
            }
            (false, false) => {
                r#"
            CREATE (n:Note {
                id: $id,
                content: $content,
                note_type: $note_type,
                created_at: datetime($timestamp),
                last_accessed_at: datetime($timestamp),
                access_count: 0
            })
            "#
            }
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

        // Stored without a vector (a GPU lease, or the embedder down): mark it so
        // `backfill_pending_embeddings` embeds and links it later, rather than the
        // note staying invisible to vector search for good.
        if embedding.is_none() && self.llm.is_some() {
            let q = neo4rs::query("MATCH (n:Note {id: $id}) SET n.embedding_pending = true")
                .param("id", note_id.clone());
            let _ = self.neo4j.run(q).await;
        }

        // Optional fields written in separate SET queries to keep the CREATE clean.
        if let Some(sc) = source_context {
            let q = neo4rs::query("MATCH (n:Note {id: $id}) SET n.source_context = $sc")
                .param("id", note_id.clone())
                .param("sc", sc);
            let _ = self.neo4j.run(q).await;
        }

        if let Some(ea) = event_at {
            let q = neo4rs::query("MATCH (n:Note {id: $id}) SET n.event_at = datetime($ea)")
                .param("id", note_id.clone())
                .param("ea", ea);
            let _ = self.neo4j.run(q).await;
        }

        let prov = provenance.unwrap_or(ProvenanceFlag::UserInput);
        let q = neo4rs::query("MATCH (n:Note {id: $id}) SET n.provenance = $prov")
            .param("id", note_id.clone())
            .param("prov", prov.as_str());
        let _ = self.neo4j.run(q).await;

        Ok((note_id, embedding))
    }

    /// Auto-link a note to similar existing notes via RELATES_TO edges.
    /// Replace a note's content in place, refreshing everything derived from it.
    ///
    /// Used by the envelope repair (`services/repair.rs`). Unlike `store_note`,
    /// this preserves the node: `id`, `created_at`, and inbound edges such as
    /// `SUMMARIZED_BY` (consolidations) and `DERIVED_FROM` (inference provenance)
    /// survive, so repairing a note does not orphan the conclusions built on it
    /// or reset its spaced-repetition schedule.
    ///
    /// Everything *derived from the old text* is rebuilt: the embedding, the
    /// `RELATES_TO` similarity edges (which otherwise still encode similarity
    /// between the old, wrong content), the chunk sub-notes, and the entity
    /// mentions. Returns `(chunks_removed, chunks_created)`.
    pub async fn rewrite_note_in_place(
        &self,
        note_id: &str,
        content: &str,
        note_type: Option<&str>,
        source_context: Option<&str>,
    ) -> Result<(usize, usize)> {
        // 1. New embedding for the corrected text.
        let embedding: Option<Vec<f32>> = match &self.llm {
            Some(llm) => match llm.embed(content).await {
                Ok(e) => Some(e),
                Err(e) => {
                    warn!(note_id = %note_id, error = %e, "Repair: embedding failed, storing text only");
                    None
                }
            },
            None => None,
        };

        // 2. Content + embedding, leaving id/created_at/inbound edges alone.
        let mut q = neo4rs::query(
            "MATCH (n:Note {id: $id}) SET n.content = $content, n.embedding = $embedding",
        )
        .param("id", note_id)
        .param("content", content);
        q = match &embedding {
            Some(e) => q.param("embedding", e.clone()),
            None => q.param("embedding", Vec::<f32>::new()),
        };
        self.neo4j.run(q).await?;

        // 3. Drop stale derived edges. RELATES_TO and MENTIONS were both computed
        //    from the polluted text and are actively misleading until rebuilt.
        self.neo4j
            .run(
                neo4rs::query("MATCH (n:Note {id: $id})-[r:RELATES_TO|MENTIONS]->() DELETE r")
                    .param("id", note_id),
            )
            .await?;

        // 4. Remove the old chunks — they are fragments of the old text.
        let removed = self
            .neo4j
            .execute(
                neo4rs::query(
                    "MATCH (c:Note)-[:PART_OF]->(p:Note {id: $id}) \
                     WITH c, count(*) AS _x DETACH DELETE c RETURN count(*) AS removed",
                )
                .param("id", note_id),
            )
            .await?
            .first()
            .and_then(|r| r.get::<i64>("removed").ok())
            .unwrap_or(0) as usize;

        // 5. Re-link the parent against the refreshed vector.
        if let Some(ref emb) = embedding {
            let _ = self.link_similar_notes(note_id, emb).await;
        }

        // 6. Regenerate chunks exactly as store_note would.
        let mut created = 0usize;
        if content.len() > CHUNK_THRESHOLD_CHARS
            && let Some(chunks) = Self::chunk_by_boundaries(content)
            && chunks.len() > 1
        {
            for chunk in &chunks {
                let (child_id, child_emb) = self
                    .store_note_raw(chunk, note_type, source_context, None, None)
                    .await?;
                let link_q = neo4rs::query(
                    "MATCH (child:Note {id: $child_id}), (parent:Note {id: $parent_id}) \
                     MERGE (child)-[:PART_OF]->(parent)",
                )
                .param("child_id", child_id.clone())
                .param("parent_id", note_id);
                let _ = self.neo4j.run(link_q).await;
                if let Some(ref emb) = child_emb {
                    let _ = self.link_similar_notes(&child_id, emb).await;
                }
                created += 1;
            }
        }

        // 7. Re-extract entities from the corrected text (background).
        self.spawn_extract_entities(note_id.to_string(), content.to_string());

        Ok((removed, created))
    }

    /// Embed and link notes stored while embeddings were unavailable (a GPU
    /// lease, or the embedder down), oldest first, and extract entities for
    /// those that have none (chunks excepted: their entities live on the
    /// parent). Stops early if a GPU lease is taken again. Returns
    /// `(embedded, failed)`; a failed note stays pending for the next run.
    pub async fn backfill_pending_embeddings(&self, max: usize) -> Result<(usize, usize)> {
        let Some(llm) = &self.llm else {
            return Ok((0, 0));
        };
        let rows = self
            .neo4j
            .execute(
                neo4rs::query(
                    "MATCH (n:Note) WHERE n.embedding_pending = true \
                     RETURN n.id AS id, n.content AS content, \
                            size([(n)-[:MENTIONS]->() | 1]) > 0 \
                            OR size([(n)-[:PART_OF]->() | 1]) > 0 AS skip_entities \
                     ORDER BY n.created_at ASC LIMIT $max",
                )
                .param("max", max as i64),
            )
            .await?;
        let (mut embedded, mut failed) = (0usize, 0usize);
        for row in rows {
            if crate::services::gpu_lease::held().is_some() {
                break;
            }
            let (Ok(id), Ok(content)) = (row.get::<String>("id"), row.get::<String>("content"))
            else {
                continue;
            };
            let skip_entities = row.get::<bool>("skip_entities").unwrap_or(true);
            match llm.embed(&content).await {
                Ok(emb) => {
                    let q = neo4rs::query(
                        "MATCH (n:Note {id: $id}) \
                         SET n.embedding = $embedding REMOVE n.embedding_pending",
                    )
                    .param("id", id.clone())
                    .param("embedding", emb.clone());
                    self.neo4j.run(q).await?;
                    let _ = self.link_similar_notes(&id, &emb).await;
                    if !skip_entities {
                        self.spawn_extract_entities(id, content);
                    }
                    embedded += 1;
                }
                Err(e) => {
                    warn!(note_id = %id, error = %e, "Backfill: embedding still failing — left pending");
                    failed += 1;
                }
            }
        }
        Ok((embedded, failed))
    }

    /// Run [`Self::backfill_pending_embeddings`] in batches until nothing is
    /// left or a batch makes no progress. The GPU lease's release hook.
    pub async fn run_embedding_backfill(&self) {
        const BATCH: usize = 100;
        let (mut embedded, mut failed) = (0usize, 0usize);
        loop {
            match self.backfill_pending_embeddings(BATCH).await {
                Ok((e, f)) => {
                    embedded += e;
                    failed += f;
                    if e == 0 || e + f < BATCH {
                        break;
                    }
                }
                Err(err) => {
                    warn!(error = %err, "Embedding backfill stopped");
                    break;
                }
            }
        }
        if embedded + failed > 0 {
            info!(embedded, failed, "Embedding backfill finished");
        }
    }

    pub(super) async fn link_similar_notes(
        &self,
        note_id: &str,
        embedding: &[f32],
    ) -> Result<usize> {
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
    /// Split long content at paragraph and sentence boundaries.
    /// Returns `Some(chunks)` when 2+ chunks of ≥200 chars can be produced, else `None`.
    /// This is deterministic and requires no LLM call.
    pub(super) fn chunk_by_boundaries(content: &str) -> Option<Vec<String>> {
        const MIN_CHUNK: usize = 200;
        const MAX_CHUNK: usize = CHUNK_THRESHOLD_CHARS;

        // First pass: split by blank lines (paragraph boundaries)
        let paragraphs: Vec<&str> = content
            .split("\n\n")
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();

        let mut chunks: Vec<String> = Vec::new();
        let mut current = String::new();

        for para in &paragraphs {
            if current.is_empty() {
                current.push_str(para);
            } else if current.len() + para.len() + 2 > MAX_CHUNK && current.len() >= MIN_CHUNK {
                chunks.push(current.trim().to_string());
                current = para.to_string();
            } else {
                current.push_str("\n\n");
                current.push_str(para);
            }
        }
        if !current.trim().is_empty() {
            chunks.push(current.trim().to_string());
        }

        if chunks.len() > 1 {
            return Some(chunks);
        }

        // Second pass: fall back to sentence boundaries (handles single-paragraph walls of text)
        let mut sentence_chunks: Vec<String> = Vec::new();
        let mut current = String::new();

        for (i, raw) in content.split(". ").enumerate() {
            let sentence = raw.trim();
            if sentence.is_empty() {
                continue;
            }
            // Restore the period on all but the final fragment
            let piece = if !content.trim_end().ends_with(sentence) {
                format!("{}.", sentence)
            } else {
                sentence.to_string()
            };
            // Use index 0 check as a proxy for "first fragment"
            if i == 0 || current.is_empty() {
                current = piece;
            } else if current.len() + piece.len() + 1 > MAX_CHUNK && current.len() >= MIN_CHUNK {
                sentence_chunks.push(current.trim().to_string());
                current = piece;
            } else {
                current.push(' ');
                current.push_str(&piece);
            }
        }
        if !current.trim().is_empty() {
            sentence_chunks.push(current.trim().to_string());
        }

        if sentence_chunks.len() > 1 {
            Some(sentence_chunks)
        } else {
            None
        }
    }

    /// Store a note with optional metadata. Chunks long content automatically.
    pub async fn store_note(
        &self,
        content: &str,
        note_type: Option<&str>,
        source_context: Option<&str>,
        event_at: Option<&str>,
        provenance: Option<ProvenanceFlag>,
    ) -> Result<(String, usize)> {
        // Attempt semantic chunking for long content (skip for consolidated/reflection types).
        let skip_chunk = matches!(
            note_type,
            Some("consolidated") | Some("reflection") | Some("news")
        );

        if !skip_chunk
            && content.len() > CHUNK_THRESHOLD_CHARS
            && let Some(chunks) = Self::chunk_by_boundaries(content)
            && chunks.len() > 1
        {
            info!(chunks = chunks.len(), "Chunking long note into sub-notes");

            // Create parent note
            let (parent_id, parent_emb) = self
                .store_note_raw(
                    content,
                    note_type,
                    source_context,
                    event_at,
                    provenance.clone(),
                )
                .await?;

            let mut total_links = if let Some(ref emb) = parent_emb {
                self.link_similar_notes(&parent_id, emb).await.unwrap_or(0)
            } else {
                0
            };

            // Create child chunk notes
            for chunk in &chunks {
                let (child_id, child_emb) = self
                    .store_note_raw(
                        chunk,
                        note_type,
                        source_context,
                        event_at,
                        provenance.clone(),
                    )
                    .await?;

                // Link child to parent
                let link_q = neo4rs::query(
                    "MATCH (child:Note {id: $child_id}), (parent:Note {id: $parent_id}) \
                                 MERGE (child)-[:PART_OF]->(parent)",
                )
                .param("child_id", child_id.clone())
                .param("parent_id", parent_id.clone());
                let _ = self.neo4j.run(link_q).await;

                if let Some(ref emb) = child_emb {
                    total_links += self.link_similar_notes(&child_id, emb).await.unwrap_or(0);
                }
            }

            // Entity extraction on the full parent content (background, non-blocking)
            self.spawn_extract_entities(parent_id.clone(), content.to_string());

            return Ok((parent_id, total_links));
        }

        // Normal (non-chunked) path
        let (note_id, embedding) = self
            .store_note_raw(content, note_type, source_context, event_at, provenance)
            .await?;

        let links_created = if let Some(ref emb) = embedding {
            self.link_similar_notes(&note_id, emb)
                .await
                .unwrap_or_else(|e| {
                    warn!("Failed to link similar notes: {}", e);
                    0
                })
        } else {
            0
        };

        self.spawn_extract_entities(note_id.clone(), content.to_string());

        Ok((note_id, links_created))
    }

    /// Fetch a single note by ID. Updates access stats and returns full note data.
    pub async fn get_note(&self, note_id: &str) -> Result<Option<Value>> {
        let cypher = r#"
        MATCH (n:Note {id: $id})
        SET n.last_accessed_at = datetime(),
            n.access_count = coalesce(n.access_count, 0) + 1
        RETURN n.id AS id,
               n.content AS content,
               n.note_type AS note_type,
               toString(n.created_at) AS created_at,
               n.access_count AS access_count,
               n.review_interval_days AS review_interval_days,
               n.provenance AS provenance
        "#;

        let rows = self
            .neo4j
            .execute(neo4rs::query(cypher).param("id", note_id))
            .await?;

        if let Some(row) = rows.into_iter().next() {
            let id = row.get::<String>("id").unwrap_or_default();
            if id.is_empty() {
                return Ok(None);
            }
            let content = row.get::<String>("content").unwrap_or_default();
            let note_type = row.get::<String>("note_type").unwrap_or_default();
            let created_at = row.get::<String>("created_at").unwrap_or_default();
            let access_count = row.get::<i64>("access_count").unwrap_or(0);
            let review_interval = row.get::<i64>("review_interval_days").unwrap_or(1);
            let provenance = row
                .get::<String>("provenance")
                .unwrap_or_else(|_| "user_input".to_string());
            Ok(Some(json!({
                "id": id,
                "content": content,
                "note_type": note_type,
                "created_at": created_at,
                "access_count": access_count,
                "review_interval_days": review_interval,
                "provenance": provenance
            })))
        } else {
            Ok(None)
        }
    }

    /// Delete a single note and all its relationships.
    pub async fn delete_note(&self, note_id: &str) -> Result<bool> {
        let cypher = "MATCH (n:Note {id: $id}) DETACH DELETE n RETURN count(n) AS deleted";
        let rows = self
            .neo4j
            .execute(neo4rs::query(cypher).param("id", note_id))
            .await?;
        let deleted = rows
            .into_iter()
            .next()
            .and_then(|r| r.get::<i64>("deleted").ok())
            .unwrap_or(0);
        Ok(deleted > 0)
    }

    /// Update the content of an existing note in-place, preserving all graph edges and metadata.
    pub async fn update_note(&self, note_id: &str, content: &str) -> Result<bool> {
        let cypher = "MATCH (n:Note {id: $id}) SET n.content = $content RETURN count(n) AS updated";
        let rows = self
            .neo4j
            .execute(
                neo4rs::query(cypher)
                    .param("id", note_id)
                    .param("content", content),
            )
            .await?;
        let updated = rows
            .into_iter()
            .next()
            .and_then(|r| r.get::<i64>("updated").ok())
            .unwrap_or(0);
        Ok(updated > 0)
    }
}
