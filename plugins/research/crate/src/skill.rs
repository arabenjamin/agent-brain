//! `deep_research`: one question to Parallel's Task API, answered as a cited
//! report. Paid per run, so every run is checked against the monthly cap
//! first, and a repeat of the same question within a day resumes its run.
//!
//! A caller inside a workflow run waits for the report (up to the
//! processor's `wait_minutes`); any other caller (a chat turn, a sub-agent
//! answering one) gets the run id at once, and the background collector
//! delivers the report to the Feed and a note when it is done.

use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::Mutex;

use crate::client::{ParallelClient, Poll};
use crate::config::Config;
use crate::runs::{Ledger, Run, render};
use agent_brain_plugin_api::{PluginContext, Skill, ToolCallResult, ToolDefinition, parse_args};

/// The question leaves the brain; a long one is a context dump, not a question.
pub const MAX_QUESTION_CHARS: usize = 2000;

/// Longest wait a caller may ask for, in minutes.
const MAX_WAIT_MINUTES: u64 = 120;

/// Sources listed in a tool answer.
const ANSWER_SOURCES: usize = 15;

/// Each long-poll asks Parallel for at most this long.
const POLL_SECS: u64 = 300;

pub struct ResearchSkill {
    ctx: PluginContext,
    client: Arc<ParallelClient>,
    ledger: Ledger,
    config: Config,
    /// Held across the cap check and the run's creation, so two calls
    /// cannot both pass a cap that has room for one.
    spend: Mutex<()>,
}

#[derive(Debug, Deserialize)]
struct Input {
    #[serde(default)]
    action: Option<String>,
    #[serde(default)]
    question: Option<String>,
    #[serde(default)]
    processor: Option<String>,
    #[serde(default)]
    run_id: Option<String>,
    #[serde(default)]
    fresh: bool,
    #[serde(default)]
    wait_minutes: Option<u64>,
}

/// How long a caller waits: what it asked for, else the processor's wait
/// inside a workflow run and none outside one.
fn wait_for(asked: Option<u64>, in_run: bool, processor_wait: u64) -> u64 {
    match asked {
        Some(m) => m.min(MAX_WAIT_MINUTES),
        None if in_run => processor_wait,
        None => 0,
    }
}

/// The question, or why it cannot be sent.
fn check_question(q: Option<&str>) -> Result<String, String> {
    let q = q.map(str::trim).unwrap_or_default();
    if q.is_empty() {
        return Err("`question` is required for action=run".into());
    }
    let n = q.chars().count();
    if n > MAX_QUESTION_CHARS {
        return Err(format!(
            "`question` is {n} characters; the limit is {MAX_QUESTION_CHARS}. Send one \
             self-contained research question, not notes or context: it leaves the brain."
        ));
    }
    Ok(q.to_string())
}

/// What the month's runs cost. Starts from +0.0: an empty f64 sum is -0.0,
/// which a JSON answer prints as "-0.0".
fn month_spend(runs: &[Run]) -> f64 {
    runs.iter().fold(0.0, |acc, r| acc + r.usd)
}

/// Why a run of `price` would pass the cap, or `None` when it fits.
fn over_cap(spent: f64, runs: usize, price: f64, cap: f64) -> Option<String> {
    // A cent's rounding must not refuse the last run that fits.
    if spent + price <= cap + 1e-9 {
        return None;
    }
    Some(format!(
        "Refused: this run (${price:.3}) would pass the monthly deep-research cap. Spent \
         ${spent:.3} of ${cap:.2} this month on {runs} run(s) (deep_research action=list shows \
         them). The cap is monthly_cap_usd in plugins/research/parallel.yaml; it resets on \
         the 1st (UTC). A cheaper processor may still fit."
    ))
}

impl ResearchSkill {
    pub fn new(
        ctx: PluginContext,
        client: Arc<ParallelClient>,
        ledger: Ledger,
        config: Config,
    ) -> Self {
        Self {
            ctx,
            client,
            ledger,
            config,
            spend: Mutex::new(()),
        }
    }

    fn def(&self) -> ToolDefinition {
        let names = self.config.names();
        let prices: Vec<String> = self
            .config
            .processors
            .iter()
            .map(|(n, p)| format!("{n} ${:.3}", p.usd))
            .collect();
        ToolDefinition {
            name: "deep_research".to_string(),
            description: format!(
                "PAID deep web research (Parallel). Sends ONE self-contained question to a \
                 research service that searches many sources and returns a cited markdown \
                 report with a confidence. Minutes, not seconds; costs money per run ({}), \
                 capped at ${:.2}/month. Use it only when asked for deep research, or for a \
                 multi-source question search_web cannot settle; never for a quick fact. \
                 The question leaves the brain: no personal details, no names of the user \
                 or their contacts, no private notes. In a chat turn it returns the run id \
                 at once and the report arrives in the Feed and as a note; inside a \
                 workflow run it waits for the report. The same question within a day \
                 resumes its earlier run instead of paying again (fresh=true starts a new \
                 one). action=result fetches a run's report; action=list shows this \
                 month's runs and spend.",
                prices.join(", "),
                self.config.monthly_cap_usd
            ),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["run", "result", "list"], "description": "run (default), result (by run_id), list (this month's runs and spend)"},
                    "question": {"type": "string", "maxLength": MAX_QUESTION_CHARS, "description": "The research question, self-contained and specific (action=run)"},
                    "processor": {"type": "string", "enum": names, "description": format!("Depth and price (default {})", self.config.default_processor)},
                    "run_id": {"type": "string", "description": "The run to fetch (action=result)"},
                    "fresh": {"type": "boolean", "description": "Start a new paid run even if this question ran in the last day"},
                    "wait_minutes": {"type": "integer", "minimum": 0, "maximum": MAX_WAIT_MINUTES, "description": "How long to wait for the report before returning the run id (default: the processor's wait inside a workflow run, 0 elsewhere)"}
                }
            }),
        }
    }

    async fn handle(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: Input = match parse_args(arguments) {
            Ok(i) => i,
            Err(e) => return e,
        };
        match input.action.as_deref().unwrap_or("run") {
            "run" => self.run(input).await,
            "result" => self.result(input.run_id.as_deref()).await,
            "list" => self.list().await,
            other => {
                ToolCallResult::error(format!("unknown action `{other}`; use run, result or list"))
            }
        }
    }

    async fn run(&self, input: Input) -> ToolCallResult {
        let question = match check_question(input.question.as_deref()) {
            Ok(q) => q,
            Err(e) => return ToolCallResult::error(e),
        };
        let processor = input
            .processor
            .clone()
            .unwrap_or_else(|| self.config.default_processor.clone());
        let Some(spec) = self.config.processors.get(&processor) else {
            return ToolCallResult::error(format!(
                "unknown processor `{processor}`; one of: {}",
                self.config.names().join(", ")
            ));
        };
        let task = self.ctx.current_task();
        let wait = wait_for(input.wait_minutes, task.is_some(), spec.wait_minutes);

        let (run_id, reused) = {
            let _guard = self.spend.lock().await;
            let earlier = if input.fresh {
                None
            } else {
                match self.ledger.reusable(&question, &processor).await {
                    Ok(r) => r,
                    Err(e) => {
                        return ToolCallResult::error(format!(
                            "reading the run ledger failed: {e}"
                        ));
                    }
                }
            };
            match earlier {
                Some(r) if r.status == "completed" => {
                    return ToolCallResult::success_text(format!(
                        "(The same question ran at {}; this is that report, at no new cost. \
                         fresh=true pays for a new run.)\n\n{}",
                        r.created_at,
                        render(&r, ANSWER_SOURCES)
                    ));
                }
                Some(r) => (r.run_id, true),
                None => {
                    let month = match self.ledger.this_month().await {
                        Ok(m) => m,
                        Err(e) => {
                            return ToolCallResult::error(format!(
                                "reading the run ledger failed: {e}"
                            ));
                        }
                    };
                    let spent = month_spend(&month);
                    if let Some(why) =
                        over_cap(spent, month.len(), spec.usd, self.config.monthly_cap_usd)
                    {
                        return ToolCallResult::error(why);
                    }
                    let id = match self.client.create(&question, &processor).await {
                        Ok(id) => id,
                        Err(e) => return ToolCallResult::error(format!("deep_research: {e}")),
                    };
                    if let Err(e) = self
                        .ledger
                        .record(
                            &id,
                            &question,
                            &processor,
                            spec.usd,
                            task.as_deref(),
                            self.ctx.caller().as_deref(),
                        )
                        .await
                    {
                        // The run exists and is billed; say so rather than lose it.
                        return ToolCallResult::error(format!(
                            "Parallel run {id} started, but recording it failed ({e}): it is \
                             not counted against the cap and will not be collected. Fetch it \
                             with action=result run_id={id}."
                        ));
                    }
                    (id, false)
                }
            }
        };

        let started = if reused {
            format!("Resumed run {run_id} (same question, started earlier; no new cost)")
        } else {
            format!(
                "Started Parallel `{processor}` run {run_id} (${:.3})",
                spec.usd
            )
        };
        if wait == 0 {
            return ToolCallResult::success_text(format!(
                "{started}. It typically takes up to {} min. The report will arrive in the \
                 Feed and as a note when it is done; deep_research action=result run_id={run_id} \
                 fetches it.",
                spec.wait_minutes
            ));
        }
        self.wait_for_report(&run_id, wait, task.is_some(), &started)
            .await
    }

    async fn wait_for_report(
        &self,
        run_id: &str,
        minutes: u64,
        in_run: bool,
        started: &str,
    ) -> ToolCallResult {
        let deadline = Utc::now() + chrono::Duration::minutes(minutes as i64);
        // A minute's grace so the collector does not take a run mid-poll.
        let _ = self
            .ledger
            .set_waiting(run_id, Some(deadline + chrono::Duration::minutes(1)))
            .await;
        let outcome = loop {
            let left = (deadline - Utc::now()).num_seconds();
            if left <= 0 {
                break None;
            }
            match self
                .client
                .result(run_id, (left as u64).min(POLL_SECS))
                .await
            {
                Ok(Poll::Running) => continue,
                Ok(other) => break Some(Ok(other)),
                Err(e) => {
                    // A dropped long-poll is not a failed run: pause, then poll again.
                    tracing::warn!(run = %run_id, error = %e, "deep_research: poll failed");
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    if (deadline - Utc::now()).num_seconds() <= 0 {
                        break Some(Err(e));
                    }
                }
            }
        };
        let _ = self.ledger.set_waiting(run_id, None).await;
        match outcome {
            Some(Ok(Poll::Done(report))) => {
                if let Err(e) = self.ledger.complete(run_id, &report).await {
                    tracing::warn!(run = %run_id, error = %e, "deep_research: storing the report failed");
                }
                self.answer_stored(run_id).await
            }
            Some(Ok(Poll::Failed(why))) => {
                let _ = self.ledger.fail(run_id, &why).await;
                ToolCallResult::error(format!("deep_research run {run_id}: {why}"))
            }
            Some(Ok(Poll::Running)) => unreachable!("Running polls again"),
            Some(Err(e)) => self.still_running(run_id, minutes, in_run, started, Some(e)),
            None => self.still_running(run_id, minutes, in_run, started, None),
        }
    }

    /// The wait ran out. Inside a workflow run that is an error, so the step
    /// is retried (resuming this run) instead of storing "still running" as
    /// its output; elsewhere it is the expected answer.
    fn still_running(
        &self,
        run_id: &str,
        minutes: u64,
        in_run: bool,
        started: &str,
        error: Option<anyhow::Error>,
    ) -> ToolCallResult {
        let why = match error {
            Some(e) => format!("polling failed ({e})"),
            None => format!("not finished after {minutes} min"),
        };
        let msg = format!(
            "{started}; {why}. Retrying resumes the same run at no new cost; otherwise the \
             report arrives in the Feed and as a note. deep_research action=result \
             run_id={run_id} fetches it."
        );
        if in_run {
            ToolCallResult::error(msg)
        } else {
            ToolCallResult::success_text(msg)
        }
    }

    async fn answer_stored(&self, run_id: &str) -> ToolCallResult {
        match self.ledger.get(run_id).await {
            Ok(Some(r)) => match r.status.as_str() {
                "completed" => ToolCallResult::success_text(render(&r, ANSWER_SOURCES)),
                "failed" => ToolCallResult::error(format!(
                    "deep_research run {run_id} failed: {}",
                    r.error.unwrap_or_default()
                )),
                _ => ToolCallResult::success_text(format!(
                    "Run {run_id} is still running (started {}). The report will arrive in \
                     the Feed and as a note.",
                    r.created_at
                )),
            },
            Ok(None) => ToolCallResult::error(format!(
                "no deep_research run {run_id}; action=list shows this month's runs"
            )),
            Err(e) => ToolCallResult::error(format!("reading run {run_id} failed: {e}")),
        }
    }

    async fn result(&self, run_id: Option<&str>) -> ToolCallResult {
        let Some(run_id) = run_id.map(str::trim).filter(|s| !s.is_empty()) else {
            return ToolCallResult::error(
                "`run_id` is required for action=result; action=list shows this month's runs",
            );
        };
        match self.ledger.get(run_id).await {
            Ok(Some(r)) if r.status == "running" => {
                // One short check: it may have finished since.
                match self.client.result(run_id, 5).await {
                    Ok(Poll::Done(report)) => {
                        let _ = self.ledger.complete(run_id, &report).await;
                    }
                    Ok(Poll::Failed(why)) => {
                        let _ = self.ledger.fail(run_id, &why).await;
                    }
                    Ok(Poll::Running) | Err(_) => {}
                }
                self.answer_stored(run_id).await
            }
            Ok(_) => self.answer_stored(run_id).await,
            Err(e) => ToolCallResult::error(format!("reading run {run_id} failed: {e}")),
        }
    }

    async fn list(&self) -> ToolCallResult {
        let runs: Vec<Run> = match self.ledger.this_month().await {
            Ok(r) => r,
            Err(e) => return ToolCallResult::error(format!("reading the run ledger failed: {e}")),
        };
        let spent = month_spend(&runs);
        let cap = self.config.monthly_cap_usd;
        ToolCallResult::success_json(json!({
            "month_spent_usd": (spent * 1000.0).round() / 1000.0,
            "monthly_cap_usd": cap,
            "remaining_usd": ((cap - spent).max(0.0) * 1000.0).round() / 1000.0,
            "count": runs.len(),
            "runs": runs.iter().map(|r| json!({
                "run_id": r.run_id,
                "created_at": r.created_at,
                "processor": r.processor,
                "usd": r.usd,
                "status": r.status,
                "question": r.question.chars().take(160).collect::<String>(),
                "error": r.error,
            })).collect::<Vec<_>>(),
            "note": if runs.is_empty() { "No deep_research runs this month (UTC)." } else { "Every run accepted by Parallel counts, failed ones too." },
        }))
    }
}

#[async_trait]
impl Skill for ResearchSkill {
    fn name(&self) -> &str {
        "Research"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![self.def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "deep_research" => Some(self.handle(arguments).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_workflow_run_waits_by_default() {
        assert_eq!(wait_for(None, true, 30), 30);
        assert_eq!(wait_for(None, false, 30), 0);
        assert_eq!(wait_for(Some(5), false, 30), 5);
        assert_eq!(wait_for(Some(999), true, 30), MAX_WAIT_MINUTES);
    }

    #[test]
    fn a_question_is_required_and_bounded() {
        assert!(check_question(None).is_err());
        assert!(check_question(Some("   ")).is_err());
        assert_eq!(check_question(Some(" why? ")).unwrap(), "why?");
        let long = "x".repeat(MAX_QUESTION_CHARS + 1);
        assert!(
            check_question(Some(&long))
                .unwrap_err()
                .contains("leaves the brain")
        );
    }

    #[test]
    fn an_empty_month_spent_nothing_not_minus_nothing() {
        let v = serde_json::to_string(&json!({"s": month_spend(&[])})).unwrap();
        assert_eq!(v, r#"{"s":0.0}"#);
    }

    #[test]
    fn the_cap_refuses_only_what_would_pass_it() {
        assert_eq!(over_cap(4.9, 49, 0.1, 5.0), None);
        let why = over_cap(4.95, 50, 0.1, 5.0).unwrap();
        assert!(why.contains("Spent $4.950 of $5.00 this month on 50 run(s)"));
        assert!(why.contains("action=list"));
        assert!(over_cap(0.0, 0, 0.1, 0.0).is_some());
    }
}
