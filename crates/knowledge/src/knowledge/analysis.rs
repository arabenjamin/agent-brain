//! Synthesis, action audit, and reasoning explanations over related notes.

use super::*;

impl KnowledgeService {
    /// Embed `topic` and return similar notes from the vector index as `(id, note_type, content)`.
    ///
    /// `fetch_limit` — how many raw candidates to pull from the index (over-fetch when
    /// `min_score > 0` to compensate for filtered-out low-quality hits).
    /// `min_score` — drop notes below this cosine similarity (0.0 = no filter).
    /// Results are capped at `limit`.
    ///
    /// Degrades to BM25 full-text when embedding is unavailable, mirroring
    /// `search_notes_inner`. Embedding is not reliably available per-input: Ollama
    /// returns `500 … json: unsupported value: NaN` for certain inputs when the model
    /// emits NaN in the vector, and it does so *deterministically* for that exact
    /// string — retrying never clears it. Hard-failing here used to kill the whole
    /// `synthesize_knowledge` call, dead-letter the job, and take the owning chain
    /// (and its Task) down with it. Retrieval quality degrades; the tool still works.
    pub(super) async fn fetch_similar_notes(
        &self,
        topic: &str,
        limit: usize,
        fetch_limit: usize,
        min_score: f64,
    ) -> Result<Vec<(String, String, String)>> {
        // 1. Vector search — only when an LLM is configured AND the embed succeeds.
        let embedding = match self.llm.as_ref() {
            Some(llm) => match llm.embed(topic).await {
                Ok(e) => Some(e),
                Err(e) => {
                    warn!("Failed to embed topic (falling back to BM25): {}", e);
                    None
                }
            },
            None => {
                warn!("LLM not configured for vector search (falling back to BM25)");
                None
            }
        };

        if let Some(embedding) = embedding {
            let cypher = r#"
            CALL db.index.vector.queryNodes('note_embeddings', $limit, $embedding)
            YIELD node, score
            WHERE score > $min_score
            RETURN node.id AS id, node.content AS content, node.note_type AS note_type
            ORDER BY score DESC
            "#;

            let rows = self
                .neo4j
                .execute(
                    neo4rs::query(cypher)
                        .param("embedding", embedding)
                        .param("limit", fetch_limit as i64)
                        .param("min_score", min_score),
                )
                .await?;

            let notes = Self::collect_note_rows(rows, limit);
            if !notes.is_empty() {
                return Ok(notes);
            }
        }

        // 2. BM25 fallback. `min_score` is a cosine threshold and has no analogue
        //    here, so ranking is left to the full-text index. Sanitizing is required:
        //    an unescaped ':' or '/' in the topic makes queryNodes throw (see
        //    sanitize_lucene_query), which would turn the fallback into a second failure.
        let cypher = r#"
        CALL db.index.fulltext.queryNodes('note_content_fulltext', $query, {limit: $limit})
        YIELD node, score
        RETURN node.id AS id, node.content AS content, node.note_type AS note_type
        ORDER BY score DESC
        "#;

        let rows = self
            .neo4j
            .execute(
                neo4rs::query(cypher)
                    .param("query", sanitize_lucene_query(topic))
                    .param("limit", fetch_limit as i64),
            )
            .await?;

        Ok(Self::collect_note_rows(rows, limit))
    }

    /// Collect `(id, note_type, content)` triples from a note query, capped at `limit`.
    /// Rows missing `id`/`content` are skipped; a missing `note_type` defaults to `semantic`.
    pub(super) fn collect_note_rows(
        rows: Vec<neo4rs::Row>,
        limit: usize,
    ) -> Vec<(String, String, String)> {
        let mut notes = Vec::new();
        for row in rows {
            if let (Ok(id), Ok(content)) = (row.get::<String>("id"), row.get::<String>("content")) {
                let note_type = row
                    .get::<String>("note_type")
                    .unwrap_or_else(|_| "semantic".to_string());
                notes.push((id, note_type, content));
                if notes.len() >= limit {
                    break;
                }
            }
        }
        notes
    }

    /// Synthesize semantic knowledge from recent notes and inferences about a topic.
    ///
    /// Finds the most relevant notes (vector search), asks the LLM to extract
    /// durable facts/patterns/insights, and stores the result as a `semantic` note.
    /// Returns `(note_id, preview)`.
    pub async fn synthesize_knowledge(
        &self,
        topic: &str,
        limit: usize,
    ) -> Result<(String, String)> {
        let llm = self.llm.as_ref().ok_or_else(|| {
            anyhow::anyhow!("LLM is required for knowledge synthesis but is not configured")
        })?;

        // 1. Fetch relevant notes (over-fetch 2x to compensate for score filtering)
        let notes = self
            .fetch_similar_notes(topic, limit, limit * 2, 0.3)
            .await?;

        if notes.is_empty() {
            return Err(anyhow::anyhow!(
                "No notes found related to topic '{}' — nothing to synthesize",
                topic
            ));
        }

        // 2. Build synthesis prompt
        let notes_block = notes
            .iter()
            .enumerate()
            .map(|(i, (_, nt, c))| format!("[{} {}]\n{}", nt, i + 1, c))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let prompt = format!(
            "You are a knowledge distillation system. Respond in English only.\n\
             Your task is to synthesize the following \
             notes into concise, durable semantic knowledge about: \"{topic}\"\n\n\
             Extract the key facts, patterns, lessons, and actionable insights. \
             Write in clear prose with section headers. Omit ephemeral details (dates of \
             individual runs, specific counts that will change). Focus on knowledge that \
             remains true across time.\n\n\
             Do NOT echo the input labels ([inference N], [outcome N], etc.).\n\n\
             {notes_block}"
        );

        // 3. Generate and store
        let content = llm
            .generate(&prompt, None)
            .await
            .map_err(|e| anyhow::anyhow!("LLM synthesis failed: {}", e))?;

        let (note_id, _) = self
            .store_note_raw(
                &content,
                Some("semantic"),
                Some(topic),
                None,
                Some(ProvenanceFlag::SynthesisInference),
            )
            .await?;

        let preview: String = content.chars().take(200).collect();
        info!(note_id = %note_id, topic = %topic, sources = notes.len(), "Synthesized semantic knowledge note");

        Ok((note_id, preview))
    }

    /// Check a proposed action against stored values and principles.
    /// Returns (aligned, confidence, concerns, suggestions, reasoning).
    pub async fn audit_action(
        &self,
        action: &str,
        context: Option<&str>,
    ) -> Result<(bool, f64, Vec<String>, Vec<String>, String)> {
        let llm = self
            .llm
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("LLM is required for audit but is not configured"))?;

        let principles_notes = self
            .search_notes_with_ids(
                "ethical principles values guidelines safety alignment",
                5,
                0,
            )
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
            "You are a values alignment auditor. Respond in English only. All JSON field values must be in English.\n\
             Evaluate the proposed action against the principles.\n\
             Output ONLY valid JSON (no markdown): \
             {{\"aligned\":true,\"confidence\":0.0,\"concerns\":[\"...\"],\"suggestions\":[\"...\"],\"reasoning\":\"...\"}}\n\n\
             PROPOSED ACTION: {}\n\
             CONTEXT: {}\n\
             RELEVANT PRINCIPLES:\n{}\n\
             ACTION CONTEXT:\n{}",
            action,
            context.unwrap_or("(none)"),
            if principles_block.is_empty() {
                "(no stored principles found)"
            } else {
                &principles_block
            },
            if action_notes_block.is_empty() {
                "(no relevant context found)"
            } else {
                &action_notes_block
            }
        );

        let response = llm
            .generate(&prompt, None)
            .await
            .map_err(|e| anyhow::anyhow!("LLM audit failed: {}", e))?;

        let text = response.trim();
        let parsed: serde_json::Value = agent_brain_llm::llm::first_json_object(text, &["aligned"])
            .unwrap_or_else(|| {
                serde_json::json!({
                    "aligned": true,
                    "confidence": 0.5,
                    "concerns": [],
                    "suggestions": [],
                    "reasoning": text
                })
            });

        let aligned = parsed
            .get("aligned")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);
        let confidence = parsed
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5);
        let concerns: Vec<String> = parsed
            .get("concerns")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let suggestions: Vec<String> = parsed
            .get("suggestions")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let reasoning = parsed
            .get("reasoning")
            .and_then(|v| v.as_str())
            .unwrap_or(text)
            .to_string();

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
                        if let (Ok(id), Ok(content)) =
                            (row.get::<String>("id"), row.get::<String>("content"))
                        {
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
            "You are an explainability engine. Respond in English only.\n\
             Using the context below, explain in plain language \
             why the following decision was taken. Be specific about what information drove it.\n\n\
             DECISION: {}\n\
             TASK CONTEXT:\n{}\n\
             RELEVANT KNOWLEDGE:\n{}",
            decision,
            if task_context.is_empty() {
                "(none)"
            } else {
                &task_context
            },
            if notes_block.is_empty() {
                "(no stored knowledge found)"
            } else {
                &notes_block
            }
        );

        let response = llm
            .generate(&prompt, None)
            .await
            .map_err(|e| anyhow::anyhow!("LLM explanation failed: {}", e))?;

        let sources: Vec<serde_json::Value> = knowledge_notes
            .iter()
            .map(|(id, content)| {
                let preview: String = content.chars().take(120).collect();
                serde_json::json!({ "note_id": id, "preview": preview })
            })
            .collect();

        Ok((response, sources))
    }
}
