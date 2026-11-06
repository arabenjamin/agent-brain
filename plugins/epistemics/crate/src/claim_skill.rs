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
use agent_brain_plugin_api::protocol::tool_schema;
use agent_brain_plugin_api::{
    Llm, PluginContext, Search, Skill, ToolCallResult, ToolDefinition, async_trait,
};

use crate::db::{Db, query};
use crate::{claims, grounding, internal, said_here};

/// How many search results are gathered as candidate evidence per claim.
/// 8, not 6: echo pages are dropped before judging, and corroboration now
/// needs two CITED independent results rather than any two results.
const EVIDENCE_RESULTS: u8 = 8;

/// Why an internal claim is dropped or never searched.
const INTERNAL_PROBLEM: &str = "it is about the brain's own activity: its own ledger answers \
     that, and no web search can";

/// Why a said-here claim is never searched.
const SAID_HERE_PROBLEM: &str = "it says only that someone in the recording said this: the \
     recording it came from is the proof, and no web search adds to it";

/// Default number of claims a single `verify` sweep will process.
const DEFAULT_VERIFY_BATCH: usize = 5;

pub struct ClaimSkill {
    neo4j: Db,
    llm: Arc<dyn Llm>,
    search: Option<Arc<dyn Search>>,
    ctx: PluginContext,
}

/// The catalog model for the contradiction second look
/// (`CLAIM_CONTRADICTION_MODEL`); unset or blank keeps it on the verifier's own.
/// What a source with no tier counts as (SOURCE_QUALITY_PLAN.md § 3a).
const DEFAULT_SOURCE_TYPE: &str = "commentary";

/// How many claims to extract from one item of a source of this tier, from the
/// caller's `limits_by_type` (the workflow owns the numbers). A tier that
/// yields none still gives one claim from every `probe_every`-th item, picked
/// by a stable hash of `key` (the source note), so the source keeps a track
/// record to re-rank it by. Returns (limit, tier, probe).
fn extraction_limit(
    source_type: Option<&str>,
    limits: &serde_json::Map<String, Value>,
    probe_every: u64,
    key: &str,
) -> (usize, String, bool) {
    // An unresolved `{{steps.x.source_type}}` or an empty one is no tier.
    let tier = source_type
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty() && !t.starts_with("{{") && t != "null")
        .unwrap_or_else(|| DEFAULT_SOURCE_TYPE.to_string());
    let limit = limits
        .get(&tier)
        .or_else(|| limits.get(DEFAULT_SOURCE_TYPE))
        .and_then(Value::as_u64)
        .unwrap_or(0) as usize;
    if limit == 0 && probe_every > 0 {
        // FNV-1a: stable across builds, unlike DefaultHasher.
        let h = key.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        });
        if h % probe_every == 0 {
            return (1, tier, true);
        }
    }
    (limit, tier, false)
}

fn contradiction_model() -> Option<String> {
    std::env::var("CLAIM_CONTRADICTION_MODEL")
        .ok()
        .map(|m| m.trim().to_string())
        .filter(|m| !m.is_empty())
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
                 action=contested: claims a contradiction passed the gate for, waiting on the \
                 owner's review, with the quote and conflict. \
                 action=confirm_refutation / dismiss_refutation (`claim_id`, `reason`): the \
                 owner's verdict on a contested claim; only an owner request may give it. \
                 Claims are stored, never filtered: a claim's standing is recorded, not enforced \
                 by exclusion, so contested material stays inspectable."
                .to_string(),
            input_schema: tool_schema!(plugin "claim"),
        }
    }

    // =========================================================================
    // Actions
    // =========================================================================

    async fn handle_extract(&self, args: &Value) -> ToolCallResult {
        let Some(text) = args["text"].as_str().filter(|t| !t.trim().is_empty()) else {
            return ToolCallResult::error("`text` is required for action=extract");
        };
        let mut max = args["limit"].as_u64().unwrap_or(5) as usize;
        // An unresolved `{{_prev.id}}` substitutes to the empty string. Treat
        // that as "no source note" rather than passing it down to a MATCH that
        // silently finds nothing — the ASSERTED_IN edge is optional, and a
        // blank id should read as absent at the boundary where it arrives.
        let source_note_id = args["source_note_id"]
            .as_str()
            .filter(|s| !s.trim().is_empty());
        let source_context = args["source_context"].as_str();
        let fallback_by = args["asserted_by"].as_str();

        // Extraction proportional to the source's tier (SOURCE_QUALITY_PLAN.md
        // Phase 1): commentary is summarised and kept, but asserts nothing into
        // the verification queue, except a sample to keep its record.
        let mut tier: Option<String> = args["source_type"]
            .as_str()
            .map(|t| t.trim().to_ascii_lowercase())
            .filter(|t| !t.is_empty() && !t.starts_with("{{") && t != "null");
        let mut probe = false;
        if let Some(limits) = args["limits_by_type"].as_object() {
            let key = source_note_id.unwrap_or(text);
            let (n, t, p) = extraction_limit(
                tier.as_deref(),
                limits,
                args["probe_every"].as_u64().unwrap_or(0),
                key,
            );
            max = n;
            tier = Some(t);
            probe = p;
        }
        if let (Some(t), Some(id)) = (tier.as_deref(), source_note_id) {
            let _ = self
                .neo4j
                .run(
                    query("MATCH (n:Note {id: $id}) SET n.source_type = $t")
                        .param("id", id)
                        .param("t", t),
                )
                .await;
        }
        if max == 0 {
            return ToolCallResult::success_json(json!({
                "stored": 0,
                "claims": [],
                "source_type": tier,
                "answer": text,
                "note": format!(
                    "A {} source yields no claims: summarised and kept, not queued for verification.",
                    tier.as_deref().unwrap_or(DEFAULT_SOURCE_TYPE)
                ),
            }));
        }

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
                tier.as_deref(),
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
            "source_type": tier,
            "probe": probe,
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
                .map_or_else(|_| "unverified".to_string(), |s| s.as_str().to_string());
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
                        .map_or_else(|_| "unverified".to_string(), |s| s.as_str().to_string());
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
        // support still counts and the edge says it was unchecked: a router
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
                        .map_or_else(|_| "unverified".to_string(), |s| s.as_str().to_string());
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

        // Contradiction gate: a second, narrower look, which must quote the
        // cited results' conflicting words (`claims::confirm_contradiction`),
        // then the grounding model's veto: results that ground the claim do
        // not refute it. Not an independence gate (one credible refutation is
        // still worth surfacing), but the judge alone filed "the evidence does
        // not mention it" as contradicts: 53 of 68 refutations were wrong
        // (2026-10-22). Anything that fails stays unverified, never refuted.
        // What passes makes the claim contested, not refuted: the owner
        // confirms or dismisses it (`claim action=contested`, weekly review).
        let mut contradiction: Option<claims::Contradiction> = None;
        if verdict.verdict == "contradicts" {
            let document = grounding::cited_document(&evidence_text, &cited);
            let dismissed = claims::dismissed_sources(&self.neo4j, claim_id)
                .await
                .unwrap_or_default();
            let why = if document.is_empty() {
                Some(format!(
                    "cited sources {:?} are not among the search results' links",
                    verdict.sources
                ))
            } else if let Some(s) = cited.iter().find(|c| dismissed.contains(*c)) {
                Some(format!(
                    "the owner already dismissed a refutation of this claim from {s}"
                ))
            } else {
                let model = contradiction_model();
                match claims::confirm_contradiction(
                    self.llm.as_ref(),
                    model.as_deref(),
                    claim_text,
                    &document,
                )
                .await
                {
                    Ok(Some(c)) => if let Some(model) = grounding::model() {
                        match grounding::is_grounded(&model, &document, claim_text).await {
                            Ok(true) => Some(format!(
                                "{model} found the claim grounded in the cited results, so they do not refute it"
                            )),
                            Ok(false) => {
                                grounding_status = Some("not grounded".to_string());
                                contradiction = Some(c);
                                None
                            }
                            Err(e) => {
                                warn!(claim_id = %claim_id, error = %e, "Grounding veto could not run; contradiction counted unchecked");
                                grounding_status = Some(format!("unchecked: {e}"));
                                contradiction = Some(c);
                                None
                            }
                        }
                    } else {
                        contradiction = Some(c);
                        None
                    },
                    Ok(None) => Some(
                        "a second look found no words in the cited results that conflict with the claim"
                            .to_string(),
                    ),
                    Err(e) => Some(format!("the second look could not run: {e}")),
                }
            };
            if let Some(why) = why {
                warn!(claim_id = %claim_id, reason = %why, "Contradiction rejected");
                let status = claims::recompute_status(&self.neo4j, claim_id)
                    .await
                    .map_or_else(|_| "unverified".to_string(), |s| s.as_str().to_string());
                return json!({
                    "claim_id": claim_id,
                    "claim": claim_text,
                    "verdict": "contradiction_rejected",
                    "evidence_recorded": false,
                    "cited_sources": cited,
                    "reasoning": format!(
                        "the judge said contradicts ({}), but {why}",
                        verdict.reasoning.as_deref().unwrap_or("no reason given")
                    ),
                    "status": status
                });
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
            contradiction.as_ref(),
        )
        .await
        .unwrap_or(false);

        let status = claims::recompute_status(&self.neo4j, claim_id)
            .await
            .map_or_else(|_| "unverified".to_string(), |s| s.as_str().to_string());

        json!({
            "claim_id": claim_id,
            "claim": claim_text,
            "verdict": verdict.verdict,
            "cited_sources": cited,
            "counted_domains": domains,
            "echo_results_dropped": echoes_dropped,
            "evidence_recorded": attached,
            "grounding": grounding_status,
            "conflict": contradiction.map(|c| c.conflict),
            "reasoning": verdict.reasoning,
            "status": status
        })
    }

    async fn handle_verify(&self, args: &Value) -> ToolCallResult {
        let batch = args["limit"]
            .as_u64()
            .unwrap_or(DEFAULT_VERIFY_BATCH as u64) as usize;

        let mut skipped_said_here = 0;
        let targets: Vec<(String, String)> = if let Some(id) = args["claim_id"].as_str() {
            match self
                .neo4j
                .execute(
                    query(
                        "MATCH (c:Note {id: $id, note_type: 'claim'}) \
                         RETURN c.content AS content, c.claim_kind AS kind, \
                                c.asserted_by AS by, c.source_context AS ctx",
                    )
                    .param("id", id),
                )
                .await
            {
                Ok(rows) => match rows.first() {
                    Some(r) => {
                        let Ok(content) = r.get::<String>("content") else {
                            return ToolCallResult::error(format!("Claim {id} has no content"));
                        };
                        let (kind, by, ctx) = (
                            r.get::<String>("kind").ok(),
                            r.get::<String>("by").ok(),
                            r.get::<String>("ctx").ok(),
                        );
                        if said_here::rules().is_said_here(&said_here::ClaimFacts {
                            text: &content,
                            kind: kind.as_deref(),
                            asserted_by: by.as_deref(),
                            source_context: ctx.as_deref(),
                        }) {
                            if let Err(e) =
                                claims::mark_verify_skipped(&self.neo4j, id, "said_here").await
                            {
                                warn!(claim_id = %id, error = %e, "Could not mark said-here claim");
                            }
                            return ToolCallResult::success_json(json!({
                                "verified": 0,
                                "results": [{
                                    "claim_id": id,
                                    "claim": content,
                                    "verdict": "skipped_said_here",
                                    "note": SAID_HERE_PROBLEM,
                                }]
                            }));
                        }
                        vec![(id.to_string(), content)]
                    }
                    None => return ToolCallResult::error(format!("No claim with id {id}")),
                },
                Err(e) => return ToolCallResult::error(format!("Lookup failed: {e}")),
            }
        } else {
            match claims::skip_said_here(&self.neo4j, said_here::rules()).await {
                Ok(n) => skipped_said_here = n,
                Err(e) => warn!(error = %e, "Could not take said-here claims out of the sweep"),
            }
            match claims::unverified_claims(&self.neo4j, batch).await {
                Ok(c) => c,
                Err(e) => return ToolCallResult::error(format!("Backlog query failed: {e}")),
            }
        };

        if targets.is_empty() {
            return ToolCallResult::success_json(json!({
                "verified": 0,
                "results": [],
                "skipped_said_here": skipped_said_here,
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
            "skipped_said_here": skipped_said_here,
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

    async fn handle_contested(&self, args: &Value) -> ToolCallResult {
        let limit = args["limit"].as_u64().unwrap_or(30) as usize;
        match claims::contested_claims(&self.neo4j, limit).await {
            Ok(items) => ToolCallResult::success_json(json!({
                "count": items.len(),
                "claims": items,
                "note": "Each waits on the owner: claim action=confirm_refutation lets it refute; \
                         action=dismiss_refutation (with a reason) when the evidence does not contradict it."
            })),
            Err(e) => ToolCallResult::error(format!("Could not list contested claims: {e}")),
        }
    }

    /// The owner's verdict on a contested claim. Owner-only: the second look
    /// was right about half the time on what it let through (2026-11-05), so a
    /// refutation needs a person, and no agent or workflow may stand in.
    async fn handle_review(&self, args: &Value, confirm: bool) -> ToolCallResult {
        let action = if confirm {
            "confirm_refutation"
        } else {
            "dismiss_refutation"
        };
        if !self.llm.started_by_owner() {
            return ToolCallResult::error(format!(
                "{action} is the owner's decision: it runs only on an owner request (an \
                 authenticated chat turn or HBI action), not from an agent or a workflow."
            ));
        }
        let Some(id) = args["claim_id"].as_str().filter(|s| !s.is_empty()) else {
            return ToolCallResult::error(format!("{action} needs `claim_id`"));
        };
        let reason = args["reason"].as_str().unwrap_or("").trim();
        if !confirm && reason.is_empty() {
            return ToolCallResult::error(
                "dismiss_refutation needs a `reason`: why the evidence does not contradict the claim",
            );
        }
        match claims::review_contradictions(&self.neo4j, id, confirm, reason).await {
            Ok(0) => ToolCallResult::error(format!(
                "Claim {id} has no contradiction waiting on review (claim action=contested lists them)"
            )),
            Ok(n) => {
                let status = claims::recompute_status(&self.neo4j, id).await.map_or_else(
                    |e| format!("not recomputed: {e}"),
                    |s| s.as_str().to_string(),
                );
                ToolCallResult::success_json(json!({
                    "claim_id": id,
                    "reviewed": n,
                    "verdict": if confirm { "confirmed" } else { "dismissed" },
                    "status": status
                }))
            }
            Err(e) => ToolCallResult::error(format!("{action} failed for {id}: {e}")),
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
            Some("contested") => self.handle_contested(&args).await,
            Some("confirm_refutation") => self.handle_review(&args, true).await,
            Some("dismiss_refutation") => self.handle_review(&args, false).await,
            Some(other) => ToolCallResult::error(format!(
                "Unknown action `{other}`. Use extract, verify, list, sources, contested, \
                 confirm_refutation or dismiss_refutation."
            )),
            None => ToolCallResult::error("`action` is required"),
        };
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::extraction_limit;
    use serde_json::{Value, json};

    fn limits() -> serde_json::Map<String, Value> {
        json!({"primary": 3, "reporting": 3, "analysis": 1, "commentary": 0})
            .as_object()
            .unwrap()
            .clone()
    }

    #[test]
    fn a_tier_takes_its_own_limit() {
        assert_eq!(extraction_limit(Some("reporting"), &limits(), 0, "n1").0, 3);
        assert_eq!(extraction_limit(Some("Analysis"), &limits(), 0, "n1").0, 1);
        assert_eq!(
            extraction_limit(Some("commentary"), &limits(), 0, "n1").0,
            0
        );
    }

    #[test]
    fn no_tier_or_an_unresolved_one_counts_as_commentary() {
        for t in [
            None,
            Some(""),
            Some("{{steps.watch.source_type}}"),
            Some("null"),
        ] {
            let (n, tier, _) = extraction_limit(t, &limits(), 0, "n1");
            assert_eq!((n, tier.as_str()), (0, "commentary"), "{t:?}");
        }
        // A tier the limits do not name falls back to commentary's.
        assert_eq!(extraction_limit(Some("reference"), &limits(), 0, "n1").0, 0);
    }

    #[test]
    fn a_zero_tier_samples_about_one_item_in_n() {
        let probed = (0..1000)
            .filter(|i| extraction_limit(Some("commentary"), &limits(), 5, &format!("note-{i}")).2)
            .count();
        assert!((150..250).contains(&probed), "{probed} of 1000");
        // Stable: the same item always decides the same way.
        let a = extraction_limit(Some("commentary"), &limits(), 5, "note-7");
        assert_eq!(
            a,
            extraction_limit(Some("commentary"), &limits(), 5, "note-7")
        );
        // A tier with claims is never "probed".
        assert!(!extraction_limit(Some("reporting"), &limits(), 5, "note-7").2);
    }
}
