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
                       AND (COALESCE(n.note_type,'semantic') IN ['claim','source_record','news','unsourced_synthesis'] \
                            OR n.verification = 'failed') \
                     RETURN n.id AS id, COALESCE(n.note_type,'semantic') AS note_type, \
                            COALESCE(n.verification, '') AS verification, \
                            COALESCE(n.verification_reason, '') AS verification_reason, \
                            n.content CONTAINS 'http' AS has_links, \
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
            let age = agent_brain_graph::clock::age_from_iso(&created_at);
            let failed = row.get::<String>("verification").unwrap_or_default() == "failed";
            let prefix = match note_type.as_str() {
                // Checked before the type: an agent answer that failed its
                // verifier is not "the brain's reasoning" but a report whose
                // test results and file claims were found false or unchecked.
                // On 2026-10-07 a deep dive whose code did not compile was
                // relayed in chat as "Success… the test passed… what was
                // actually built", under an UNSOURCED SYNTHESIS label that
                // never mentioned the failure.
                _ if failed => failed_verification_label(
                    &row.get::<String>("verification_reason").unwrap_or_default(),
                    &created_at,
                    age.as_deref(),
                ),
                "claim" => super::claim_label::label_claim(
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
                // A note that lists links is told apart: "cites no source" next
                // to a SOURCES block reads as a wrong label and gets discounted.
                "unsourced_synthesis" => {
                    let at: String = created_at.chars().take(10).collect();
                    let what = if row.get::<bool>("has_links").unwrap_or(false) {
                        "the brain's own reasoning; the links it lists were not checked against its claims"
                    } else {
                        "the brain's own reasoning, cites no source"
                    };
                    let mut parts = vec![format!(
                        "UNSOURCED SYNTHESIS — {what}; not established knowledge"
                    )];
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

/// The retrieval prefix for a note that failed its verifier.
pub(super) fn failed_verification_label(
    reason: &str,
    created_at: &str,
    age: Option<&str>,
) -> String {
    let reason = if reason.trim().is_empty() {
        "see the note"
    } else {
        reason.trim()
    };
    let mut parts = vec![format!(
        "FAILED VERIFICATION — {reason}. Its test results and code claims are false or \
         unchecked; nothing in it was built, run successfully, or changed in the codebase"
    )];
    let at: String = created_at.chars().take(10).collect();
    if !at.is_empty() {
        parts.push(at);
    }
    if let Some(age) = age {
        parts.push(age.to_string());
    }
    format!("[{}]\n", parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::failed_verification_label;

    #[test]
    fn failed_verification_label_names_the_reason_and_disclaims_the_work() {
        let label = failed_verification_label(
            "the answer's own Rust code, re-run by the verifier, did not pass: Compilation failed",
            "2026-10-07T16:58:27Z",
            Some("1h old"),
        );
        assert!(label.starts_with("[FAILED VERIFICATION — the answer's own Rust code"));
        assert!(label.contains("nothing in it was built"));
        assert!(label.contains("· 2026-10-07 · 1h old]"));
        assert!(label.ends_with("]\n"));
    }

    #[test]
    fn failed_verification_label_without_reason_still_says_failed() {
        let label = failed_verification_label("  ", "", None);
        assert!(label.starts_with("[FAILED VERIFICATION — see the note."));
    }
}
