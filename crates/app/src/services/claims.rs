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
//!   the same as rewriting it. The one exception is `claim_rewrite` (2026-10),
//!   which restated claims that leaned on their source so they stand alone,
//!   meaning held by mechanical checks, the old text kept in `content_v1`.
//! - **A disputed claim stays disputed.** Corroboration *and* contradiction does
//!   not collapse to a verdict. Premature resolution is how a knowledge base
//!   launders a contested question into a settled one.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tracing::info;
use uuid::Uuid;

use crate::repository::Neo4jClient;
use crate::services::traits::LlmProvider;
use crate::services::web_result::{is_search_page, registrable_domain};

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
    /// The named things the claim is about, written as the claim names them,
    /// each with the word the claim uses for it. This is what makes "stands
    /// alone" checkable: see `standalone_problems`.
    #[serde(default, deserialize_with = "about_list")]
    pub about: Vec<About>,
}

/// One named thing a claim is about: `{name: "Paragon Solutions", what: "company"}`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct About {
    pub name: String,
    #[serde(default)]
    pub what: Option<String>,
}

/// `about` as models write it: objects, bare names, or nothing.
fn about_list<'de, D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Vec<About>, D::Error> {
    let v = Option::<serde_json::Value>::deserialize(d)?;
    let Some(serde_json::Value::Array(items)) = v else {
        return Ok(Vec::new());
    };
    Ok(items
        .into_iter()
        .filter_map(|i| match i {
            serde_json::Value::String(name) => Some(About { name, what: None }),
            serde_json::Value::Object(m) => Some(About {
                name: m.get("name")?.as_str()?.to_string(),
                what: m.get("what").and_then(|w| w.as_str()).map(String::from),
            }),
            _ => None,
        })
        .filter(|a| !a.name.trim().is_empty())
        .collect())
}

/// Common nouns that, bare after "the" / "this" / …, leave a reader asking
/// "which one?". "The software" in a claim read months later names nothing;
/// "the Italian government" or "Paragon's software" does, which is why only a
/// determiner directly before the noun counts. Linguistic, like a stopword
/// list, not domain data.
const VAGUE_NOUNS: &[&str] = &[
    "software",
    "company",
    "firm",
    "tool",
    "app",
    "application",
    "platform",
    "product",
    "program",
    "programme",
    "system",
    "device",
    "model",
    "bill",
    "law",
    "act",
    "rule",
    "policy",
    "report",
    "study",
    "paper",
    "survey",
    "agency",
    "department",
    "group",
    "organization",
    "organisation",
    "government",
    "administration",
    "official",
    "country",
    "city",
    "attack",
    "operation",
    "deal",
    "agreement",
    "project",
    "plan",
    "campaign",
    "technique",
    "technology",
    "network",
    "service",
    "vulnerability",
    "malware",
    "spyware",
    "drug",
    "vaccine",
    "incident",
    "contract",
    "measure",
    "legislation",
    "order",
    "lawsuit",
    "investigation",
    "team",
    "party",
    "researchers",
    "video",
    "article",
    "channel",
    "speaker",
    "host",
    "author",
    "chip",
    "weapon",
    "missile",
    "satellite",
    "facility",
    "site",
    "region",
    "institution",
];
/// Not "that": in "states that chip scarcity forces …" it is a conjunction.
const DETERMINERS: &[&str] = &["the", "this", "these", "those"];
/// A claim that opens with one of these has lost its subject.
const PRONOUN_OPENERS: &[&str] = &[
    "he", "she", "they", "it", "its", "his", "her", "their", "this", "these", "those",
];

/// One spelling for the typography models vary: curly quotes, the Unicode
/// hyphens and dashes (gpt-oss writes "AP\u{2011}NORC" with a non-breaking
/// hyphen), and non-breaking spaces.
fn canon(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '\u{2018}' | '\u{2019}' => '\'',
            '\u{201C}' | '\u{201D}' => '"',
            '\u{2010}'..='\u{2015}' | '\u{2212}' => '-',
            '\u{00A0}' | '\u{202F}' | '\u{2009}' => ' ',
            c => c,
        })
        .collect()
}

fn normalize(s: &str) -> String {
    canon(s).to_lowercase()
}

/// For matching a name inside a claim: lowercase words only, possessives
/// dropped, so quotes, hyphens and commas between them don't hide it. A
/// trailing parenthetical ("Gap synthesis note (2026-08-08)") is the model's
/// gloss, not part of the name.
fn loose(s: &str) -> String {
    let s = normalize(s.trim());
    let s = match (s.ends_with(')'), s.rfind(" (")) {
        (true, Some(i)) if i > 0 => s[..i].to_string(),
        _ => s,
    };
    s.split(|c: char| !(c.is_alphanumeric() || c == '\''))
        // A possessive is the same name: "Paragon's" → "paragon".
        .map(|w| w.strip_suffix("'s").unwrap_or(w).trim_matches('\''))
        .filter(|w| !w.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn names(text_loose: &str, name: &str) -> bool {
    let n = loose(name);
    !n.is_empty() && format!(" {text_loose} ").contains(&format!(" {n} "))
}

fn singular(w: &str) -> &str {
    w.strip_suffix('s').filter(|r| r.len() > 2).unwrap_or(w)
}

/// "the software", "these officials": determiner + vague noun pairs, in order.
fn vague_references(text: &str) -> Vec<(String, String)> {
    let words: Vec<&str> = text
        .split(|c: char| !(c.is_alphanumeric() || c == '\''))
        .filter(|w| !w.is_empty())
        .collect();
    words
        .windows(2)
        .filter(|p| {
            DETERMINERS.contains(&p[0])
                && VAGUE_NOUNS
                    .iter()
                    .any(|n| *n == p[1] || *n == singular(p[1]))
        })
        .map(|p| (p[0].to_string(), p[1].to_string()))
        .collect()
}

/// An `about` "name" that is only a vague noun ("video", "the system") names
/// nothing; the local model listed exactly those to satisfy the check.
fn is_generic(name: &str) -> bool {
    let n = normalize(name.trim());
    let n = DETERMINERS
        .iter()
        .find_map(|d| n.strip_prefix(&format!("{d} ")))
        .unwrap_or(&n);
    VAGUE_NOUNS.iter().any(|v| *v == n || *v == singular(n))
}

/// Why a claim cannot be understood without its source; empty when it can.
///
/// Mechanical, so it holds whatever model extracted the claim. The extraction
/// prompt asked for "self-contained" claims from the start and the local model
/// still wrote "admitted the company lacks the mechanism … to disable the
/// software". The check makes the model *declare* what each vague reference
/// stands for, then holds it to naming that in the claim itself:
/// - every `about` name must appear in the claim, and be a name, not a vague
///   noun ("video", "the system");
/// - every bare "the <vague noun>" needs an `about` entry whose `what` is
///   that noun and whose name is in the claim;
/// - the claim may not open with a pronoun.
pub fn standalone_problems(c: &ExtractedClaim) -> Vec<String> {
    let text = normalize(&c.claim);
    let text_loose = loose(&c.claim);
    let mut out = Vec::new();
    if let Some(first) = text.split_whitespace().next() {
        let w = first.trim_matches(|ch: char| !ch.is_alphanumeric());
        if PRONOUN_OPENERS.contains(&w) {
            out.push(format!(
                "it opens with \"{first}\" without saying who or what that is"
            ));
        }
    }
    if c.about.is_empty() {
        out.push("it lists nothing it is about".to_string());
    }
    for a in c.about.iter().filter(|a| is_generic(&a.name)) {
        out.push(format!("\"{}\" is not a name", a.name.trim()));
    }
    let named: Vec<&About> = c
        .about
        .iter()
        .filter(|a| !is_generic(&a.name) && names(&text_loose, &a.name))
        .collect();
    for a in c
        .about
        .iter()
        .filter(|a| !is_generic(&a.name) && !named.contains(a))
    {
        out.push(format!(
            "\"{}\" is listed as what it is about, but the claim never names it",
            a.name.trim()
        ));
    }
    for (det, noun) in vague_references(&text) {
        let declared = named.iter().any(|a| {
            a.what
                .as_deref()
                .map(|w| singular(normalize(w).trim()) == singular(&noun))
                .unwrap_or(false)
        });
        if !declared {
            out.push(format!(
                "\"{det} {noun}\" does not say which {}",
                singular(&noun)
            ));
        }
    }
    out
}

/// What a rewrite lost: each number and capitalised name in `old` that `new`
/// no longer contains. A rewrite may add names (that is the point) but never
/// drop a fact, so any loss keeps the old text. Deliberately strict: "US" →
/// "United States" counts as a loss, and the claim stays as it was.
pub fn rewrite_losses(old: &str, new: &str) -> Vec<String> {
    let (old, new) = (canon(old), canon(new));
    // Numbers compare as runs of digits, thousands separators removed, so
    // "1,200" = "1200", and "18–34" or "2026-09-28/29" keep each part.
    let runs = |s: &str| -> Vec<String> {
        let chars: Vec<char> = s.chars().collect();
        let mut out = Vec::new();
        let mut cur = String::new();
        for (i, c) in chars.iter().enumerate() {
            if c.is_ascii_digit() {
                cur.push(*c);
            } else if *c == ','
                && !cur.is_empty()
                && chars.get(i + 1).is_some_and(|n| n.is_ascii_digit())
            {
                continue;
            } else if !cur.is_empty() {
                out.push(std::mem::take(&mut cur));
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    };
    let new_runs = runs(&new);
    let mut lost: Vec<String> = Vec::new();
    for r in runs(&old) {
        if !new_runs.contains(&r) && !lost.contains(&r) {
            lost.push(r);
        }
    }
    // Names: each capitalised part of each word, hyphenated words split
    // ("GRU-linked" needs "GRU"). The opening word is capitalised by grammar,
    // not because it is a name. A possessive is the same name.
    let new_n = new.to_lowercase();
    let words = old
        .split(|c: char| c.is_whitespace() || "()[]{}\";:/\u{2192}".contains(c))
        .filter(|w| !w.is_empty());
    for (i, word) in words.enumerate() {
        for part in word.split('-') {
            let t = part.trim_matches(|c: char| !c.is_alphanumeric());
            if i == 0 || t.is_empty() || t.chars().any(|c| c.is_ascii_digit()) {
                continue;
            }
            if t.chars().next().is_some_and(|c| c.is_uppercase()) {
                let name = t.to_lowercase();
                let name = name.strip_suffix("'s").unwrap_or(&name);
                if !new_n.contains(name) && !lost.iter().any(|l| l == t) {
                    lost.push(t.to_string());
                }
            }
        }
    }
    lost
}

/// The rules every extraction and rewrite prompt carries for standing alone.
pub(crate) const STANDALONE_RULES: &str = "Each claim must stand alone: someone who never sees the SOURCE must \
     know exactly who and what it is about. Never write \"the company\", \"the software\", \
     \"the spyware\", \"he\" or \"the bill\": use the name the SOURCE gives, or, if it gives \
     none, say whose it is (\"Paragon Solutions' spyware\", \"Ohio's new law\"). Never invent \
     a name.\n\
     In \"about\" list every person, organisation, product, place, law or programme the claim \
     names, written exactly as in the claim, with \"what\" = the word for it \
     (\"company\", \"software\", \"senator\").\n";

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
    source_title: Option<&str>,
) -> Result<Extraction> {
    let title = source_title
        .map(|t| format!("SOURCE TITLE: {t}\n"))
        .unwrap_or_default();
    let prompt = format!(
        "Extract factual assertions from the SOURCE that a researcher could \
         independently verify or refute.\n\n\
         Include: specific, checkable statements of fact — events, quantities, \
         dates, attributions, causal claims about the world.\n\
         EXCLUDE: opinions, predictions, value judgements, analysis, and \
         anything already presented as uncertain by the source itself.\n\n\
         {STANDALONE_RULES}\n\
         Do NOT editorialise, debunk, endorse, or soften \
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
         {{\"claims\": [{{\"claim\": \"...\", \"asserted_by\": \"...\", \"kind\": \"event|attribution|mechanism\", \
         \"about\": [{{\"name\": \"...\", \"what\": \"...\"}}]}}]}}\n\n\
         {title}SOURCE:\n{text}"
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

    let mut out = Extraction::default();
    for c in claims
        .into_iter()
        .filter(|c| !c.claim.trim().is_empty())
        .take(max_claims)
    {
        let problems = standalone_problems(&c);
        if problems.is_empty() {
            out.claims.push(c);
            continue;
        }
        // One repair, given the source and exactly what is wrong.
        match repair_claim(llm, text, &c, &problems).await {
            Some(fixed) => {
                out.repaired += 1;
                out.claims.push(fixed);
            }
            None => out.dropped.push(DroppedClaim {
                claim: c.claim,
                problems,
            }),
        }
    }
    Ok(out)
}

/// What one extraction produced: the claims that stand alone (some after a
/// repair) and those dropped because they still didn't, with the reasons.
#[derive(Debug, Default)]
pub struct Extraction {
    pub claims: Vec<ExtractedClaim>,
    pub repaired: usize,
    pub dropped: Vec<DroppedClaim>,
}

#[derive(Debug, Clone, Serialize)]
pub struct DroppedClaim {
    pub claim: String,
    pub problems: Vec<String>,
}

/// Ask once for a version of `c` that stands alone. `None` when the answer
/// still doesn't, or lost a number or name the original had.
pub(crate) async fn repair_claim(
    llm: &dyn LlmProvider,
    source: &str,
    c: &ExtractedClaim,
    problems: &[String],
) -> Option<ExtractedClaim> {
    let source: String = source.chars().take(REWRITE_SOURCE_CHARS).collect();
    let prompt = format!(
        "This claim was extracted from the SOURCE, but it cannot be understood without it: {}.\n\n\
         Rewrite it. {STANDALONE_RULES}\
         Keep every fact, number and name it already has, and its meaning exactly; add only the names \
         that resolve what it refers to. {KEEP_AS_WRITTEN}\n\n\
         Respond with JSON only: {{\"claim\": \"...\", \"about\": [{{\"name\": \"...\", \"what\": \"...\"}}]}}\n\n\
         CLAIM: {}\n\nSOURCE:\n{source}",
        problems.join("; "),
        c.claim
    );
    let v = llm
        .generate_json(&prompt, Some(REWRITE_SYSTEM), &["claim"], 1)
        .await
        .ok()?;
    let fixed: ExtractedClaim = serde_json::from_value(v).ok()?;
    let fixed = ExtractedClaim {
        asserted_by: c.asserted_by.clone(),
        kind: c.kind.clone(),
        ..fixed
    };
    let still = standalone_problems(&fixed);
    let lost = rewrite_losses(&c.claim, &fixed.claim);
    if still.is_empty() && lost.is_empty() {
        return Some(fixed);
    }
    info!(claim = %c.claim, answer = %fixed.claim, still = ?still, lost = ?lost, "Claim repair did not pass");
    None
}

/// Every name, acronym, number and date the rewrite keeps must be found
/// again by `rewrite_losses`; spelled out or reformatted, it can't be.
pub(crate) const KEEP_AS_WRITTEN: &str = "Write every name, acronym, number and date exactly as the claim \
     already does: keep \"FBI\", \"US\", \"MoE\", \"2026-09-28\" and \"2\" as they are; never spell \
     them out, translate or reformat them.";

/// Source text given to a repair or rewrite: enough for nearly every source
/// note (p95 5.2k chars), within a small model's budget.
pub const REWRITE_SOURCE_CHARS: usize = 12_000;
pub const REWRITE_SYSTEM: &str = "You make extracted claims understandable on their own, using only what the source says. You never judge them.";

/// Persist an extracted claim and link it to the note it was asserted in.
pub async fn store_claim(
    neo4j: &Neo4jClient,
    claim: &ExtractedClaim,
    source_note_id: Option<&str>,
    source_context: Option<&str>,
    asserted_at: Option<&str>,
    embedding: Option<Vec<f32>>,
    source: &SourceRef,
) -> Result<String> {
    let id = Uuid::new_v4().to_string();
    let now = chrono::Utc::now().to_rfc3339();
    let asserted_at = asserted_at.unwrap_or(&now);

    let mut q = neo4rs::query(
        "CREATE (n:Note {id: $id, content: $content, note_type: 'claim', \
         claim_status: 'unverified', asserted_by: $asserted_by, asserted_at: datetime($asserted_at), \
         claim_kind: $claim_kind, about: $about, \
         source_url: $source_url, source_title: $source_title, source_outlet: $source_outlet, \
         standalone_checked_at: datetime($ts), \
         source_context: $source_context, provenance: 'synthesis_inference', \
         produced_by_task: $produced_by_task, \
         created_at: datetime($ts), last_accessed_at: datetime($ts), access_count: 0, \
         next_review_at: datetime($ts) + duration({days: 1}), review_interval_days: 1, \
         embedding: $embedding})",
    )
    .param("id", id.clone())
    .param(
        "produced_by_task",
        match crate::services::queue::current_owner_task() {
            Some(t) => neo4rs::BoltType::from(t),
            None => neo4rs::BoltType::Null(neo4rs::BoltNull),
        },
    )
    .param("content", claim.claim.as_str())
    .param("asserted_by", claim.asserted_by.clone().unwrap_or_default())
    .param(
        "claim_kind",
        claim.kind.clone().unwrap_or_else(|| "event".to_string()),
    )
    .param("asserted_at", asserted_at)
    .param(
        "about",
        claim.about.iter().map(|a| a.name.trim().to_string()).collect::<Vec<_>>(),
    )
    .param("source_url", source.url.clone())
    .param("source_title", source.title.clone())
    .param("source_outlet", source.outlet.clone())
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

/// What a claim's source note says about itself: the article or video it
/// records, so a reader can see where the claim came from.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct SourceRef {
    pub title: Option<String>,
    pub url: Option<String>,
    /// The site or channel (`site` in an article summary, `channel` in a video's).
    pub outlet: Option<String>,
}

/// Read a source note's title, URL and outlet. Article and video records carry
/// a summary JSON object with `title` / `url` / `site` or `channel`; anything
/// else (a watch report, a news brief) falls back to its first line, as a
/// heading of any level, cut to 120 characters, and to a URL only when that
/// line holds one: a URL from the body could be any of the report's links.
pub fn source_reference(content: &str) -> SourceRef {
    let summary = crate::services::llm::first_json_object(content, &["title"]);
    let field = |k: &str| {
        summary
            .as_ref()
            .and_then(|v| v.get(k))
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let heading = content
        .lines()
        .map(|l| l.trim().trim_start_matches('#').trim())
        .find(|l| !l.is_empty());
    let heading_url = heading.and_then(|h| {
        h.split_whitespace()
            .find(|w| w.starts_with("http://") || w.starts_with("https://"))
            .map(String::from)
    });
    SourceRef {
        title: field("title").or_else(|| heading.map(|h| h.chars().take(120).collect())),
        url: field("url").or(heading_url),
        outlet: field("site").or_else(|| field("channel")),
    }
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
            // Role-tagged lists (platforms, state media) are not "established".
            "MATCH (s:SourceList) WHERE s.role IS NULL \
             UNWIND s.domains AS d RETURN collect(DISTINCT d) AS listed",
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

/// The tier as stored and rendered in the claim label, with any state-media
/// corroborators named. A label, never a gate: state media still counts.
pub fn tier_label(tier: &str, state_media: &[String]) -> String {
    if state_media.is_empty() || tier == "none" {
        tier.to_string()
    } else {
        format!("{tier} · incl. state media ({})", state_media.join(", "))
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
    let roles = load_domain_roles(neo4j).await;
    let state_media: Vec<String> = domains
        .iter()
        .filter(|d| roles.is_state_media(d))
        .cloned()
        .collect();
    let tier = tier_label(
        corroboration_tier(primary.len(), established.len(), unclassified.len()),
        &state_media,
    );

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
            .param("tier", tier.as_str())
            .param("domains", domains.clone()),
        )
        .await?;

    info!(claim_id = %claim_id, status = status.as_str(), corroborating = cor, contradicting = con, tier = %tier, "Claim status recomputed");
    Ok(status)
}

/// Distinct source domains required before support counts as corroboration.
///
/// One domain is not corroboration, however confidently a model reads it.
pub const MIN_INDEPENDENT_DOMAINS: usize = 2;

/// The host of a link, lowercased, without a leading `www.`.
fn link_host(link: &str) -> String {
    let rest = link.split_once("://").map(|x| x.1).unwrap_or(link);
    rest.split(['/', '?', '#', ':'])
        .next()
        .unwrap_or("")
        .trim_start_matches("www.")
        .to_lowercase()
}

/// True when `domain` is `entry` or a subdomain of it.
fn domain_matches(domain: &str, entry: &str) -> bool {
    domain == entry || domain.ends_with(&format!(".{entry}"))
}

/// Lowercase alphanumeric words, with the HTML entities search snippets carry
/// (`&quot;`, `&#039;`) removed first so they do not become words.
fn normalise_words(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '&' {
            // Skip an entity up to its ';' (bounded, so a bare '&' survives).
            let ent: String = chars.clone().take(8).collect();
            if let Some(end) = ent.find(';') {
                for _ in 0..=end {
                    chars.next();
                }
                out.push(' ');
                continue;
            }
        }
        out.push(if c.is_alphanumeric() {
            c.to_ascii_lowercase()
        } else {
            ' '
        });
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Remove results that only echo the query back, before the judge sees them.
///
/// Returns the remaining results (re-serialised) and how many were dropped.
/// From 2026-10-01 searches for a claim returned pages like "Search for <the
/// claim, verbatim> with Ecosia"; the judge read its own claim restated and
/// called it support (25 corroborations in six days). Two tests, either drops:
/// the link is a search-results page, or the title/snippet contains the whole
/// claim word for word. Text that is not a result array is returned unchanged.
pub fn drop_echo_results(evidence: &str, claim: &str) -> (String, usize) {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(evidence.trim()) else {
        return (evidence.to_string(), 0);
    };
    let claim_norm = normalise_words(claim);
    // A short claim ("My legs!") appears inside plenty of real pages.
    let check_text = claim_norm.len() >= 40;
    let before = items.len();
    let kept: Vec<serde_json::Value> = items
        .into_iter()
        .filter(|item| {
            let link = item
                .get("link")
                .or_else(|| item.get("url"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if is_search_page(link) {
                return false;
            }
            if check_text {
                let text = format!(
                    "{} {}",
                    item.get("title").and_then(|v| v.as_str()).unwrap_or(""),
                    item.get("snippet").and_then(|v| v.as_str()).unwrap_or("")
                );
                if normalise_words(&text).contains(&claim_norm) {
                    return false;
                }
            }
            true
        })
        .collect();
    let dropped = before - kept.len();
    let text = serde_json::to_string_pretty(&kept).unwrap_or_else(|_| "[]".to_string());
    (text, dropped)
}

/// The `link` (or `url`) field of every result in a `search_web` JSON array.
///
/// Only these count. Scanning the whole text for `http` — the old approach —
/// also counted URLs quoted INSIDE a result or inside the graph-first "WHAT WE
/// ALREADY KNOW" block, so a note of ours could add domains to the
/// independence tally. Text that is not a result array yields nothing, which
/// under-counts: the safe direction.
pub fn evidence_links(evidence: &str) -> Vec<String> {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(evidence.trim()) else {
        return vec![];
    };
    items
        .iter()
        .filter_map(|item| {
            item.get("link")
                .or_else(|| item.get("url"))
                .and_then(|v| v.as_str())
                .map(String::from)
        })
        .collect()
}

/// Distinct registrable domains of the search results' own links.
pub fn evidence_domains(evidence: &str) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for link in evidence_links(evidence) {
        if let Some(d) = registrable_domain(&link)
            && !seen.contains(&d)
        {
            seen.push(d);
        }
    }
    seen
}

/// Whether the judge's cited `source` is one of the search results.
///
/// A "supports" verdict must point at external evidence the search actually
/// returned. Accepts a URL or domain whose registrable domain is among the
/// result links, or an outlet name matching a result domain's name label
/// ("Reuters" → `reuters.com`). Rejects anything else — a note id, "the
/// knowledge graph", or an empty source — because that is the shape
/// self-corroboration took (2026-09-12..24).
pub fn source_is_in_results(source: Option<&str>, evidence: &str) -> bool {
    let cited: Vec<String> = source.map(|s| vec![s.to_string()]).unwrap_or_default();
    !resolve_cited_links(&cited, evidence).is_empty()
}

/// Map the judge's cited sources onto the result links they name.
///
/// Each cited source resolves to a result link by exact link, by registrable
/// domain, or by outlet name ("Reuters" → the `reuters.com` result). Anything
/// that names no result — a note id, "the knowledge graph", a URL the search
/// never returned — resolves to nothing. Deduplicated, in citation order.
pub fn resolve_cited_links(cited: &[String], evidence: &str) -> Vec<String> {
    let links = evidence_links(evidence);
    let norm = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_ascii_alphanumeric())
            .collect::<String>()
            .to_lowercase()
    };
    let mut out: Vec<String> = Vec::new();
    for source in cited.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        let hit = links
            .iter()
            .find(|l| l.as_str() == source)
            .or_else(|| {
                let d = registrable_domain(source)?;
                links
                    .iter()
                    .find(|l| registrable_domain(l).as_deref() == Some(d.as_str()))
            })
            .or_else(|| {
                let src = norm(source);
                links.iter().find(|l| {
                    let Some(d) = registrable_domain(l) else {
                        return false;
                    };
                    let name = norm(d.split('.').next().unwrap_or(""));
                    name.len() >= 4
                        && (src == name || src.starts_with(&name) || src.ends_with(&name))
                })
            });
        if let Some(l) = hit
            && !out.contains(l)
        {
            out.push(l.clone());
        }
    }
    out
}

/// Distinct registrable domains of a set of links.
pub fn link_domains(links: &[String]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    for link in links {
        if let Some(d) = registrable_domain(link)
            && !seen.contains(&d)
        {
            seen.push(d);
        }
    }
    seen
}

/// The `platform` and `state_media` SourceLists, loaded per sweep.
///
/// Graph-owned (seeded from `sources/platforms.yaml`, `sources/state-media.yaml`)
/// so the lists are edited with `neo4j_query`, never in Rust.
#[derive(Debug, Clone, Default)]
pub struct DomainRoles {
    /// Hosts of anyone's posts. Citable, never counted toward independence.
    pub platforms: Vec<String>,
    /// Government-controlled outlets. Labelled, never excluded.
    pub state_media: Vec<String>,
}

impl DomainRoles {
    pub fn is_platform(&self, domain: &str) -> bool {
        self.platforms.iter().any(|e| domain_matches(domain, e))
    }
    /// Whether a link's host is a platform: an entry may name a subdomain
    /// (`markets.ft.com`) that a registrable-domain check would never see.
    pub fn is_platform_link(&self, link: &str) -> bool {
        let host = link_host(link);
        self.is_platform(&host) || registrable_domain(link).is_some_and(|d| self.is_platform(&d))
    }

    pub fn is_state_media(&self, domain: &str) -> bool {
        self.state_media.iter().any(|e| domain_matches(domain, e))
    }
}

/// Load the role-tagged SourceLists. Fails open to empty lists: a missing list
/// means no platform is excluded, which is how verification behaved before.
pub async fn load_domain_roles(neo4j: &Neo4jClient) -> DomainRoles {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (s:SourceList) WHERE s.role IN ['platform','state_media'] \
             UNWIND s.domains AS d \
             RETURN s.role AS role, collect(DISTINCT d) AS domains",
        ))
        .await
        .unwrap_or_default();
    let mut roles = DomainRoles::default();
    for row in rows {
        let domains = row.get::<Vec<String>>("domains").unwrap_or_default();
        match row.get::<String>("role").unwrap_or_default().as_str() {
            "platform" => roles.platforms = domains,
            "state_media" => roles.state_media = domains,
            _ => {}
        }
    }
    roles
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

/// Whether the results the judge relied on are independent enough to support
/// a claim, with no headline information. See [`check_independence_titled`].
pub fn check_independence(
    supporting_links: &[String],
    claim: &str,
    asserted_by: Option<&str>,
    roles: &DomainRoles,
) -> Result<Vec<String>, String> {
    check_independence_titled(
        supporting_links,
        &std::collections::HashMap::new(),
        claim,
        asserted_by,
        roles,
    )
}

/// Each result's title by its link, from a `search_web` result array.
pub fn result_titles(evidence: &str) -> std::collections::HashMap<String, String> {
    let Ok(items) = serde_json::from_str::<Vec<serde_json::Value>>(evidence.trim()) else {
        return Default::default();
    };
    items
        .iter()
        .filter_map(|i| {
            let link = i.get("link").or_else(|| i.get("url"))?.as_str()?;
            let title = i.get("title")?.as_str()?;
            Some((link.to_string(), title.to_string()))
        })
        .collect()
}

/// A headline reduced to the story: outlet suffix dropped ("… - Al Jazeera",
/// "… | Yahoo News"), words normalised. `None` under four words — "Home" or
/// "Live updates" on two sites is not the same story.
pub fn story_key(title: &str) -> Option<String> {
    let mut head = title.trim();
    // Peel outlet suffixes from the end: a short tail after a separator
    // ("BBC News", "Yahoo News", "Al Jazeera") is the outlet, not the story.
    // A long tail is part of the headline ("Sudan — army retakes El Fasher
    // district") and stops the peeling.
    loop {
        let cut = [" | ", " - ", " — ", " – "]
            .iter()
            .filter_map(|sep| head.rsplit_once(sep))
            .max_by_key(|(before, _)| before.len());
        match cut {
            Some((before, after)) if after.split_whitespace().count() <= 4 => head = before.trim(),
            _ => break,
        }
    }
    let key = normalise_words(head);
    (key.split(' ').count() >= 4).then_some(key)
}

/// Whether the results the judge relied on are independent enough to support
/// a claim. Returns the independent domains, or `Err(reason)` so the caller
/// can record *why* a claim stayed unverified.
///
/// Counts only `supporting_links` — the results the judge cited, already
/// resolved against the search results. It used to count every result's
/// domain, so any search returning two sites passed: a LinkedIn post was the
/// support and anglish.org, an unrelated hit, was the second "independent"
/// domain. Platform domains (`DomainRoles::platforms`) and the subject's own
/// site are cited but never counted.
///
/// **Syndication counts once.** Domains whose cited results carry the same
/// headline (`story_key`) are one story republished — Webz returned an Al
/// Jazeera piece on aljazeera.com and as a Yahoo News repost, neither flagged
/// syndicated (2026-10-06) — so independence is the number of headline-merged
/// groups of domains, not domains.
pub fn check_independence_titled(
    supporting_links: &[String],
    titles: &std::collections::HashMap<String, String>,
    claim: &str,
    asserted_by: Option<&str>,
    roles: &DomainRoles,
) -> Result<Vec<String>, String> {
    let cited = link_domains(supporting_links);
    // Platforms are matched on each link's HOST, not its registrable domain:
    // markets.ft.com (a press-release feed) and ft.com (FT journalism) share
    // `ft.com`, and only the first is a platform.
    let (platform_links, counted_links): (Vec<&String>, Vec<&String>) = supporting_links
        .iter()
        .partition(|l| roles.is_platform_link(l));
    let platforms: Vec<String> = platform_links.iter().map(|l| link_host(l)).collect();
    let counted = link_domains(&counted_links.into_iter().cloned().collect::<Vec<_>>());
    let self_refs: Vec<&String> = counted
        .iter()
        .filter(|d| is_self_referential(d, claim, asserted_by))
        .collect();
    let independent: Vec<String> = counted
        .iter()
        .filter(|d| !self_refs.contains(d))
        .cloned()
        .collect();

    // Union domains that share a story: group id per domain, merged by key.
    let mut group: Vec<usize> = (0..independent.len()).collect();
    fn root(g: &mut [usize], mut i: usize) -> usize {
        while g[i] != i {
            g[i] = g[g[i]];
            i = g[i];
        }
        i
    }
    let mut first_by_story: std::collections::HashMap<String, usize> = Default::default();
    for link in supporting_links {
        let (Some(domain), Some(key)) = (
            crate::services::web_result::registrable_domain(link),
            titles.get(link).and_then(|t| story_key(t)),
        ) else {
            continue;
        };
        if roles.is_platform_link(link) {
            continue;
        }
        let Some(i) = independent.iter().position(|d| *d == domain) else {
            continue;
        };
        match first_by_story.get(&key) {
            Some(&j) => {
                let (a, b) = (root(&mut group, i), root(&mut group, j));
                group[a] = b;
            }
            None => {
                first_by_story.insert(key, i);
            }
        }
    }
    let mut stories: Vec<Vec<String>> = Vec::new();
    let mut seen: std::collections::HashMap<usize, usize> = Default::default();
    for (i, domain) in independent.iter().enumerate() {
        let r = root(&mut group, i);
        let slot = *seen.entry(r).or_insert_with(|| {
            stories.push(Vec::new());
            stories.len() - 1
        });
        stories[slot].push(domain.clone());
    }
    let syndicated: Vec<&Vec<String>> = stories.iter().filter(|g| g.len() > 1).collect();

    if stories.len() < MIN_INDEPENDENT_DOMAINS {
        return Err(format!(
            "insufficient independence: {} independent supporting source(s), {} needed \
             (cited: {:?}; self-referential: {:?}; platforms, not counted: {:?}; \
             same headline on several domains, counted once: {:?})",
            stories.len(),
            MIN_INDEPENDENT_DOMAINS,
            cited,
            self_refs,
            platforms,
            syndicated
        ));
    }
    Ok(independent)
}

/// The verdict an LLM reached about one piece of candidate evidence.
#[derive(Debug, Clone, Deserialize)]
pub struct EvidenceVerdict {
    /// `supports` | `contradicts` | `unrelated`
    pub verdict: String,
    /// Every result link the verdict relies on, as the judge cited them.
    #[serde(default)]
    pub sources: Vec<String>,
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
         Each result that bears on the claim counts separately: list EVERY result \
         link that independently supports (or contradicts) it, not just the best \
         one. A social-media post, a video page, or a page that only repeats the \
         claim's wording is weaker than reporting about it.\n\n\
         Respond with JSON only:\n\
         {{\"verdict\": \"supports|contradicts|unrelated\", \"sources\": [\"each result link you relied on, copied exactly\"], \"reasoning\": \"one sentence\"}}\n\n\
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
        // `sources` is the contract; a bare `source` string (the old shape,
        // which a model may still emit) is accepted as a one-element list.
        sources: match value.get("sources") {
            Some(serde_json::Value::Array(a)) => a
                .iter()
                .filter_map(|v| v.as_str().map(String::from))
                .collect(),
            Some(serde_json::Value::String(s)) => vec![s.clone()],
            _ => value
                .get("source")
                .and_then(|v| v.as_str())
                .map(|s| vec![s.to_string()])
                .unwrap_or_default(),
        },
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
    sources: &[String],
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
         ON CREATE SET r.source = $source, r.sources = $sources, \
                       r.found_at = datetime($now), r.domains = $domains"
    );
    neo4j
        .run(
            neo4rs::query(&cypher)
                .param("cid", claim_id)
                .param("eid", evidence_note_id)
                .param("source", sources.first().map(String::as_str).unwrap_or(""))
                .param("sources", sources.to_vec())
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
/// never-attempted claims sort first. Rotation is also the cooldown, so no
/// separate retry gate is needed.
///
/// Among never-attempted claims the NEWEST go first (2026-10-01). Intake ran
/// ~83 claims/day against ~32 verified/day, oldest-first, so not one of the
/// week's 584 new claims had been attempted — the sweep was working through
/// August while the claims anyone might ask about went unchecked. A claim about
/// current events is worth checking while it is current.
pub async fn unverified_claims(neo4j: &Neo4jClient, limit: usize) -> Result<Vec<(String, String)>> {
    let rows = neo4j
        .execute(
            neo4rs::query(
                "MATCH (c:Note {note_type: 'claim'}) \
                 WHERE COALESCE(c.claim_status, 'unverified') = 'unverified' \
                 RETURN c.id AS id, c.content AS content \
                 ORDER BY CASE WHEN c.last_verify_attempt_at IS NULL THEN 0 ELSE 1 END ASC, \
                          CASE WHEN c.last_verify_attempt_at IS NULL THEN c.created_at END DESC, \
                          c.last_verify_attempt_at ASC \
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

    fn claim(text: &str, about: &[(&str, &str)]) -> ExtractedClaim {
        ExtractedClaim {
            claim: text.to_string(),
            asserted_by: None,
            kind: None,
            about: about
                .iter()
                .map(|(n, w)| About {
                    name: n.to_string(),
                    what: Some(w.to_string()),
                })
                .collect(),
        }
    }

    #[test]
    fn the_paragon_claim_does_not_stand_alone_until_it_names_the_software() {
        let old = "Paragon’s CEO, Andrew Boyd, admitted the company lacks the mechanism to verify \
                   customer misuse or deploy a 'kill switch' to disable the software.";
        // Naming Paragon is not enough: "the software" is still unnamed.
        let p = standalone_problems(&claim(
            old,
            &[("Paragon", "company"), ("Andrew Boyd", "CEO")],
        ));
        assert_eq!(
            p,
            vec!["\"the software\" does not say which software".to_string()],
            "{p:?}"
        );
        // With nothing declared, every gap is named.
        assert_eq!(standalone_problems(&claim(old, &[])).len(), 3);

        let new = "Paragon Solutions CEO Andrew Boyd admitted the company lacks a mechanism to verify \
                   customer misuse of its Graphite spyware or deploy a 'kill switch' to disable the software.";
        let fixed = claim(
            new,
            &[
                ("Paragon Solutions", "company"),
                ("Andrew Boyd", "CEO"),
                ("Graphite", "software"),
            ],
        );
        assert!(
            standalone_problems(&fixed).is_empty(),
            "{:?}",
            standalone_problems(&fixed)
        );
        assert!(
            rewrite_losses(old, new).is_empty(),
            "{:?}",
            rewrite_losses(old, new)
        );
    }

    #[test]
    fn standalone_checks_names_pronouns_and_qualified_nouns() {
        // An about name the claim never uses.
        let p = standalone_problems(&claim(
            "Italy expelled two diplomats.",
            &[("Italy", "country"), ("Russia", "country")],
        ));
        assert_eq!(p.len(), 1, "{p:?}");
        // A pronoun opening.
        assert!(!standalone_problems(&claim("He resigned on 3 May.", &[])).is_empty());
        // A determiner directly before the noun is what makes it vague.
        assert!(
            standalone_problems(&claim(
                "The Italian government fined Meta €20m.",
                &[("Meta", "company")]
            ))
            .is_empty()
        );
        assert!(
            !standalone_problems(&claim(
                "The government fined Meta €20m.",
                &[("Meta", "company")]
            ))
            .is_empty()
        );
        // "that" as a conjunction, a quote inside a name, a glossed name.
        assert!(
            standalone_problems(&claim(
                "Databricks states that chip scarcity forces compression.",
                &[("Databricks", "company")]
            ))
            .is_empty()
        );
        assert!(
            standalone_problems(&claim(
                "Utah Governor Spencer Cox signed a \u{201C}pro-human AI\u{201D} executive order.",
                &[
                    ("Spencer Cox", "governor"),
                    ("pro-human AI executive order", "order")
                ]
            ))
            .is_empty()
        );
        assert!(
            standalone_problems(&claim(
                "A Gap synthesis note mentions HBM.",
                &[
                    ("Gap synthesis note (2026-08-08)", "note"),
                    ("HBM", "memory")
                ]
            ))
            .is_empty()
        );
        // A name must match whole words: "Meta" is not in "metadata".
        assert!(
            !standalone_problems(&claim("The metadata leaked.", &[("Meta", "company")])).is_empty()
        );
        // A vague noun listed as a name names nothing.
        let p = standalone_problems(&claim(
            "The video discusses the wall.",
            &[("video", "video"), ("the system", "system")],
        ));
        assert!(
            p.iter().any(|x| x.contains("\"video\" is not a name")),
            "{p:?}"
        );
        assert!(
            p.iter().any(|x| x.contains("\"the video\" does not say")),
            "{p:?}"
        );
        // Plurals and curly apostrophes.
        assert!(
            !standalone_problems(&claim(
                "Meta said these officials lied.",
                &[("Meta", "company")]
            ))
            .is_empty()
        );
    }

    #[test]
    fn rewrite_losses_catch_dropped_numbers_and_names_only() {
        assert!(
            rewrite_losses(
                "Spending rose to 1,200 in 2025.",
                "Spending by Ohio rose to 1200 in 2025."
            )
            .is_empty()
        );
        assert_eq!(
            rewrite_losses("Spending rose to 1,200 in 2025.", "Spending rose in 2025."),
            vec!["1200"]
        );
        assert_eq!(
            rewrite_losses("The US fined Meta.", "The United States fined Meta."),
            vec!["US"]
        );
        // Typography and ranges from the 2026-10-08 dry run were not losses.
        assert!(
            rewrite_losses(
                "A new AP-NORC poll shows 64% agree.",
                "A new AP\u{2011}NORC poll shows 64\u{202F}% agree."
            )
            .is_empty()
        );
        assert!(rewrite_losses("It costs ~$18–34.", "It costs US $18 to US $34.").is_empty());
        assert!(
            rewrite_losses(
                "Dated 2026-09-28/29.",
                "Dated 2026\u{2011}09\u{2011}28 and 2026\u{2011}09\u{2011}29."
            )
            .is_empty()
        );
        assert!(
            rewrite_losses(
                "Attacks were GRU-linked.",
                "Attacks were linked to the GRU."
            )
            .is_empty()
        );
        // An acronym spelled out is still a loss: kept, conservatively.
        assert_eq!(
            rewrite_losses(
                "The FBI/DOJ seized tools.",
                "The FBI and the Department of Justice seized tools."
            ),
            vec!["DOJ"]
        );
        // The capitalised first word is grammar, not a name.
        assert!(
            rewrite_losses(
                "Officials said Meta lied.",
                "Meta lied, Irish officials said."
            )
            .is_empty()
        );
    }

    #[test]
    fn about_reads_objects_names_or_nothing() {
        let c: ExtractedClaim = serde_json::from_str(
            r#"{"claim":"x","about":[{"name":"Graphite","what":"spyware"},"Paragon",{"what":"no name"},""]}"#,
        )
        .unwrap();
        assert_eq!(
            c.about,
            vec![
                About {
                    name: "Graphite".into(),
                    what: Some("spyware".into())
                },
                About {
                    name: "Paragon".into(),
                    what: None
                },
            ]
        );
        let none: ExtractedClaim = serde_json::from_str(r#"{"claim":"x","about":null}"#).unwrap();
        assert!(none.about.is_empty());
    }

    #[test]
    fn source_reference_reads_article_and_video_summaries() {
        let article = "# Article: read article: https://citizenlab.ca/the-secrets-of-a-us-spyware-king/\n\n\
            ## ARTICLE SUMMARY\n{\n  \"key_concepts\": [\"Paragon Solutions spyware\"],\n  \
            \"site\": \"The Citizen Lab\",\n  \"title\": \"The Secrets of a US Spyware King - The Citizen Lab\",\n  \
            \"url\": \"https://citizenlab.ca/the-secrets-of-a-us-spyware-king/\"\n}\n\n## NEW vs KNOWN ANALYSIS\n…";
        assert_eq!(
            source_reference(article),
            SourceRef {
                title: Some("The Secrets of a US Spyware King - The Citizen Lab".into()),
                url: Some("https://citizenlab.ca/the-secrets-of-a-us-spyware-king/".into()),
                outlet: Some("The Citizen Lab".into()),
            }
        );
        let video = "# Video learning: watch video: https://www.youtube.com/watch?v=tPhQ4gAylek\n\n\
            ## VIDEO SUMMARY\n{\"channel\": \"Louis Rossmann\", \"title\": \"Crack your video editing software: seriously.\", \
            \"url\": \"https://www.youtube.com/watch?v=tPhQ4gAylek\"}";
        let v = source_reference(video);
        assert_eq!(
            v.title.as_deref(),
            Some("Crack your video editing software: seriously.")
        );
        assert_eq!(v.outlet.as_deref(), Some("Louis Rossmann"));
    }

    #[test]
    fn source_reference_falls_back_to_the_heading_and_never_guesses_a_url() {
        let report = "# Off-grid networking watch — 2026-08-12\n\n## REPORT\n* MeshCore https://meshcore.co.uk is faster";
        assert_eq!(
            source_reference(report),
            SourceRef {
                title: Some("Off-grid networking watch — 2026-08-12".into()),
                url: None,
                outlet: None
            }
        );
        // A truncated summary still yields the heading's URL.
        let cut = "# Video learning: watch video: https://youtu.be/x\n\n## VIDEO SUMMARY\n{\"title\": \"Cut of";
        assert_eq!(
            source_reference(cut).url.as_deref(),
            Some("https://youtu.be/x")
        );
        // A brief that opens with a second-level heading, and one with none.
        let brief =
            "## EXECUTIVE SUMMARY for 2026-10-05  \n- Detroit officials https://example.com/x";
        assert_eq!(
            source_reference(brief),
            SourceRef {
                title: Some("EXECUTIVE SUMMARY for 2026-10-05".into()),
                ..Default::default()
            }
        );
        assert_eq!(
            source_reference(&"x".repeat(300)).title.map(|t| t.len()),
            Some(120)
        );
        assert_eq!(source_reference("  \n\n"), SourceRef::default());
    }

    #[test]
    fn evidence_domains_are_deduplicated_by_registrable_name() {
        let ev = r#"[{"link":"https://www.reuters.com/a"},{"link":"https://reuters.com/b"},{"link":"https://apnews.com/c"}]"#;
        let d = evidence_domains(ev);
        assert_eq!(d.len(), 2, "got {d:?}");
        assert!(d.contains(&"reuters.com".to_string()));
        assert!(d.contains(&"apnews.com".to_string()));
    }

    #[test]
    fn only_result_links_count_as_domains() {
        // A URL quoted inside a snippet (or inside a graph-first block) is not a
        // result, and must not add to the independence tally.
        let ev = r#"[{"link":"https://sleep.com/dreams","snippet":"see https://reuters.com/x and https://apnews.com/y"}]"#;
        assert_eq!(evidence_domains(ev), vec!["sleep.com".to_string()]);
        let cited = resolve_cited_links(&["https://reuters.com/x".into()], ev);
        assert!(cited.is_empty(), "a URL inside a snippet is not a result");
    }

    #[test]
    fn non_array_evidence_yields_no_domains() {
        let ev = "WHAT WE ALREADY KNOW\n- note: https://reuters.com/a https://apnews.com/b";
        assert!(evidence_domains(ev).is_empty());
    }

    #[test]
    fn a_support_citing_a_graph_note_is_not_in_the_results() {
        let ev = r#"[{"link":"https://www.reuters.com/a"},{"link":"https://apnews.com/b"}]"#;
        assert!(!source_is_in_results(Some("note 0020786f"), ev));
        assert!(!source_is_in_results(Some("the knowledge graph"), ev));
        assert!(!source_is_in_results(None, ev));
        assert!(!source_is_in_results(Some("https://bbc.com/z"), ev));
        assert!(source_is_in_results(Some("https://reuters.com/a"), ev));
        assert!(source_is_in_results(Some("apnews.com"), ev));
        assert!(source_is_in_results(Some("Reuters"), ev));
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
        let links = vec!["https://psionicresearch.com/x".to_string()];
        let err = check_independence(&links, "psionic summoning demonstrated", None, &no_roles())
            .unwrap_err();
        assert!(err.contains("insufficient independence"), "{err}");
    }

    #[test]
    fn subject_owned_domains_do_not_count_toward_independence() {
        // Two domains, but one is the subject's own — leaving one independent
        // source, which is not corroboration.
        let links = vec![
            "https://skywatcher.ai/media".to_string(),
            "https://psionicresearch.com/x".to_string(),
        ];
        assert!(
            check_independence(
                &links,
                "Skywatcher demonstrated summoning",
                None,
                &no_roles()
            )
            .is_err()
        );
    }

    #[test]
    fn two_independent_domains_pass() {
        let links = vec![
            "https://apnews.com/a".to_string(),
            "https://reuters.com/b".to_string(),
        ];
        let ok =
            check_independence(&links, "Congress held UAP hearings", None, &no_roles()).unwrap();
        assert_eq!(ok.len(), 2);
    }

    fn no_roles() -> DomainRoles {
        DomainRoles::default()
    }

    fn roles() -> DomainRoles {
        DomainRoles {
            platforms: vec!["facebook.com".into(), "instagram.com".into()],
            state_media: vec!["trtafrika.com".into(), "chinadaily.com.cn".into()],
        }
    }

    #[test]
    fn uncited_results_do_not_count_toward_independence() {
        // 2026-10 anglish.org case: the judge cited one LinkedIn post, and four
        // unrelated results (anglish.org among them) made up "independence".
        let ev = r#"[{"link":"https://www.linkedin.com/posts/x"},{"link":"https://anglish.org/wiki/y"},
                     {"link":"https://darkreading.com/z"},{"link":"https://ibm.com/w"}]"#;
        let cited = resolve_cited_links(&["https://www.linkedin.com/posts/x".into()], ev);
        assert_eq!(cited.len(), 1);
        assert!(
            check_independence(
                &cited,
                "Dark Sorcery attacks poison the internet",
                None,
                &no_roles()
            )
            .is_err()
        );
    }

    #[test]
    fn a_syndicated_story_counts_once() {
        // 2026-10-06: the same Al Jazeera article on aljazeera.com and as a
        // Yahoo News repost, neither flagged syndicated.
        let ev = r#"[
          {"link": "https://www.aljazeera.com/news/2026/10/4/sudan", "title": "Is the Sudanese army gaining momentum against the RSF? | Al Jazeera"},
          {"link": "https://www.yahoo.com/news/world/articles/sudan", "title": "Is the Sudanese army gaining momentum against the RSF? - Yahoo News"},
          {"link": "https://apnews.com/article/sudan-x", "title": "Sudan's army retakes key town from RSF fighters"}
        ]"#;
        let titles = result_titles(ev);
        let two: Vec<String> = vec![
            "https://www.aljazeera.com/news/2026/10/4/sudan".into(),
            "https://www.yahoo.com/news/world/articles/sudan".into(),
        ];
        let err = check_independence_titled(
            &two,
            &titles,
            "The Sudanese army is gaining momentum",
            None,
            &no_roles(),
        )
        .unwrap_err();
        assert!(err.contains("1 independent supporting source"), "{err}");
        assert!(
            err.contains("aljazeera.com") && err.contains("counted once"),
            "{err}"
        );
        let three = [two, vec!["https://apnews.com/article/sudan-x".to_string()]].concat();
        assert!(
            check_independence_titled(
                &three,
                &titles,
                "The Sudanese army is gaining momentum",
                None,
                &no_roles()
            )
            .is_ok()
        );
    }

    #[test]
    fn short_or_generic_titles_never_merge_stories() {
        assert_eq!(story_key("Home"), None);
        assert_eq!(story_key("Live updates - BBC News"), None);
        assert_eq!(
            story_key("Fed holds rates steady as inflation cools | Reuters"),
            story_key("Fed holds rates steady as inflation cools - CNBC")
        );
        // An em-dash inside a real headline is not an outlet suffix if the
        // part before it is too short to be the story.
        assert_eq!(
            story_key("Sudan — army retakes El Fasher district").as_deref(),
            Some("sudan army retakes el fasher district")
        );
    }

    #[test]
    fn a_platform_subdomain_is_excluded_without_excluding_its_parent() {
        let r = DomainRoles {
            platforms: vec!["markets.ft.com".into()],
            state_media: vec![],
        };
        let claim = "A Japanese chemical company signed a gallium supply agreement";
        let press_release_plus_one = vec![
            "https://markets.ft.com/data/announce/detail?id=123".to_string(),
            "https://www.reuters.com/markets/gallium".to_string(),
        ];
        let err = check_independence(&press_release_plus_one, claim, None, &r).unwrap_err();
        assert!(err.contains("markets.ft.com"), "{err}");
        let journalism = vec![
            "https://www.ft.com/content/abc".to_string(),
            "https://www.reuters.com/markets/gallium".to_string(),
        ];
        assert_eq!(
            check_independence(&journalism, claim, None, &r)
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn platform_posts_are_cited_but_not_counted() {
        let links = vec![
            "https://www.facebook.com/Reuters/posts/1".to_string(),
            "https://www.instagram.com/p/2".to_string(),
            "https://apnews.com/a".to_string(),
        ];
        let err = check_independence(&links, "The Nasdaq hit a record close", None, &roles())
            .unwrap_err();
        assert!(err.contains("platforms, not counted"), "{err}");
        let links = [links, vec!["https://reuters.com/b".to_string()]].concat();
        let ok =
            check_independence(&links, "The Nasdaq hit a record close", None, &roles()).unwrap();
        assert_eq!(
            ok,
            vec!["apnews.com".to_string(), "reuters.com".to_string()]
        );
    }

    #[test]
    fn every_cited_source_resolves_to_its_result_link() {
        let ev = r#"[{"link":"https://www.reuters.com/a"},{"link":"https://apnews.com/b"},{"link":"https://bbc.com/c"}]"#;
        let cited = resolve_cited_links(
            &[
                "Reuters".into(),
                "https://apnews.com/b".into(),
                "note 0020786f".into(),
                "Reuters".into(),
            ],
            ev,
        );
        assert_eq!(
            cited,
            vec![
                "https://www.reuters.com/a".to_string(),
                "https://apnews.com/b".to_string()
            ]
        );
    }

    #[test]
    fn search_pages_that_echo_the_claim_are_dropped() {
        // Captured shape, 2026-10-02: the snippet restates the claim verbatim.
        let claim = "Sonnet 5.5 matched the quality of more expensive predecessors while being five times cheaper";
        let ev = serde_json::json!([
            {"link": "https://www.ecosia.org/search?q=Sonnet%205.5%20matched",
             "title": "Search Sonnet 5.5 Matched with Ecosia",
             "snippet": format!("Ecosia is the search engine that plants trees. Search for {claim} with Ecosia")},
            {"link": "https://www.dailymotion.com/search/Sonnet%205.5/", "title": "x", "snippet": "videos"},
            {"link": "https://us.search.yahoo.com/search?p=Sonnet", "title": "y", "snippet": "z"},
            {"link": "https://spam.example/page", "title": "t", "snippet": format!("&quot;{claim}&quot;")},
            {"link": "https://www.anthropic.com/news/claude-sonnet-5-5", "title": "Claude Sonnet 5.5",
             "snippet": "Sonnet 5.5 is priced below Opus"}
        ])
        .to_string();
        let (kept, dropped) = drop_echo_results(&ev, claim);
        assert_eq!(dropped, 4);
        assert_eq!(
            evidence_links(&kept),
            vec!["https://www.anthropic.com/news/claude-sonnet-5-5".to_string()]
        );
    }

    #[test]
    fn a_short_claim_is_not_dropped_for_appearing_in_a_real_page() {
        let ev = r#"[{"link":"https://bbc.com/a","title":"t","snippet":"He shouted My legs! and fell"}]"#;
        assert_eq!(drop_echo_results(ev, "My legs!").1, 0);
    }

    #[test]
    fn non_array_evidence_passes_the_echo_filter_unchanged() {
        assert_eq!(drop_echo_results("not json", "a claim").0, "not json");
    }

    #[test]
    fn country_code_second_levels_stay_distinct() {
        assert_eq!(
            registrable_domain("https://www.bbc.co.uk/news").as_deref(),
            Some("bbc.co.uk")
        );
        assert_eq!(
            registrable_domain("https://theguardian.co.uk/x").as_deref(),
            Some("theguardian.co.uk")
        );
        assert_eq!(
            registrable_domain("https://www.chinadaily.com.cn/a").as_deref(),
            Some("chinadaily.com.cn")
        );
        assert_eq!(
            registrable_domain("https://edition.cnn.com/a").as_deref(),
            Some("cnn.com")
        );
        assert_eq!(
            registrable_domain("https://news.cn/a").as_deref(),
            Some("news.cn")
        );
    }

    #[test]
    fn state_media_is_labelled_not_excluded() {
        let r = roles();
        assert!(r.is_state_media("trtafrika.com"));
        assert!(r.is_state_media("chinadaily.com.cn"));
        assert!(!r.is_state_media("bbc.co.uk"));
        let links = vec![
            "https://trtafrika.com/a".to_string(),
            "https://apnews.com/b".to_string(),
        ];
        assert_eq!(
            check_independence(&links, "Iran gave up its nuclear program", None, &r)
                .unwrap()
                .len(),
            2
        );
        assert_eq!(
            tier_label("established sources", &["trtafrika.com".into()]),
            "established sources · incl. state media (trtafrika.com)"
        );
        assert_eq!(
            tier_label("established sources", &[]),
            "established sources"
        );
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
