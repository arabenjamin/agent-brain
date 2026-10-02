//! Retrieval labelling: claims and source records are marked before they reach a model.

use super::*;

impl KnowledgeService {
    /// Prefix any retrieved `claim` note with its epistemic status.
    ///
    /// Retrieval is the only place this can be enforced. A claim that reaches the
    /// context window looking like every other note *is* every other note as far
    /// as the model is concerned, and it will restate it as fact — which is the
    /// exact failure the claim type exists to prevent. Labelling here covers every
    /// consumer of retrieval (`reason`, chains, chat) without each having to
    /// remember to do it.
    ///
    /// Non-claim notes pass through untouched, and a lookup failure degrades to
    /// unlabelled content rather than dropping the note.
    /// [`Self::label_claims`] over `{id, snippet, …}` rows (the `recall_recent`
    /// sample and bulk delivery). The label is a prefix, so a snippet already
    /// clipped for length keeps it.
    pub(super) async fn label_sample_rows(&self, mut rows: Vec<Value>) -> Vec<Value> {
        let pairs: Vec<(String, String)> = rows
            .iter()
            .map(|r| {
                (
                    r["id"].as_str().unwrap_or_default().to_string(),
                    r["snippet"].as_str().unwrap_or_default().to_string(),
                )
            })
            .collect();
        let labelled = self.label_claims(pairs).await;
        for (row, (_, text)) in rows.iter_mut().zip(labelled) {
            row["snippet"] = Value::String(text);
        }
        rows
    }

    pub(super) async fn label_claims(
        &self,
        merged: Vec<(String, String)>,
    ) -> Vec<(String, String)> {
        if merged.is_empty() {
            return merged;
        }
        let ids: Vec<String> = merged.iter().map(|(id, _)| id.clone()).collect();
        let rows = match self
            .neo4j
            .execute(
                neo4rs::query(
                    "MATCH (n:Note) WHERE n.id IN $ids \
                       AND COALESCE(n.note_type,'semantic') IN ['claim','source_record','news','unsourced_synthesis'] \
                     RETURN n.id AS id, COALESCE(n.note_type,'semantic') AS note_type, \
                            COALESCE(n.claim_status, 'unverified') AS status, \
                            COALESCE(n.asserted_by, '') AS asserted_by, \
                            COALESCE(toString(n.asserted_at), '') AS asserted_at, \
                            COALESCE(n.source_context, '') AS source_context, \
                            COALESCE(n.corroboration_tier, '') AS tier, \
                            COALESCE(n.claim_kind, '') AS kind, \
                            toString(n.created_at) AS created_at",
                )
                .param("ids", ids),
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "Provenance labelling lookup failed — returning unlabelled notes");
                return merged;
            }
        };

        let mut labels: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();
        for row in &rows {
            let Ok(id) = row.get::<String>("id") else {
                continue;
            };
            let note_type = row.get::<String>("note_type").unwrap_or_default();
            let created_at = row.get::<String>("created_at").unwrap_or_default();
            // How old the material is, in the label. A date alone requires the
            // model to know today's date and do the subtraction; a relative age
            // does not. This is the missing half of the ToRA-7B failure — a
            // Sept-2023 benchmark result was relayed as a current standing
            // because nothing in the context window said it was two years old.
            let age = crate::services::clock::age_from_iso(&created_at);
            let prefix = match note_type.as_str() {
                "claim" => crate::services::claims::label_claim(
                    "",
                    &row.get::<String>("status").unwrap_or_default(),
                    Some(&row.get::<String>("asserted_by").unwrap_or_default()),
                    Some(&row.get::<String>("asserted_at").unwrap_or_default()),
                    Some(&row.get::<String>("tier").unwrap_or_default()),
                    Some(&row.get::<String>("kind").unwrap_or_default()),
                    age.as_deref(),
                ),
                // A record of what an external source said is not the brain's own
                // knowledge, and typing it 'semantic' told every consumer it was.
                // 656 video summaries and 94 news briefs sat unlabelled next to
                // established facts, which is why extracting claims alone did not
                // change how `reason` read the material — the unlabelled copy of
                // the same assertion was still in the context window.
                // The brain's own reasoning, citing nothing. Typed 'semantic'
                // these read as knowledge the brain established, and there is
                // no other signal on them that says otherwise — no asserted_by,
                // no source_context, no URL in the body. The curiosity engine
                // wrote 380 of them before 2026-08-10, when the gap chain
                // searched only internal notes and so could not fill a gap by
                // construction. One of them — asserting a "clear correlation"
                // between HBM/CoWoS scarcity and SLM adoption — was retrieved
                // in chat on 2026-08-24 and relayed as a confirmed finding at
                // "Confidence: High", exactly inverting the synthesis note that
                // had concluded INSUFFICIENT EVIDENCE and spawned it. The label
                // has to say the note cites nothing: that absence is the defect,
                // and it is invisible in the content itself.
                "unsourced_synthesis" => {
                    let at: String = created_at.chars().take(10).collect();
                    let mut parts = vec![
                        "UNSOURCED SYNTHESIS — the brain's own reasoning, cites no source; not established knowledge"
                            .to_string(),
                    ];
                    if !at.is_empty() {
                        parts.push(at);
                    }
                    if let Some(age) = &age {
                        parts.push(age.clone());
                    }
                    format!("[{}]\n", parts.join(" · "))
                }
                _ => {
                    let src = row.get::<String>("source_context").unwrap_or_default();
                    let at: String = created_at.chars().take(10).collect();
                    let mut parts =
                        vec!["SOURCE RECORD — what a source said, not verified".to_string()];
                    if !src.is_empty() {
                        parts.push(src);
                    }
                    if !at.is_empty() {
                        parts.push(at);
                    }
                    if let Some(age) = &age {
                        parts.push(age.clone());
                    }
                    format!("[{}]\n", parts.join(" · "))
                }
            };
            labels.insert(id, prefix);
        }
        if labels.is_empty() {
            return merged;
        }

        merged
            .into_iter()
            .map(|(id, content)| match labels.get(&id) {
                Some(prefix) => (id, format!("{prefix}{content}")),
                None => (id, content),
            })
            .collect()
    }

    // =========================================================================
    // Cognitive layer — reason, audit_action, explain_reasoning
    // =========================================================================
}
