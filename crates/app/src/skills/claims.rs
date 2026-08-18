//! Claim Skill — extract assertions, gather evidence, and record epistemic status.
//!
//! One tool (`claim`) with four actions, following the codebase's merged-action
//! pattern (`manage_job`, `reason`, `context`, `resource`).
//!
//! Verification reuses `SearchSkill` rather than re-implementing search: the
//! engine failover ladder, the usage ledger, and the source-list restriction all
//! apply to evidence gathering exactly as they do everywhere else.

use async_trait::async_trait;
use serde_json::{Value, json};
use std::sync::Arc;
use tracing::{info, warn};

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::claims;
use crate::services::traits::LlmProvider;
use crate::skills::Skill;
use crate::skills::search::SearchSkill;
use agent_brain_protocol::{Content, ToolCallResult, ToolDefinition};

/// How many search results are gathered as candidate evidence per claim.
const EVIDENCE_RESULTS: u8 = 6;

/// Default number of claims a single `verify` sweep will process.
const DEFAULT_VERIFY_BATCH: usize = 5;

pub struct ClaimSkill {
    neo4j: Neo4jClient,
    llm: Arc<dyn LlmProvider>,
    search: SearchSkill,
}

impl ClaimSkill {
    pub fn new(
        neo4j: Neo4jClient,
        llm: Arc<dyn LlmProvider>,
        telemetry: Option<TelemetryClient>,
    ) -> Self {
        let search = SearchSkill::new(telemetry, Some(neo4j.clone()));
        Self { neo4j, llm, search }
    }

    fn claim_def() -> ToolDefinition {
        ToolDefinition {
            name: "claim".to_string(),
            description: "Track factual assertions and what the evidence says about them. \
                 action=extract: pull checkable claims out of source text and store them as \
                 unverified, attributed, linked to the source note. \
                 action=verify: gather independent evidence for unverified claims and record \
                 whether it supports or contradicts them — never edits the claim itself. \
                 action=list: list claims by status. \
                 action=sources: per-source claim counts by status. \
                 Claims are stored, never filtered: a claim's standing is recorded, not enforced \
                 by exclusion, so contested material stays inspectable."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["extract", "verify", "list", "sources"],
                        "description": "extract: find claims in text. verify: gather evidence. list: show claims. sources: per-source breakdown."
                    },
                    "text": {
                        "type": "string",
                        "description": "extract only: the source text to extract claims from."
                    },
                    "source_note_id": {
                        "type": "string",
                        "description": "extract only: id of the note the text came from; links each claim via ASSERTED_IN."
                    },
                    "source_context": {
                        "type": "string",
                        "description": "extract only: provenance label stored on each claim (e.g. 'video_learning')."
                    },
                    "asserted_by": {
                        "type": "string",
                        "description": "extract only: fallback attribution (e.g. the channel) when the text does not name a source."
                    },
                    "claim_id": {
                        "type": "string",
                        "description": "verify only: verify one specific claim. Omit to sweep the unverified backlog."
                    },
                    "status": {
                        "type": "string",
                        "enum": ["unverified", "corroborated", "disputed", "refuted"],
                        "description": "list only: filter by epistemic status."
                    },
                    "limit": {
                        "type": "integer",
                        "description": "extract: max claims (default 5). verify: batch size (default 5). list: max rows (default 20)."
                    }
                },
                "required": ["action"]
            }),
        }
    }

    // =========================================================================
    // Actions
    // =========================================================================

    async fn handle_extract(&self, args: &Value) -> ToolCallResult {
        let Some(text) = args["text"].as_str().filter(|t| !t.trim().is_empty()) else {
            return ToolCallResult::error("`text` is required for action=extract");
        };
        let max = args["limit"].as_u64().unwrap_or(5) as usize;
        // An unresolved `{{_prev.id}}` substitutes to the empty string. Treat
        // that as "no source note" rather than passing it down to a MATCH that
        // silently finds nothing — the ASSERTED_IN edge is optional, and a
        // blank id should read as absent at the boundary where it arrives.
        let source_note_id = args["source_note_id"]
            .as_str()
            .filter(|s| !s.trim().is_empty());
        let source_context = args["source_context"].as_str();
        let fallback_by = args["asserted_by"].as_str();

        let extracted = match claims::extract_claims(self.llm.as_ref(), text, max).await {
            Ok(c) => c,
            Err(e) => return ToolCallResult::error(format!("Claim extraction failed: {e}")),
        };

        if extracted.is_empty() {
            return ToolCallResult::success_json(json!({
                "stored": 0,
                "claims": [],
                "answer": text,
                "note": "No independently checkable factual assertions found in this source."
            }));
        }

        let mut stored = Vec::new();
        for mut c in extracted {
            // Fall back to the channel/outlet when the text itself names no source.
            if c.asserted_by.as_deref().unwrap_or("").is_empty() {
                c.asserted_by = fallback_by.map(String::from);
            }
            // Embed the claim so it is retrievable on its own terms, not only
            // through the note it came from.
            let embedding = self.llm.embed(&c.claim).await.ok();
            match claims::store_claim(
                &self.neo4j,
                &c,
                source_note_id,
                source_context,
                None,
                embedding,
            )
            .await
            {
                Ok(id) => stored.push(json!({
                    "id": id,
                    "claim": c.claim,
                    "asserted_by": c.asserted_by,
                    "status": "unverified"
                })),
                Err(e) => warn!(error = %e, "Failed to store claim"),
            }
        }

        info!(count = stored.len(), "Extracted and stored claims");
        // "answer" echoes the source text so extraction is TRANSPARENT to a
        // chain: `{{_prev}}` in the next step yields the same content this step
        // received, exactly as store_note and notify_user already do. Without
        // it, inserting a claim step mid-chain silently replaces the payload
        // with claim metadata — which is what would have shipped the daily news
        // brief to the user as `{"stored":6,"claims":[…]}`.
        ToolCallResult::success_json(json!({
            "stored": stored.len(),
            "claims": stored,
            "answer": text,
            "note": "Stored as unverified. Run claim(action=verify) to gather evidence."
        }))
    }

    /// Who a claim is attributed to, for the self-reference check.
    async fn claim_asserted_by(&self, claim_id: &str) -> Option<String> {
        self.neo4j
            .execute(
                neo4rs::query("MATCH (c:Note {id: $id}) RETURN c.asserted_by AS by")
                    .param("id", claim_id),
            )
            .await
            .ok()?
            .first()
            .and_then(|r| r.get::<String>("by").ok())
            .filter(|s| !s.is_empty())
    }

    /// Gather evidence for one claim and record the verdict.
    async fn verify_one(&self, claim_id: &str, claim_text: &str) -> Value {
        // 1. Search for independent coverage of the claim.
        let search_result = self
            .search
            .execute(
                "search_web",
                Some(json!({ "query": claim_text, "count": EVIDENCE_RESULTS })),
            )
            .await;

        let evidence_text = match search_result {
            Some(r) if r.is_error != Some(true) => r
                .content
                .first()
                .and_then(|c| match c {
                    Content::Text { text } => Some(text.clone()),
                    _ => None,
                })
                .unwrap_or_default(),
            Some(r) => {
                let err = r
                    .content
                    .first()
                    .and_then(|c| match c {
                        Content::Text { text } => Some(text.clone()),
                        _ => None,
                    })
                    .unwrap_or_default();
                return json!({ "claim_id": claim_id, "error": format!("evidence search failed: {err}") });
            }
            None => return json!({ "claim_id": claim_id, "error": "search unavailable" }),
        };

        if evidence_text.trim().is_empty() || evidence_text.trim() == "[]" {
            // No evidence found is NOT refutation — the claim stays unverified.
            // Treating absence of evidence as evidence of absence would quietly
            // mark every niche-but-true claim false.
            return json!({
                "claim_id": claim_id,
                "verdict": "no_evidence_found",
                "status": "unverified"
            });
        }

        // 2. Record the evidence itself, so the verdict is auditable later.
        let evidence_note = format!("Evidence gathered for claim: {claim_text}\n\n{evidence_text}");
        let evidence_id = match self
            .neo4j
            .store_episodic_note(&evidence_note, Some("claim_evidence"))
            .await
        {
            Ok(id) => id,
            Err(e) => {
                return json!({ "claim_id": claim_id, "error": format!("could not store evidence: {e}") });
            }
        };

        // 3. Judge it — strictly from the gathered evidence.
        let verdict = match claims::assess_evidence(self.llm.as_ref(), claim_text, &evidence_text)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                return json!({ "claim_id": claim_id, "error": format!("assessment failed: {e}") });
            }
        };

        // Independence gate. A model reading a claim's own promoters will happily
        // answer "supports" — observed 2026-08-10, where a claim about Skywatcher
        // was "corroborated" by psionicresearch.com and skywatcher.ai, the
        // subject's own site. Recording that as corroboration is worse than
        // recording nothing: it launders an assertion through a verification that
        // never happened. Contradiction is NOT gated — a single credible refutation
        // is worth surfacing, and gating it would bias the system toward belief.
        let asserted_by = self.claim_asserted_by(claim_id).await;
        let independence = if verdict.verdict == "supports" {
            claims::check_independence(&evidence_text, claim_text, asserted_by.as_deref())
        } else {
            Ok(vec![])
        };

        if let Err(ref reason) = independence {
            warn!(claim_id = %claim_id, reason = %reason, "Support rejected — evidence not independent");
            let status = claims::recompute_status(&self.neo4j, claim_id)
                .await
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|_| "unverified".to_string());
            return json!({
                "claim_id": claim_id,
                "claim": claim_text,
                "verdict": "support_rejected_not_independent",
                "evidence_recorded": false,
                "reasoning": reason,
                "status": status
            });
        }

        let attached = claims::attach_evidence(
            &self.neo4j,
            claim_id,
            &evidence_id,
            &verdict.verdict,
            verdict.source.as_deref(),
            &claims::evidence_domains(&evidence_text),
        )
        .await
        .unwrap_or(false);

        let status = claims::recompute_status(&self.neo4j, claim_id)
            .await
            .map(|s| s.as_str().to_string())
            .unwrap_or_else(|_| "unverified".to_string());

        json!({
            "claim_id": claim_id,
            "claim": claim_text,
            "verdict": verdict.verdict,
            "evidence_domains": claims::evidence_domains(&evidence_text),
            "evidence_recorded": attached,
            "reasoning": verdict.reasoning,
            "status": status
        })
    }

    async fn handle_verify(&self, args: &Value) -> ToolCallResult {
        let batch = args["limit"]
            .as_u64()
            .unwrap_or(DEFAULT_VERIFY_BATCH as u64) as usize;

        let targets: Vec<(String, String)> = if let Some(id) = args["claim_id"].as_str() {
            match self
                .neo4j
                .execute(
                    neo4rs::query(
                        "MATCH (c:Note {id: $id, note_type: 'claim'}) RETURN c.content AS content",
                    )
                    .param("id", id),
                )
                .await
            {
                Ok(rows) => match rows.first().and_then(|r| r.get::<String>("content").ok()) {
                    Some(content) => vec![(id.to_string(), content)],
                    None => return ToolCallResult::error(format!("No claim with id {id}")),
                },
                Err(e) => return ToolCallResult::error(format!("Lookup failed: {e}")),
            }
        } else {
            match claims::unverified_claims(&self.neo4j, batch).await {
                Ok(c) => c,
                Err(e) => return ToolCallResult::error(format!("Backlog query failed: {e}")),
            }
        };

        if targets.is_empty() {
            return ToolCallResult::success_json(json!({
                "verified": 0,
                "results": [],
                "note": "No unverified claims outstanding."
            }));
        }

        let mut results = Vec::new();
        for (id, text) in &targets {
            // Stamp the attempt before making it. `verify_one` has several early
            // returns (search unavailable, evidence unstorable, assessment failed)
            // and none of them change claim_status, so a claim stamped only on
            // success would be re-selected by every future sweep and block the
            // backlog behind it — the exact deadlock this cursor exists to break.
            if let Err(e) = claims::mark_verify_attempt(&self.neo4j, id).await {
                warn!(claim_id = %id, error = %e, "Could not stamp verification attempt");
            }
            results.push(self.verify_one(id, text).await);
        }

        info!(count = results.len(), "Claim verification sweep complete");
        ToolCallResult::success_json(json!({
            "verified": results.len(),
            "results": results
        }))
    }

    async fn handle_list(&self, args: &Value) -> ToolCallResult {
        let limit = args["limit"].as_u64().unwrap_or(20) as i64;
        let status = args["status"].as_str();

        let cypher = if status.is_some() {
            "MATCH (c:Note {note_type: 'claim'}) \
             WHERE COALESCE(c.claim_status, 'unverified') = $status \
             RETURN c.id AS id, c.content AS content, \
                    COALESCE(c.claim_status,'unverified') AS status, \
                    COALESCE(c.asserted_by,'') AS asserted_by, \
                    COALESCE(c.corroborating_count,0) AS corroborating, \
                    COALESCE(c.contradicting_count,0) AS contradicting \
             ORDER BY c.created_at DESC LIMIT $limit"
        } else {
            "MATCH (c:Note {note_type: 'claim'}) \
             RETURN c.id AS id, c.content AS content, \
                    COALESCE(c.claim_status,'unverified') AS status, \
                    COALESCE(c.asserted_by,'') AS asserted_by, \
                    COALESCE(c.corroborating_count,0) AS corroborating, \
                    COALESCE(c.contradicting_count,0) AS contradicting \
             ORDER BY c.created_at DESC LIMIT $limit"
        };

        let mut q = neo4rs::query(cypher).param("limit", limit);
        if let Some(s) = status {
            q = q.param("status", s);
        }

        match self.neo4j.execute(q).await {
            Ok(rows) => {
                let items: Vec<Value> = rows
                    .iter()
                    .map(|r| {
                        json!({
                            "id":            r.get::<String>("id").unwrap_or_default(),
                            "claim":         r.get::<String>("content").unwrap_or_default(),
                            "status":        r.get::<String>("status").unwrap_or_default(),
                            "asserted_by":   r.get::<String>("asserted_by").unwrap_or_default(),
                            "corroborating": r.get::<i64>("corroborating").unwrap_or(0),
                            "contradicting": r.get::<i64>("contradicting").unwrap_or(0),
                        })
                    })
                    .collect();
                ToolCallResult::success_json(json!({ "count": items.len(), "claims": items }))
            }
            Err(e) => ToolCallResult::error(format!("List failed: {e}")),
        }
    }

    async fn handle_sources(&self) -> ToolCallResult {
        match claims::source_claim_profile(&self.neo4j).await {
            Ok(v) => ToolCallResult::success_json(v),
            Err(e) => ToolCallResult::error(format!("Source profile failed: {e}")),
        }
    }
}

#[async_trait]
impl Skill for ClaimSkill {
    fn name(&self) -> &str {
        "Claims"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::claim_def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        if tool_name != "claim" {
            return None;
        }
        let args = arguments.unwrap_or_default();
        let result = match args["action"].as_str() {
            Some("extract") => self.handle_extract(&args).await,
            Some("verify") => self.handle_verify(&args).await,
            Some("list") => self.handle_list(&args).await,
            Some("sources") => self.handle_sources().await,
            Some(other) => ToolCallResult::error(format!(
                "Unknown action `{other}`. Use extract, verify, list, or sources."
            )),
            None => ToolCallResult::error("`action` is required"),
        };
        Some(result)
    }
}
