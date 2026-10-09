//! Rewrite stored claims so each stands alone (HBI plan Phase 9, §9.6).
//!
//! Claims extracted before Phase 9 often lean on their source: "admitted the
//! company lacks the mechanism … to disable the software" names neither the
//! company's product nor, on its own, which software. Each source note's
//! claims are rewritten together in one call that sees the source text, and
//! every answer is held to two mechanical checks before it replaces anything:
//!
//! - `claims::standalone_problems` — the claim names what it is about;
//! - `claims::rewrite_losses` — no number or name of the original went missing.
//!
//! An answer that fails either gets one repair call, told exactly what is
//! wrong; one that still fails keeps the original text, marked `ambiguous`
//! with the reason, so what could not be resolved is a visible count rather
//! than a guess. A rewritten claim keeps its id, status, attribution and
//! evidence edges; the old text stays in `content_v1`. `standalone_checked_at`
//! marks a claim done, so a run can stop and resume.
//!
//! A dry run picks sources round-robin across writers until it has `sample`
//! claims, asks the model, and writes nothing: its report is for a person to
//! read before the full run.

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

use anyhow::{Context, Result};
use serde::Serialize;
use tracing::{info, warn};

use crate::repository::Neo4jClient;
use crate::services::claims::{
    About, ExtractedClaim, KEEP_AS_WRITTEN, REWRITE_SOURCE_CHARS, REWRITE_SYSTEM, STANDALONE_RULES,
    SourceRef, canon, repair_claim, rewrite_losses, source_reference, standalone_problems,
};
use crate::services::traits::LlmProvider;

pub struct Options {
    pub dry_run: bool,
    /// Dry run: how many claims to show.
    pub sample: usize,
    /// Apply: stop after this many sources (the rest wait for the next run).
    pub limit_sources: Option<usize>,
    /// Only these source notes (a dry run on a chosen source, e.g. one a
    /// person flagged). Empty: every source with unchecked claims.
    pub source_ids: Vec<String>,
}

/// What became of one claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, PartialOrd, Ord)]
#[serde(rename_all = "snake_case")]
pub enum Outcome {
    /// Replaced by a version that stands alone and lost nothing.
    Rewritten,
    /// The model returned it unchanged, and it passes the check.
    AlreadyStandsAlone,
    /// Kept as it was: the rewrite still doesn't stand alone.
    Ambiguous,
    /// Kept as it was: the rewrite dropped a number or a name.
    KeptMeaningGuard,
    /// Kept as it was, marked ambiguous: no source note to rewrite from.
    NoSource,
    /// The model's answer had no entry for it; retried on the next run.
    NoAnswer,
    /// The rewrite passed but could not be embedded; retried on the next run.
    EmbedFailed,
}

#[derive(Debug, Clone, Serialize)]
pub struct Item {
    pub claim_id: String,
    pub source_id: Option<String>,
    pub source_context: String,
    pub source_title: Option<String>,
    pub before: String,
    pub after: Option<String>,
    pub about: Vec<About>,
    pub outcome: Outcome,
    pub reason: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct Report {
    pub dry_run: bool,
    pub sources: usize,
    /// Sources whose call failed outright (their claims are left for a rerun).
    pub calls_failed: usize,
    pub snapshot: Option<String>,
    pub items: Vec<Item>,
}

impl Report {
    pub fn counts(&self) -> BTreeMap<Outcome, usize> {
        let mut c = BTreeMap::new();
        for i in &self.items {
            *c.entry(i.outcome).or_default() += 1;
        }
        c
    }

    /// Before / after for a person to read, grouped by writer.
    pub fn render_markdown(&self) -> String {
        let mut out = format!(
            "# Claim rewrite {}\n\n{} claims from {} sources ({} calls failed).\n\n",
            if self.dry_run {
                "dry run (nothing written)"
            } else {
                "run"
            },
            self.items.len(),
            self.sources,
            self.calls_failed
        );
        for (o, n) in self.counts() {
            out.push_str(&format!(
                "- {}: {n}\n",
                serde_json::to_value(o)
                    .unwrap_or_default()
                    .as_str()
                    .unwrap_or("?")
            ));
        }
        let mut by_ctx: BTreeMap<&str, Vec<&Item>> = BTreeMap::new();
        for i in &self.items {
            by_ctx.entry(&i.source_context).or_default().push(i);
        }
        for (ctx, items) in by_ctx {
            out.push_str(&format!("\n## {ctx} ({})\n", items.len()));
            for i in items {
                let outcome = serde_json::to_value(i.outcome).unwrap_or_default();
                out.push_str(&format!(
                    "\n**{}** · `{}`{}\n\n- before: {}\n",
                    outcome.as_str().unwrap_or("?"),
                    &i.claim_id[..8.min(i.claim_id.len())],
                    i.source_title
                        .as_deref()
                        .map(|t| format!(" · {t}"))
                        .unwrap_or_default(),
                    i.before
                ));
                if let Some(a) = &i.after
                    && a != &i.before
                {
                    out.push_str(&format!("- after: {a}\n"));
                }
                if !i.about.is_empty() {
                    let names: Vec<String> = i
                        .about
                        .iter()
                        .map(|a| match &a.what {
                            Some(w) => format!("{} ({w})", a.name),
                            None => a.name.clone(),
                        })
                        .collect();
                    out.push_str(&format!("- about: {}\n", names.join(", ")));
                }
                if let Some(r) = &i.reason {
                    out.push_str(&format!("- why kept: {r}\n"));
                }
            }
        }
        out
    }
}

struct StoredClaim {
    id: String,
    content: String,
}

struct Source {
    id: String,
    content: String,
    context: String,
    claims: Vec<StoredClaim>,
}

/// Sources with claims not yet checked, newest first.
async fn pending_sources(neo4j: &Neo4jClient) -> Result<Vec<Source>> {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (c:Note {note_type: 'claim'})-[:ASSERTED_IN]->(s:Note) \
             WHERE c.standalone_checked_at IS NULL \
             WITH s, collect({id: c.id, content: c.content, ctx: c.source_context}) AS claims \
             RETURN s.id AS id, s.content AS content, claims \
             ORDER BY s.created_at DESC",
        ))
        .await?;
    Ok(rows
        .iter()
        .filter_map(|r| {
            let claims: Vec<neo4rs::BoltMap> = r.get("claims").ok()?;
            let context = claims
                .first()
                .and_then(|m| m.get::<String>("ctx").ok())
                .unwrap_or_default();
            Some(Source {
                id: r.get("id").ok()?,
                content: r.get::<String>("content").unwrap_or_default(),
                context,
                claims: claims
                    .iter()
                    .filter_map(|m| {
                        Some(StoredClaim {
                            id: m.get("id").ok()?,
                            content: m.get::<String>("content").unwrap_or_default(),
                        })
                    })
                    .collect(),
            })
        })
        .collect())
}

/// For a dry run: whole sources, taken round-robin across writers (newest
/// first within each), until there are `want` claims.
fn sample_round_robin(sources: Vec<Source>, want: usize) -> Vec<Source> {
    let mut by_ctx: BTreeMap<String, VecDeque<Source>> = BTreeMap::new();
    for s in sources {
        by_ctx.entry(s.context.clone()).or_default().push_back(s);
    }
    let mut picked = Vec::new();
    let mut have = 0;
    while have < want && by_ctx.values().any(|q| !q.is_empty()) {
        for q in by_ctx.values_mut() {
            if have >= want {
                break;
            }
            if let Some(s) = q.pop_front() {
                have += s.claims.len();
                picked.push(s);
            }
        }
    }
    picked
}

fn rewrite_prompt(source: &Source, title: Option<&str>) -> String {
    let claims: String = source
        .claims
        .iter()
        .enumerate()
        .map(|(i, c)| format!("c{}: {}\n", i + 1, c.content.replace('\n', " ")))
        .collect();
    let text: String = source.content.chars().take(REWRITE_SOURCE_CHARS).collect();
    let title = title
        .map(|t| format!("SOURCE TITLE: {t}\n"))
        .unwrap_or_default();
    format!(
        "Below are claims extracted earlier from the SOURCE. Rewrite each so it stands alone.\n\
         {STANDALONE_RULES}\
         Keep each claim's meaning exactly: the same facts, numbers, names and attribution. Add only \
         names or descriptions the SOURCE gives that resolve what the claim refers to. Do not judge, \
         soften or correct it, and add nothing else: no new facts, interpretation or conclusions \
         (\"indicating that …\"). {KEEP_AS_WRITTEN} A claim that already stands alone is returned \
         unchanged.\n\n\
         Respond with JSON only: {{\"claims\": [{{\"id\": \"c1\", \"claim\": \"...\", \
         \"about\": [{{\"name\": \"...\", \"what\": \"...\"}}]}}]}}\n\n\
         CLAIMS:\n{claims}\n{title}SOURCE:\n{text}"
    )
}

/// Judge one answer against the stored claim. Pure, so the rules are tested.
pub fn judge(
    before: &str,
    answer: Option<ExtractedClaim>,
) -> (Outcome, Option<ExtractedClaim>, Option<String>) {
    let Some(a) = answer else {
        return (
            Outcome::NoAnswer,
            None,
            Some("the model's answer had no entry for it".into()),
        );
    };
    let lost = rewrite_losses(before, &a.claim);
    if !lost.is_empty() {
        return (
            Outcome::KeptMeaningGuard,
            Some(a),
            Some(format!("the rewrite dropped {}", lost.join(", "))),
        );
    }
    let problems = standalone_problems(&a);
    if !problems.is_empty() {
        return (Outcome::Ambiguous, Some(a), Some(problems.join("; ")));
    }
    // A rewrite that only changed typography or spacing is no rewrite: keep
    // the stored text rather than re-embed it for a non-breaking hyphen.
    let same = |s: &str| canon(s).split_whitespace().collect::<Vec<_>>().join(" ");
    if same(&a.claim) == same(before) {
        (Outcome::AlreadyStandsAlone, Some(a), None)
    } else {
        (Outcome::Rewritten, Some(a), None)
    }
}

async fn rewrite_source(llm: &dyn LlmProvider, source: &Source) -> Result<Vec<Item>> {
    let title = source_reference(&source.content).title;
    let answer = llm
        .generate_json(
            &rewrite_prompt(source, title.as_deref()),
            Some(REWRITE_SYSTEM),
            &["claims"],
            1,
        )
        .await;
    let answer = answer?;
    let by_label: BTreeMap<String, ExtractedClaim> = answer
        .get("claims")
        .and_then(|c| c.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    let label = v.get("id")?.as_str()?.trim().to_lowercase();
                    let c: ExtractedClaim = serde_json::from_value(v.clone()).ok()?;
                    Some((label, c))
                })
                .collect()
        })
        .unwrap_or_default();
    let mut items = Vec::with_capacity(source.claims.len());
    for (i, c) in source.claims.iter().enumerate() {
        let (mut outcome, mut a, mut reason) =
            judge(&c.content, by_label.get(&format!("c{}", i + 1)).cloned());
        // One targeted repair, told exactly what is wrong, as extraction gets:
        // the batch answer left "the software" in the Paragon claim.
        if matches!(outcome, Outcome::Ambiguous | Outcome::KeptMeaningGuard)
            && let Some(why) = reason.clone()
        {
            let original = ExtractedClaim {
                claim: c.content.clone(),
                asserted_by: None,
                kind: None,
                about: Vec::new(),
            };
            if let Some(fixed) = repair_claim(llm, &source.content, &original, &[why]).await {
                (outcome, a, reason) = judge(&c.content, Some(fixed));
            }
        }
        items.push(Item {
            claim_id: c.id.clone(),
            source_id: Some(source.id.clone()),
            source_context: source.context.clone(),
            source_title: title.clone(),
            before: c.content.clone(),
            after: a.as_ref().map(|a| a.claim.clone()),
            about: a.map(|a| a.about).unwrap_or_default(),
            outcome,
            reason,
        });
    }
    Ok(items)
}

/// Write one claim's outcome. A rewrite whose embedding fails is not written:
/// `item` becomes `EmbedFailed` and the claim waits for the next run.
async fn apply(
    neo4j: &Neo4jClient,
    llm: &dyn LlmProvider,
    item: &mut Item,
    source: &SourceRef,
) -> Result<()> {
    let about: Vec<String> = item
        .about
        .iter()
        .map(|a| a.name.trim().to_string())
        .collect();
    let base = |q: &str| {
        neo4rs::query(q)
            .param("id", item.claim_id.as_str())
            .param("about", about.clone())
            .param("source_url", source.url.clone())
            .param("source_title", source.title.clone())
            .param("source_outlet", source.outlet.clone())
    };
    match item.outcome {
        Outcome::Rewritten => {
            let after = item.after.clone().unwrap_or_default();
            let embedding = match llm.embed(&after).await {
                Ok(e) if !e.is_empty() => e,
                Ok(_) | Err(_) => {
                    item.outcome = Outcome::EmbedFailed;
                    item.reason =
                        Some("the rewrite could not be embedded; left for the next run".into());
                    return Ok(());
                }
            };
            neo4j
                .run(
                    base(
                        "MATCH (c:Note {id: $id}) \
                         SET c.content_v1 = coalesce(c.content_v1, c.content), c.content = $content, \
                             c.embedding = $embedding, c.about = $about, \
                             c.source_url = $source_url, c.source_title = $source_title, \
                             c.source_outlet = $source_outlet, \
                             c.rewritten_at = datetime(), c.standalone_checked_at = datetime() \
                         REMOVE c.topics_version, c.ambiguous, c.ambiguous_reason",
                    )
                    .param("content", after)
                    .param("embedding", embedding),
                )
                .await?;
        }
        Outcome::AlreadyStandsAlone => {
            neo4j
                .run(base(
                    "MATCH (c:Note {id: $id}) \
                     SET c.about = $about, c.source_url = $source_url, c.source_title = $source_title, \
                         c.source_outlet = $source_outlet, c.standalone_checked_at = datetime()",
                ))
                .await?;
        }
        Outcome::Ambiguous | Outcome::KeptMeaningGuard | Outcome::NoSource => {
            neo4j
                .run(
                    base(
                        "MATCH (c:Note {id: $id}) \
                         SET c.ambiguous = true, c.ambiguous_reason = $reason, \
                             c.source_url = $source_url, c.source_title = $source_title, \
                             c.source_outlet = $source_outlet, c.standalone_checked_at = datetime()",
                    )
                    .param("reason", item.reason.clone().unwrap_or_default()),
                )
                .await?;
        }
        Outcome::NoAnswer | Outcome::EmbedFailed => {}
    }
    Ok(())
}

/// Claims with no ASSERTED_IN edge: nothing to rewrite from, so each is
/// marked ambiguous unless it already stands alone (it can't: it declares
/// nothing it is about), with the reason.
async fn mark_sourceless(neo4j: &Neo4jClient, dry_run: bool) -> Result<Vec<Item>> {
    let rows = neo4j
        .execute(neo4rs::query(
            "MATCH (c:Note {note_type: 'claim'}) \
             WHERE c.standalone_checked_at IS NULL AND NOT (c)-[:ASSERTED_IN]->() \
             RETURN c.id AS id, c.content AS content, coalesce(c.source_context, '') AS ctx",
        ))
        .await?;
    let mut items = Vec::new();
    for r in rows {
        let item = Item {
            claim_id: r.get("id").unwrap_or_default(),
            source_id: None,
            source_context: r.get("ctx").unwrap_or_default(),
            source_title: None,
            before: r.get::<String>("content").unwrap_or_default(),
            after: None,
            about: Vec::new(),
            outcome: Outcome::NoSource,
            reason: Some("no source note recorded to rewrite it from".into()),
        };
        if !dry_run {
            neo4j
                .run(
                    neo4rs::query(
                        "MATCH (c:Note {id: $id}) SET c.ambiguous = true, c.ambiguous_reason = $reason, \
                         c.standalone_checked_at = datetime()",
                    )
                    .param("id", item.claim_id.as_str())
                    .param("reason", item.reason.clone().unwrap_or_default()),
                )
                .await?;
        }
        items.push(item);
    }
    Ok(items)
}

pub async fn run(
    neo4j: &Neo4jClient,
    llm: &dyn LlmProvider,
    opts: &Options,
    snapshot_dir: Option<&Path>,
) -> Result<Report> {
    let mut report = Report {
        dry_run: opts.dry_run,
        ..Default::default()
    };
    let mut all = pending_sources(neo4j).await?;
    if !opts.source_ids.is_empty() {
        all.retain(|s| opts.source_ids.contains(&s.id));
    }
    let mut sources = if opts.dry_run {
        sample_round_robin(all, opts.sample)
    } else {
        all
    };
    if let Some(n) = opts.limit_sources {
        sources.truncate(n);
    }
    if !opts.dry_run && !sources.is_empty() {
        let dir = snapshot_dir
            .context("KNOWLEDGE_SNAPSHOT_DIR is not set: no rewrite without a snapshot")?;
        let svc = crate::services::snapshot::SnapshotService::new(neo4j.clone(), dir.to_path_buf());
        let (path, _) = svc
            .take_snapshot(Some("pre_claim_rewrite"))
            .await
            .context("snapshot before the claim rewrite — nothing rewritten")?;
        report.snapshot = Some(path.display().to_string());
    }
    let total = sources.len();
    for (n, source) in sources.iter().enumerate() {
        let r = source_reference(&source.content);
        match rewrite_source(llm, source).await {
            Ok(mut items) => {
                if !opts.dry_run {
                    for item in items.iter_mut() {
                        if let Err(e) = apply(neo4j, llm, item, &r).await {
                            warn!(claim = %item.claim_id, error = %e, "Claim rewrite: write failed");
                            item.outcome = Outcome::NoAnswer;
                            item.reason = Some(format!("write failed: {e}"));
                        }
                    }
                }
                report.items.extend(items);
            }
            Err(e) => {
                report.calls_failed += 1;
                warn!(source = %source.id, error = %e, "Claim rewrite: the call failed; its claims wait for the next run");
            }
        }
        report.sources += 1;
        if (n + 1) % 25 == 0 || n + 1 == total {
            info!(done = n + 1, total, counts = ?report.counts(), "Claim rewrite progress");
        }
    }
    if opts.limit_sources.is_none() && opts.source_ids.is_empty() && !opts.dry_run {
        report.items.extend(mark_sourceless(neo4j, false).await?);
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer(text: &str, about: &[(&str, &str)]) -> Option<ExtractedClaim> {
        Some(ExtractedClaim {
            claim: text.into(),
            asserted_by: None,
            kind: None,
            about: about
                .iter()
                .map(|(n, w)| About {
                    name: (*n).into(),
                    what: Some((*w).into()),
                })
                .collect(),
        })
    }

    #[test]
    fn a_rewrite_replaces_only_when_it_stands_alone_and_keeps_every_fact() {
        let before = "The company fined 3 banks in 2024.";
        let (o, _, _) = judge(
            before,
            answer("Ofgem fined 3 banks in 2024.", &[("Ofgem", "regulator")]),
        );
        assert_eq!(o, Outcome::Rewritten);
        // Typography alone is not a rewrite.
        let tidy = "Ofgem fined 3 banks in 2024-25.";
        let (o, _, _) = judge(
            tidy,
            answer(
                "Ofgem fined 3  banks in 2024\u{2011}25.",
                &[("Ofgem", "regulator")],
            ),
        );
        assert_eq!(o, Outcome::AlreadyStandsAlone);
        // Lost a number: kept.
        let (o, _, why) = judge(
            before,
            answer("Ofgem fined banks in 2024.", &[("Ofgem", "regulator")]),
        );
        assert_eq!(o, Outcome::KeptMeaningGuard);
        assert!(why.unwrap().contains('3'));
        // Still vague: kept, with the reason.
        let (o, _, why) = judge(before, answer("The company fined 3 banks in 2024.", &[]));
        assert_eq!(o, Outcome::Ambiguous);
        assert!(why.unwrap().contains("the company"));
        // Unchanged and fine.
        let fine = "Ofgem fined 3 banks in 2024.";
        assert_eq!(
            judge(fine, answer(fine, &[("Ofgem", "regulator")])).0,
            Outcome::AlreadyStandsAlone
        );
        assert_eq!(judge(fine, None).0, Outcome::NoAnswer);
    }

    #[test]
    fn a_dry_run_samples_across_writers() {
        let src = |ctx: &str, n: usize| Source {
            id: format!("{ctx}-{n}"),
            content: String::new(),
            context: ctx.into(),
            claims: (0..n)
                .map(|i| StoredClaim {
                    id: i.to_string(),
                    content: String::new(),
                })
                .collect(),
        };
        let picked = sample_round_robin(
            vec![
                src("video_learning", 3),
                src("video_learning", 3),
                src("daily_news", 2),
                src("article_learning", 1),
            ],
            6,
        );
        let ctxs: Vec<&str> = picked.iter().map(|s| s.context.as_str()).collect();
        assert_eq!(
            ctxs,
            vec!["article_learning", "daily_news", "video_learning"]
        );
    }
}
