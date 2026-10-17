//! Claim Skill — extract assertions, gather evidence, and record epistemic status.
//!
//! One tool (`claim`) with four actions, following the codebase's merged-action
//! pattern (`manage_job`, `reason`, `context`, `resource`).
//!
//! Verification uses the core's search (the host's `search` capability, which
//! is `search_web`) rather than re-implementing it: the engine failover ladder,
//! the usage ledger, and the source-list restriction all apply to evidence
//! gathering exactly as they do everywhere else.

use serde_json::{Value, json};
use std::sync::Arc;
use tracing::{info, warn};

use agent_brain_plugin_api::notes::source_reference;
use agent_brain_plugin_api::{
    Llm, PluginContext, Search, Skill, ToolCallResult, ToolDefinition, async_trait,
};

use crate::db::{Db, query};
use crate::{claims, grounding, internal};

/// How many search results are gathered as candidate evidence per claim.
/// 8, not 6: echo pages are dropped before judging, and corroboration now
/// needs two CITED independent results rather than any two results.
const EVIDENCE_RESULTS: u8 = 8;

/// Why an internal claim is dropped or never searched.
const INTERNAL_PROBLEM: &str = "it is about the brain's own activity: its own ledger answers \
     that, and no web search can";

/// Default number of claims a single `verify` sweep will process.
const DEFAULT_VERIFY_BATCH: usize = 5;

pub struct ClaimSkill {
    neo4j: Db,
    llm: Arc<dyn Llm>,
    search: Option<Arc<dyn Search>>,
    ctx: PluginContext,
}

impl ClaimSkill {
    pub fn new(neo4j: Db, llm: Arc<dyn Llm>, ctx: PluginContext) -> Self {
        Self {
            neo4j,
            llm,
            search: ctx.search.clone(),
            ctx,
        }
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
                    "exclude_sections": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "extract only: Markdown sections to leave out, by heading prefix (case-insensitive), e.g. a brief's self-report sections. Each runs to the next heading."
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

        // Sections about the brain itself (a brief's "what I did", "system
        // status") are its own bookkeeping, not assertions about the world.
        let exclude: Vec<String> = match &args["exclude_sections"] {
            Value::Array(a) => a
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            Value::String(s) => s.split(',').map(|p| p.trim().to_string()).collect(),
            _ => vec![],
        };
        let (kept_text, removed_sections) = internal::strip_sections(text, &exclude);
        if !removed_sections.is_empty() {
            info!(sections = ?removed_sections, "Claim extraction: left out sections");
        }

        // What the source is (title, URL, outlet), stored on every claim so a
        // reader sees where it came from without following ASSERTED_IN. The text
        // usually carries it; otherwise the source note does.
        let mut source = source_reference(text);
        if source.title.is_none()
            && let Some(id) = source_note_id
            && let Some(content) = self.note_content(id).await
        {
            source = source_reference(&content);
        }

        let mut extraction = match claims::extract_claims(
            self.llm.as_ref(),
            &kept_text,
            max,
            source.title.as_deref(),
        )
        .await
        {
            Ok(c) => c,
            Err(e) => return ToolCallResult::error(format!("Claim extraction failed: {e}")),
        };
        // The prompt says so too; this is the check that holds when a model
        // does not listen.
        let rules = internal::rules();
        let (internal_claims, world): (Vec<_>, Vec<_>) = extraction
            .claims
            .into_iter()
            .partition(|c| rules.is_internal(&c.claim));
        extraction.claims = world;
        extraction
            .dropped
            .extend(internal_claims.into_iter().map(|c| claims::DroppedClaim {
                claim: c.claim,
                problems: vec![INTERNAL_PROBLEM.to_string()],
            }));
        let (repaired, dropped) = (extraction.repaired, extraction.dropped);
        for d in &dropped {
            warn!(claim = %d.claim, problems = ?d.problems, "Claim dropped");
        }

        if extraction.claims.is_empty() {
            return ToolCallResult::success_json(json!({
                "stored": 0,
                "claims": [],
                "dropped": dropped,
                "excluded_sections": removed_sections,
                "answer": text,
                "note": if dropped.is_empty() {
                    "No independently checkable factual assertions found in this source."
                } else {
                    "Every claim found was dropped (see dropped for why)."
                }
            }));
        }

        let mut stored = Vec::new();
        for mut c in extraction.claims {
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
                embedding,
                &source,
                self.ctx.current_task(),
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
            "repaired": repaired,
            "dropped": dropped,
            "excluded_sections": removed_sections,
            "answer": text,
            "note": "Stored as unverified. Run claim(action=verify) to gather evidence."
        }))
    }

    /// A note's text, for reading what source it records.
    async fn note_content(&self, id: &str) -> Option<String> {
        self.neo4j
            .execute(query("MATCH (n:Note {id: $id}) RETURN n.content AS c").param("id", id))
            .await
            .ok()?
            .first()
            .and_then(|r| r.get::<String>("c").ok())
    }

    /// Who a claim is attributed to, for the self-reference check.
    async fn claim_asserted_by(&self, claim_id: &str) -> Option<String> {
        self.neo4j
            .execute(
                query("MATCH (c:Note {id: $id}) RETURN c.asserted_by AS by").param("id", claim_id),
            )
            .await
            .ok()?
            .first()
            .and_then(|r| r.get::<String>("by").ok())
            .filter(|s| !s.is_empty())
    }

    /// Gather evidence for one claim and record the verdict.
    ///
    /// Every return path carries `claim` — the assertion in words. The happy
    /// paths always did, but the early returns (no evidence, search down,
    /// assessment failed) reported a bare `claim_id`, and those are exactly the
    /// outcomes a human needs to read: the `claims-<date>` sweep report is the
    /// only surface where verification is visible, and a page of UUIDs against
    /// "no_evidence_found" cannot be read, chatted about, or acted on. A result
    /// that says nothing happened must still say what it was that did not
    /// happen.
    async fn verify_one(&self, claim_id: &str, claim_text: &str) -> Value {
        // 0. A claim about the brain itself is answered by its own ledger, not
        //    the web: never search for it, and keep the sweep from picking it again.
        if internal::rules().is_internal(claim_text) {
            if let Err(e) = claims::mark_verify_skipped(&self.neo4j, claim_id, "internal").await {
                warn!(claim_id = %claim_id, error = %e, "Could not mark internal claim");
            }
            return json!({
                "claim_id": claim_id,
                "claim": claim_text,
                "verdict": "skipped_internal",
                "note": INTERNAL_PROBLEM,
            });
        }

        // 1. Search for independent coverage of the claim.
        let Some(search) = &self.search else {
            return json!({ "claim_id": claim_id, "claim": claim_text, "error": "search unavailable" });
        };
        let raw_evidence = match search.web(claim_text, EVIDENCE_RESULTS).await {
            Ok(text) => text,
            Err(err) => {
                return json!({ "claim_id": claim_id, "claim": claim_text, "error": format!("evidence search failed: {err}") });
            }
        };

        // Search pages that echo the claim back are the query, not evidence.
        let (evidence_text, echoes_dropped) = claims::drop_echo_results(&raw_evidence, claim_text);
        if echoes_dropped > 0 {
            info!(claim_id = %claim_id, dropped = echoes_dropped, "Dropped echo results before judging");
        }

        if evidence_text.trim().is_empty() || evidence_text.trim() == "[]" {
            // No evidence found is NOT refutation — the claim stays unverified.
            // Treating absence of evidence as evidence of absence would quietly
            // mark every niche-but-true claim false.
            return json!({
                "claim_id": claim_id,
                "claim": claim_text,
                "verdict": "no_evidence_found",
                "echo_results_dropped": echoes_dropped,
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
                return json!({ "claim_id": claim_id, "claim": claim_text, "error": format!("could not store evidence: {e}") });
            }
        };

        // 3. Judge it — strictly from the gathered evidence.
        let verdict = match claims::assess_evidence(self.llm.as_ref(), claim_text, &evidence_text)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                return json!({ "claim_id": claim_id, "claim": claim_text, "error": format!("assessment failed: {e}") });
            }
        };

        // The results the judge relied on, resolved against what the search
        // actually returned. Everything below counts only these.
        let cited = claims::resolve_cited_links(&verdict.sources, &evidence_text);

        // Provenance gate. A "supports" must cite a result the search actually
        // returned. A judge that names a note id, "the knowledge graph", or
        // nothing at all is corroborating from somewhere other than the
        // evidence — the shape the 2026-09-12 self-corroboration took.
        if verdict.verdict == "supports" && cited.is_empty() {
            warn!(claim_id = %claim_id, sources = ?verdict.sources, "Support rejected — cited source is not a search result");
            let status = claims::recompute_status(&self.neo4j, claim_id)
                .await
                .map(|s| s.as_str().to_string())
                .unwrap_or_else(|_| "unverified".to_string());
            return json!({
                "claim_id": claim_id,
                "claim": claim_text,
                "verdict": "support_rejected_unsourced",
                "evidence_recorded": false,
                "reasoning": format!(
                    "cited sources {:?} are not among the search results' links",
                    verdict.sources
                ),
                "status": status
            });
        }

        // Independence gate, over the CITED results only. A model reading a
        // claim's own promoters will happily answer "supports" — observed
        // 2026-08-10, where a claim about Skywatcher was "corroborated" by
        // skywatcher.ai, the subject's own site. Platform posts (facebook.com,
        // instagram.com) are cited but not counted. Contradiction is NOT
        // gated — a single credible refutation is worth surfacing, and gating
        // it would bias the system toward belief.
        let domains = if verdict.verdict == "supports" {
            let asserted_by = self.claim_asserted_by(claim_id).await;
            let roles = claims::load_domain_roles(&self.neo4j).await;
            let titles = claims::result_titles(&evidence_text);
            match claims::check_independence_titled(
                &cited,
                &titles,
                claim_text,
                asserted_by.as_deref(),
                &roles,
            ) {
                Ok(independent) => independent,
                Err(reason) => {
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
                        "cited_sources": cited,
                        "reasoning": reason,
                        "status": status
                    });
                }
            }
        } else {
            claims::link_domains(&cited)
        };

        // Grounding gate: do the cited results, read together, say what the
        // claim says? A second model, because the judge that picked the
        // results is the one being checked. When the check cannot run, the
        // support still counts and the edge says it was unchecked: an Ollama
        // outage must not stall verification, and must not hide either.
        let mut grounding_status: Option<String> = None;
        if verdict.verdict == "supports"
            && let Some(model) = grounding::model()
        {
            let document = grounding::cited_document(&evidence_text, &cited);
            match grounding::is_grounded(&model, &document, claim_text).await {
                Ok(true) => grounding_status = Some("grounded".to_string()),
                Ok(false) => {
                    warn!(claim_id = %claim_id, model = %model, "Support rejected — the cited results do not ground the claim");
                    let status = claims::recompute_status(&self.neo4j, claim_id)
                        .await
                        .map(|s| s.as_str().to_string())
                        .unwrap_or_else(|_| "unverified".to_string());
                    return json!({
                        "claim_id": claim_id,
                        "claim": claim_text,
                        "verdict": "support_rejected_ungrounded",
                        "evidence_recorded": false,
                        "cited_sources": cited,
                        "reasoning": format!(
                            "the judge said supports ({}), but {model} found the claim not grounded in the cited results read together",
                            verdict.reasoning.as_deref().unwrap_or("no reason given")
                        ),
                        "status": status
                    });
                }
                Err(e) => {
                    warn!(claim_id = %claim_id, error = %e, "Grounding check could not run; support counted unchecked");
                    grounding_status = Some(format!("unchecked: {e}"));
                }
            }
        }

        let attached = claims::attach_evidence(
            &self.neo4j,
            claim_id,
            &evidence_id,
            &verdict.verdict,
            &cited,
            &domains,
            grounding_status.as_deref(),
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
            "cited_sources": cited,
            "counted_domains": domains,
            "echo_results_dropped": echoes_dropped,
            "evidence_recorded": attached,
            "grounding": grounding_status,
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
                    query(
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

        let mut q = query(cypher).param("limit", limit);
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
