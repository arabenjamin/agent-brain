//! Retrieval evaluation harness.
//!
//! There is exactly one way to know whether a change to retrieval — a freshness
//! weight, the RRF `k`, a different embedding model, a contamination migration —
//! made retrieval *better* rather than merely different: measure recall against a
//! fixed set of (query → note-that-should-come-back) judgements. Without that,
//! every knob in `KnowledgeService::search_notes_inner` is set by intuition and
//! every future "improvement" is an argument instead of a number.
//!
//! This module runs a **golden set** (a YAML fixture, human-owned and
//! version-controlled) through the *real* retrieval pipeline via
//! [`KnowledgeService::search_notes_readonly`] — the non-perturbing path, so
//! scoring the graph never moves the freshness signal it is scoring — and
//! reports recall@k and mean reciprocal rank, per case and in aggregate.
//!
//! The friction that kills eval harnesses is writing the first thirty cases by
//! hand, so [`bootstrap`] samples distinctive notes from the live graph and
//! emits *proposed* cases for a human to curate. A proposal is a starting point,
//! never ground truth: its query is derived from the note's own text, so it
//! tests "can retrieval find a note from a phrase inside it", which is the floor,
//! not the real target. Edit the queries to what a person would actually ask.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::repository::Neo4jClient;
use crate::services::knowledge::KnowledgeService;

/// One judgement: a query, and what a correct retrieval must surface for it.
///
/// A case matches if **any** expected id or substring appears in the top-k.
/// Two ways to express "expected" on purpose:
/// - `expect_ids` is exact and survives content edits, but a note's uuid is
///   opaque and churns if the note is ever rewritten/re-ingested.
/// - `expect_substrings` (case-insensitive) survives id churn and re-embeds,
///   which is exactly the case where you most want the harness to keep working
///   (e.g. before/after swapping the embedding model). Prefer a short,
///   distinctive phrase from the note body.
///
/// Provide at least one of the two; a case with neither can never pass and is
/// reported as malformed.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct GoldenCase {
    /// The query as a person (or a chain) would actually phrase it.
    pub query: String,
    /// Note ids, any of which counts as a hit.
    #[serde(default)]
    pub expect_ids: Vec<String>,
    /// Case-insensitive content substrings, any of which counts as a hit.
    #[serde(default)]
    pub expect_substrings: Vec<String>,
    /// Optional: why this pair matters. Never read by the runner; it is here so
    /// the fixture explains itself to the next human.
    #[serde(default)]
    pub note: Option<String>,
    /// Optional per-case override of the note_type filter (e.g. "claim").
    /// `None` searches all types, which is what `search_notes` does by default.
    #[serde(default)]
    pub note_type: Option<String>,
}

impl GoldenCase {
    fn is_malformed(&self) -> bool {
        self.expect_ids.is_empty() && self.expect_substrings.is_empty()
    }

    /// Rank (1-indexed) of the first result that satisfies this case, if any.
    fn first_hit_rank(&self, results: &[SearchHit]) -> Option<usize> {
        results.iter().position(|hit| self.hits(hit)).map(|i| i + 1)
    }

    fn hits(&self, hit: &SearchHit) -> bool {
        if self.expect_ids.iter().any(|id| id == &hit.id) {
            return true;
        }
        let content_lc = hit.content.to_lowercase();
        self.expect_substrings
            .iter()
            .any(|s| content_lc.contains(&s.to_lowercase()))
    }
}

/// The whole fixture.
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct GoldenSet {
    #[serde(default)]
    pub cases: Vec<GoldenCase>,
}

/// A flattened search result the case matchers can read.
struct SearchHit {
    id: String,
    content: String,
}

/// The outcome for one case.
#[derive(Debug, Clone)]
pub struct CaseResult {
    pub query: String,
    /// 1-indexed rank of the first expected hit within top-k; `None` = miss.
    pub hit_rank: Option<usize>,
    /// True if the case was malformed (no expectations) and was skipped.
    pub malformed: bool,
}

/// Aggregate report across all cases.
#[derive(Debug, Clone)]
pub struct EvalReport {
    pub k: usize,
    pub cases: Vec<CaseResult>,
}

impl EvalReport {
    fn scored(&self) -> impl Iterator<Item = &CaseResult> {
        self.cases.iter().filter(|c| !c.malformed)
    }

    /// Fraction of scored cases with at least one expected hit in top-k.
    pub fn recall_at_k(&self) -> f64 {
        let scored: Vec<_> = self.scored().collect();
        if scored.is_empty() {
            return 0.0;
        }
        let hits = scored.iter().filter(|c| c.hit_rank.is_some()).count();
        hits as f64 / scored.len() as f64
    }

    /// Mean reciprocal rank over scored cases (a miss contributes 0).
    pub fn mrr(&self) -> f64 {
        let scored: Vec<_> = self.scored().collect();
        if scored.is_empty() {
            return 0.0;
        }
        let sum: f64 = scored
            .iter()
            .map(|c| c.hit_rank.map(|r| 1.0 / r as f64).unwrap_or(0.0))
            .sum();
        sum / scored.len() as f64
    }

    /// A human-readable, greppable report. This is the thing you watch move when
    /// you change a weight.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "RETRIEVAL EVAL — recall@{k} and MRR over {n} cases\n",
            k = self.k,
            n = self.scored().count()
        ));
        out.push_str(&"-".repeat(72));
        out.push('\n');
        for c in &self.cases {
            let query = truncate(&c.query, 56);
            if c.malformed {
                out.push_str(&format!("  SKIP (no expectations)  {query}\n"));
            } else if let Some(rank) = c.hit_rank {
                out.push_str(&format!("  HIT  @{rank:<3}             {query}\n"));
            } else {
                out.push_str(&format!("  MISS                    {query}\n"));
            }
        }
        out.push_str(&"-".repeat(72));
        out.push('\n');
        let malformed = self.cases.len() - self.scored().count();
        out.push_str(&format!(
            "recall@{k} = {recall:.3}   MRR = {mrr:.3}   ({scored} scored, {malformed} skipped)\n",
            k = self.k,
            recall = self.recall_at_k(),
            mrr = self.mrr(),
            scored = self.scored().count(),
        ));
        out
    }
}

fn truncate(s: &str, max: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() <= max {
        s
    } else {
        let head: String = s.chars().take(max.saturating_sub(1)).collect();
        format!("{head}…")
    }
}

/// Load a golden set from a YAML file.
pub fn load_fixture(path: &str) -> Result<GoldenSet> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading retrieval fixture {path}"))?;
    let set: GoldenSet =
        serde_yaml::from_str(&text).with_context(|| format!("parsing retrieval fixture {path}"))?;
    Ok(set)
}

/// Run every case through the real (non-perturbing) retrieval pipeline and score
/// it. `k` is the cutoff for both the search `limit` and the recall/MRR window.
pub async fn run_eval(
    knowledge: &KnowledgeService,
    set: &GoldenSet,
    k: usize,
) -> Result<EvalReport> {
    let mut cases = Vec::with_capacity(set.cases.len());
    for case in &set.cases {
        if case.is_malformed() {
            cases.push(CaseResult {
                query: case.query.clone(),
                hit_rank: None,
                malformed: true,
            });
            continue;
        }

        let results = knowledge
            .search_notes_readonly(&case.query, k, 0, case.note_type.as_deref())
            .await
            .with_context(|| format!("searching for {:?}", case.query))?;

        let hits: Vec<SearchHit> = results
            .into_iter()
            .map(|v| SearchHit {
                id: v
                    .get("id")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
                content: v
                    .get("content")
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string(),
            })
            .collect();

        cases.push(CaseResult {
            query: case.query.clone(),
            hit_rank: case.first_hit_rank(&hits),
            malformed: false,
        });
    }
    Ok(EvalReport { k, cases })
}

/// Sample `n` distinctive notes from the graph and emit proposed golden cases.
///
/// "Distinctive" here means: has real content, is not a chunk (chunks are
/// retrieved via their parent, so a chunk id is the wrong expectation), and is
/// not one of the LLM-generated meta types whose text is a poor query seed. For
/// each, the proposed query is the note's own first substantial line and the
/// expectation is both its id and a distinctive substring — so the human only
/// has to rewrite the *query* into something a person would ask.
///
/// Read-only. Emits YAML to return, which the caller writes or prints.
pub async fn bootstrap(neo4j: &Neo4jClient, n: usize) -> Result<String> {
    // Exclude chunks (PART_OF children, retrieved via their parent) and keep to
    // the note types a human actually asks the brain about. `episodic` is left
    // out on purpose: it is dominated by operational logs — "Scheduler
    // dispatched task (id: …)" and the like — which are noise for a golden set,
    // not knowledge anyone queries for. The `STARTS WITH` guard also drops any
    // such log that slipped in under another type. `rand()` gives a fresh sample
    // each run, which is what you want while curating.
    let cypher = r#"
        MATCH (note:Note)
        WHERE NOT (note)-[:PART_OF]->()
          AND note.content IS NOT NULL
          AND size(note.content) > 120
          AND COALESCE(note.note_type, 'semantic') IN
              ['semantic', 'source_record', 'claim', 'news']
          AND NOT note.content STARTS WITH 'Scheduler dispatched task'
        RETURN note.id AS id, note.content AS content
        ORDER BY rand()
        LIMIT $n
    "#;
    let rows = neo4j
        .execute(neo4rs::query(cypher).param("n", n as i64))
        .await
        .context("sampling notes for bootstrap")?;

    let mut set = GoldenSet::default();
    for row in rows {
        let id = row.get::<String>("id").unwrap_or_default();
        let content = row.get::<String>("content").unwrap_or_default();
        if id.is_empty() || content.is_empty() {
            continue;
        }
        let query = first_substantial_line(&content, 90);
        let substring = distinctive_substring(&content, 40);
        set.cases.push(GoldenCase {
            query,
            expect_ids: vec![id],
            expect_substrings: vec![substring],
            note: Some(
                "PROPOSED — rewrite `query` into what a person would actually ask; \
                 the seed query is a phrase copied from the note itself."
                    .to_string(),
            ),
            note_type: None,
        });
    }

    let header = "# Retrieval golden set — PROPOSED cases from `eval-retrieval --bootstrap`.\n\
                  # Each case's `query` is a phrase lifted from the note; that only tests\n\
                  # the retrieval floor. Rewrite every `query` into a real question before\n\
                  # trusting the numbers, then append these to eval/retrieval_golden.yaml.\n";
    let body = serde_yaml::to_string(&set).context("serializing proposed cases")?;
    Ok(format!("{header}{body}"))
}

/// The first line with real words, trimmed of a leading markdown heading marker,
/// capped at `max` chars. Used as a query seed, not as ground truth.
fn first_substantial_line(content: &str, max: usize) -> String {
    // Prefer a real prose line over a markdown heading — a heading like
    // "# METRO DETROIT — raw" is a banner, not a question, and makes a poor
    // query seed. Fall back to any substantial line (heading-stripped) if the
    // note is all headings.
    let substantial = |l: &str| l.split_whitespace().count() >= 4;
    let non_heading = content
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.starts_with('#') && substantial(l));
    let line = non_heading
        .or_else(|| {
            content
                .lines()
                .map(|l| l.trim().trim_start_matches('#').trim())
                .find(|l| substantial(l))
        })
        .unwrap_or_else(|| content.trim());
    truncate(line, max)
}

/// A mid-body substring likely to be unique to this note, for id-independent
/// matching. Skips the first line (often a shared heading banner) when possible.
fn distinctive_substring(content: &str, len: usize) -> String {
    let body = content
        .lines()
        .skip(1)
        .find(|l| l.split_whitespace().count() >= 5)
        .unwrap_or_else(|| content.trim());
    truncate(body.trim(), len).trim_end_matches('…').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hit(id: &str, content: &str) -> SearchHit {
        SearchHit {
            id: id.to_string(),
            content: content.to_string(),
        }
    }

    #[test]
    fn matches_by_id_and_substring_case_insensitively() {
        let case = GoldenCase {
            query: "q".into(),
            expect_ids: vec!["abc".into()],
            expect_substrings: vec!["HBM Shortage".into()],
            note: None,
            note_type: None,
        };
        // id match at rank 2
        let by_id = vec![hit("x", "nope"), hit("abc", "whatever")];
        assert_eq!(case.first_hit_rank(&by_id), Some(2));
        // substring match, different casing, at rank 1
        let by_sub = vec![hit("y", "the hbm shortage worsened"), hit("abc2", "z")];
        assert_eq!(case.first_hit_rank(&by_sub), Some(1));
        // no match
        let none = vec![hit("y", "unrelated"), hit("z", "also unrelated")];
        assert_eq!(case.first_hit_rank(&none), None);
    }

    #[test]
    fn malformed_case_is_flagged() {
        let case = GoldenCase {
            query: "q".into(),
            expect_ids: vec![],
            expect_substrings: vec![],
            note: None,
            note_type: None,
        };
        assert!(case.is_malformed());
    }

    #[test]
    fn recall_and_mrr_ignore_malformed_and_score_ranks() {
        let report = EvalReport {
            k: 5,
            cases: vec![
                CaseResult {
                    query: "a".into(),
                    hit_rank: Some(1),
                    malformed: false,
                },
                CaseResult {
                    query: "b".into(),
                    hit_rank: Some(4),
                    malformed: false,
                },
                CaseResult {
                    query: "c".into(),
                    hit_rank: None,
                    malformed: false,
                },
                CaseResult {
                    query: "d".into(),
                    hit_rank: None,
                    malformed: true, // skipped, must not count
                },
            ],
        };
        // 2 of 3 scored cases hit.
        assert!((report.recall_at_k() - 2.0 / 3.0).abs() < 1e-9);
        // MRR = (1/1 + 1/4 + 0) / 3
        assert!((report.mrr() - (1.0 + 0.25) / 3.0).abs() < 1e-9);
    }

    #[test]
    fn empty_report_is_zero_not_nan() {
        let report = EvalReport {
            k: 5,
            cases: vec![],
        };
        assert_eq!(report.recall_at_k(), 0.0);
        assert_eq!(report.mrr(), 0.0);
    }

    #[test]
    fn query_seed_strips_heading_and_finds_words() {
        let content =
            "# METRO DETROIT — raw\nThe city council approved the new water bond measure today.";
        let q = first_substantial_line(content, 90);
        assert!(q.starts_with("The city council"), "got: {q}");
    }
}
