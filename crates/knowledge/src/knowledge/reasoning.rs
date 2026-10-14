//! `reason` (free-text and structured), the critic pass, and gap-task creation.

use super::*;

impl KnowledgeService {
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

        let notes = self
            .search_notes_with_ids(question, limit, 1)
            .await
            .unwrap_or_default();

        let notes_block = notes
            .iter()
            .enumerate()
            .map(|(i, (_, content))| format!("Note {}:\n{}", i + 1, content))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let prompt = format!(
            "You are a reasoning engine. Given the following retrieved knowledge, answer the question \
             by logical inference. Clearly distinguish what is known vs inferred.\n\
             IMPORTANT: Respond in English only. All text fields in the JSON must be in English.\n\
             Output ONLY valid JSON (no markdown, no code fences): \
             {{\"answer\":\"...\",\"inferences\":[\"...\"],\"confidence\":0.0,\"gaps\":[\"...\"]}}\n\n\
             QUESTION: {}\n\
             KNOWLEDGE:\n{}",
            question, notes_block
        );

        let response = llm
            .generate(&prompt, None)
            .await
            .map_err(|e| anyhow::anyhow!("LLM reasoning failed: {}", e))?;

        let text = response.trim();
        let parsed: serde_json::Value = agent_brain_llm::llm::first_json_object(text, &["answer"])
            .unwrap_or_else(|| {
                serde_json::json!({
                    "answer": text,
                    "inferences": [],
                    "confidence": 0.5,
                    "gaps": []
                })
            });

        let answer = parsed
            .get("answer")
            .and_then(|v| v.as_str())
            .unwrap_or(text)
            .to_string();
        let inferences: Vec<String> = parsed
            .get("inferences")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let confidence = parsed
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5);
        let gaps: Vec<String> = parsed
            .get("gaps")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Cited links must be in the notes it was given (Phase R step 3).
        let (answer, inferences, _) = super::exact::check_answer(&answer, &inferences, &prompt);

        // Store inference note and create DERIVED_FROM edges
        let inference_note_id = if store_inference && !inferences.is_empty() {
            let inference_content = format!(
                "Q: {}\n\nAnswer: {}\n\nInferences:\n{}",
                question,
                answer,
                inferences
                    .iter()
                    .enumerate()
                    .map(|(i, inf)| format!("{}. {}", i + 1, inf))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            // source_context is a short provenance tag for grouping/analytics, not a
            // payload field — the full question is already in the note content above.
            match self
                .store_note_raw(
                    &inference_content,
                    Some("inference"),
                    Some("reason"),
                    None,
                    Some(ProvenanceFlag::SynthesisInference),
                )
                .await
            {
                Ok((inf_id, _)) => {
                    // Create DERIVED_FROM edges to source notes
                    let source_ids: Vec<String> = notes.iter().map(|(id, _)| id.clone()).collect();
                    if !source_ids.is_empty() {
                        let link_cypher = r#"
                        MATCH (inf:Note {id: $inf_id})
                        MATCH (src:Note) WHERE src.id IN $source_ids
                        MERGE (inf)-[:DERIVED_FROM]->(src)
                        "#;
                        let _ = self
                            .neo4j
                            .run(
                                neo4rs::query(link_cypher)
                                    .param("inf_id", inf_id.clone())
                                    .param("source_ids", source_ids),
                            )
                            .await;
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

    /// Store an inference note for a context-grounded `reason()` call.
    /// Unlike the RAG path there are no retrieved source notes, so no
    /// DERIVED_FROM edges are created. Returns the note id on success.
    pub async fn store_context_inference(
        &self,
        question: &str,
        answer: &str,
        inferences: &[String],
    ) -> Option<String> {
        let inference_content = format!(
            "Q: {}\n\nAnswer: {}\n\nInferences:\n{}",
            question,
            answer,
            inferences
                .iter()
                .enumerate()
                .map(|(i, inf)| format!("{}. {}", i + 1, inf))
                .collect::<Vec<_>>()
                .join("\n")
        );
        match self
            .store_note_raw(
                &inference_content,
                Some("inference"),
                Some("reason"),
                None,
                Some(ProvenanceFlag::SynthesisInference),
            )
            .await
        {
            Ok((id, _)) => Some(id),
            Err(e) => {
                warn!("Failed to store context inference note: {}", e);
                None
            }
        }
    }

    /// Structured variant of `reason()` — returns a typed `ReasonOutput` with source refs,
    /// caveats, follow-up questions, and optional adversarial critic pass.
    /// The existing `reason()` is unchanged for backward compatibility with scheduler chains.
    pub async fn reason_structured(
        &self,
        question: &str,
        limit: usize,
        store_inference: bool,
        run_critic: bool,
    ) -> Result<ReasonOutput> {
        let llm = self.llm.as_ref().ok_or_else(|| {
            anyhow::anyhow!("LLM is required for reasoning but is not configured")
        })?;

        let notes = self
            .search_notes_with_ids(question, limit, 1)
            .await
            .unwrap_or_default();

        let notes_block = notes
            .iter()
            .enumerate()
            .map(|(i, (_, content))| format!("Note {}:\n{}", i + 1, content))
            .collect::<Vec<_>>()
            .join("\n\n---\n\n");

        let prompt = format!(
            "You are a reasoning engine. Given the following retrieved knowledge, answer the question \
             by logical inference. Clearly distinguish what is known vs inferred.\n\
             IMPORTANT: Respond in English only. All text fields in the JSON must be in English.\n\
             Output ONLY valid JSON (no markdown, no code fences):\n\
             {{\"answer\":\"...\",\"inferences\":[\"...\"],\"confidence\":0.0,\
             \"gaps\":[\"...\"],\"caveats\":[\"...\"],\"follow_up_questions\":[\"...\"]}}\n\n\
             QUESTION: {}\n\
             KNOWLEDGE:\n{}",
            question, notes_block
        );

        // Strict-JSON structured output with self-correcting retries. If the
        // model still can't produce valid JSON after retries, fall back to
        // wrapping its raw prose as the answer so reasoning degrades gracefully.
        let (parsed, text): (serde_json::Value, String) =
            match llm.generate_json(&prompt, None, &["answer"], 2).await {
                Ok(value) => {
                    let answer_text = value
                        .get("answer")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    (value, answer_text)
                }
                Err(e) => {
                    // WARN, not INFO: at LOG_LEVEL=info this is the only trace that
                    // a `reason` result is unstructured prose rather than a graded
                    // answer. On 2026-08-24 it logged at debug-adjacent volume while
                    // a chat session built an "intelligence report" on top of the
                    // fallback, and nothing in the reply or the log level said so.
                    warn!(
                        error = %e,
                        "Structured reasoning fell back to raw prose — confidence/gaps/caveats \
                         in this result are placeholders, NOT model-reported values"
                    );
                    let raw = llm
                        .generate(&prompt, None)
                        .await
                        .map_err(|e| anyhow::anyhow!("LLM reasoning failed: {}", e))?
                        .trim()
                        .to_string();
                    // confidence 0.0, not 0.5. The old 0.5 was a fabricated number
                    // that read like a measurement, and it sat exactly on the wrong
                    // side of `LOW_CONFIDENCE` in the chat marker, so a fallback
                    // answer was presented with no confidence warning at all. The
                    // same class of bug as the evaluator fabricating a 3.0 score.
                    let fallback = serde_json::json!({
                        "answer": raw,
                        "inferences": [],
                        "confidence": 0.0,
                        "gaps": [],
                        "caveats": [],
                        "follow_up_questions": [],
                        "structured_output_failed": true
                    });
                    (fallback, raw)
                }
            };

        let answer = parsed
            .get("answer")
            .and_then(|v| v.as_str())
            .unwrap_or(&text)
            .to_string();
        let inferences: Vec<String> = parsed
            .get("inferences")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut confidence = parsed
            .get("confidence")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5);
        let gaps: Vec<String> = parsed
            .get("gaps")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let mut caveats: Vec<String> = parsed
            .get("caveats")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();
        let follow_up_questions: Vec<String> = parsed
            .get("follow_up_questions")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        // Cited links must be in the notes it was given (Phase R step 3).
        let (answer, inferences, links) = super::exact::check_answer(&answer, &inferences, &prompt);
        if !links.not_in_sources.is_empty() {
            caveats.push(format!(
                "{} cited link(s) are not in the notes it was given and are marked{}",
                links.not_in_sources.len(),
                super::exact::NOT_IN_SOURCES
            ));
        }

        // Build source refs from retrieved notes.
        let sources: Vec<SourceRef> = notes
            .iter()
            .map(|(id, content)| SourceRef {
                note_id: id.clone(),
                preview: content.chars().take(120).collect(),
            })
            .collect();

        // Adversarial critic pass.
        let mut critic_counter_arguments = Vec::new();
        if run_critic {
            let (adj_confidence, counter_args) = self
                .run_critic_pass(llm, question, &answer, confidence)
                .await;
            confidence = adj_confidence;
            critic_counter_arguments = counter_args;
            if confidence < 0.3 {
                caveats.push("Low confidence after adversarial review".to_string());
            }
        }

        // Store inference note and create DERIVED_FROM edges.
        let inference_note_id = if store_inference && !inferences.is_empty() {
            let inference_content = format!(
                "Q: {}\n\nAnswer: {}\n\nInferences:\n{}",
                question,
                answer,
                inferences
                    .iter()
                    .enumerate()
                    .map(|(i, inf)| format!("{}. {}", i + 1, inf))
                    .collect::<Vec<_>>()
                    .join("\n")
            );
            // source_context is a short provenance tag for grouping/analytics, not a
            // payload field — the full question is already in the note content above.
            match self
                .store_note_raw(
                    &inference_content,
                    Some("inference"),
                    Some("reason"),
                    None,
                    Some(ProvenanceFlag::SynthesisInference),
                )
                .await
            {
                Ok((inf_id, _)) => {
                    let source_ids: Vec<String> = notes.iter().map(|(id, _)| id.clone()).collect();
                    if !source_ids.is_empty() {
                        let link_cypher = r#"
                        MATCH (inf:Note {id: $inf_id})
                        MATCH (src:Note) WHERE src.id IN $source_ids
                        MERGE (inf)-[:DERIVED_FROM]->(src)
                        "#;
                        let _ = self
                            .neo4j
                            .run(
                                neo4rs::query(link_cypher)
                                    .param("inf_id", inf_id.clone())
                                    .param("source_ids", source_ids),
                            )
                            .await;
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

        Ok(ReasonOutput {
            answer,
            sources,
            confidence,
            caveats,
            follow_up_questions,
            inferences,
            gaps,
            inference_note_id,
            critic_counter_arguments,
            structured_output_failed: parsed
                .get("structured_output_failed")
                .and_then(serde_json::Value::as_bool)
                .unwrap_or(false),
        })
    }

    /// Adversarial critic pass: challenges `initial_answer` with a skeptical persona.
    /// Confidence adjustment is clamped to `[-0.5, 0.0]`.
    pub(super) async fn run_critic_pass(
        &self,
        llm: &Arc<dyn LlmProvider>,
        question: &str,
        initial_answer: &str,
        initial_confidence: f64,
    ) -> (f64, Vec<String>) {
        let prompt = format!(
            "You are a skeptical critic. Respond in English only. All JSON field values must be in English.\n\
             Given the following question and answer, identify the \
             strongest counter-arguments and flaws. Be concise and specific.\n\
             Output ONLY valid JSON: \
             {{\"counter_arguments\":[\"...\"],\"confidence_adjustment\":-0.1}}\n\
             (confidence_adjustment must be between -0.5 and 0.0)\n\n\
             QUESTION: {}\nANSWER: {}",
            question, initial_answer
        );

        let response = match llm.generate(&prompt, None).await {
            Ok(r) => r,
            Err(e) => {
                warn!("Critic pass LLM call failed: {}", e);
                return (initial_confidence, Vec::new());
            }
        };

        let text = response.trim();
        let parsed: serde_json::Value = match agent_brain_llm::llm::first_json_object(text, &[]) {
            Some(v) => v,
            None => return (initial_confidence, Vec::new()),
        };

        let counter_args: Vec<String> = parsed
            .get("counter_arguments")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|v| v.as_str().map(String::from))
                    .collect()
            })
            .unwrap_or_default();

        let raw_adj = parsed
            .get("confidence_adjustment")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0)
            .clamp(-0.5, 0.0);

        let adjusted = (initial_confidence + raw_adj).clamp(0.0, 1.0);
        (adjusted, counter_args)
    }

    /// Create `Task` nodes for identified knowledge gaps, linking each to the triggering note
    /// via `GAP_IDENTIFIED_BY` edge. Skips gaps that already have an open matching task.
    pub async fn create_gap_tasks(
        &self,
        gaps: &[String],
        triggering_note_id: &str,
    ) -> Result<Vec<String>> {
        let mut task_ids = Vec::new();
        let timestamp = Utc::now().to_rfc3339();

        for gap in gaps {
            let snippet: String = gap.chars().take(60).collect();

            let check = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE t.goal CONTAINS $snippet \
                   AND t.status IN ['created', 'in_progress'] \
                 RETURN count(t) AS cnt",
            )
            .param("snippet", snippet.as_str());

            let existing: i64 = self
                .neo4j
                .execute(check)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                .unwrap_or(0);

            if existing > 0 {
                continue;
            }

            let task_id = Uuid::new_v4().to_string();
            let goal = format!("Fill knowledge gap: {}", gap);
            let context = format!("Triggered by note: {}", triggering_note_id);

            let create_q = neo4rs::query(
                "CREATE (t:Task {id: $id, goal: $goal, context: $ctx, \
                 workflow: 'fill-knowledge-gap', inputs_json: $inputs, \
                 status: 'created', created_at: datetime($ts), updated_at: datetime($ts)})",
            )
            .param("inputs", serde_json::json!({ "topic": gap }).to_string())
            .param("id", task_id.as_str())
            .param("goal", goal.as_str())
            .param("ctx", context.as_str())
            .param("ts", timestamp.as_str());

            if let Err(e) = self.neo4j.run(create_q).await {
                warn!("Failed to create gap task: {}", e);
                continue;
            }

            let link_q = neo4rs::query(
                "MATCH (t:Task {id: $tid}), (n:Note {id: $nid}) \
                 MERGE (t)-[:GAP_IDENTIFIED_BY]->(n)",
            )
            .param("tid", task_id.as_str())
            .param("nid", triggering_note_id);

            let _ = self.neo4j.run(link_q).await;

            info!(task_id = %task_id, gap = %snippet, "Created gap task");
            task_ids.push(task_id);
        }

        Ok(task_ids)
    }
}
