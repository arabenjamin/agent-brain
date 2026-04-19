//! Autonomous scheduler — polls for pending tasks and dispatches job chains.
//!
//! # Design
//!
//! - Runs a single background Tokio task that wakes every `interval_secs`.
//! - On each tick: lists tasks with `status = "created"`, maps each goal to a
//!   `Vec<ChainStep>` via a keyword heuristic, and calls `QueueService::enqueue_chain`.
//! - Immediately marks dispatched tasks as `InProgress` to prevent double-dispatch.
//! - Auto-pauses after `error_budget` consecutive tick failures.
//! - Controllable at runtime via `SchedulerSkill` (5 MCP tools).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::{Notify, RwLock};
use tracing::{debug, info, warn};

use crate::models::TaskStatus;
use crate::repository::{Neo4jClient, ScheduledTask};
use crate::services::context_builder::ContextBuilderService;
use crate::services::queue::{ChainStep, QueueService};
use crate::services::{LlmConfig, LlmProviderType};

/// Runtime configuration for the scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerConfig {
    /// How often the scheduler polls for pending tasks (seconds). Default: 300.
    pub interval_secs: u64,
    /// When `false`, ticks are skipped. Default: reads `SCHEDULER_ENABLED` env.
    pub enabled: bool,
    /// Maximum number of tasks to dispatch per tick. Default: 3.
    pub max_tasks_per_run: usize,
    /// Auto-pause after this many consecutive errors. Default: 5.
    pub error_budget: u32,
    /// Optional session ID to attach to enqueued jobs.
    pub session_id: Option<String>,
    /// Number of consecutive idle ticks before entering sleep mode. Default: 3.
    pub idle_sleep_after_ticks: u32,
    /// Scheduler tick interval while in sleep mode (seconds). Default: 1800.
    pub sleep_interval_secs: u64,
    /// Ollama model used for background/scheduled jobs on the local instance.
    /// Default: reads `OLLAMA_LOCAL_MODEL` env var, falls back to `"gemma4:latest"`.
    pub local_model: String,
}

impl Default for SchedulerConfig {
    fn default() -> Self {
        Self {
            interval_secs: std::env::var("SCHEDULER_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(300),
            enabled: std::env::var("SCHEDULER_ENABLED")
                .map(|v| v != "false" && v != "0")
                .unwrap_or(true),
            max_tasks_per_run: 3,
            error_budget: 5,
            session_id: None,
            idle_sleep_after_ticks: std::env::var("IDLE_SLEEP_AFTER_TICKS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(3),
            sleep_interval_secs: std::env::var("SLEEP_INTERVAL_SECS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(1800),
            local_model: std::env::var("OLLAMA_LOCAL_MODEL")
                .unwrap_or_else(|_| "gemma4:latest".to_string()),
        }
    }
}

/// Live runtime state for the scheduler.
#[derive(Debug, Clone, Default)]
pub struct SchedulerState {
    pub tasks_dispatched: u64,
    pub consecutive_errors: u32,
    pub last_run_at: Option<String>,
    pub last_error: Option<String>,
    pub is_running: bool,
    /// Consecutive idle ticks since last activity (tasks_dispatched + new_tasks_created == 0).
    pub idle_ticks: u32,
    /// Whether the scheduler is in sleep mode (longer tick interval, bedtime routine queued).
    pub is_sleeping: bool,
    /// RFC3339 timestamp of the last `notify_activity()` call (i.e. last incoming tool call).
    pub last_activity_at: Option<String>,
}

/// Result returned from a single scheduler tick.
#[derive(Debug)]
pub struct TickResult {
    /// Total tasks with `status = "created"` found (up to query cap).
    pub tasks_found: usize,
    /// Tasks successfully enqueued this tick.
    pub tasks_dispatched: usize,
    /// Tasks found but not dispatched (over limit or enqueue failed).
    pub skipped: usize,
    /// New tasks created by proactive perception scan.
    pub new_tasks_created: usize,
}

/// Background scheduler service.
pub struct SchedulerService {
    neo4j: Neo4jClient,
    queue: Arc<QueueService>,
    pub config: Arc<RwLock<SchedulerConfig>>,
    pub state: Arc<RwLock<SchedulerState>>,
    shutdown: Arc<AtomicBool>,
    wakeup: Arc<Notify>,
    context_builder: Option<Arc<ContextBuilderService>>,
    /// Live local-Ollama config shared with `SharedLlm`. Mutated when `local_model` is updated.
    local_config: Option<Arc<RwLock<Option<LlmConfig>>>>,
}

impl SchedulerService {
    /// Create and start the scheduler.
    ///
    /// Reads `SCHEDULER_INTERVAL_SECS` and `SCHEDULER_ENABLED` from the environment,
    /// then spawns the background loop immediately.
    pub fn new(neo4j: Neo4jClient, queue: Arc<QueueService>) -> Arc<Self> {
        Self::new_with_context(neo4j, queue, None, None)
    }

    /// Create and start the scheduler with an optional context builder for profile auto-assignment.
    /// `local_config` is the shared `Arc<RwLock<Option<LlmConfig>>>` for background jobs.
    /// When `configure_scheduler` updates `local_model`, the model inside this arc is updated
    /// in-place so `SharedLlm` (which holds the same arc) picks up the change automatically.
    pub fn new_with_context(
        neo4j: Neo4jClient,
        queue: Arc<QueueService>,
        context_builder: Option<Arc<ContextBuilderService>>,
        local_config: Option<Arc<RwLock<Option<LlmConfig>>>>,
    ) -> Arc<Self> {
        let svc = Arc::new(Self {
            neo4j,
            queue,
            config: Arc::new(RwLock::new(SchedulerConfig::default())),
            state: Arc::new(RwLock::new(SchedulerState::default())),
            shutdown: Arc::new(AtomicBool::new(false)),
            wakeup: Arc::new(Notify::new()),
            context_builder,
            local_config,
        });

        let svc_clone = Arc::clone(&svc);
        tokio::spawn(async move {
            svc_clone.run_loop().await;
        });

        let enabled = std::env::var("SCHEDULER_ENABLED")
            .map(|v| v != "false" && v != "0")
            .unwrap_or(true);
        let interval = std::env::var("SCHEDULER_INTERVAL_SECS")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(300);
        info!(
            interval_secs = interval,
            enabled = enabled,
            "SchedulerService started"
        );
        svc
    }

    // =========================================================================
    // Background loop
    // =========================================================================

    async fn run_loop(self: Arc<Self>) {
        loop {
            // Snapshot the effective interval before sleeping.
            // When in sleep mode use the longer sleep_interval_secs; otherwise interval_secs.
            let interval_secs = {
                let s = self.state.read().await;
                let c = self.config.read().await;
                if s.is_sleeping {
                    c.sleep_interval_secs
                } else {
                    c.interval_secs
                }
            };

            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(interval_secs)) => {}
                _ = self.wakeup.notified() => {}
            }

            if self.shutdown.load(Ordering::Relaxed) {
                info!("SchedulerService shutdown signal received, stopping loop");
                break;
            }

            let enabled = self.config.read().await.enabled;
            if !enabled {
                debug!("Scheduler disabled, skipping tick");
                continue;
            }

            // Mark running (snapshot, don't hold across await)
            {
                let mut st = self.state.write().await;
                st.is_running = true;
            }

            let now = Utc::now().to_rfc3339();
            match self.do_tick().await {
                Ok(result) => {
                    let mut st = self.state.write().await;
                    st.consecutive_errors = 0;
                    st.tasks_dispatched += result.tasks_dispatched as u64;
                    st.last_run_at = Some(now);
                    st.last_error = None;
                    st.is_running = false;
                    info!(
                        tasks_found = result.tasks_found,
                        dispatched = result.tasks_dispatched,
                        skipped = result.skipped,
                        new_tasks = result.new_tasks_created,
                        "Scheduler tick completed"
                    );
                }
                Err(e) => {
                    let error_budget = self.config.read().await.error_budget;
                    let consecutive = {
                        let mut st = self.state.write().await;
                        st.consecutive_errors += 1;
                        st.last_error = Some(e.clone());
                        st.last_run_at = Some(now);
                        st.is_running = false;
                        st.consecutive_errors
                    };

                    warn!(error = %e, consecutive = consecutive, "Scheduler tick failed");

                    if consecutive >= error_budget {
                        self.config.write().await.enabled = false;
                        warn!(
                            consecutive = consecutive,
                            "Scheduler auto-paused after exhausting error budget"
                        );
                    }
                }
            }
        }

        info!("SchedulerService loop exited");
    }

    // =========================================================================
    // Core tick logic
    // =========================================================================

    async fn do_tick(&self) -> Result<TickResult, String> {
        // Snapshot config — never hold an RwLock guard across .await.
        let (max_tasks, session_id) = {
            let cfg = self.config.read().await;
            (cfg.max_tasks_per_run, cfg.session_id.clone())
        };

        // Reset tasks that got stuck in_progress (e.g. because a job chain failed and the final
        // update_task step was cancelled).  Any task in_progress for > 6 hours is reset to
        // 'failed' so the user can see it clearly, rather than silently blocking future runs.
        if let Err(e) = self.neo4j.reset_stale_in_progress_tasks(6).await {
            warn!(error = %e, "Failed to reset stale in_progress tasks");
        }

        // Dispatch any due ScheduledTask nodes before processing one-off tasks.
        let scheduled_dispatched = self.dispatch_scheduled_tasks().await;
        let mut tasks_dispatched = scheduled_dispatched;

        let tasks = self
            .neo4j
            .list_tasks(Some("created"), 20)
            .await
            .map_err(|e| e.to_string())?;

        let tasks_found = tasks.len();

        for task in tasks.iter().take(max_tasks) {
            let task_id = task["id"].as_str().unwrap_or("").to_string();
            let goal = task["goal"].as_str().unwrap_or("").to_string();

            if task_id.is_empty() || goal.is_empty() {
                continue;
            }

            let mut steps = self.goal_to_steps(&goal, &task_id).await;

            // Auto-assign a context profile to all steps if context_builder is available.
            if let Some(cb) = &self.context_builder {
                let profile = cb.auto_assign(&goal).await;
                for step in &mut steps {
                    if step.context_profile.is_none() {
                        step.context_profile = Some(profile.clone());
                    }
                }
            }

            match self
                .queue
                .enqueue_chain(&steps, session_id.as_deref())
                .await
            {
                Ok(ids) => {
                    // Mark in_progress immediately to prevent double-dispatch on next tick.
                    if let Err(e) = self
                        .neo4j
                        .update_task_status(&task_id, TaskStatus::InProgress)
                        .await
                    {
                        warn!(task_id = %task_id, error = %e, "Failed to mark task in_progress");
                    }
                    tasks_dispatched += 1;
                    info!(
                        task_id = %task_id,
                        jobs = ids.len(),
                        goal = %goal,
                        "Scheduler dispatched task chain"
                    );
                }
                Err(e) => {
                    warn!(task_id = %task_id, error = %e, "Failed to enqueue task chain");
                }
            }
        }

        // `tasks_dispatched` includes scheduled-task dispatches that are not counted in
        // `tasks_found` (which is only one-off Task nodes).  Subtract the scheduled portion
        // before computing skipped to avoid a usize underflow / panic in debug builds.
        let one_off_dispatched = tasks_dispatched.saturating_sub(scheduled_dispatched);
        let skipped = tasks_found.saturating_sub(one_off_dispatched);

        // Proactive perception: scan outcomes for failure patterns, create new tasks.
        let new_tasks_created = self.perception_scan().await.unwrap_or_else(|e| {
            warn!(error = %e, "Perception scan failed");
            0
        });

        // Idle detection: if nothing happened this tick, increment the idle counter.
        let was_idle = tasks_dispatched == 0 && new_tasks_created == 0;
        if was_idle {
            let threshold = self.config.read().await.idle_sleep_after_ticks;
            let mut st = self.state.write().await;
            st.idle_ticks += 1;
            let should_enter_sleep = st.idle_ticks >= threshold && !st.is_sleeping;
            if should_enter_sleep {
                st.is_sleeping = true;
                drop(st);
                self.enter_sleep().await;
            }
        } else {
            let mut st = self.state.write().await;
            st.idle_ticks = 0;
            st.is_sleeping = false;
        }

        Ok(TickResult {
            tasks_found,
            tasks_dispatched,
            skipped,
            new_tasks_created,
        })
    }

    // -------------------------------------------------------------------------
    // ScheduledTask dispatch
    // -------------------------------------------------------------------------

    /// Query Neo4j for due `ScheduledTask` nodes, create a run-record `Task` for
    /// each, enqueue its job chain with an auto-appended `update_task` step, and
    /// advance `last_run_at` / `next_run_at`.
    ///
    /// Returns the number of tasks successfully dispatched.
    async fn dispatch_scheduled_tasks(&self) -> usize {
        let due = match self.neo4j.get_due_scheduled_tasks().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Failed to query due ScheduledTasks");
                return 0;
            }
        };

        if due.is_empty() {
            return 0;
        }

        let session_id = self.config.read().await.session_id.clone();
        let mut dispatched = 0usize;

        for st in &due {
            if let Err(count) = self
                .dispatch_one_scheduled_task(st, session_id.as_deref())
                .await
            {
                warn!(
                    scheduled_task_id = %st.id,
                    name = %st.name,
                    error = %count,
                    "Failed to dispatch ScheduledTask"
                );
            } else {
                dispatched += 1;
            }
        }

        dispatched
    }

    /// Dispatch a single `ScheduledTask`: create Task node, enqueue chain, record run.
    /// Returns `Ok(task_id)` on success, `Err(reason)` on failure.
    async fn dispatch_one_scheduled_task(
        &self,
        st: &ScheduledTask,
        session_id: Option<&str>,
    ) -> Result<String, String> {
        // 1. Create a Task node as a run record.
        let task_id = self
            .neo4j
            .create_task(
                &st.name,
                Some(&format!(
                    "Run of ScheduledTask '{}' (id: {})",
                    st.name, st.id
                )),
            )
            .await
            .map_err(|e| e.to_string())?;

        // 2. Deserialise steps, substituting template vars.
        let date = Utc::now().format("%Y-%m-%d").to_string();
        let steps_json = st
            .steps
            .replace("{{task_id}}", &task_id)
            .replace("{{goal}}", &st.name)
            .replace("{{date}}", &date);

        let mut steps: Vec<ChainStep> = serde_json::from_str(&steps_json)
            .map_err(|e| format!("ScheduledTask '{}' steps JSON invalid: {}", st.name, e))?;

        // 3. Auto-assign context profile if available.
        if let Some(cb) = &self.context_builder {
            let profile = cb.auto_assign(&st.name).await;
            for step in &mut steps {
                if step.context_profile.is_none() {
                    step.context_profile = Some(profile.clone());
                }
            }
        }

        // 4. Auto-append update_task so the Task node is marked completed when the chain finishes.
        steps.push(ChainStep {
            tool_name: "update_task".to_string(),
            arguments: Some(json!({
                "task_id": task_id,
                "status": "completed",
                "note": format!("ScheduledTask '{}' completed", st.name)
            })),
            priority: Some(1),
            max_attempts: Some(3),
            provider_hint: Some("ollama".to_string()),
            context_profile: None,
            ttl_secs: None,
            description: None,
        });

        // 5. Enqueue the chain.
        self.queue
            .enqueue_chain(&steps, session_id)
            .await
            .map_err(|e| e.to_string())?;

        // 6. Mark Task in_progress and advance ScheduledTask timestamps.
        let _ = self
            .neo4j
            .update_task_status(&task_id, TaskStatus::InProgress)
            .await;

        let now = Utc::now().to_rfc3339();
        let next = Neo4jClient::compute_next_run_at(st.interval_seconds);
        if let Err(e) = self
            .neo4j
            .record_scheduled_task_run(&st.id, &now, &next)
            .await
        {
            warn!(id = %st.id, error = %e, "Failed to record ScheduledTask run timestamps");
        }

        info!(
            scheduled_task_id = %st.id,
            task_id = %task_id,
            name = %st.name,
            jobs = steps.len(),
            "ScheduledTask dispatched"
        );
        Ok(task_id)
    }

    // -------------------------------------------------------------------------

    /// Enqueue a low-priority bedtime chain: consolidate → prune → store note.
    ///
    /// Called once when the scheduler transitions into sleep mode after `idle_sleep_after_ticks`
    /// consecutive idle ticks.  Skipped if a sleep/consolidation chain ran within the last
    /// 6 hours to prevent the chain from firing every 15 minutes on busy days.
    async fn enter_sleep(&self) {
        // Cooldown: skip if a prune_old_notes job completed within the last 6 hours.
        // (We check the job table rather than a note because the final store_note step can be
        // cancelled if an intermediate step fails — the job table is always written.)
        let cooldown_q = neo4rs::query(
            "MATCH (j:AgentJob {tool_name: 'prune_old_notes', status: 'completed'}) \
             WHERE j.created_at >= datetime() - duration({hours: 6}) \
             RETURN count(j) AS cnt",
        );
        let recent_sleep: i64 = self
            .neo4j
            .execute(cooldown_q)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
            .unwrap_or(0);
        if recent_sleep > 0 {
            debug!("Skipping bedtime chain — sleep cycle ran within the last 6 hours");
            return;
        }

        let (session_id, timestamp) = {
            let cfg = self.config.read().await;
            (cfg.session_id.clone(), chrono::Utc::now().to_rfc3339())
        };

        let steps = vec![
            ChainStep {
                tool_name: "consolidate_memories".to_string(),
                arguments: Some(json!({
                    "topic": "recent experiences and knowledge",
                    "limit": 10
                })),
                priority: Some(0),
                max_attempts: Some(2),
                provider_hint: Some("ollama".to_string()),
                context_profile: None,
                ttl_secs: None,
                description: None,
            },
            ChainStep {
                tool_name: "prune_old_notes".to_string(),
                arguments: Some(json!({ "dry_run": false })),
                priority: Some(0),
                max_attempts: Some(2),
                provider_hint: Some("ollama".to_string()),
                context_profile: None,
                ttl_secs: None,
                description: None,
            },
            ChainStep {
                tool_name: "store_note".to_string(),
                arguments: Some(json!({
                    "content": format!(
                        "Brain entering sleep mode at {timestamp}. Idle cleanup complete."
                    ),
                    "note_type": "outcome",
                    "source_context": "sleep"
                })),
                priority: Some(0),
                max_attempts: Some(2),
                provider_hint: Some("ollama".to_string()),
                context_profile: None,
                ttl_secs: None,
                description: None,
            },
        ];

        if let Err(e) = self
            .queue
            .enqueue_chain(&steps, session_id.as_deref())
            .await
        {
            warn!(error = %e, "Failed to enqueue bedtime chain on sleep entry");
        } else {
            info!("Brain entering sleep mode after idle ticks; bedtime chain enqueued");
        }
    }

    /// Scan recent outcome notes for repeated failure patterns and auto-create analysis tasks.
    async fn perception_scan(&self) -> Result<usize, String> {
        // Find all outcome notes from the last 7 days that recorded failures.
        let cypher = r#"
        MATCH (n:Note)
        WHERE n.note_type = 'outcome'
          AND n.content CONTAINS 'Success: false'
          AND n.created_at >= datetime() - duration({days: 7})
        RETURN n.content AS content
        "#;

        let rows = self
            .neo4j
            .execute(neo4rs::query(cypher))
            .await
            .map_err(|e| e.to_string())?;

        // Count failures per tool name (content format: "Tool: <name> | Success: false\n...")
        let mut tool_failures: HashMap<String, u32> = HashMap::new();
        for row in &rows {
            if let Ok(content) = row.get::<String>("content")
                && let Some(rest) = content.strip_prefix("Tool: ")
                && let Some(tool_name) = rest.split(" | ").next()
            {
                *tool_failures
                    .entry(tool_name.trim().to_string())
                    .or_insert(0) += 1;
            }
        }

        let mut created = 0usize;
        for (tool, count) in tool_failures {
            if count < 3 {
                continue;
            }
            // Only create if no open task about this tool already exists.
            let check = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE t.goal CONTAINS $tool \
                   AND t.status IN ['created', 'in_progress'] \
                 RETURN count(t) AS cnt",
            )
            .param("tool", tool.as_str());

            let existing: i64 = self
                .neo4j
                .execute(check)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                .unwrap_or(0);

            if existing == 0 {
                let goal = format!(
                    "Analyze repeated failures for '{}' and identify root cause or documentation gap",
                    tool
                );
                match self
                    .neo4j
                    .create_task(&goal, Some("Auto-generated by proactive perception scan"))
                    .await
                {
                    Ok(_) => {
                        created += 1;
                        info!(tool = %tool, failures = count, "Perception scan created failure analysis task");
                    }
                    Err(e) => warn!(tool = %tool, error = %e, "Failed to create perception task"),
                }
            }
        }

        // Helper: check if a consolidation task is active OR was completed recently (within 24h).
        // Evaluated once and shared across all consolidation triggers so at most one new
        // consolidation task can be created per perception_scan tick.
        let open_consolidation_exists = || async {
            let q = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE toLower(t.goal) CONTAINS 'consolidat' \
                   AND (t.status IN ['created', 'in_progress'] \
                     OR (t.status = 'completed' \
                         AND t.created_at >= datetime() - duration({hours: 24}))) \
                 RETURN count(t) AS cnt",
            );
            // Default to 1 (assume consolidation exists) on any DB error so that
            // a transient Neo4j failure never causes a spurious duplicate task to
            // be created — the safe direction is to skip, not to enqueue.
            self.neo4j
                .execute(q)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                .unwrap_or(1)
                > 0
        };

        // Evaluate the guard once so both triggers share the same snapshot and a task created
        // by trigger 1 is guaranteed to block trigger 2 within the same tick.
        let mut consolidation_queued = open_consolidation_exists().await;

        // Trigger 1: many overdue spaced-repetition notes.
        let due_check = neo4rs::query(
            "MATCH (n:Note) \
             WHERE n.next_review_at <= datetime() \
               AND n.note_type <> 'consolidated' \
             RETURN count(n) AS cnt",
        );
        let due_count: i64 = self
            .neo4j
            .execute(due_check)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
            .unwrap_or(0);

        // Raised threshold from 10 → 25 → 50: lower values were too easily hit by normal note
        // accumulation, causing consolidation to run too frequently and consume excess resources.
        if due_count >= 50 && !consolidation_queued {
            let goal = format!(
                "Consolidate {} overdue spaced-repetition notes into long-term memory",
                due_count
            );
            if self
                .neo4j
                .create_task(
                    &goal,
                    Some("Auto-generated by proactive perception scan (spaced-repetition backlog)"),
                )
                .await
                .is_ok()
            {
                created += 1;
                consolidation_queued = true;

                // Immediately advance next_review_at on ALL currently-overdue notes so that
                // subsequent perception scan ticks don't see them as still overdue and queue
                // another consolidation task before this one has a chance to run.
                // The actual consolidation job will extend dates further when it processes notes.
                let bump = neo4rs::query(
                    "MATCH (n:Note) \
                     WHERE n.next_review_at <= datetime() \
                       AND NOT COALESCE(n.note_type, 'semantic') IN ['consolidated', 'reflection'] \
                     SET n.next_review_at = datetime() + duration({days: 14}), \
                         n.review_interval_days = CASE \
                             WHEN COALESCE(n.review_interval_days, 1) < 14 THEN 14 \
                             ELSE COALESCE(n.review_interval_days, 14) \
                         END",
                );
                if let Err(e) = self.neo4j.run(bump).await {
                    warn!(
                        "Failed to advance overdue note dates after scheduling consolidation: {e}"
                    );
                }

                info!(
                    due_count = due_count,
                    "Perception scan created consolidation task (overdue notes)"
                );
            }
        }

        // Trigger 2: high episodic note volume (sleep-cycle analogue).
        let episodic_check =
            neo4rs::query("MATCH (n:Note) WHERE n.note_type = 'episodic' RETURN count(n) AS cnt");
        let episodic_count: i64 = self
            .neo4j
            .execute(episodic_check)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
            .unwrap_or(0);

        if episodic_count >= 75 && !consolidation_queued {
            let goal = format!(
                "Consolidate {} episodic notes — distil recurring patterns into semantic memory",
                episodic_count
            );
            if self.neo4j.create_task(
                &goal,
                Some("Auto-generated by proactive perception scan (episodic note volume threshold)"),
            ).await.is_ok() {
                created += 1;
                info!(episodic_count = episodic_count, "Perception scan created consolidation task (episodic volume)");
            }
        }

        // Trigger 3: routing coverage gap — many jobs fell back to the "general" profile,
        // meaning the semantic router couldn't match their goals to a specific profile.
        // Create a routing effectiveness review task so the agent can audit the gaps and
        // build targeted profiles.  (Red-Run takeaway #3 + #4)
        const ROUTING_FALLBACK_THRESHOLD: i64 = 5;
        let routing_check = neo4rs::query(
            "MATCH (j:AgentJob) \
             WHERE (j.context_profile = 'general' OR j.context_profile IS NULL) \
               AND j.created_at >= datetime() - duration({days: 7}) \
             RETURN count(j) AS cnt",
        );
        let routing_fallback_count: i64 = self
            .neo4j
            .execute(routing_check)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
            .unwrap_or(0);

        if routing_fallback_count >= ROUTING_FALLBACK_THRESHOLD {
            let check_existing = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE (t.goal CONTAINS 'routing' OR t.goal CONTAINS 'context profile') \
                   AND t.status IN ['created', 'in_progress'] \
                 RETURN count(t) AS cnt",
            );
            let existing: i64 = self
                .neo4j
                .execute(check_existing)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                .unwrap_or(0);

            if existing == 0 {
                let goal = format!(
                    "Review routing effectiveness: {} jobs in the last 7 days used the \
                     'general' context profile — audit which goals could not be matched \
                     to a specific profile and create targeted context profiles to improve \
                     routing coverage",
                    routing_fallback_count
                );
                if self
                    .neo4j
                    .create_task(
                        &goal,
                        Some(
                            "Auto-generated by proactive perception scan \
                             (routing coverage gap)",
                        ),
                    )
                    .await
                    .is_ok()
                {
                    created += 1;
                    info!(
                        count = routing_fallback_count,
                        "Perception scan created routing effectiveness review task"
                    );
                }
            }
        }

        // Trigger 4: codebase self-knowledge — bootstrap if none exists, refresh if stale (>7 days).
        let codebase_check = neo4rs::query(
            "MATCH (n:Note) \
             WHERE n.source_context = 'codebase_self_analysis' \
             RETURN count(n) AS cnt, \
                    toString(max(n.created_at)) AS newest",
        );
        let codebase_rows = self.neo4j.execute(codebase_check).await.ok();
        let codebase_note_count: i64 = codebase_rows
            .as_ref()
            .and_then(|rows| rows.first())
            .and_then(|r| r.get::<i64>("cnt").ok())
            .unwrap_or(0);
        // newest is None when count == 0; treat missing/parse error as stale → trigger re-analysis.
        let codebase_is_stale: bool = codebase_rows
            .as_ref()
            .and_then(|rows| rows.first())
            .and_then(|r| r.get::<String>("newest").ok())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| (Utc::now() - dt.to_utc()).num_days() >= 7)
            .unwrap_or(true);

        if codebase_note_count == 0 || codebase_is_stale {
            let check_existing = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE (t.goal CONTAINS 'codebase' OR t.goal CONTAINS 'self-knowledge') \
                   AND t.status IN ['created', 'in_progress'] \
                 RETURN count(t) AS cnt",
            );
            let existing: i64 = self
                .neo4j
                .execute(check_existing)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                .unwrap_or(0);

            if existing == 0 {
                let (goal, context) = if codebase_note_count == 0 {
                    (
                        "Analyze own codebase structure and store self-knowledge in graph",
                        "Auto-generated: no codebase self-analysis note found in graph",
                    )
                } else {
                    (
                        "Refresh codebase self-knowledge — re-analyze structure and recent changes",
                        "Auto-generated: codebase self-analysis note is more than 7 days old",
                    )
                };
                if self
                    .neo4j
                    .create_task(goal, Some(context))
                    .await
                    .is_ok()
                {
                    created += 1;
                    info!(
                        stale = codebase_is_stale,
                        count = codebase_note_count,
                        "Perception scan created codebase self-analysis task"
                    );
                }
            }
        }

        // Trigger 5: dead jobs accumulating — same tool dying repeatedly in 24h
        // signals a broken tool definition or bad external dependency.
        let dead_jobs_check = neo4rs::query(
            "MATCH (j:AgentJob) \
             WHERE j.status = 'dead' \
               AND j.updated_at >= datetime() - duration({hours: 24}) \
             RETURN j.tool_name AS tool_name, count(j) AS n \
             ORDER BY n DESC",
        );
        if let Ok(dead_rows) = self.neo4j.execute(dead_jobs_check).await {
            for row in &dead_rows {
                let tool_name = row.get::<String>("tool_name").unwrap_or_default();
                let n = row.get::<i64>("n").unwrap_or(0);
                if n < 3 || tool_name.is_empty() {
                    continue;
                }
                let check_existing = neo4rs::query(
                    "MATCH (t:Task) \
                     WHERE t.goal CONTAINS $tool \
                       AND toLower(t.goal) CONTAINS 'dead' \
                       AND t.status IN ['created', 'in_progress'] \
                     RETURN count(t) AS cnt",
                )
                .param("tool", tool_name.as_str());
                let existing: i64 = self
                    .neo4j
                    .execute(check_existing)
                    .await
                    .ok()
                    .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                    .unwrap_or(0);
                if existing == 0 {
                    let goal = format!(
                        "Investigate dead jobs: '{}' died {} times in the last 24 hours — \
                         diagnose the tool definition, input schema, or external dependency",
                        tool_name, n
                    );
                    if self
                        .neo4j
                        .create_task(
                            &goal,
                            Some(
                                "Auto-generated by proactive perception scan \
                                 (dead job accumulation)",
                            ),
                        )
                        .await
                        .is_ok()
                    {
                        created += 1;
                        info!(tool = %tool_name, dead_count = n, "Perception scan created dead-job investigation task");
                    }
                }
            }
        }

        // Trigger 6: queue backlog — many jobs stuck queued/parked with none running
        // may indicate the coordinator is blocked or a semaphore is exhausted.
        let backlog_check = neo4rs::query(
            "MATCH (j:AgentJob) \
             WHERE j.status IN ['queued', 'parked'] \
             RETURN count(j) AS n",
        );
        let backlog_count: i64 = self
            .neo4j
            .execute(backlog_check)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("n").ok()))
            .unwrap_or(0);

        if backlog_count >= 20 {
            let check_existing = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE toLower(t.goal) CONTAINS 'queue' \
                   AND toLower(t.goal) CONTAINS 'backlog' \
                   AND t.status IN ['created', 'in_progress'] \
                 RETURN count(t) AS cnt",
            );
            let existing: i64 = self
                .neo4j
                .execute(check_existing)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                .unwrap_or(0);
            if existing == 0 {
                let goal = format!(
                    "Investigate queue backlog: {} jobs are queued or parked — \
                     check if the coordinator is running, semaphores are free, \
                     and whether any jobs are stuck in a dependency cycle",
                    backlog_count
                );
                if self
                    .neo4j
                    .create_task(
                        &goal,
                        Some("Auto-generated by proactive perception scan (queue backlog)"),
                    )
                    .await
                    .is_ok()
                {
                    created += 1;
                    info!(
                        backlog = backlog_count,
                        "Perception scan created queue backlog task"
                    );
                }
            }
        }

        // Trigger 7: knowledge staleness — no semantic notes created or accessed
        // in the last 14 days means the knowledge base may be going cold.
        let staleness_check = neo4rs::query(
            "MATCH (n:Note) \
             WHERE n.note_type = 'semantic' \
               AND (n.last_accessed_at >= datetime() - duration({days: 14}) \
                    OR n.created_at >= datetime() - duration({days: 14})) \
             RETURN count(n) AS n",
        );
        let recent_semantic: i64 = self
            .neo4j
            .execute(staleness_check)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("n").ok()))
            .unwrap_or(1); // default to 1 → skip trigger on DB error

        if recent_semantic == 0 {
            let check_existing = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE toLower(t.goal) CONTAINS 'stale' \
                   AND t.status IN ['created', 'in_progress'] \
                 RETURN count(t) AS cnt",
            );
            let existing: i64 = self
                .neo4j
                .execute(check_existing)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                .unwrap_or(0);
            if existing == 0 {
                let goal = "Knowledge base is stale: no semantic notes created or accessed \
                            in the last 14 days — review recent episodic notes, run \
                            consolidation, and synthesize fresh semantic knowledge";
                if self
                    .neo4j
                    .create_task(
                        goal,
                        Some(
                            "Auto-generated by proactive perception scan \
                             (knowledge staleness)",
                        ),
                    )
                    .await
                    .is_ok()
                {
                    created += 1;
                    info!("Perception scan created knowledge staleness task");
                }
            }
        }

        Ok(created)
    }

    // =========================================================================
    // Goal → chain-step mapper
    // =========================================================================

    /// Look up a matching `SchedulerChain` node in Neo4j.
    /// Returns `Some(steps)` if a pattern match is found; `None` to fall through
    /// to the hardcoded heuristics.  Template vars `{{task_id}}`, `{{goal}}`,
    /// and `{{date}}` are substituted before deserialization.
    async fn try_load_chain_from_neo4j(&self, goal: &str, task_id: &str) -> Option<Vec<ChainStep>> {
        let cypher = "MATCH (c:SchedulerChain) \
                      WHERE toLower($goal) CONTAINS toLower(c.pattern) \
                      RETURN c.steps AS steps \
                      ORDER BY c.priority ASC \
                      LIMIT 1";
        let rows = self
            .neo4j
            .execute(neo4rs::query(cypher).param("goal", goal))
            .await
            .ok()?;
        let steps_json = rows.first()?.get::<String>("steps").ok()?;
        let date = Utc::now().format("%Y-%m-%d").to_string();
        let substituted = steps_json
            .replace("{{task_id}}", task_id)
            .replace("{{goal}}", goal)
            .replace("{{date}}", &date);
        match serde_json::from_str::<Vec<ChainStep>>(&substituted) {
            Ok(steps) => {
                info!(goal = %goal, steps = steps.len(), "Scheduler: routing via SchedulerChain from Neo4j");
                Some(steps)
            }
            Err(e) => {
                warn!(goal = %goal, error = %e, "SchedulerChain deserialization failed — falling back to heuristics");
                None
            }
        }
    }

    async fn goal_to_steps(&self, goal: &str, task_id: &str) -> Vec<ChainStep> {
        // Agent-defined routing chains take priority over hardcoded heuristics.
        if let Some(steps) = self.try_load_chain_from_neo4j(goal, task_id).await {
            return steps;
        }

        let g = goal.to_lowercase();

        // Note: recurring tasks (daily news, health monitor, weekly news) are handled by
        // ScheduledTask nodes dispatched in dispatch_scheduled_tasks(). Only reactive
        // one-off goals reach this function.
        // Recurring tasks are dispatched via ScheduledTask nodes.
        let mut steps = if g.contains("document") || g.contains("current state") {
            // Document / capture state: search knowledge, then consolidate
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "consolidate_memories".to_string(),
                    arguments: Some(json!({ "topic": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
            ]
        } else if g.contains("prioriti") || g.contains("roadmap") || g.contains("plan") {
            // Planning / prioritisation: search, reason, persist plan note
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "synthesize_knowledge".to_string(),
                    arguments: Some(json!({ "topic": goal, "limit": 8 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
            ]
        } else if g.contains("learn")
            || g.contains("research")
            || g.contains("study")
            || g.contains("understand")
        {
            // Learning / research: search existing knowledge, reason, persist new knowledge.
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "synthesize_knowledge".to_string(),
                    arguments: Some(json!({ "topic": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
            ]
        } else if g.contains("codebase")
            || g.contains("own structure")
            || g.contains("self-knowledge")
            || g.contains("self knowledge")
        {
            // Codebase self-analysis: generate overview, log history, reason over structure.
            // Triggered by perception_scan when no codebase note exists, or manually.
            vec![
                ChainStep {
                    tool_name: "analyze_own_structure".to_string(),
                    arguments: Some(json!({ "store_as_note": true })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "get_git_log".to_string(),
                    arguments: Some(json!({ "n": 10 })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({
                        "question": "Based on the codebase structure and recent commits, what are the key architectural patterns, current capabilities, and areas for improvement?",
                        "store_inference": true
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
            ]
        } else if g.contains("git history")
            || g.contains("recent changes")
            || g.contains("what changed")
            || g.contains("commit history")
        {
            // Git history analysis: fetch recent commits, diff, reason over changes.
            vec![
                ChainStep {
                    tool_name: "get_git_log".to_string(),
                    arguments: Some(json!({ "n": 20 })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "get_git_diff".to_string(),
                    arguments: Some(json!({ "from_ref": "HEAD~10" })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "synthesize_knowledge".to_string(),
                    arguments: Some(json!({ "topic": goal, "limit": 8 })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
            ]
        } else if g.contains("diagnos")
            || g.contains("root cause")
            || g.contains("repeated failure")
            || g.contains("dead job")
            || (g.contains("failure") && g.contains("analyz"))
        {
            // Diagnosis: find the affected code, gather failure notes, reason over root cause,
            // and persist a structured diagnosis note for human or future-agent review.
            vec![
                ChainStep {
                    tool_name: "search_codebase".to_string(),
                    arguments: Some(json!({ "query": goal, "max_results": 10, "context_lines": 2 })),
                    priority: Some(2),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: Some("Search codebase for relevant code".to_string()),
                },
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 8 })),
                    priority: Some(2),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: Some("Retrieve related failure and outcome notes".to_string()),
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({
                        "question": format!(
                            "{} — Based on the codebase search results and historical failure \
                             notes, identify the root cause. Structure your response as: \
                             DIAGNOSIS: <root cause> | AFFECTED_FILE: <path or unknown> | \
                             PROPOSED_FIX: <description of the fix> | \
                             SEVERITY: <low|medium|high>",
                            goal
                        ),
                        "store_inference": true
                    })),
                    priority: Some(2),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: Some("Reason over root cause and propose fix".to_string()),
                },
                ChainStep {
                    tool_name: "write_proposal".to_string(),
                    arguments: Some(json!({
                        "title": format!("Fix: {}", goal),
                        "task_id": task_id,
                        "diagnosis": "See inference note stored by previous reasoning step.",
                        "affected_file": "unknown",
                        "proposed_fix": "Review the stored inference note for the full diagnosis and proposed fix.",
                        "severity": "medium"
                    })),
                    priority: Some(2),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: Some("Stage proposal file for human review".to_string()),
                },
            ]
        } else if g.contains("review") || g.contains("analyz") || g.contains("source") {
            // Review / analysis: search context, reason over it, distill findings
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "synthesize_knowledge".to_string(),
                    arguments: Some(json!({ "topic": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
            ]
        } else {
            // Default: search context, reason, reflect, and distill semantic knowledge.
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "reflect_on_work".to_string(),
                    arguments: Some(json!({
                        "goal": goal,
                        "current_state": "Completed autonomous reasoning pass",
                        "task_id": task_id
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
                ChainStep {
                    tool_name: "synthesize_knowledge".to_string(),
                    arguments: Some(json!({ "topic": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                    ttl_secs: None,
                    description: None,
                },
            ]
        };

        // Always close the task when the chain finishes successfully.
        steps.push(ChainStep {
            tool_name: "update_task".to_string(),
            arguments: Some(json!({
                "task_id": task_id,
                "status": "completed",
                "note": format!("Task completed: {}", goal)
            })),
            priority: Some(1),
            max_attempts: Some(3),
            provider_hint: Some("ollama".to_string()),
            context_profile: None,
            ttl_secs: None,
            description: None,
        });

        steps
    }

    // =========================================================================
    // Public control API
    // =========================================================================

    /// Notify the scheduler that a tool call just arrived from a client.
    ///
    /// Resets the idle counter and sleep flag. If the scheduler was sleeping,
    /// interrupts the long sleep immediately so the next tick runs at `interval_secs`.
    pub async fn notify_activity(&self) {
        let now = chrono::Utc::now().to_rfc3339();
        let was_sleeping = {
            let mut st = self.state.write().await;
            let was = st.is_sleeping;
            st.idle_ticks = 0;
            st.is_sleeping = false;
            st.last_activity_at = Some(now);
            was
        };
        if was_sleeping {
            self.wakeup.notify_one();
            info!("Brain waking from sleep mode due to incoming tool call");
        }
    }

    /// Update scheduler configuration fields (all optional).
    ///
    /// If `interval_secs` is changed the background loop's current sleep is
    /// interrupted via the wakeup `Notify` so the new interval takes effect
    /// on the very next iteration (not after the old sleep expires).
    /// If `enabled` is set to `true` the consecutive-error counter is reset.
    #[allow(clippy::too_many_arguments)]
    pub async fn update_config(
        &self,
        interval_secs: Option<u64>,
        enabled: Option<bool>,
        max_tasks_per_run: Option<usize>,
        error_budget: Option<u32>,
        session_id: Option<Option<String>>,
        idle_sleep_after_ticks: Option<u32>,
        sleep_interval_secs: Option<u64>,
        local_model: Option<String>,
    ) -> SchedulerConfig {
        let interval_changed;
        let re_enabling;
        let result = {
            let mut cfg = self.config.write().await;
            interval_changed = interval_secs.is_some_and(|v| v != cfg.interval_secs);
            if let Some(v) = interval_secs {
                cfg.interval_secs = v;
            }
            re_enabling = enabled == Some(true);
            if let Some(v) = enabled {
                cfg.enabled = v;
            }
            if let Some(v) = max_tasks_per_run {
                cfg.max_tasks_per_run = v;
            }
            if let Some(v) = error_budget {
                cfg.error_budget = v;
            }
            if let Some(v) = session_id {
                cfg.session_id = v;
            }
            if let Some(v) = idle_sleep_after_ticks {
                cfg.idle_sleep_after_ticks = v;
            }
            if let Some(v) = sleep_interval_secs {
                cfg.sleep_interval_secs = v;
            }
            if let Some(ref v) = local_model {
                cfg.local_model = v.clone();
            }
            cfg.clone()
        };

        // Propagate local_model change into the shared local_config arc so that
        // SharedLlm picks up the new model without a restart.
        if let (Some(model), Some(cfg_arc)) = (local_model, &self.local_config) {
            let mut guard = cfg_arc.write().await;
            if let Some(ref mut lc) = *guard {
                lc.model = model.clone();
                info!(local_model = %model, "Updated scheduler local LLM model");
            } else {
                // local_config was None — seed it from defaults.
                let local_url = std::env::var("OLLAMA_LOCAL_URL")
                    .unwrap_or_else(|_| "http://localhost:11434".to_string());
                *guard = Some(
                    LlmConfig::default()
                        .with_provider(LlmProviderType::Ollama)
                        .with_base_url(local_url.clone())
                        .with_model(model.clone())
                        .with_embed_base_url(local_url),
                );
                info!(local_model = %model, "Seeded scheduler local LLM config");
            }
        }

        // Reset error counter when re-enabling (don't hold config guard across await).
        if re_enabling {
            self.state.write().await.consecutive_errors = 0;
        }

        // Wake the background loop so it picks up the new interval immediately.
        if interval_changed || re_enabling {
            self.wakeup.notify_one();
        }

        result
    }

    /// Return a JSON snapshot of current config + state.
    /// Delegate to `QueueService::stats()` so `SchedulerSkill` can expose queue
    /// depth and per-provider utilization without needing a direct queue reference.
    pub async fn queue_stats(&self) -> Value {
        self.queue.stats().await
    }

    /// Delegate to `QueueService::drain()` so the REST layer can drain the queue
    /// without needing a direct `QueueService` reference in `RestState`.
    pub async fn queue_drain(&self) -> Result<usize, String> {
        self.queue.drain().await
    }

    pub async fn status(&self) -> Value {
        let cfg = self.config.read().await.clone();
        let st = self.state.read().await.clone();
        json!({
            "config": {
                "interval_secs": cfg.interval_secs,
                "enabled": cfg.enabled,
                "max_tasks_per_run": cfg.max_tasks_per_run,
                "error_budget": cfg.error_budget,
                "session_id": cfg.session_id,
                "idle_sleep_after_ticks": cfg.idle_sleep_after_ticks,
                "sleep_interval_secs": cfg.sleep_interval_secs,
                "local_model": cfg.local_model,
            },
            "state": {
                "tasks_dispatched": st.tasks_dispatched,
                "consecutive_errors": st.consecutive_errors,
                "last_run_at": st.last_run_at,
                "last_error": st.last_error,
                "is_running": st.is_running,
                "idle_ticks": st.idle_ticks,
                "is_sleeping": st.is_sleeping,
                "last_activity_at": st.last_activity_at,
            }
        })
    }

    /// Signal the background loop to stop cleanly.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.wakeup.notify_one();
    }

    /// Execute a tick immediately (synchronous, bypasses the timer).
    /// Updates state counters the same way the background loop does.
    pub async fn run_tick(&self) -> Result<TickResult, String> {
        let now = Utc::now().to_rfc3339();
        match self.do_tick().await {
            Ok(result) => {
                let mut st = self.state.write().await;
                st.tasks_dispatched += result.tasks_dispatched as u64;
                st.last_run_at = Some(now);
                st.last_error = None;
                Ok(result)
            }
            Err(e) => {
                let error_budget = self.config.read().await.error_budget;
                let consecutive = {
                    let mut st = self.state.write().await;
                    st.consecutive_errors += 1;
                    st.last_error = Some(e.clone());
                    st.last_run_at = Some(now);
                    st.consecutive_errors
                };
                if consecutive >= error_budget {
                    self.config.write().await.enabled = false;
                    warn!(
                        consecutive = consecutive,
                        "Scheduler auto-paused after exhausting error budget (via run_tick)"
                    );
                }
                Err(e)
            }
        }
    }
}
