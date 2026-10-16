//! `(:ResearchRun)`: one node per Parallel run. It is the spend ledger (the
//! monthly cap sums `usd` over this month's runs), the resume point (a retry
//! or a repeat of the same question finds its run instead of paying again),
//! and the delivery state for the background collector.
//!
//! A run is `running` until one caller moves it to `completed` or `failed`;
//! that transition is conditional, so only the caller that made it delivers
//! the report. `waiting_until` marks a tool call still waiting on the run:
//! the collector leaves such a run alone, and takes it over once the wait has
//! passed (the caller gave up, or the process restarted).

use std::sync::Arc;
use std::time::Duration;

use agent_brain_plugin_api::graph::{Row, query};
use agent_brain_plugin_api::{DateTime, FeedItem, Graph, PluginContext, Provenance, Utc};
use anyhow::Result;
use chrono::Datelike;
use tracing::{info, warn};

use crate::client::{Citation, ParallelClient, Poll, Report};

/// Sources kept on the node (the report itself cites inline).
const STORED_SOURCES: usize = 50;

/// How long the collector keeps checking a run before calling it lost.
const GIVE_UP_AFTER_HOURS: i64 = 6;

/// Characters of the report shown in its Feed item; the note holds it all.
const FEED_CHARS: usize = 4000;

/// One run as stored.
#[derive(Debug, Clone)]
pub struct Run {
    pub run_id: String,
    pub question: String,
    pub processor: String,
    pub usd: f64,
    pub status: String,
    pub created_at: String,
    pub report: Option<String>,
    pub confidence: Option<String>,
    pub sources: Vec<Citation>,
    pub error: Option<String>,
    /// The Task whose run started it, if a run did.
    pub task_id: Option<String>,
    /// Who started it, in words (`PluginContext::caller`).
    pub requested_by: Option<String>,
}

const RETURN_RUN: &str = "RETURN r.run_id AS run_id, r.question AS question, \
     r.processor AS processor, r.usd AS usd, r.status AS status, \
     toString(r.created_at) AS created_at, r.report AS report, \
     r.confidence AS confidence, r.sources AS sources, r.error AS error, \
     r.task_id AS task_id, r.requested_by AS requested_by";

fn run_from(row: &Row) -> Result<Run> {
    let sources: Vec<Citation> = row
        .get::<String>("sources")
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    Ok(Run {
        run_id: row.get("run_id")?,
        question: row.get("question")?,
        processor: row.get("processor")?,
        usd: row.get("usd")?,
        status: row.get("status")?,
        created_at: row.get("created_at")?,
        report: row.get("report").ok(),
        confidence: row.get("confidence").ok(),
        sources,
        error: row.get("error").ok(),
        task_id: row.get::<String>("task_id").ok().filter(|s| !s.is_empty()),
        requested_by: row
            .get::<String>("requested_by")
            .ok()
            .filter(|s| !s.is_empty()),
    })
}

/// The start of this calendar month, UTC: the cap's window.
pub fn month_start(now: DateTime<Utc>) -> DateTime<Utc> {
    now.date_naive()
        .with_day(1)
        .and_then(|d| d.and_hms_opt(0, 0, 0))
        .map(|d| d.and_utc())
        .unwrap_or(now)
}

#[derive(Clone)]
pub struct Ledger {
    graph: Arc<dyn Graph>,
}

impl Ledger {
    pub fn new(graph: Arc<dyn Graph>) -> Self {
        Self { graph }
    }

    pub async fn ensure_schema(&self) -> Result<()> {
        self.graph
            .run(query(
                "CREATE CONSTRAINT research_run_id IF NOT EXISTS \
                 FOR (r:ResearchRun) REQUIRE r.run_id IS UNIQUE",
            ))
            .await?;
        self.graph
            .run(query(
                "CREATE INDEX research_run_created IF NOT EXISTS \
                 FOR (r:ResearchRun) ON (r.created_at)",
            ))
            .await
    }

    /// This month's runs, newest first.
    pub async fn this_month(&self) -> Result<Vec<Run>> {
        let rows = self
            .graph
            .execute(
                query(&format!(
                    "MATCH (r:ResearchRun) WHERE r.created_at >= datetime($since) \
                     WITH r ORDER BY r.created_at DESC {RETURN_RUN}"
                ))
                .param("since", month_start(Utc::now()).to_rfc3339()),
            )
            .await?;
        rows.iter().map(run_from).collect()
    }

    pub async fn get(&self, run_id: &str) -> Result<Option<Run>> {
        let rows = self
            .graph
            .execute(
                query(&format!(
                    "MATCH (r:ResearchRun {{run_id: $id}}) {RETURN_RUN}"
                ))
                .param("id", run_id),
            )
            .await?;
        rows.first().map(run_from).transpose()
    }

    /// The newest run of this exact question and processor in the last day
    /// that has not failed: a retry or a repeat resumes it instead of paying.
    pub async fn reusable(&self, question: &str, processor: &str) -> Result<Option<Run>> {
        let rows = self
            .graph
            .execute(
                query(&format!(
                    "MATCH (r:ResearchRun {{question: $q, processor: $p}}) \
                     WHERE r.status <> 'failed' \
                       AND r.created_at >= datetime() - duration({{hours: 24}}) \
                     WITH r ORDER BY r.created_at DESC LIMIT 1 {RETURN_RUN}"
                ))
                .param("q", question)
                .param("p", processor),
            )
            .await?;
        rows.first().map(run_from).transpose()
    }

    pub async fn record(
        &self,
        run_id: &str,
        question: &str,
        processor: &str,
        usd: f64,
        task_id: Option<&str>,
        requested_by: Option<&str>,
    ) -> Result<()> {
        self.graph
            .run(
                query(
                    "CREATE (:ResearchRun {run_id: $id, question: $q, processor: $p, \
                     usd: $usd, status: 'running', task_id: $task, \
                     requested_by: $by, created_at: datetime()})",
                )
                .param("id", run_id)
                .param("q", question)
                .param("p", processor)
                .param("usd", usd)
                .param("task", task_id)
                .param("by", requested_by),
            )
            .await
    }

    /// Mark a caller waiting on the run until `until` (`None`: no longer).
    pub async fn set_waiting(&self, run_id: &str, until: Option<DateTime<Utc>>) -> Result<()> {
        self.graph
            .run(
                query(
                    "MATCH (r:ResearchRun {run_id: $id}) \
                     SET r.waiting_until = CASE WHEN $until IS NULL THEN null \
                                                ELSE datetime($until) END",
                )
                .param("id", run_id)
                .param("until", until.map(|u| u.to_rfc3339())),
            )
            .await
    }

    /// Store the report. True when this call moved the run out of `running`
    /// (and so is the one to deliver it).
    pub async fn complete(&self, run_id: &str, report: &Report) -> Result<bool> {
        let sources: Vec<&Citation> = report.citations.iter().take(STORED_SOURCES).collect();
        let rows = self
            .graph
            .execute(
                query(
                    "MATCH (r:ResearchRun {run_id: $id}) WHERE r.status = 'running' \
                     SET r.status = 'completed', r.report = $report, \
                         r.confidence = $confidence, r.sources = $sources, \
                         r.source_count = $count, r.completed_at = datetime(), \
                         r.waiting_until = null \
                     RETURN r.run_id AS run_id",
                )
                .param("id", run_id)
                .param("report", &report.content)
                .param("confidence", &report.confidence)
                .param(
                    "sources",
                    serde_json::to_string(&sources).unwrap_or_else(|_| "[]".into()),
                )
                .param("count", report.citations.len()),
            )
            .await?;
        Ok(!rows.is_empty())
    }

    /// Record a failure. True when this call moved the run out of `running`.
    pub async fn fail(&self, run_id: &str, error: &str) -> Result<bool> {
        let rows = self
            .graph
            .execute(
                query(
                    "MATCH (r:ResearchRun {run_id: $id}) WHERE r.status = 'running' \
                     SET r.status = 'failed', r.error = $error, \
                         r.completed_at = datetime(), r.waiting_until = null \
                     RETURN r.run_id AS run_id",
                )
                .param("id", run_id)
                .param("error", error),
            )
            .await?;
        Ok(!rows.is_empty())
    }

    /// Running runs nobody is waiting on, with their age in hours.
    async fn unwatched(&self) -> Result<Vec<(Run, i64)>> {
        let rows = self
            .graph
            .execute(query(&format!(
                "MATCH (r:ResearchRun) WHERE r.status = 'running' \
                   AND (r.waiting_until IS NULL OR r.waiting_until < datetime()) \
                 WITH r, duration.inSeconds(r.created_at, datetime()).hours AS age \
                 {RETURN_RUN}, age"
            )))
            .await?;
        rows.iter()
            .map(|row| Ok((run_from(row)?, row.get::<i64>("age").unwrap_or(0))))
            .collect()
    }
}

/// The report as a tool answer, a note or a Feed item: header, report,
/// then the sources (all of them, or the first `max_sources` and a count).
pub fn render(run: &Run, max_sources: usize) -> String {
    let mut out = format!(
        "Deep research: Parallel `{}` run {} (${:.3}, started {})",
        run.processor, run.run_id, run.usd, run.created_at
    );
    if let Some(c) = &run.confidence {
        out.push_str(&format!(", confidence: {c}"));
    }
    out.push_str("\n\n");
    out.push_str(run.report.as_deref().unwrap_or("(no report text)"));
    if !run.sources.is_empty() {
        let shown = run.sources.len().min(max_sources);
        out.push_str(&format!("\n\nSources ({} shown", shown));
        if shown < run.sources.len() {
            out.push_str(&format!(
                " of {}; the rest are on (:ResearchRun {{run_id: '{}'}})",
                run.sources.len(),
                run.run_id
            ));
        }
        out.push_str("):\n");
        for (i, s) in run.sources.iter().take(shown).enumerate() {
            out.push_str(&format!("{}. ", i + 1));
            if let Some(t) = &s.title {
                out.push_str(&format!("{t} — "));
            }
            out.push_str(&s.url);
            if let Some(e) = &s.excerpt {
                out.push_str(&format!("\n   \"{e}\""));
            }
            out.push('\n');
        }
    }
    out
}

/// Store a run the collector finished as a note, and tell the owner.
async fn deliver(ctx: &PluginContext, run: &Run) {
    let body = render(run, 20);
    let mut note_id = None;
    if let Some(k) = &ctx.knowledge {
        match k
            .store_note_as(
                &format!("## Deep research: {}\n\n{body}", run.question),
                "semantic",
                Some("deep_research"),
                Provenance::SynthesisInference,
            )
            .await
        {
            Ok(id) => note_id = Some(id),
            Err(e) => {
                warn!(run = %run.run_id, error = %e, "deep_research: storing the note failed")
            }
        }
    }
    let Some(feed) = &ctx.feed else { return };
    let mut text: String = body.chars().take(FEED_CHARS).collect();
    if body.chars().count() > FEED_CHARS {
        text.push_str(&format!(
            "\n\n[cut at {FEED_CHARS} of {} characters]",
            body.chars().count()
        ));
    }
    match &note_id {
        Some(id) => text.push_str(&format!("\n\nFull report: note {id}.")),
        None => text.push_str(&format!(
            "\n\nFull report: deep_research action=result run_id={}.",
            run.run_id
        )),
    }
    let title: String = run.question.chars().take(90).collect();
    if let Err(e) = feed
        .create_item(FeedItem {
            title: format!("Deep research done: {title}"),
            body: text,
            priority: 2,
            origin_kind: "deep_research".into(),
            origin_ref: run.run_id.clone(),
            ask: Some(format!(
                "Nothing is blocked: this is the report you asked for. Read it{} and mark \
                 this done, or reply in its thread to act on it.",
                note_id
                    .as_ref()
                    .map(|id| format!(" (note {id})"))
                    .unwrap_or_default()
            )),
            source_task: run.task_id.clone(),
            source: requested(run),
        })
        .await
    {
        warn!(run = %run.run_id, error = %e, "deep_research: Feed item failed");
    }
}

/// Who started a run, in words, when no task did (a task's run is named by
/// the host from `source_task`).
fn requested(run: &Run) -> Option<String> {
    if run.task_id.is_some() {
        return None;
    }
    Some(format!(
        "deep_research, started by {}",
        run.requested_by
            .as_deref()
            .unwrap_or("an unrecorded caller")
    ))
}

async fn tell_failed(ctx: &PluginContext, run: &Run, why: &str) {
    let Some(feed) = &ctx.feed else { return };
    let title: String = run.question.chars().take(90).collect();
    let _ = feed
        .create_item(FeedItem {
            title: format!("Deep research failed: {title}"),
            body: format!(
                "Parallel `{}` run {} (${:.3}, started {}): {why}",
                run.processor, run.run_id, run.usd, run.created_at
            ),
            priority: 3,
            origin_kind: "deep_research".into(),
            origin_ref: run.run_id.clone(),
            ask: Some(
                "Nothing is blocked. Decide whether to ask again (a failed run is still \
                 billed) or mark this done."
                    .into(),
            ),
            source_task: run.task_id.clone(),
            source: requested(run),
        })
        .await;
}

/// The background collector: every minute, finish the runs nobody is
/// waiting on. Only in the deployed brain.
pub fn spawn_collector(ctx: &PluginContext, ledger: Ledger, client: Arc<ParallelClient>) {
    if !ctx.background_allowed() {
        return;
    }
    let ctx = ctx.clone();
    info!("deep_research collector started");
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(60)).await;
            let pending = match ledger.unwatched().await {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "deep_research collector: could not list runs");
                    continue;
                }
            };
            for (run, age_hours) in pending {
                match client.result(&run.run_id, 1).await {
                    Ok(Poll::Done(report)) => match ledger.complete(&run.run_id, &report).await {
                        Ok(true) => {
                            if let Ok(Some(done)) = ledger.get(&run.run_id).await {
                                deliver(&ctx, &done).await;
                                info!(run = %run.run_id, "deep_research: delivered");
                            }
                        }
                        Ok(false) => {}
                        Err(e) => {
                            warn!(run = %run.run_id, error = %e, "deep_research: store failed")
                        }
                    },
                    Ok(Poll::Failed(why)) => {
                        if let Ok(true) = ledger.fail(&run.run_id, &why).await {
                            tell_failed(&ctx, &run, &why).await;
                        }
                    }
                    Ok(Poll::Running) if age_hours >= GIVE_UP_AFTER_HOURS => {
                        let why = format!("still not finished after {age_hours} h; given up");
                        if let Ok(true) = ledger.fail(&run.run_id, &why).await {
                            tell_failed(&ctx, &run, &why).await;
                        }
                    }
                    Ok(Poll::Running) => {}
                    // Network or a Parallel outage: try again next pass.
                    Err(e) => warn!(run = %run.run_id, error = %e, "deep_research: poll failed"),
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run() -> Run {
        Run {
            run_id: "trun_1".into(),
            question: "q".into(),
            processor: "pro".into(),
            usd: 0.1,
            status: "completed".into(),
            created_at: "2026-10-16T16:07:22Z".into(),
            report: Some("Body.".into()),
            confidence: Some("high".into()),
            sources: (0..3)
                .map(|i| Citation {
                    url: format!("https://s{i}.example"),
                    title: (i == 0).then(|| "First".to_string()),
                    excerpt: (i == 1).then(|| "quoted".to_string()),
                })
                .collect(),
            error: None,
            task_id: None,
            requested_by: None,
        }
    }

    #[test]
    fn a_short_source_list_says_where_the_rest_are() {
        let s = render(&run(), 2);
        assert!(s.starts_with("Deep research: Parallel `pro` run trun_1 ($0.100"));
        assert!(s.contains("confidence: high"));
        assert!(s.contains(
            "Sources (2 shown of 3; the rest are on (:ResearchRun {run_id: 'trun_1'})):"
        ));
        assert!(s.contains("1. First — https://s0.example"));
        assert!(s.contains("2. https://s1.example\n   \"quoted\""));
        assert!(!s.contains("s2.example"));
    }

    #[test]
    fn every_source_shown_has_no_count() {
        let s = render(&run(), 10);
        assert!(s.contains("Sources (3 shown):"));
    }

    #[test]
    fn the_cap_window_is_the_calendar_month() {
        let now: DateTime<Utc> = "2026-10-16T16:07:22Z".parse().unwrap();
        assert_eq!(month_start(now).to_rfc3339(), "2026-10-01T00:00:00+00:00");
    }
}
