//! Claims — epistemic status for assertions the brain ingests.
//!
//! The brain learns from sources of wildly varying reliability: CRS reports and
//! late-night cable segments both arrive as text and, before this module, both
//! were stored flat as `semantic` knowledge with equal standing. Retrieval then
//! handed them to `reason` indistinguishably, so an assertion made once on a talk
//! show could come back out phrased as established fact.
//!
//! The response is *not* to filter sources. Dropping fringe material at ingest
//! also destroys the record needed to notice that a narrative is being pushed —
//! you cannot detect a coordinated shift in messaging you never stored. Instead,
//! assertions are separated from facts and carry their evidentiary state:
//!
//! ```text
//! (:Note {note_type:'claim', claim_status, asserted_by, asserted_at})
//!   -[:ASSERTED_IN]->      (:Note)      the source note it came from
//!   -[:CORROBORATED_BY]->  (:Note)      independent supporting evidence
//!   -[:CONTRADICTED_BY]->  (:Note)      independent contradicting evidence
//! ```
//!
//! Three properties of the design matter:
//!
//! - **Status is derived, never asserted.** `recompute_status` reads the edges,
//!   so a claim's standing can't drift away from its evidence.
//! - **Verification never edits the claim.** `verify_claim` attaches evidence and
//!   recomputes status; the claim text is immutable. Correcting the record is not
//!   the same as rewriting it.
//! - **A disputed claim stays disputed.** Corroboration *and* contradiction does
//!   not collapse to a verdict. Premature resolution is how a knowledge base
//!   launders a contested question into a settled one.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use crate::repository::Neo4jClient;
use crate::services::traits::LlmProvider;

/// What the evidence currently says about a claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClaimStatus {
    /// No evidence gathered yet. The default, and honest about it.
    Unverified,
    /// Independent sources support it, none contradict.
    Corroborated,
    /// Support *and* contradiction both exist. A real state, not a failure.
    Disputed,
    /// Independent sources contradict it, none support.
    Refuted,
}

impl ClaimStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            ClaimStatus::Unverified => "unverified",
            ClaimStatus::Corroborated => "corroborated",
            ClaimStatus::Disputed => "disputed",
            ClaimStatus::Refuted => "refuted",
        }
    }

    /// Derive status from evidence counts. The only place status is decided.
    pub fn from_evidence(corroborating: usize, contradicting: usize) -> Self {
        match (corroborating, contradicting) {
            (0, 0) => ClaimStatus::Unverified,
            (_, 0) => ClaimStatus::Corroborated,
            (0, _) => ClaimStatus::Refuted,
            _ => ClaimStatus::Disputed,
        }
    }
}

/// A proposition extracted from a source, before it is stored.
#[derive(Debug, Clone, Deserialize)]
pub struct ExtractedClaim {
    /// The proposition itself, stated plainly and self-contained.
    pub claim: String,
    /// Who asserted it, as identified in the source.
    #[serde(default)]
    pub asserted_by: Option<String>,
    /// `event` | `attribution` | `mechanism` — see `kind_qualifier`.
    #[serde(default)]
    pub kind: Option<String>,
}

/// Render a claim for inclusion in an LLM context window.
///
/// This is the piece that makes the whole design work. A `claim` note that
/// reaches `reason` looking like any other note *is* any other note as far as the
/// model is concerned — the type would be cosmetic. The label travels with the
/// text so the model cannot restate the claim without also seeing its standing.
/// How to read a corroborated claim of this kind.
///
/// The distinction that "corroborated" alone destroys: confirming that a group
/// *demonstrated a technique* is not confirming that the technique *works*.
/// Reporting can establish the former and says nothing about the latter, yet
/// both render as "corroborated" without this qualifier.
pub fn kind_qualifier(kind: &str, status: &str) -> Option<String> {
    if status != "corroborated" {
        return None;
    }
    match kind {
        "attribution" => Some("corroborates that it was asserted, not that it is true".to_string()),
        "mechanism" => Some("corroborates the causal claim itself".to_string()),
        _ => None,
    }
}

/// `age` is a pre-rendered relative age ("11 months ago") supplied by the
/// caller rather than computed here, so this stays a pure function of its
/// inputs and its tests stay deterministic. An absolute date alone does not
/// tell a model whether a figure is current — see `services/clock.rs`.
pub fn label_claim(
    content: &str,
    status: &str,
    asserted_by: Option<&str>,
    at: Option<&str>,
    tier: Option<&str>,
    kind: Option<&str>,
    age: Option<&str>,
) -> String {
    let mut parts = vec![format!("CLAIM · {status}")];
    if let Some(k) = kind.filter(|k| !k.is_empty()) {
        parts.push(k.to_string());
        if let Some(q) = kind_qualifier(k, status) {
            parts.push(q);
        }
    }
    // The tier is what separates "two wire services agree" from "five aligned
    // outlets repeat one origin" — both of which read as plain "corroborated".
    if let Some(t) = tier.filter(|t| !t.is_empty() && *t != "none") {
        parts.push(t.to_string());
    }
    if let Some(by) = asserted_by.filter(|s| !s.is_empty()) {
        parts.push(format!("asserted by {by}"));
    }
    if let Some(at) = at.filter(|s| !s.is_empty()) {
        parts.push(at.chars().take(10).collect());
    }
    if let Some(age) = age.filter(|s| !s.is_empty()) {
        parts.push(age.to_string());
    }
    format!("[{}]\n{content}", parts.join(" · "))
}

/// Extract checkable propositions from a piece of source text.
///
/// Deliberately narrow: only assertions of fact that could in principle be
/// checked against another source. Opinions, predictions, and analysis are left
/// as ordinary note content — labelling those as unverifiable claims would make
/// the status field meaningless through sheer volume.
pub async fn extract_claims(
    llm: &dyn LlmProvider,
    text: &str,
    max_claims: usize,
) -> Result<Vec<ExtractedClaim>> {
    let prompt = format!(
        "Extract factual assertions from the SOURCE that a researcher could \
         independently verify or refute.\n\n\
         Include: specific, checkable statements of fact — events, quantities, \
         dates, attributions, causal claims about the world.\n\
         EXCLUDE: opinions, predictions, value judgements, analysis, and \
         anything already presented as uncertain by the source itself.\n\n\
         State each claim plainly and self-contained, so it can be understood \
         without the source. Do NOT editorialise, debunk, endorse, or soften \
         them — record what was asserted, exactly as asserted. An extraordinary \
         claim is recorded the same way as a mundane one.\n\n\
         Set asserted_by to whoever the SOURCE attributes the claim to (a person, \
         outlet, agency, or study). Use null when the source does not attribute it.\n\n\
         Classify each claim's kind by asking ONE question: if this claim were \
         fully confirmed, would that settle whether the underlying phenomenon is \
         real?\n\
         - \"attribution\": NO — confirming it establishes only that a named party \
         said, claimed, demonstrated, or showcased something. Use this whenever \
         the claim is about someone asserting or exhibiting a disputed or \
         extraordinary capability, even though their doing so is itself an event. \
         Example: \"Group G demonstrated a technique to summon UAPs\" is \
         attribution — confirming G did a demonstration says nothing about \
         whether the technique works.\n\
         - \"mechanism\": the claim IS the causal or efficacy assertion itself \
         (X causes Y; technique T produces effect E).\n\
         - \"event\": YES — a plain occurrence, publication, decision, or quantity \
         with nothing contested behind it (a hearing was held, a report was \
         released, a budget was N dollars).\n\n\
         Return at most {max_claims} claims, most significant first. If the source \
         contains no checkable factual assertions, return an empty array.\n\n\
         Respond with JSON only:\n\
         {{\"claims\": [{{\"claim\": \"...\", \"asserted_by\": \"...\", \"kind\": \"event|attribution|mechanism\"}}]}}\n\n\
         SOURCE:\n{text}"
    );

    let value = llm
        .generate_json(
            &prompt,
            Some("You extract verifiable factual assertions verbatim in meaning. You never judge them."),
            &["claims"],
            2,
        )
        .await?;

    let claims: Vec<ExtractedClaim> = value
        .get("claims")
        .and_then(|c| serde_json::from_value(c.clone()).ok())
        .unwrap_or_default();

    Ok(claims
        .into_iter()
        .filter(|c| !c.claim.trim().is_empty())
        .take(max_claims)
        .collect())
}

/// Persist an extracted claim and link it to the note it was asserted in.
pub async fn store_claim(
    neo4j: &Neo4jClient,
    claim: &ExtractedClaim,
    source_note_id: Option<&str>,
    source_context: Option<&str>,
    asserted_at: Option<&str>,
    embedding: Option<Vec<f32>>,
) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let asserted_at = asserted_at.unwrap_or(&now);

    let mut q = neo4rs::query(
        "CREATE (n:Note {id: $id, content: $content, note_type: 'claim', \
         claim_status: 'unverified', asserted_by: $asserted_by, asserted_at: datetime($asserted_at), \
         claim_kind: $claim_kind, \
         source_context: $source_context, provenance: 'user_input', \
         created_at: datetime($ts), last_accessed_at: datetime($ts), access_count: 0, \
         next_review_at: datetime($ts) + duration({days: 1}), review_interval_days: 1, \
         embedding: $embedding})",
    )
    .param("id", id.clone())
    .param("content", claim.claim.as_str())
    .param("asserted_by", claim.asserted_by.clone().unwrap_or_default())
    .param(
        "claim_kind",
        claim.kind.clone().unwrap_or_else(|| "event".to_string()),
    )
    .param("asserted_at", asserted_at)
    .param(
        "source_context",
        source_context.unwrap_or("claim_extraction"),
    )
    .param("ts", now.as_str());
    q = match embedding {
        Some(e) => q.param("embedding", e),
        None => q.param("embedding", Vec::<f32>::new()),
    };
    neo4j.run(q).await?;

    if let Some(src) = source_note_id {
        let _ = neo4j
            .run(
                neo4rs::query(
                    "MATCH (c:Note {id: $cid}), (s:Note {id: $sid}) \
                     MERGE (c)-[:ASSERTED_IN]->(s)",
                )
                .param("cid", id.as_str())
                .param("sid", src),
            )
            .await;
    }

    Ok(id)
}

/// Institutional/primary-source suffixes, identified mechanically.
///
/// A claim about a Congressional hearing corroborated by `congress.gov` is
/// qualitatively different from the same claim corroborated by a blog, and that
/// difference is decidable from the domain alone — no editorial judgement about
/// which outlets are "good" required.
const PRIMARY_SUFFIXES: &[&str] = &[
    ".gov",
    ".mil",
    ".edu",
    ".int",
    ".gov.uk",
    ".ac.uk",
    ".europa.eu",
    ".who.int",
];

/// True when a domain is an institutional or primary source.
pub fn is_primary_source(domain: &str) -> bool {
    PRIMARY_SUFFIXES.iter().any(|suf| domain.ends_with(suf))
}

/// Classify corroborating domains into primary, established, and unclassified.
///
/// **`unclassified` does not mean unreliable.** The curated lists are sets of
/// large general-interest outlets; a specialist journal, a regional paper, or a
/// trade publication sits outside them and is often the *better* source on its
/// subject. This describes the character of a corroboration, never its quality —
/// gating verification on list membership would encode "mainstream equals true"
/// and would make niche-but-accurate sources permanently unverifiable, which is
/// its own kind of censorship.
///
/// The `:SourceList` nodes alone were not enough: they were curated for search
/// restriction, not source classification, so `congress.gov` and `c-span.org`
/// fell outside them and a well-sourced claim about a Congressional hearing was
/// labelled "unclassified sources only" — a mislabel worse than no label.
pub async fn classify_domains(
    neo4j: &Neo4jClient,
    domains: &[String],
) -> (Vec<String>, Vec<String>, Vec<String>) {
    if domains.is_empty() {
        return (vec![], vec![], vec![]);
    }
    let listed: Vec<String> = neo4j
        .execute(neo4rs::query(
            "MATCH (s:SourceList) UNWIND s.domains AS d RETURN collect(DISTINCT d) AS listed",
        ))
        .await
        .ok()
        .and_then(|rows| {
            rows.first()
                .and_then(|r| r.get::<Vec<String>>("listed").ok())
        })
        .unwrap_or_default();

    let mut primary = Vec::new();
    let mut established = Vec::new();
    let mut unclassified = Vec::new();
    for d in domains {
        if is_primary_source(d) {
            primary.push(d.clone());
        } else if listed
            .iter()
            .any(|l| l == d || d.ends_with(&format!(".{l}")))
        {
            established.push(d.clone());
        } else {
            unclassified.push(d.clone());
        }
    }
    (primary, established, unclassified)
}

/// How to describe the character of a claim's corroboration.
///
/// Recorded alongside the status because "corroborated" alone hides the thing
/// that matters most when a narrative is being pushed: *who* agreed. Five
/// topic-aligned outlets republishing one origin and two government primary
/// sources both read as "corroborated" without this.
pub fn corroboration_tier(primary: usize, established: usize, unclassified: usize) -> &'static str {
    match (primary, established, unclassified) {
        (0, 0, 0) => "none",
        (p, _, _) if p > 0 => "primary sources",
        (_, e, _) if e > 0 => "established sources",
        _ => "unclassified sources only",
    }
}

/// Recompute a claim's status from its evidence edges and persist it.
///
/// Status is a *view* of the edges, so this is the single writer. Called after
/// every verification pass; safe to call at any time.
pub async fn recompute_status(neo4j: &Neo4jClient, claim_id: &str) -> Result<ClaimStatus> {
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (c:Note {id: $id}) \
                 OPTIONAL MATCH (c)-[cor:CORROBORATED_BY]->() \
                 WITH c, count(cor) AS corroborating \
                 OPTIONAL MATCH (c)-[con:CONTRADICTED_BY]->() \
                 RETURN corroborating, count(con) AS contradicting",
            )
            .param("id", claim_id),
        )
        .await?;

    let (cor, con) = rows
        .first()
        .map(|r| {
            (
                r.get::<i64>("corroborating").unwrap_or(0) as usize,
                r.get::<i64>("contradicting").unwrap_or(0) as usize,
            )
        })
        .unwrap_or((0, 0));

    let status = ClaimStatus::from_evidence(cor, con);

    // Describe WHO corroborated, not just how many. Collected from the domains
    // recorded on the supporting edges.
    let domains: Vec<String> = neo4j
        .execute(
            neo4rs::query(
                "MATCH (c:Note {id: $id})-[r:CORROBORATED_BY]->() \
                 UNWIND COALESCE(r.domains, []) AS d \
                 RETURN collect(DISTINCT d) AS domains",
            )
            .param("id", claim_id),
        )
        .await
        .ok()
        .and_then(|rows| {
            rows.first()
                .and_then(|r| r.get::<Vec<String>>("domains").ok())
        })
        .unwrap_or_default();
    let (primary, established, unclassified) = classify_domains(neo4j, &domains).await;
    let tier = corroboration_tier(primary.len(), established.len(), unclassified.len());

    neo4j
        .run(
            neo4rs::query(
                "MATCH (c:Note {id: $id}) \
                 SET c.claim_status = $status, c.verified_at = datetime($now), \
                     c.corroborating_count = $cor, c.contradicting_count = $con, \
                     c.corroboration_tier = $tier, c.corroborating_domains = $domains",
            )
            .param("id", claim_id)
            .param("status", status.as_str())
            .param("now", chrono::Utc::now().to_rfc3339())
            .param("cor", cor as i64)
            .param("con", con as i64)
            .param("tier", tier)
            .param("domains", domains.clone()),
        )
        .await?;

    info!(claim_id = %claim_id, status = status.as_str(), corroborating = cor, contradicting = con, tier, "Claim status recomputed");
    Ok(status)
}

/// Distinct source domains required before support counts as corroboration.
///
/// One domain is not corroboration, however confidently a model reads it.
pub const MIN_INDEPENDENT_DOMAINS: usize = 2;

/// Registrable-ish domains appearing in a block of search results.
///
/// Deliberately crude — the last two labels of the host. It over-merges a few
/// country-code domains (`bbc.co.uk` → `co.uk`), which errs toward *under*-
/// counting independence, and under-counting is the safe direction here.
pub fn evidence_domains(evidence: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for part in evidence.split("http") {
        let Some(rest) = part.split_once("://").map(|x| x.1) else {
            continue;
        };
        let host = rest
            .split(['/', '"', '\\', ' ', '\''])
            .next()
            .unwrap_or("")
            .trim_start_matches("www.")
            .to_lowercase();
        if host.is_empty() || !host.contains('.') {
            continue;
        }
        let labels: Vec<&str> = host.split('.').collect();
        let registrable = if labels.len() >= 2 {
            labels[labels.len() - 2..].join(".")
        } else {
            host.clone()
        };
        if !seen.contains(&registrable) {
            seen.push(registrable);
        }
    }
    seen
}

/// True when a source domain looks like it belongs to the claim's own subject.
///
/// The circular case this exists for: a claim about "Skywatcher" corroborated by
/// `skywatcher.ai`. A subject restating its own assertion is not evidence, and it
/// is precisely the shape a promoted narrative takes — the story and the site
/// pushing it are the same entity.
pub fn is_self_referential(domain: &str, claim: &str, asserted_by: Option<&str>) -> bool {
    let name = domain.split('.').next().unwrap_or("");
    if name.len() < 5 {
        // Too short to match meaningfully; "cnn" would hit half the language.
        return false;
    }
    let haystack = format!(
        "{} {}",
        claim.to_lowercase(),
        asserted_by.unwrap_or("").to_lowercase()
    );
    haystack.contains(name)
}

/// Whether gathered evidence is independent enough to support a claim.
///
/// Returns `Err(reason)` when it is not, so the caller can record *why* a claim
/// stayed unverified rather than silently dropping the verdict.
pub fn check_independence(
    evidence: &str,
    claim: &str,
    asserted_by: Option<&str>,
) -> Result<Vec<String>, String> {
    let all = evidence_domains(evidence);
    let independent: Vec<String> = all
        .iter()
        .filter(|d| !is_self_referential(d, claim, asserted_by))
        .cloned()
        .collect();

    let self_refs = all.len() - independent.len();
    if independent.len() < MIN_INDEPENDENT_DOMAINS {
        return Err(format!(
            "insufficient independence: {} independent domain(s) ({} self-referential, {} needed). Domains: {:?}",
            independent.len(),
            self_refs,
            MIN_INDEPENDENT_DOMAINS,
            all
        ));
    }
    Ok(independent)
}

/// The verdict an LLM reached about one piece of candidate evidence.
#[derive(Debug, Clone, Deserialize)]
pub struct EvidenceVerdict {
    /// `supports` | `contradicts` | `unrelated`
    pub verdict: String,
    #[serde(default)]
    pub source: Option<String>,
    #[serde(default)]
    pub reasoning: Option<String>,
}

/// Judge whether gathered search results support or contradict a claim.
///
/// The prompt forbids answering from the model's own knowledge: only the
/// supplied evidence counts. A model asked "is this true?" will happily answer
/// from its priors, which would make the corroboration edges fiction.
pub async fn assess_evidence(
    llm: &dyn LlmProvider,
    claim: &str,
    evidence: &str,
) -> Result<EvidenceVerdict> {
    let prompt = format!(
        "Decide whether the EVIDENCE supports or contradicts the CLAIM.\n\n\
         Judge ONLY from the EVIDENCE below. Do not use your own knowledge of the \
         topic, and do not guess. If the evidence does not clearly bear on the \
         claim, answer \"unrelated\" — that is the correct answer far more often \
         than not, and a wrong verdict is worse than no verdict.\n\n\
         \"supports\"    — the evidence independently asserts the claim is true\n\
         \"contradicts\" — the evidence independently asserts it is false\n\
         \"unrelated\"   — the evidence is off-topic, or merely repeats the same \
         original source rather than confirming it independently\n\n\
         Respond with JSON only:\n\
         {{\"verdict\": \"supports|contradicts|unrelated\", \"source\": \"url or outlet\", \"reasoning\": \"one sentence\"}}\n\n\
         CLAIM:\n{claim}\n\nEVIDENCE:\n{evidence}"
    );

    let value = llm
        .generate_json(
            &prompt,
            Some("You assess evidence. You answer only from the evidence given, never from prior knowledge."),
            &["verdict"],
            2,
        )
        .await?;

    let verdict = value
        .get("verdict")
        .and_then(|v| v.as_str())
        .unwrap_or("unrelated")
        .to_lowercase();

    Ok(EvidenceVerdict {
        verdict,
        source: value
            .get("source")
            .and_then(|v| v.as_str())
            .map(String::from),
        reasoning: value
            .get("reasoning")
            .and_then(|v| v.as_str())
            .map(String::from),
    })
}

/// Attach an evidence note to a claim with the given verdict.
pub async fn attach_evidence(
    neo4j: &Neo4jClient,
    claim_id: &str,
    evidence_note_id: &str,
    verdict: &str,
    source: Option<&str>,
    domains: &[String],
) -> Result<bool> {
    let rel = match verdict {
        "supports" => "CORROBORATED_BY",
        "contradicts" => "CONTRADICTED_BY",
        // "unrelated" attaches nothing — silence is the correct record.
        _ => return Ok(false),
    };

    // The domains are recorded on the edge, not just counted. A status of
    // "corroborated" says nothing about WHO corroborated, and domain diversity is
    // not source independence: five topic-aligned outlets republishing one origin
    // pass any count-based test. Storing the domains keeps that inspectable
    // instead of hidden behind a one-word status.
    let cypher = format!(
        "MATCH (c:Note {{id: $cid}}), (e:Note {{id: $eid}}) \
         MERGE (c)-[r:{rel}]->(e) \
         ON CREATE SET r.source = $source, r.found_at = datetime($now), r.domains = $domains"
    );
    neo4j
        .run(
            neo4rs::query(&cypher)
                .param("cid", claim_id)
                .param("eid", evidence_note_id)
                .param("source", source.unwrap_or(""))
                .param("domains", domains.to_vec())
                .param("now", chrono::Utc::now().to_rfc3339()),
        )
        .await?;
    Ok(true)
}

/// Record that a verification attempt was made against a claim.
///
/// Stamped *before* the attempt runs, not after, so a claim advances the cursor
/// even when the attempt dies partway (search failure, assessment error, the job
/// itself dying). A claim that blocks the sweep is worse than one retried a cycle
/// early — see [`unverified_claims`].
pub async fn mark_verify_attempt(neo4j: &Neo4jClient, claim_id: &str) -> Result<()> {
    neo4j
        .run(
            neo4rs::query(
                "MATCH (c:Note {id: $id, note_type: 'claim'}) \
                 SET c.last_verify_attempt_at = datetime($now)",
            )
            .param("id", claim_id)
            .param("now", chrono::Utc::now().to_rfc3339()),
        )
        .await?;
    Ok(())
}

/// Fetch claims awaiting verification: never-attempted first, then
/// least-recently-attempted.
///
/// Ordering by `created_at` alone deadlocks the sweep. Finding no evidence
/// correctly leaves a claim `unverified` (absence of evidence is not refutation),
/// so the oldest N claims re-qualify on every run and are re-selected forever.
/// Observed 2026-08-18: twelve 6-hourly sweeps each processed the *same eight*
/// claim ids, attached zero edges, and reported success, while 465 other claims
/// were never once attempted — corroboration frozen at 17/482 for weeks with no
/// error anywhere to show for it.
///
/// `last_verify_attempt_at` is the cursor. Neo4j sorts NULL *last* in ascending
/// order, which is backwards for us — never-attempted claims must go first — so
/// the COALESCE maps unset to the epoch. Rotation is also the cooldown: at 8
/// claims per 6h a full backlog cycle takes weeks, so no separate retry gate is
/// needed.
pub async fn unverified_claims(neo4j: &Neo4jClient, limit: usize) -> Result<Vec<(String, String)>> {
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (c:Note {note_type: 'claim'}) \
                 WHERE COALESCE(c.claim_status, 'unverified') = 'unverified' \
                 RETURN c.id AS id, c.content AS content \
                 ORDER BY COALESCE(c.last_verify_attempt_at, datetime('1970-01-01T00:00:00Z')) ASC, \
                          c.created_at ASC \
                 LIMIT $limit",
            )
            .param("limit", limit as i64),
        )
        .await?;

    Ok(rows
        .iter()
        .filter_map(
            |r| match (r.get::<String>("id"), r.get::<String>("content")) {
                (Ok(id), Ok(content)) => Some((id, content)),
                _ => None,
            },
        )
        .collect())
}

/// Warn when a source's claims are overwhelmingly unverified.
///
/// Not a filter and not a score — a signal. A source producing many claims that
/// nothing independently corroborates is exactly what an information-operation
/// looks like from the inside, and it is also what a niche-but-accurate source
/// looks like. The brain reports the asymmetry; a human judges it.
pub async fn source_claim_profile(neo4j: &Neo4jClient) -> Result<serde_json::Value> {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (c:Note {note_type: 'claim'}) \
             WHERE c.asserted_by IS NOT NULL AND c.asserted_by <> '' \
             RETURN c.asserted_by AS source, count(*) AS claims, \
                    sum(CASE WHEN c.claim_status = 'corroborated' THEN 1 ELSE 0 END) AS corroborated, \
                    sum(CASE WHEN c.claim_status = 'refuted' THEN 1 ELSE 0 END) AS refuted, \
                    sum(CASE WHEN c.claim_status = 'disputed' THEN 1 ELSE 0 END) AS disputed, \
                    sum(CASE WHEN COALESCE(c.claim_status,'unverified') = 'unverified' THEN 1 ELSE 0 END) AS unverified \
             ORDER BY claims DESC LIMIT 40",
        ))
        .await?;

    let sources: Vec<serde_json::Value> = rows
        .iter()
        .map(|r| {
            serde_json::json!({
                "source":       r.get::<String>("source").unwrap_or_default(),
                "claims":       r.get::<i64>("claims").unwrap_or(0),
                "corroborated": r.get::<i64>("corroborated").unwrap_or(0),
                "refuted":      r.get::<i64>("refuted").unwrap_or(0),
                "disputed":     r.get::<i64>("disputed").unwrap_or(0),
                "unverified":   r.get::<i64>("unverified").unwrap_or(0),
            })
        })
        .collect();

    Ok(serde_json::json!({
        "note": "Counts only. A high unverified count may mean an unreliable source, \
                 a niche-but-accurate one, or simply that verification has not run yet. \
                 Interpret with the verification backlog in mind; do not treat as a score.",
        "sources": sources,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn evidence_domains_are_deduplicated_by_registrable_name() {
        let ev = r#"[{"link":"https://www.reuters.com/a"},{"link":"https://reuters.com/b"},{"link":"https://apnews.com/c"}]"#;
        let d = evidence_domains(ev);
        assert_eq!(d.len(), 2, "got {d:?}");
        assert!(d.contains(&"reuters.com".to_string()));
        assert!(d.contains(&"apnews.com".to_string()));
    }

    #[test]
    fn a_subjects_own_site_is_self_referential() {
        // The circular case: a claim about Skywatcher "corroborated" by
        // skywatcher.ai, observed in the first live verification run.
        assert!(is_self_referential(
            "skywatcher.ai",
            "A group called Skywatchers at Sierra Blanca demonstrated psionic summoning.",
            Some("NewsNation")
        ));
        assert!(!is_self_referential(
            "reuters.com",
            "A group called Skywatchers…",
            None
        ));
    }

    #[test]
    fn short_domain_names_do_not_trigger_self_reference() {
        // "cnn" or "bbc" would otherwise match incidental substrings.
        assert!(!is_self_referential(
            "cnn.com",
            "a claim mentioning cnn somewhere",
            None
        ));
    }

    #[test]
    fn single_domain_support_is_rejected_as_not_independent() {
        let ev = r#"[{"link":"https://psionicresearch.com/x"}]"#;
        let err = check_independence(ev, "psionic summoning demonstrated", None).unwrap_err();
        assert!(err.contains("insufficient independence"), "{err}");
    }

    #[test]
    fn subject_owned_domains_do_not_count_toward_independence() {
        // Two domains, but one is the subject's own — leaving one independent
        // source, which is not corroboration.
        let ev =
            r#"[{"link":"https://skywatcher.ai/media"},{"link":"https://psionicresearch.com/x"}]"#;
        assert!(check_independence(ev, "Skywatcher demonstrated summoning", None).is_err());
    }

    #[test]
    fn two_independent_domains_pass() {
        let ev = r#"[{"link":"https://apnews.com/a"},{"link":"https://reuters.com/b"}]"#;
        let ok = check_independence(ev, "Congress held UAP hearings", None).unwrap();
        assert_eq!(ok.len(), 2);
    }

    #[test]
    fn an_attribution_claim_says_what_corroboration_actually_established() {
        // The failure this exists for: "corroborated" next to a claim about
        // psionic summoning reads as endorsement of efficacy, when all that was
        // corroborated is that a group said they did it.
        let out = label_claim(
            "A group demonstrated psionic summoning.",
            "corroborated",
            Some("NewsNation"),
            Some("2026-08-10"),
            Some("unclassified sources only"),
            Some("attribution"),
            None,
        );
        assert!(
            out.contains("corroborates that it was asserted, not that it is true"),
            "{out}"
        );
    }

    #[test]
    fn an_event_claim_needs_no_qualifier() {
        // "Congress held hearings" corroborated means the hearings happened.
        assert!(kind_qualifier("event", "corroborated").is_none());
        let out = label_claim(
            "Congress held hearings.",
            "corroborated",
            None,
            None,
            Some("primary sources"),
            Some("event"),
            None,
        );
        assert!(!out.contains("not that it is true"), "{out}");
        assert!(out.contains("event"), "{out}");
    }

    #[test]
    fn qualifiers_only_apply_to_corroborated_claims() {
        // An unverified attribution has nothing to qualify, and adding the
        // phrase would imply a verification that never happened.
        assert!(kind_qualifier("attribution", "unverified").is_none());
        assert!(kind_qualifier("attribution", "refuted").is_none());
        assert!(kind_qualifier("mechanism", "corroborated").is_some());
    }

    #[test]
    fn institutional_domains_are_recognised_as_primary() {
        // The observed mislabel: congress.gov corroborating a claim about a
        // Congressional hearing was tagged "unclassified", because the curated
        // SourceList was built for search restriction, not classification.
        for d in [
            "congress.gov",
            "house.gov",
            "nasa.gov",
            "mit.edu",
            "defense.mil",
            "parliament.gov.uk",
        ] {
            assert!(is_primary_source(d), "{d} should be primary");
        }
        for d in ["nbcnews.com", "psionicresearch.com", "medium.com"] {
            assert!(!is_primary_source(d), "{d} should not be primary");
        }
    }

    #[test]
    fn corroboration_tier_describes_who_agreed() {
        assert_eq!(corroboration_tier(0, 0, 0), "none");
        assert_eq!(corroboration_tier(0, 2, 0), "established sources");
        assert_eq!(corroboration_tier(0, 1, 4), "established sources");
        assert_eq!(corroboration_tier(0, 0, 5), "unclassified sources only");
        // Primary outranks the rest: congress.gov on a claim about Congress is
        // the source, not a report about it.
        assert_eq!(corroboration_tier(1, 0, 9), "primary sources");
    }

    #[test]
    fn tier_appears_in_the_label_so_the_distinction_is_visible() {
        // "corroborated" alone cannot distinguish two wire services agreeing
        // from five aligned outlets repeating one origin.
        let out = label_claim(
            "Skywatchers demonstrated psionic summoning.",
            "corroborated",
            Some("NewsNation"),
            Some("2026-08-10"),
            Some("unclassified sources only"),
            None,
            None,
        );
        assert!(out.starts_with("[CLAIM · corroborated · unclassified sources only · asserted by NewsNation · 2026-08-10]"), "{out}");
    }

    #[test]
    fn a_none_tier_is_omitted_rather_than_rendered() {
        let out = label_claim("x", "unverified", None, None, Some("none"), None, None);
        assert_eq!(out, "[CLAIM · unverified]\nx");
    }

    #[test]
    fn status_is_derived_from_evidence_counts() {
        assert_eq!(ClaimStatus::from_evidence(0, 0), ClaimStatus::Unverified);
        assert_eq!(ClaimStatus::from_evidence(3, 0), ClaimStatus::Corroborated);
        assert_eq!(ClaimStatus::from_evidence(0, 2), ClaimStatus::Refuted);
    }

    #[test]
    fn conflicting_evidence_stays_disputed() {
        // Never collapses to a verdict, and never resolves by majority —
        // laundering a contested question into a settled one is the failure
        // mode this whole module exists to prevent.
        assert_eq!(ClaimStatus::from_evidence(1, 1), ClaimStatus::Disputed);
        assert_eq!(ClaimStatus::from_evidence(9, 1), ClaimStatus::Disputed);
        assert_eq!(ClaimStatus::from_evidence(1, 9), ClaimStatus::Disputed);
    }

    #[test]
    fn label_carries_status_and_attribution_into_context() {
        let out = label_claim(
            "The DoD uses TFRs to create controlled UAP zones.",
            "unverified",
            Some("NewsNation"),
            Some("2026-08-10T16:00:00Z"),
            None,
            None,
            None,
        );
        assert!(out.starts_with("[CLAIM · unverified · asserted by NewsNation · 2026-08-10]\n"));
        assert!(out.contains("The DoD uses TFRs"));
    }

    #[test]
    fn label_degrades_gracefully_without_attribution() {
        let out = label_claim(
            "Something was asserted.",
            "disputed",
            None,
            None,
            None,
            None,
            None,
        );
        assert_eq!(out, "[CLAIM · disputed]\nSomething was asserted.");
        // An empty attribution must not render as "asserted by ".
        let out = label_claim(
            "x",
            "unverified",
            Some(""),
            Some(""),
            Some(""),
            Some(""),
            Some(""),
        );
        assert_eq!(out, "[CLAIM · unverified]\nx");
    }

    #[test]
    fn unrelated_evidence_creates_no_edge() {
        // Guarded at the caller via attach_evidence's match; assert the mapping
        // here so a future edit cannot silently start recording non-evidence.
        for v in ["unrelated", "", "maybe", "SUPPORTS"] {
            assert!(
                !matches!(v, "supports" | "contradicts"),
                "{v} must not map to an evidence edge"
            );
        }
    }
}
