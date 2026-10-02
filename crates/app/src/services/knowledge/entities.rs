//! Entity extraction and entity-based search.

use super::*;

impl KnowledgeService {
    /// Spawn entity extraction as a fire-and-forget background task.
    ///
    /// The LLM call takes 10–70s on local models; running it inline made every
    /// `store_note` (and thus every chain ending in one) block for that long.
    /// Failures were already swallowed on the inline path, so detaching loses
    /// nothing but the latency.
    pub(super) fn spawn_extract_entities(&self, note_id: String, content: String) {
        let Some(llm) = self.llm.clone() else { return };
        // Extraction runs on the local model. During a GPU lease it would fail
        // for every note; the post-lease backfill extracts them instead.
        if crate::services::gpu_lease::held().is_some() {
            debug!(note_id = %note_id, "GPU leased: entity extraction left to the post-lease backfill");
            return;
        }
        let neo4j = self.neo4j.clone();
        tokio::spawn(async move {
            if let Err(e) = Self::extract_entities(neo4j, &note_id, &content, llm).await {
                warn!("Background entity extraction failed for {}: {}", note_id, e);
            }
        });
    }

    /// Extract named entities from note content and persist them to the graph.
    pub(super) async fn extract_entities(
        neo4j: Neo4jClient,
        note_id: &str,
        content: &str,
        llm: Arc<dyn LlmProvider>,
    ) -> Result<usize> {
        let prompt = format!(
            "Extract named entities from the text. Classify each into one of: \
             person, tool, technology, concept, organisation, url, date.\n\
             Rules:\n\
             - Only extract specific, meaningful entities (no generic words like \"system\", \"data\", \"task\")\n\
             - Normalise names to lowercase (e.g. \"Rust\" → \"rust\", \"Neo4J\" → \"neo4j\")\n\
             - Merge obvious aliases (e.g. \"the brain\" and \"agent-brain\" → \"agent-brain\")\n\
             - Output ONLY a valid JSON array, no markdown, no explanation\n\
             - If no entities, output: []\n\n\
             Examples: [{{\"name\":\"neo4j\",\"type\":\"technology\"}},{{\"name\":\"tokio\",\"type\":\"tool\"}}]\n\n\
             TEXT:\n{}",
            content
        );

        let response = match llm.generate(&prompt, None).await {
            Ok(r) => r,
            Err(e) => {
                warn!("Entity extraction LLM call failed: {}", e);
                return Ok(0);
            }
        };

        let text = response.trim();
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

        // Stopwords that produce useless entity nodes
        const STOPWORDS: &[&str] = &[
            "system", "data", "task", "note", "user", "tool", "value", "result", "process", "item",
            "object", "type", "name", "list", "set", "get", "this", "that", "the", "a", "an",
        ];

        let timestamp = Utc::now().to_rfc3339();

        // Collect valid entities, then persist them in one UNWIND round-trip
        // instead of one query per entity.
        let mut batch: Vec<neo4rs::BoltType> = Vec::new();
        for entity in &entities {
            let name = match entity.get("name").and_then(|v| v.as_str()) {
                Some(n) if !n.trim().is_empty() => n.trim().to_lowercase(),
                _ => continue,
            };
            // Skip stopwords and single-character names
            if name.len() < 2 || STOPWORDS.contains(&name.as_str()) {
                continue;
            }
            let entity_type = entity
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown")
                .to_string();

            let mut row = std::collections::HashMap::new();
            row.insert("name".to_string(), neo4rs::BoltType::from(name));
            row.insert("entity_id".to_string(), Uuid::new_v4().to_string().into());
            row.insert("entity_type".to_string(), entity_type.into());
            batch.push(neo4rs::BoltType::from(row));
        }

        if batch.is_empty() {
            return Ok(0);
        }
        let count = batch.len();

        let cypher = r#"
        MATCH (n:Note {id: $note_id})
        UNWIND $entities AS ent
        MERGE (e:Entity {name: ent.name})
        ON CREATE SET e.id = ent.entity_id, e.entity_type = ent.entity_type, e.created_at = datetime($ts)
        MERGE (n)-[r:MENTIONS]->(e)
        ON CREATE SET r.count = 1
        ON MATCH SET r.count = r.count + 1
        "#;

        neo4j
            .run(
                neo4rs::query(cypher)
                    .param("entities", batch)
                    .param("ts", timestamp)
                    .param("note_id", note_id),
            )
            .await?;

        Ok(count)
    }

    // =========================================================================
    // Public API
    // =========================================================================

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
            self.neo4j
                .execute(
                    neo4rs::query(cypher)
                        .param("entity_name", entity_name)
                        .param("entity_type", et)
                        .param("limit", limit as i64),
                )
                .await?
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
            self.neo4j
                .execute(
                    neo4rs::query(cypher_no_type)
                        .param("entity_name", entity_name)
                        .param("limit", limit as i64),
                )
                .await?
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
}
