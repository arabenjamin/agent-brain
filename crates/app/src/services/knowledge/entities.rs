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
    ///
    /// Each name is resolved to its canonical entity (`entity_resolution`):
    /// alias, exact name, then normalised key; only a miss creates a node.
    /// The mention keeps the name as written (`surface`), the name it resolved
    /// through (`via`) and the type the extractor gave (`type`). An entity's
    /// type is the most common type across its mentions, unless the seed fixed it.
    pub(super) async fn extract_entities(
        neo4j: Neo4jClient,
        note_id: &str,
        content: &str,
        llm: Arc<dyn LlmProvider>,
    ) -> Result<usize> {
        use crate::services::entity_resolution::{self as er, Probe, Rules, Target};

        let prompt = format!(
            "Extract the named entities from the text. Classify each as one of: \
             person, organisation, location, technology, tool, concept, url, date.\n\
             Rules:\n\
             - Only specific, meaningful entities: no generic words (\"system\", \"data\", \"task\") and no pronouns\n\
             - Write each name as it appears in the text, capitals included (\"U.S.\", \"OpenAI\", \"Neo4j\")\n\
             - Use the fullest form the text gives (\"Donald Trump\" rather than \"Trump\" when both appear)\n\
             - Output ONLY a JSON object: {{\"entities\":[{{\"name\":\"Neo4j\",\"type\":\"technology\"}}]}}\n\
             - If there are none: {{\"entities\":[]}}\n\n\
             TEXT:\n{}",
            content
        );

        let parsed = match llm.generate_json(&prompt, None, &["entities"], 1).await {
            Ok(v) => v,
            Err(e) => {
                warn!(note_id = %note_id, "Entity extraction produced no usable JSON: {}", e);
                return Ok(0);
            }
        };
        let entities: Vec<Value> = parsed
            .get("entities")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();

        // Stopwords that produce useless entity nodes
        const STOPWORDS: &[&str] = &[
            "system", "data", "task", "note", "user", "tool", "value", "result", "process", "item",
            "object", "type", "name", "list", "set", "get", "this", "that", "the", "a", "an",
        ];

        let rules = Rules::global();
        let mut probes: Vec<Probe> = Vec::new();
        for entity in &entities {
            let surface = match entity.get("name").and_then(|v| v.as_str()) {
                Some(n) if !n.trim().is_empty() => n.trim().to_string(),
                _ => continue,
            };
            let name = er::display_name(&surface);
            // Skip stopwords and single-character names
            if name.len() < 2 || STOPWORDS.contains(&name.as_str()) {
                continue;
            }
            if probes.iter().any(|p| p.name == name) {
                continue;
            }
            let key = rules.key(&name);
            let entity_type =
                rules.entity_type(entity.get("type").and_then(|v| v.as_str()).unwrap_or(""));
            probes.push(Probe {
                upper_in_text: er::text_has_upper(content, &key),
                name,
                key,
                surface,
                entity_type,
            });
        }
        if probes.is_empty() {
            return Ok(0);
        }
        let found = er::lookup(&neo4j, &probes).await?;

        let timestamp = Utc::now().to_rfc3339();
        let mut existing: Vec<neo4rs::BoltType> = Vec::new();
        let mut created: Vec<neo4rs::BoltType> = Vec::new();
        for p in &probes {
            let mut row = std::collections::HashMap::new();
            row.insert(
                "surface".to_string(),
                neo4rs::BoltType::from(p.surface.clone()),
            );
            row.insert("entity_type".to_string(), p.entity_type.clone().into());
            match er::resolve(p, found.get(&p.name).unwrap_or(&Default::default())) {
                Target::Existing { id, .. } => {
                    row.insert("id".to_string(), id.into());
                    row.insert("name".to_string(), p.name.clone().into());
                    existing.push(neo4rs::BoltType::from(row));
                }
                Target::Create => {
                    row.insert("name".to_string(), p.name.clone().into());
                    row.insert("key".to_string(), p.key.clone().into());
                    row.insert("entity_id".to_string(), Uuid::new_v4().to_string().into());
                    created.push(neo4rs::BoltType::from(row));
                }
            }
        }
        let count = existing.len() + created.len();

        // Resolved: mention the canonical entity. `via` records the name it
        // came through when that isn't the canonical name (so an unmerge can
        // take these mentions back).
        if !existing.is_empty() {
            neo4j
                .run(
                    neo4rs::query(
                        "MATCH (n:Note {id: $note_id}) \
                         UNWIND $rows AS ent \
                         MATCH (e:Entity {id: ent.id}) \
                         MERGE (n)-[r:MENTIONS]->(e) \
                         ON CREATE SET r.count = 1, r.surface = ent.surface, r.type = ent.entity_type, \
                                       r.via = CASE WHEN ent.name <> e.name THEN ent.name END \
                         ON MATCH SET r.count = r.count + 1",
                    )
                    .param("rows", existing)
                    .param("note_id", note_id),
                )
                .await?;
        }
        // New: MERGE on name, so two notes racing to create one entity still share it.
        if !created.is_empty() {
            neo4j
                .run(
                    neo4rs::query(
                        "MATCH (n:Note {id: $note_id}) \
                         UNWIND $rows AS ent \
                         MERGE (e:Entity {name: ent.name}) \
                         ON CREATE SET e.id = ent.entity_id, e.key = ent.key, \
                                       e.entity_type = ent.entity_type, e.created_at = datetime($ts) \
                         MERGE (n)-[r:MENTIONS]->(e) \
                         ON CREATE SET r.count = 1, r.surface = ent.surface, r.type = ent.entity_type \
                         ON MATCH SET r.count = r.count + 1",
                    )
                    .param("rows", created)
                    .param("ts", timestamp)
                    .param("note_id", note_id),
                )
                .await?;
        }

        // Type by majority of typed mentions (the first note used to fix it forever).
        if let Err(e) = neo4j
            .run(
                neo4rs::query(
                    "MATCH (n:Note {id: $note_id})-[:MENTIONS]->(e:Entity) \
                     WHERE coalesce(e.type_source, '') <> 'seed' \
                     MATCH (:Note)-[m:MENTIONS]->(e) WHERE m.type IS NOT NULL \
                     WITH e, m.type AS t, count(*) AS c ORDER BY c DESC \
                     WITH e, collect(t)[0] AS top \
                     SET e.entity_type = top",
                )
                .param("note_id", note_id),
            )
            .await
        {
            warn!(note_id = %note_id, "Entity type refresh failed (non-fatal): {}", e);
        }

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
        WHERE (toLower(e.name) CONTAINS toLower($entity_name)
               OR EXISTS { MATCH (a:EntityAlias)-[:ALIAS_OF]->(e) WHERE a.name = toLower($entity_name) })
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
            WHERE (toLower(e.name) CONTAINS toLower($entity_name)
               OR EXISTS { MATCH (a:EntityAlias)-[:ALIAS_OF]->(e) WHERE a.name = toLower($entity_name) })
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
