//! What runs after a step: the evaluator and adversarial gates, their requeues, and meta-learning from a failure.

use super::{ChainStep, QueueService, STEP_MODEL_ERROR, is_transient_infra_error, job_arg};
use agent_brain_models::{AgentJob, TaskStatus};
use serde_json::Value;
use tracing::{info, warn};

/// Parse a 1–5 score from the output of a `reflect_on_work` evaluator step.
///
/// Looks for an explicit `Score: N/5` line first; falls back to verdict keywords
/// ("FULLY MET" → 5, "PARTIALLY MET" → 3, "NOT MET" → 1).
///
/// Returns `None` when neither a score line nor a verdict keyword is present.
/// The caller treats `None` as a pass rather than inventing a mid-scale score:
/// a fabricated 3.0 sits below the default `min_score` of 3.5, so format drift
/// from the local model would otherwise fail the task and burn retry chains.
fn parse_evaluator_score(text: &str) -> Option<f32> {
    for line in text.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("Score:")
            && let Some(n_str) = rest.trim().split('/').next()
            && let Ok(n) = n_str.trim().parse::<f32>()
        {
            return Some(n.clamp(1.0, 5.0));
        }
    }
    let lower = text.to_lowercase();
    if lower.contains("fully met") {
        Some(5.0)
    } else if lower.contains("partially met") {
        Some(3.0)
    } else if lower.contains("not met") {
        Some(1.0)
    } else {
        None
    }
}

/// The Analyze→Hypothesize→Test→Integrate chain enqueued for a dead job, all on
/// the local model.
fn meta_learning_chain(tool_name: &str, attempt: u32, error_text: &str) -> Vec<ChainStep> {
    let hypothesis_question = format!(
        "You are a meta-learning system. Respond in English only.\n\
         A job running '{tool_name}' just died after {attempt} \
         attempts with error: {error_text}\n\n\
         Based on any related notes above:\n\
         1. ANALYZE: What is causing this failure?\n\
         2. HYPOTHESIZE: Form a specific, testable hypothesis.\n\
         3. TEST: Propose a concrete test to confirm/refute the hypothesis.\n\
         4. INTEGRATE: What single change would prevent this failure from recurring?"
    );
    let step = |tool: &str, arguments: Value, what: &str| ChainStep {
        tool_name: tool.to_string(),
        arguments: Some(arguments),
        priority: Some(0),
        max_attempts: Some(2),
        provider_hint: Some("local".to_string()),
        description: Some(format!("Meta-learn: {what} for '{tool_name}'")),
        ..Default::default()
    };
    vec![
        step(
            "search_notes",
            serde_json::json!({
                "query": format!("failure {tool_name} root cause error"),
                "limit": 6
            }),
            "gather evidence",
        ),
        step(
            "reason",
            serde_json::json!({ "question": hypothesis_question, "store_inference": true }),
            "hypothesize root cause",
        ),
        step(
            "store_note",
            serde_json::json!({
                "content": "{{_prev}}",
                "note_type": "meta_learning_result",
                "source_context": format!("dead_job:{tool_name}"),
                "provenance": "synthesis_inference"
            }),
            "store result",
        ),
    ]
}

/// Parse `overall_robustness` (1.0–5.0) from `adversarial_plan_review` JSON output.
///
/// Tries the JSON field first; falls back to a `Robustness: N/5` text pattern;
/// defaults to 3.0 (neutral) if neither is found.
fn parse_adversarial_robustness(text: &str) -> f32 {
    // Try to parse the JSON blob returned by the tool.
    if let Some(v) = agent_brain_llm::llm::first_json_object(text, &["overall_robustness"])
        && let Some(r) = v.get("overall_robustness").and_then(|x| x.as_f64())
    {
        return (r as f32).clamp(1.0, 5.0);
    }
    // Fallback: look for `Robustness: N/5` or `overall_robustness: N`.
    for line in text.lines() {
        let lower = line.trim().to_lowercase();
        if let Some(rest) = lower
            .strip_prefix("robustness:")
            .or_else(|| lower.strip_prefix("overall_robustness:"))
            && let Some(n_str) = rest.trim().split('/').next()
            && let Ok(n) = n_str.trim().parse::<f32>()
        {
            return n.clamp(1.0, 5.0);
        }
    }
    3.0
}

/// Returns `true` if exhausted retries for `tool_name` should trigger a
/// meta-learning chain rather than only a reflection note.
///
/// Maintenance and infrastructure tools are excluded to prevent the meta-
/// learning loop from generating endless self-referential failures.
fn should_meta_learn(tool_name: &str) -> bool {
    const EXCLUDED: &[&str] = &[
        "store_note",
        "update_task",
        "consolidate_memories",
        "synthesize_knowledge",
        "reason",
        "reflect_on_work",
        "adversarial_plan_review",
        "prune_old_notes",
        "review_due_notes",
        "get_task",
        "list_tasks",
    ];
    !EXCLUDED.contains(&tool_name)
}

impl QueueService {
    /// Returns true if a meta-learning result note for `tool_name` was stored within the
    /// last 24 hours. Used to dedupe the dead-job meta-learning chain so a repeatedly
    /// failing tool doesn't spawn a fresh Analyze→Hypothesize→Integrate chain every time.
    async fn recently_meta_learned(&self, tool_name: &str) -> bool {
        let ctx = format!("dead_job:{}", tool_name);
        let cypher = "MATCH (n:Note) \
                      WHERE n.note_type = 'meta_learning_result' \
                        AND n.source_context = $ctx \
                        AND n.created_at >= datetime() - duration({hours: 24}) \
                      RETURN count(n) AS c";
        match self
            .neo4j
            .execute(neo4rs::query(cypher).param("ctx", ctx))
            .await
        {
            Ok(rows) => {
                rows.first()
                    .and_then(|r| r.get::<i64>("c").ok())
                    .unwrap_or(0)
                    > 0
            }
            Err(e) => {
                warn!(
                    "recently_meta_learned check failed (proceeding without dedupe): {}",
                    e
                );
                false
            }
        }
    }

    /// Evaluator step: parse the score and re-queue the task if it is below the
    /// threshold. Returns whether the chain is blocked. When the score fails,
    /// parked children (e.g. `update_task`) are cancelled so the task is not
    /// prematurely marked completed — the retry task created by
    /// `handle_evaluator_requeue` drives the next attempt.
    pub(super) async fn evaluator_gate(&self, job: &AgentJob, result_text: &str) -> bool {
        let Some(min_score) = job_arg(job, "__evaluator_min_score").and_then(Value::as_f64) else {
            return false;
        };
        let task_id = job_arg(job, "__evaluator_task_id").and_then(Value::as_str);

        let Some(score) = parse_evaluator_score(result_text) else {
            // No parseable score or verdict keyword. Treating this as a failing
            // 3.0 (the old behaviour) burned up to 3 retry chains on mere format
            // drift from the local model. Unparseable output is a pass.
            warn!(
                job_id = %job.id,
                "Evaluator output unparseable (no Score line or verdict) — treating as pass"
            );
            return false;
        };

        if score >= min_score as f32 {
            info!(job_id = %job.id, score = score, "Evaluator: score passed");
            return false;
        }
        warn!(
            job_id = %job.id,
            score = score,
            min_score = min_score,
            "Evaluator: score below threshold — cancelling downstream steps and re-queuing task"
        );
        let _ = self.neo4j.cancel_parked_children(&job.id).await;
        if let Some(tid) = task_id {
            self.handle_evaluator_requeue(tid, score, result_text).await;
        } else if let Some(owner) = job_arg(job, "__owner_task_id").and_then(Value::as_str) {
            // A workflow's own evaluator step (`is_evaluator` in its YAML)
            // fails the run without a retry: the default chain retried could
            // only read the same notes again (2026-10-25, IBM API research
            // scored 1/5 and was marked completed).
            let _ = self
                .neo4j
                .update_task_status(owner, TaskStatus::Failed)
                .await;
        }
        true
    }

    /// Adversarial pre-flight: abort the chain if the plan is too risky. Mirrors
    /// the evaluator gate but fires on the first step rather than the last.
    pub(super) async fn adversarial_gate(&self, job: &AgentJob, result_text: &str) -> bool {
        let Some(min_robustness) =
            job_arg(job, "__adversarial_min_robustness").and_then(Value::as_f64)
        else {
            return false;
        };
        let robustness = parse_adversarial_robustness(result_text);
        if robustness >= min_robustness as f32 {
            info!(job_id = %job.id, robustness, "Adversarial: robustness passed — proceeding");
            return false;
        }
        warn!(
            job_id = %job.id,
            robustness,
            min_robustness,
            "Adversarial: robustness below threshold — cancelling chain and re-queuing task"
        );
        let _ = self.neo4j.cancel_parked_children(&job.id).await;
        if let Some(tid) = job_arg(job, "__adversarial_task_id").and_then(Value::as_str) {
            self.handle_adversarial_requeue(tid, robustness, result_text)
                .await;
        }
        true
    }

    /// Enqueue the Analyze→Hypothesize→Test→Integrate chain for a dead job —
    /// immediately, rather than waiting for perception_scan to accumulate 3+
    /// failures. Skipped for infrastructure tools, for transient/infra errors
    /// (quota, rate limits, timeouts) the brain cannot fix, and when the tool
    /// was already meta-learned in the last 24h.
    pub(super) async fn maybe_meta_learn(&self, job: &AgentJob, attempt: u32, error_text: &str) {
        if !should_meta_learn(&job.tool_name) || error_text.starts_with(STEP_MODEL_ERROR) {
            return;
        }
        if is_transient_infra_error(error_text) {
            info!(job_id = %job.id, tool = %job.tool_name, "Dead job is a transient/infra error — skipping meta-learning");
            return;
        }
        if self.recently_meta_learned(&job.tool_name).await {
            info!(job_id = %job.id, tool = %job.tool_name, "Meta-learning already ran for this tool in the last 24h — skipping");
            return;
        }
        let steps = meta_learning_chain(&job.tool_name, attempt, error_text);
        if let Err(e) = self.enqueue_chain(&steps, job.session_id.as_deref()).await {
            warn!(job_id = %job.id, error = %e, "Failed to enqueue meta-learning chain for dead job");
        } else {
            info!(job_id = %job.id, tool = %job.tool_name, "Enqueued meta-learning chain for dead job");
        }
    }

    /// Called when an evaluator step scores a task below its threshold.
    ///
    /// Marks the original task `failed` and creates a new `Task` node with the
    /// critique injected as context so the scheduler will re-dispatch it on the
    /// next tick.
    async fn handle_evaluator_requeue(&self, task_id: &str, score: f32, critique: &str) {
        let task = match self.neo4j.get_task(task_id).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                warn!(task_id = %task_id, "Evaluator re-queue: task not found");
                return;
            }
            Err(e) => {
                warn!(task_id = %task_id, error = %e, "Evaluator re-queue: failed to fetch task");
                return;
            }
        };

        // Count previous retry attempts embedded in the context string.
        // Each retry prepends "RETRY —", so the count == number of prior retries.
        let retry_count = task
            .context
            .as_deref()
            .unwrap_or("")
            .matches("RETRY —")
            .count();

        // Mark the original task failed.
        let _ = self
            .neo4j
            .update_task_status(task_id, TaskStatus::Failed)
            .await;

        if retry_count >= 3 {
            warn!(
                task_id = %task_id,
                retry_count,
                score,
                "Evaluator: retry cap reached — marking terminal failure, not re-queuing"
            );
            return;
        }

        let retry_context = format!(
            "RETRY — previous attempt scored {:.1}/5.\n\nEvaluator critique:\n{}\n\nOriginal context: {}",
            score,
            critique.chars().take(800).collect::<String>(),
            task.context.as_deref().unwrap_or("none"),
        );

        // The retry runs the same workflow with the same inputs.
        match crate::workflows::start_run(
            &self.neo4j,
            task.workflow.as_deref(),
            &task.inputs.clone().unwrap_or_default(),
            Some(&task.goal),
            Some(&retry_context),
            task.success_criteria.as_deref(),
        )
        .await
        .map(|run| run.task_id)
        {
            Ok(new_id) => info!(
                original_task_id = %task_id,
                new_task_id = %new_id,
                score,
                "Evaluator: created retry task"
            ),
            Err(e) => warn!(error = %e, "Evaluator: failed to create retry task"),
        }
    }

    /// Called when an adversarial pre-flight step scores a plan below its robustness threshold.
    ///
    /// Marks the original task `failed` and creates a new `Task` node with the adversarial
    /// critique injected as context so the scheduler will re-dispatch a hardened attempt.
    async fn handle_adversarial_requeue(&self, task_id: &str, robustness: f32, critique: &str) {
        let task = match self.neo4j.get_task(task_id).await {
            Ok(Some(t)) => t,
            Ok(None) => {
                warn!(task_id = %task_id, "Adversarial re-queue: task not found");
                return;
            }
            Err(e) => {
                warn!(task_id = %task_id, error = %e, "Adversarial re-queue: failed to fetch task");
                return;
            }
        };

        // Count previous adversarial aborts to enforce the same retry cap as the evaluator.
        let abort_count = task
            .context
            .as_deref()
            .unwrap_or("")
            .matches("ADVERSARIAL ABORT")
            .count();

        let _ = self
            .neo4j
            .update_task_status(task_id, TaskStatus::Failed)
            .await;

        if abort_count >= 3 {
            warn!(
                task_id = %task_id,
                abort_count,
                robustness,
                "Adversarial: abort cap reached — marking terminal failure"
            );
            return;
        }

        let retry_context = format!(
            "ADVERSARIAL ABORT — plan robustness {:.1}/5 (threshold 2.5).\n\nAdversarial critique:\n{}\n\nOriginal context: {}",
            robustness,
            critique.chars().take(800).collect::<String>(),
            task.context.as_deref().unwrap_or("none"),
        );

        // The retry runs the same workflow with the same inputs.
        match crate::workflows::start_run(
            &self.neo4j,
            task.workflow.as_deref(),
            &task.inputs.clone().unwrap_or_default(),
            Some(&task.goal),
            Some(&retry_context),
            task.success_criteria.as_deref(),
        )
        .await
        .map(|run| run.task_id)
        {
            Ok(new_id) => info!(
                original_task_id = %task_id,
                new_task_id = %new_id,
                robustness,
                "Adversarial: created hardened retry task"
            ),
            Err(e) => warn!(error = %e, "Adversarial: failed to create retry task"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_evaluator_score_explicit_line() {
        assert_eq!(parse_evaluator_score("Score: 4/5\nGood."), Some(4.0));
        assert_eq!(parse_evaluator_score("  Score: 2 / 5"), Some(2.0));
    }

    #[test]
    fn parse_evaluator_score_verdict_keywords() {
        assert_eq!(parse_evaluator_score("The goal was FULLY MET."), Some(5.0));
        assert_eq!(
            parse_evaluator_score("partially met, needs work"),
            Some(3.0)
        );
        assert_eq!(parse_evaluator_score("This was NOT MET at all"), Some(1.0));
    }

    #[test]
    fn parse_evaluator_score_unparseable_is_none() {
        // Format drift / prose with no score line and no verdict keyword must be None,
        // so the caller treats it as a pass instead of a failing fabricated 3.0.
        assert_eq!(
            parse_evaluator_score("Here is my assessment of the work done so far."),
            None
        );
        assert_eq!(parse_evaluator_score(""), None);
    }

    #[test]
    fn parse_evaluator_score_clamps_out_of_range() {
        assert_eq!(parse_evaluator_score("Score: 9/5"), Some(5.0));
        assert_eq!(parse_evaluator_score("Score: 0/5"), Some(1.0));
    }
}

#[cfg(test)]
mod meta_learning_chain_tests {
    use super::*;

    /// The dead-job chain is background work: every step must run on the local
    /// model (an omitted provider_hint falls through to cloud), and its result
    /// must be persisted with a source_context naming the tool that died.
    #[test]
    fn the_meta_learning_chain_stays_local_and_names_the_dead_tool() {
        let steps = meta_learning_chain("ingest_media", 3, "m3u8 download detected");
        let tools: Vec<&str> = steps.iter().map(|s| s.tool_name.as_str()).collect();
        assert_eq!(tools, vec!["search_notes", "reason", "store_note"]);
        assert!(
            steps
                .iter()
                .all(|s| s.provider_hint.as_deref() == Some("local"))
        );
        let question = steps[1].arguments.as_ref().unwrap()["question"]
            .as_str()
            .unwrap();
        assert!(question.contains("'ingest_media' just died after 3 attempts"));
        assert!(question.contains("m3u8 download detected"));
        let store = steps[2].arguments.as_ref().unwrap();
        assert_eq!(store["content"], "{{_prev}}");
        assert_eq!(store["source_context"], "dead_job:ingest_media");
    }
}
