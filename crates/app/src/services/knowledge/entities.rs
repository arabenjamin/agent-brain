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

    /// Give claims their entities, while the brain is idle (HBI plan Phase 16).
    ///
    /// Claims are written by `claims::store_claim` in raw Cypher, never
    /// through `store_note`, so none of the 4,546 had a single `MENTIONS`
    /// edge, and no entity view could see the most entity-dense notes the
    /// brain holds ("Amanda Thursk is Prince Andrew's chief of staff" linked
    /// neither). This loop takes claims with no mentions and no
    /// `entities_at`, newest first, a few at a time, and stamps each with
    /// `entities_at` (also when nothing was found), so it covers the backlog
    /// and every new claim. `CLAIM_ENTITY_BACKFILL=false` stops it.
    pub fn spawn_claim_entity_backfill(neo4j: Neo4jClient, llm: Arc<dyn LlmProvider>) {
        if std::env::var("CLAIM_ENTITY_BACKFILL")
            .map(|v| v.eq_ignore_ascii_case("false") || v == "0")
            .unwrap_or(false)
        {
            info!("Claim entity backfill disabled (CLAIM_ENTITY_BACKFILL=false)");
            return;
        }
        tokio::spawn(async move {
            const IDLE_POLL: std::time::Duration = std::time::Duration::from_secs(45);
            const BATCH: i64 = 10;
            let mut keep_going = false;
            loop {
                if !keep_going {
                    tokio::time::sleep(IDLE_POLL).await;
                }
                keep_going = false;
                if crate::services::topics::busy(&neo4j).await {
                    continue;
                }
                let rows = match neo4j
                    .execute(
                        neo4rs::query(
                            "MATCH (n:Note {note_type: 'claim'}) \
                             WHERE n.entities_at IS NULL AND NOT (n)-[:MENTIONS]->() \
                             RETURN n.id AS id, n.content AS content \
                             ORDER BY n.created_at DESC LIMIT $n",
                        )
                        .param("n", BATCH),
                    )
                    .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        warn!(error = %e, "claim entity backfill: could not read claims");
                        continue;
                    }
                };
                let mut done = 0usize;
                for r in &rows {
                    if crate::services::gpu_lease::held().is_some() {
                        break;
                    }
                    let (Ok(id), Ok(content)) = (r.get::<String>("id"), r.get::<String>("content"))
                    else {
                        continue;
                    };
                    let found = Self::extract_entities_with(
                        neo4j.clone(),
                        &id,
                        &content,
                        llm.clone(),
                        &Self::claim_prompt(&content),
                    )
                    .await
                        .unwrap_or_else(|e| {
                            warn!(note_id = %id, error = %e, "claim entity backfill: extraction failed");
                            0
                        });
                    // Stamped either way: a claim naming no entity is not retried forever.
                    let _ = neo4j
                        .run(
                            neo4rs::query(
                                "MATCH (n:Note {id: $id}) SET n.entities_at = datetime(), n.entities_found = $found",
                            )
                            .param("id", id.as_str())
                            .param("found", found as i64),
                        )
                        .await;
                    done += 1;
                }
                debug!(done, "claim entity backfill batch");
                keep_going = done as i64 == BATCH && !crate::services::topics::busy(&neo4j).await;
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
        Self::extract_entities_with(neo4j, note_id, content, llm, &prompt).await
    }

    /// The prompt for one claim: a single sentence. With the general prompt
    /// the local model answered `[]` for 22 of 30 claims, most of them naming
    /// someone ("Senator Mitch McConnell was noted as voting 'aye'"); this one
    /// left 5 of the same 30 empty, each naming nothing (Phase 16).
    fn claim_prompt(content: &str) -> String {
        format!(
            "List every named person, organisation, place, product and technology in the text, \
             and any named law, event or programme. Classify each as one of: \
             person, organisation, location, technology, tool, concept, url, date.\n\
             Rules:\n\
             - A proper name counts even if it appears once (\"Hasan Minhaj\", \"Tillis\", \"Mossad\", \"Detroit\")\n\
             - No generic words (\"system\", \"data\", \"the government\") and no pronouns\n\
             - Write each name as it appears in the text, capitals included; use the fullest form the text gives\n\
             - Output ONLY a JSON object: {{\"entities\":[{{\"name\":\"...\",\"type\":\"...\"}}]}}\n\
             - Only when the text names nothing at all: {{\"entities\":[]}}\n\n\
             TEXT:\n{content}"
        )
    }

    async fn extract_entities_with(
        neo4j: Neo4jClient,
        note_id: &str,
        content: &str,
        llm: Arc<dyn LlmProvider>,
        prompt: &str,
    ) -> Result<usize> {
        use crate::services::entity_resolution::{self as er, Probe, Rules, Target};

        let parsed = match llm.generate_json(prompt, None, &["entities"], 1).await {
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
        // Demonyms resolve to their place ("Chinese hackers" mentions China,
        // HBI plan Phase 20) unless the user answered that pair "different"
        // or "related" in the merge review.
        let demonyms: Vec<neo4rs::BoltType> = entities
            .iter()
            .filter_map(|e| e.get("name").and_then(|v| v.as_str()))
            .map(er::display_name)
            .filter_map(|w| {
                let place = rules.demonym_place(&w)?.to_string();
                let mut m = std::collections::HashMap::new();
                m.insert("w".to_string(), neo4rs::BoltType::from(w));
                m.insert("p".to_string(), place.into());
                Some(neo4rs::BoltType::from(m))
            })
            .collect();
        let answered: Option<std::collections::HashSet<String>> = if demonyms.is_empty() {
            Some(Default::default())
        } else {
            match neo4j
                .execute(
                    neo4rs::query(
                        "UNWIND $pairs AS p \
                         MATCH (:Entity {name: p.w})-[:NOT_SAME|RELATED_ENTITY]-(:Entity {name: p.p}) \
                         RETURN DISTINCT p.w AS w",
                    )
                    .param("pairs", demonyms),
                )
                .await
            {
                Ok(rows) => Some(rows.iter().filter_map(|r| r.get::<String>("w").ok()).collect()),
                Err(e) => {
                    // Unknown answers: leave demonyms as they are this time.
                    warn!(note_id = %note_id, "Demonym answers not read (non-fatal): {}", e);
                    None
                }
            }
        };
        // Place name → the demonym it came through, recorded as `MENTIONS.via`.
        let mut via: std::collections::HashMap<String, String> = Default::default();
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
            let (name, demonym) = match (rules.demonym_place(&name), &answered) {
                (Some(place), Some(a)) if !a.contains(&name) => (place.to_string(), Some(name)),
                _ => (name, None),
            };
            if probes.iter().any(|p| p.name == name) {
                continue;
            }
            let key = rules.key(&name);
            let entity_type = if demonym.is_some() {
                // The vote counts the place, whatever the word was filed as.
                "location".to_string()
            } else {
                rules.entity_type(entity.get("type").and_then(|v| v.as_str()).unwrap_or(""))
            };
            if let Some(w) = demonym {
                via.insert(name.clone(), w);
            }
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
                    // `via` is stamped when this name differs from the entity's.
                    let through = via.get(&p.name).unwrap_or(&p.name);
                    row.insert("name".to_string(), through.clone().into());
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

        // Type by majority of typed mentions (the first note used to fix it
        // forever). A curated seed's type and the user's own are final.
        if let Err(e) = neo4j
            .run(
                neo4rs::query(
                    "MATCH (n:Note {id: $note_id})-[:MENTIONS]->(e:Entity) \
                     WHERE NOT coalesce(e.type_source, '') IN ['seed', 'manual'] \
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
