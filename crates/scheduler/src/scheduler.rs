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

use agent_brain_llm::LlmConfig;
use agent_brain_models::TaskStatus;
use agent_brain_repository::{Neo4jClient, Workflow};
use agent_brain_work::context_builder::ContextBuilderService;
use agent_brain_work::queue::{ChainStep, QueueService};

/// The `context` of the Task a firing creates: `trigger` is `schedule` or `run now`.
pub fn run_context(name: &str, trigger: &str) -> String {
    format!("Run of workflow '{name}' ({trigger})")
}

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
    /// When the loop's current sleep ends: the next tick is due by then (a
    /// wakeup can bring it sooner). Lets a fresh process say when its first
    /// tick comes instead of "never".
    pub next_tick_at: Option<String>,
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
    /// The job queue this scheduler dispatches into (the item dispatcher uses it too).
    pub fn queue(&self) -> Arc<QueueService> {
        Arc::clone(&self.queue)
    }

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
        self.restore_last_tick().await;
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

            self.state.write().await.next_tick_at =
                Some((Utc::now() + chrono::Duration::seconds(interval_secs as i64)).to_rfc3339());
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

            // Bookkeeping, logging and the error budget are run_tick's.
            let _ = self.run_tick().await;
        }

        info!("SchedulerService loop exited");
    }

    /// Record a finished tick on the singleton `(:SchedulerState)`, so the
    /// last tick survives a restart. Best-effort: a failed write only costs
    /// the display.
    async fn persist_tick(&self, at: &str, error: Option<&str>) {
        let q = neo4rs::query(
            "MERGE (s:SchedulerState {id: 'scheduler'}) \
             SET s.last_run_at = datetime($at), s.last_error = $error",
        )
        .param("at", at)
        .param("error", error.map(str::to_string));
        if let Err(e) = self.neo4j.run(q).await {
            debug!(error = %e, "scheduler: last tick not persisted");
        }
    }

    /// Before the first sleep: the last tick of an earlier process, so the
    /// HBI shows when the scheduler last ran instead of "never" for the
    /// first interval after every restart.
    async fn restore_last_tick(&self) {
        let q = neo4rs::query(
            "MATCH (s:SchedulerState {id: 'scheduler'}) \
             RETURN toString(s.last_run_at) AS at, s.last_error AS error",
        );
        let Ok(rows) = self.neo4j.execute(q).await else {
            return;
        };
        if let Some(r) = rows.first() {
            let mut st = self.state.write().await;
            st.last_run_at = r.get::<String>("at").ok();
            st.last_error = r.get::<String>("error").ok();
        }
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

        // Fire due scheduled workflows before processing created runs.
        let scheduled_dispatched = self.dispatch_due_workflows().await;
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
            let success_criteria = task["success_criteria"].as_str();
            let task_context = task["context"].as_str();

            if task_id.is_empty() || goal.is_empty() {
                continue;
            }

            // A run names its workflow; a free-text Task runs the default one.
            let workflow = task["workflow"].as_str();
            let inputs = task["inputs"].as_object().cloned().unwrap_or_default();
            let wf =
                match agent_brain_work::workflows::resolve(&self.neo4j, workflow, &inputs).await {
                    Ok(wf) => wf,
                    Err(reason) => {
                        // Fail it visibly rather than leave it `created` forever.
                        warn!(task_id = %task_id, reason = %reason, "Run refused");
                        let _ = self.neo4j.fail_task_with_reason(&task_id, &reason).await;
                        continue;
                    }
                };
            match self
                .dispatch_run(
                    &wf,
                    &task_id,
                    &goal,
                    &inputs,
                    success_criteria,
                    task_context,
                    session_id.as_deref(),
                )
                .await
            {
                Ok(jobs) => {
                    tasks_dispatched += 1;
                    info!(task_id = %task_id, workflow = %wf.name, jobs, goal = %goal, "Scheduler dispatched run");
                    // A first-person log of what it decided to work on and when.
                    let note = format!(
                        "Scheduler dispatched task (id: {task_id}) as workflow '{}': {goal}\n\
                         Chain length: {jobs} jobs",
                        wf.name
                    );
                    if let Err(e) = self
                        .neo4j
                        .store_episodic_note(&note, Some("scheduler_dispatch"))
                        .await
                    {
                        warn!(task_id = %task_id, error = %e, "Failed to store scheduler episodic note");
                    }
                }
                Err(e) => {
                    warn!(task_id = %task_id, error = %e, "Failed to enqueue run");
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
    // Workflow dispatch
    // -------------------------------------------------------------------------

    /// Fire every enabled workflow whose schedule is due. Returns how many fired.
    async fn dispatch_due_workflows(&self) -> usize {
        let due = match self.neo4j.get_due_workflows().await {
            Ok(v) => v,
            Err(e) => {
                warn!(error = %e, "Failed to query due workflows");
                return 0;
            }
        };
        let session_id = self.config.read().await.session_id.clone();
        let mut dispatched = 0usize;
        for wf in &due {
            match self
                .fire_workflow(
                    wf,
                    &serde_json::Map::new(),
                    "schedule",
                    session_id.as_deref(),
                )
                .await
            {
                Ok(_) => dispatched += 1,
                Err(e) => {
                    warn!(workflow = %wf.name, error = %e, "Failed to fire scheduled workflow")
                }
            }
        }
        dispatched
    }

    /// Run a workflow now with `inputs`, whatever its `next_run_at`, even when
    /// it is disabled (a manual run is a one-off), but never when it is
    /// unavailable (its steps would fail). A scheduled workflow's
    /// `next_run_at` advances like a timed run. Returns the run Task id.
    pub async fn run_workflow_now(
        &self,
        id: &str,
        inputs: &serde_json::Map<String, Value>,
    ) -> Result<String, String> {
        let wf = self
            .neo4j
            .get_workflow(id)
            .await
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("No workflow with id {id}"))?;
        if let Some(why) = wf.unavailable.as_deref() {
            return Err(format!("Workflow '{}' cannot run: {why}.", wf.name));
        }
        let missing = wf.missing_inputs(inputs);
        if !missing.is_empty() {
            return Err(format!(
                "Workflow '{}' needs input {}.",
                wf.name,
                missing.join(", ")
            ));
        }
        let session_id = self.config.read().await.session_id.clone();
        self.fire_workflow(&wf, inputs, "run now", session_id.as_deref())
            .await
    }

    /// One firing: a run Task, the timers advanced, the chain enqueued.
    async fn fire_workflow(
        &self,
        wf: &Workflow,
        inputs: &serde_json::Map<String, Value>,
        trigger: &str,
        session_id: Option<&str>,
    ) -> Result<String, String> {
        let goal = agent_brain_work::workflows::default_goal(&wf.name, inputs);
        let inputs_v = Value::Object(inputs.clone());
        let task_id = self
            .neo4j
            .create_run(
                &goal,
                Some(&run_context(&wf.name, trigger)),
                None,
                Some(&wf.name),
                Some(&inputs_v),
                "created",
            )
            .await
            .map_err(|e| e.to_string())?;
        // Advance the timers BEFORE enqueuing so a concurrent tick that reads
        // the graph in between cannot fire it twice.
        if let Some(interval) = wf.interval_seconds {
            let now = Utc::now().to_rfc3339();
            let next = Neo4jClient::compute_next_run_at(interval);
            if let Err(e) = self.neo4j.record_workflow_run(&wf.id, &now, &next).await {
                warn!(workflow = %wf.name, error = %e, "Failed to record workflow run timestamps");
            }
        }
        let jobs = self
            .dispatch_run(wf, &task_id, &goal, inputs, None, None, session_id)
            .await?;
        info!(workflow = %wf.name, task_id = %task_id, jobs, "Scheduled workflow fired");
        Ok(task_id)
    }

    /// Enqueue one run of `wf` as a job chain owned by `task_id`, and mark the
    /// Task in progress. Adds the opted-in checks, the confidence guardrail, a
    /// context profile, and a closing `update_task`. Returns the job count.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch_run(
        &self,
        wf: &Workflow,
        task_id: &str,
        goal: &str,
        inputs: &serde_json::Map<String, Value>,
        success_criteria: Option<&str>,
        task_context: Option<&str>,
        session_id: Option<&str>,
    ) -> Result<usize, String> {
        let mut steps = agent_brain_work::workflows::render_steps(wf, task_id, goal, inputs)?;

        // Opt-in checks; each needs something to check against.
        let rubric = wf
            .evaluation_rubric
            .as_deref()
            .filter(|s| !s.is_empty())
            .or(success_criteria);
        if wf.checks.iter().any(|c| c == "evaluator") {
            steps = self.maybe_append_evaluator(steps, task_id, rubric);
        }
        if wf.checks.iter().any(|c| c == "critic") {
            steps = Self::maybe_prepend_adversarial(steps, task_id, goal, rubric, task_context);
        }

        // Guardrail: a first step with a confidence threshold the chain falls
        // below is replaced by a diagnosis chain.
        if let Some(threshold) = steps.first().and_then(|s| s.confidence_threshold) {
            let score = Self::estimate_chain_confidence(goal, &steps);
            if score < threshold {
                warn!(
                    task_id,
                    goal,
                    confidence = score,
                    threshold,
                    "Guardrail triggered — replacing chain with diagnosis chain"
                );
                steps = Self::build_diagnosis_chain(goal, task_id);
            }
        }

        // ANNOTATION ONLY for background jobs: the coordinator never reads
        // job.context_profile (it runs each step's fixed tool). Profiles take
        // effect on /chat turns and MCP tools/list only.
        if let Some(cb) = &self.context_builder {
            let profile = cb.auto_assign(goal).await;
            for step in &mut steps {
                if step.context_profile.is_none() {
                    step.context_profile = Some(profile.clone());
                }
            }
        }

        // Close the run, unless the workflow closes it itself (some do so
        // before a final cleanup step). No `note`: a fixed "completed" string
        // per run floods the graph with boilerplate; the status change is the record.
        let closes_itself = steps.iter().any(|s| {
            s.tool_name == "update_task"
                && s.arguments.as_ref().and_then(|a| a["task_id"].as_str()) == Some(task_id)
        });
        if !closes_itself {
            steps.push(ChainStep {
                tool_name: "update_task".to_string(),
                arguments: Some(json!({ "task_id": task_id, "status": "completed" })),
                priority: Some(1),
                max_attempts: Some(3),
                provider_hint: Some("ollama".to_string()),
                ..Default::default()
            });
        }

        // Tagged with the run Task id so a chain death fails that Task with the real error.
        let ids = self
            .queue
            .enqueue_chain_owned(&steps, session_id, Some(task_id))
            .await
            .map_err(|e| e.to_string())?;
        // Mark in_progress at once so the next tick cannot dispatch it again.
        if let Err(e) = self
            .neo4j
            .update_task_status(task_id, TaskStatus::InProgress)
            .await
        {
            warn!(task_id, error = %e, "Failed to mark run in_progress");
        }
        Ok(ids.len())
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

        // Housekeeping: drop old completed/cancelled tasks so the Task label
        // stays small (the scheduler scans it every tick). Runs at most once
        // per sleep entry, gated by the same 6-hour cooldown above.
        let retention_days: u32 = std::env::var("TASK_RETENTION_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(30);
        if let Err(e) = self.neo4j.delete_old_completed_tasks(retention_days).await {
            warn!("Old-task cleanup on sleep entry failed: {}", e);
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
                ..Default::default()
            },
            ChainStep {
                tool_name: "prune_old_notes".to_string(),
                arguments: Some(json!({
                    "dry_run": false,
                    "days_stale": 60,
                    "min_accesses": 1,
                    "min_retain": 75,
                    "max_pct": 0.20
                })),
                priority: Some(0),
                max_attempts: Some(2),
                provider_hint: Some("ollama".to_string()),
                context_profile: None,
                ttl_secs: None,
                description: None,
                ..Default::default()
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
                ..Default::default()
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

    /// Scan the graph for conditions that need attention and create a Task for
    /// each — at most one per condition while an earlier one is still open.
    /// Returns how many tasks were created.
    async fn perception_scan(&self) -> Result<usize, String> {
        let mut created = self.trigger_repeated_tool_failures().await?;

        // Consolidation triggers share one guard, evaluated once, so a task
        // created by the first blocks the second within the same tick.
        let mut consolidation_queued = self.open_consolidation_exists().await;
        if !consolidation_queued && self.trigger_overdue_consolidation().await {
            created += 1;
            consolidation_queued = true;
        }
        if !consolidation_queued && self.trigger_episodic_consolidation().await {
            created += 1;
        }

        // Trigger 3 (routing coverage gap) was REMOVED 2026-08-17. It fired a
        // "Review routing effectiveness" task whenever >=5 background jobs used the
        // 'general'/NULL context profile in 7 days. Three defects made it a pure
        // hallucination generator:
        //   1. The premise is void: a job's context_profile is stamped for annotation
        //      only and is NEVER read at execution (the coordinator calls the step's one
        //      fixed tool regardless), so 'general' fallback on a background job costs
        //      nothing and cannot be a "coverage gap" to fix.
        //   2. The metric was an absolute count of 5, i.e. ~0.6% of weekly volume — a
        //      healthy default fallback, permanently above threshold.
        //   3. The dedup guard only blocked while a routing task was open, so the loop
        //      re-created itself every ~10 min. Over Aug 14-17 it produced 132 tasks and
        //      132 semantic notes of generic advice about a non-problem (5% of all
        //      semantic notes in the window), each relayed on retrieval as established fact.
        // A meaningful routing audit would measure /chat routing (where profiles DO apply),
        // not background jobs — build that deliberately if wanted; do not restore this.

        created += usize::from(self.trigger_codebase_self_knowledge().await);
        created += self.trigger_dead_jobs().await;
        created += usize::from(self.trigger_queue_backlog().await);
        created += usize::from(self.trigger_knowledge_staleness().await);
        Ok(created)
    }

    /// The first row's integer column `col`, or `None` on any error.
    async fn count_query(&self, q: neo4rs::Query, col: &str) -> Option<i64> {
        self.neo4j
            .execute(q)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>(col).ok()))
    }

    /// Create a perception-scan task; true when it was created.
    /// Start a run of `workflow` (the default one when `None`) for a
    /// perception-scan finding.
    async fn create_scan_task(&self, workflow: Option<&str>, goal: &str, context: &str) -> bool {
        match agent_brain_work::workflows::start_run(
            &self.neo4j,
            workflow,
            &serde_json::Map::new(),
            Some(goal),
            Some(context),
            None,
        )
        .await
        {
            Ok(_) => true,
            Err(e) => {
                warn!(goal, error = %e, "Perception scan failed to create task");
                false
            }
        }
    }

    /// The same tool failing ≥3 times in 7 days, per its outcome notes, gets a
    /// diagnosis run.
    async fn trigger_repeated_tool_failures(&self) -> Result<usize, String> {
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

        let tool_failures =
            count_tool_failures(rows.iter().filter_map(|r| r.get::<String>("content").ok()));

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
            if self.count_query(check, "cnt").await.unwrap_or(0) > 0 {
                continue;
            }
            let goal = format!(
                "Analyze repeated failures for '{tool}' — hypothesize root cause, \
                 propose a concrete test, and integrate findings into permanent knowledge \
                 ({count} failures in 7 days)"
            );
            let context = serde_json::json!({
                "meta_learning_phase": "analyze",
                "failure_tool": tool,
                "failure_count": count,
                "cycle": 1
            })
            .to_string();
            if self
                .create_scan_task(Some("diagnose"), &goal, &context)
                .await
            {
                created += 1;
                info!(tool = %tool, failures = count, "Perception scan started a diagnose run");
            }
        }
        Ok(created)
    }

    /// Whether a consolidation task is active or completed within 24h.
    async fn open_consolidation_exists(&self) -> bool {
        let q = neo4rs::query(
            "MATCH (t:Task) \
             WHERE toLower(t.goal) CONTAINS 'consolidat' \
               AND (t.status IN ['created', 'in_progress'] \
                 OR (t.status = 'completed' \
                     AND t.created_at >= datetime() - duration({hours: 24}))) \
             RETURN count(t) AS cnt",
        );
        // Default to "exists" on any DB error so a transient Neo4j failure never
        // causes a duplicate task — the safe direction is to skip.
        self.count_query(q, "cnt").await.unwrap_or(1) > 0
    }

    /// Many overdue spaced-repetition notes → consolidate them.
    ///
    /// Counts AND bumps with `CONSOLIDATION_SOURCE_PREDICATE`, the predicate
    /// consolidation selects its sources by: a type counted here but refused as
    /// a source stays overdue forever and re-triggers tasks that can never
    /// consume it (reflection notes drove ~10 consolidations/day). Until
    /// 2026-10-02 the bump used its own hard-coded type list, which pushed back
    /// review dates on notes the count had never included.
    async fn trigger_overdue_consolidation(&self) -> bool {
        let predicate = agent_brain_knowledge::knowledge::CONSOLIDATION_SOURCE_PREDICATE;
        let due_check = neo4rs::query(&format!(
            "MATCH (n:Note) WHERE n.next_review_at <= datetime() AND {predicate} \
             RETURN count(n) AS cnt"
        ));
        let due_count = self.count_query(due_check, "cnt").await.unwrap_or(0);

        // Raised 10 → 25 → 50: lower values were hit by normal note accumulation,
        // so consolidation ran too often.
        if due_count < 50 {
            return false;
        }
        let goal = format!(
            "Consolidate {due_count} overdue spaced-repetition notes into long-term memory"
        );
        if !self
            .create_scan_task(
                Some("consolidate"),
                &goal,
                "Auto-generated by proactive perception scan (spaced-repetition backlog)",
            )
            .await
        {
            return false;
        }

        // Advance the counted notes now so later ticks don't see them as still
        // overdue and queue another task before this one runs. The consolidation
        // job extends the dates further for the notes it actually merges.
        let bump = neo4rs::query(&format!(
            "MATCH (n:Note) WHERE n.next_review_at <= datetime() AND {predicate} \
             SET n.next_review_at = datetime() + duration({{days: 14}}), \
                 n.review_interval_days = CASE \
                     WHEN COALESCE(n.review_interval_days, 1) < 14 THEN 14 \
                     ELSE COALESCE(n.review_interval_days, 14) \
                 END"
        ));
        if let Err(e) = self.neo4j.run(bump).await {
            warn!("Failed to advance overdue note dates after scheduling consolidation: {e}");
        }
        info!(
            due_count,
            "Perception scan created consolidation task (overdue notes)"
        );
        true
    }

    /// High episodic note volume → consolidate (the sleep-cycle analogue).
    /// Counts only notes currently due, so the post-creation bump suppresses
    /// re-firing.
    async fn trigger_episodic_consolidation(&self) -> bool {
        let episodic_check = neo4rs::query(
            "MATCH (n:Note) \
             WHERE n.note_type = 'episodic' AND n.next_review_at <= datetime() \
             RETURN count(n) AS cnt",
        );
        let episodic_count = self.count_query(episodic_check, "cnt").await.unwrap_or(0);
        if episodic_count < 75 {
            return false;
        }
        let goal = format!(
            "Consolidate {episodic_count} episodic notes — distil recurring patterns into semantic memory"
        );
        if !self
            .create_scan_task(
                Some("consolidate"),
                &goal,
                "Auto-generated by proactive perception scan (episodic note volume threshold)",
            )
            .await
        {
            return false;
        }
        let bump = neo4rs::query(
            "MATCH (n:Note) \
             WHERE n.note_type = 'episodic' AND n.next_review_at <= datetime() \
             SET n.next_review_at = datetime() + duration({days: 14}), \
                 n.review_interval_days = 14",
        );
        if let Err(e) = self.neo4j.run(bump).await {
            warn!("Failed to advance episodic note dates after scheduling consolidation: {e}");
        }
        info!(
            episodic_count,
            "Perception scan created consolidation task (episodic volume)"
        );
        true
    }

    /// Codebase self-knowledge: bootstrap if none exists, refresh if >7 days old.
    async fn trigger_codebase_self_knowledge(&self) -> bool {
        let codebase_check = neo4rs::query(
            "MATCH (n:Note) \
             WHERE n.source_context = 'codebase_self_analysis' \
             RETURN count(n) AS cnt, \
                    toString(max(n.created_at)) AS newest",
        );
        let rows = self.neo4j.execute(codebase_check).await.ok();
        let first = rows.as_ref().and_then(|rows| rows.first());
        let note_count = first.and_then(|r| r.get::<i64>("cnt").ok()).unwrap_or(0);
        // `newest` is absent when there are no notes; missing or unparseable
        // counts as stale.
        let is_stale = first
            .and_then(|r| r.get::<String>("newest").ok())
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(&s).ok())
            .map(|dt| (Utc::now() - dt.to_utc()).num_days() >= 7)
            .unwrap_or(true);
        if note_count > 0 && !is_stale {
            return false;
        }

        let check_existing = neo4rs::query(
            "MATCH (t:Task) \
             WHERE (t.goal CONTAINS 'codebase' OR t.goal CONTAINS 'self-knowledge') \
               AND t.status IN ['created', 'in_progress'] \
             RETURN count(t) AS cnt",
        );
        if self.count_query(check_existing, "cnt").await.unwrap_or(0) > 0 {
            return false;
        }
        let (goal, context) = if note_count == 0 {
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
        let created = self.create_scan_task(Some("codebase"), goal, context).await;
        if created {
            info!(
                stale = is_stale,
                count = note_count,
                "Perception scan created codebase self-analysis task"
            );
        }
        created
    }

    /// The same tool dying ≥3 times in 24h signals a broken tool definition or
    /// a bad external dependency.
    async fn trigger_dead_jobs(&self) -> usize {
        let dead_jobs_check = neo4rs::query(
            "MATCH (j:AgentJob) \
             WHERE j.status = 'dead' \
               AND j.updated_at >= datetime() - duration({hours: 24}) \
             RETURN j.tool_name AS tool_name, count(j) AS n \
             ORDER BY n DESC",
        );
        let Ok(dead_rows) = self.neo4j.execute(dead_jobs_check).await else {
            return 0;
        };
        let mut created = 0usize;
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
            if self.count_query(check_existing, "cnt").await.unwrap_or(0) > 0 {
                continue;
            }
            let goal = format!(
                "Investigate dead jobs: '{tool_name}' died {n} times in the last 24 hours — \
                 diagnose the tool definition, input schema, or external dependency"
            );
            if self
                .create_scan_task(
                    Some("diagnose"),
                    &goal,
                    "Auto-generated by proactive perception scan (dead job accumulation)",
                )
                .await
            {
                created += 1;
                info!(tool = %tool_name, dead_count = n, "Perception scan created dead-job investigation task");
            }
        }
        created
    }

    /// Many jobs stuck *queued* with nothing running: the coordinator may be
    /// blocked or a semaphore exhausted. Parked jobs are deliberately excluded —
    /// parked is the normal state for chain steps 2..N awaiting their parent, so
    /// counting them fired on healthy busy periods (153 duplicate tasks).
    async fn trigger_queue_backlog(&self) -> bool {
        let backlog_check = neo4rs::query(
            "MATCH (j:AgentJob {status: 'queued'}) \
             WITH count(j) AS queued \
             OPTIONAL MATCH (r:AgentJob {status: 'running'}) \
             RETURN queued, count(r) AS running",
        );
        let (backlog, running) = self
            .neo4j
            .execute(backlog_check)
            .await
            .ok()
            .and_then(|rows| {
                rows.first().map(|r| {
                    (
                        r.get::<i64>("queued").unwrap_or(0),
                        r.get::<i64>("running").unwrap_or(0),
                    )
                })
            })
            .unwrap_or((0, 0));
        if backlog < 20 || running > 0 {
            return false;
        }

        // Dedup against open investigations AND a 24h cooldown on finished ones:
        // investigating cannot itself drain the queue, so re-creating the task
        // every tick while the backlog persists only multiplies noise. The
        // cooldown compared the DATETIME `created_at` to a string until
        // 2026-10-02 — always null, so it never applied.
        let check_existing = neo4rs::query(
            "MATCH (t:Task) \
             WHERE toLower(t.goal) CONTAINS 'queue' \
               AND toLower(t.goal) CONTAINS 'backlog' \
               AND (t.status IN ['created', 'in_progress'] \
                    OR t.created_at >= datetime() - duration({hours: 24})) \
             RETURN count(t) AS cnt",
        );
        if self.count_query(check_existing, "cnt").await.unwrap_or(0) > 0 {
            return false;
        }
        let goal = format!(
            "Investigate queue backlog: {backlog} jobs are queued with none running — \
             check if the coordinator is running, semaphores are free, \
             and whether any jobs are stuck in a dependency cycle"
        );
        let created = self
            .create_scan_task(
                None,
                &goal,
                "Auto-generated by proactive perception scan (queue backlog)",
            )
            .await;
        if created {
            info!(backlog, "Perception scan created queue backlog task");
        }
        created
    }

    /// No semantic note created or accessed in 14 days: the knowledge base may
    /// be going cold.
    async fn trigger_knowledge_staleness(&self) -> bool {
        let staleness_check = neo4rs::query(
            "MATCH (n:Note) \
             WHERE n.note_type = 'semantic' \
               AND (n.last_accessed_at >= datetime() - duration({days: 14}) \
                    OR n.created_at >= datetime() - duration({days: 14})) \
             RETURN count(n) AS n",
        );
        // Default to 1 → skip the trigger on a DB error.
        if self.count_query(staleness_check, "n").await.unwrap_or(1) > 0 {
            return false;
        }
        let check_existing = neo4rs::query(
            "MATCH (t:Task) \
             WHERE toLower(t.goal) CONTAINS 'stale' \
               AND t.status IN ['created', 'in_progress'] \
             RETURN count(t) AS cnt",
        );
        if self.count_query(check_existing, "cnt").await.unwrap_or(0) > 0 {
            return false;
        }
        let created = self
            .create_scan_task(
                Some("consolidate"),
                "Knowledge base is stale: no semantic notes created or accessed \
                 in the last 14 days — review recent episodic notes, run \
                 consolidation, and synthesize fresh semantic knowledge",
                "Auto-generated by proactive perception scan (knowledge staleness)",
            )
            .await;
        if created {
            info!("Perception scan created knowledge staleness task");
        }
        created
    }

    // =========================================================================
    // Goal → chain-step mapper
    // =========================================================================

    /// If `criteria` is set, append an evaluator step that calls `reflect_on_work`
    /// and triggers a re-queue if the score falls below the threshold.
    fn maybe_append_evaluator(
        &self,
        mut steps: Vec<ChainStep>,
        task_id: &str,
        criteria: Option<&str>,
    ) -> Vec<ChainStep> {
        if let Some(c) = criteria {
            // Insert the evaluator BEFORE the trailing update_task step so that a
            // failed score cancels update_task (keeping the task in_progress) rather
            // than firing after the task is already marked completed.
            let update_task_step = steps
                .last()
                .filter(|s| s.tool_name == "update_task")
                .cloned();
            if update_task_step.is_some() {
                steps.pop();
            }
            steps.push(ChainStep {
                tool_name: "reflect_on_work".to_string(),
                arguments: Some(json!({
                    "goal": c,
                    "current_state": "{{_prev}}",
                    "task_id": task_id
                })),
                priority: Some(1),
                max_attempts: Some(2),
                provider_hint: Some("ollama".to_string()),
                description: Some("Evaluate output against success criteria".to_string()),
                is_evaluator: true,
                min_score: Some(3.5),
                evaluator_task_id: Some(task_id.to_string()),
                ..Default::default()
            });
            if let Some(ut) = update_task_step {
                steps.push(ut);
            }
        }
        steps
    }

    /// Prepend an `adversarial_plan_review` step to the chain when the task is
    /// high-stakes (has `success_criteria`) and hasn't already been through an
    /// adversarial abort cycle.
    ///
    /// Skipped when:
    /// - `criteria` is `None` (no success criteria → not high-stakes)
    /// - `no_adversarial` is `true` at the chain YAML level
    /// - `task_context` already contains `"ADVERSARIAL ABORT"` (retry from prior abort)
    fn maybe_prepend_adversarial(
        mut steps: Vec<ChainStep>,
        task_id: &str,
        goal: &str,
        criteria: Option<&str>,
        task_context: Option<&str>,
    ) -> Vec<ChainStep> {
        if criteria.is_none() {
            return steps;
        }
        if task_context
            .map(|c| c.contains("ADVERSARIAL ABORT"))
            .unwrap_or(false)
        {
            return steps;
        }

        // Build a compact plan description from step descriptions for the LLM prompt.
        let plan_description = steps
            .iter()
            .filter_map(|s| s.description.as_deref().or(Some(s.tool_name.as_str())))
            .collect::<Vec<_>>()
            .join(" → ");

        steps.insert(
            0,
            ChainStep {
                tool_name: "adversarial_plan_review".to_string(),
                arguments: Some(json!({
                    "goal": goal,
                    "proposed_plan": plan_description,
                    "n_hypotheses": 3,
                    "task_id": task_id
                })),
                priority: Some(1),
                max_attempts: Some(2),
                provider_hint: Some("ollama".to_string()),
                description: Some(
                    "Adversarial pre-flight: stress-test plan before execution".to_string(),
                ),
                is_adversarial: true,
                min_robustness: Some(2.5),
                adversarial_task_id: Some(task_id.to_string()),
                ..Default::default()
            },
        );
        steps
    }

    // =========================================================================
    // Guardrail Protocol
    // =========================================================================

    /// Estimate how confident the scheduler is that a chain will succeed (0.0–1.0).
    ///
    /// Pure heuristic — no LLM call, no I/O.  Used by the guardrail check in
    /// `do_tick()` before a chain is dispatched.
    fn estimate_chain_confidence(goal: &str, steps: &[ChainStep]) -> f32 {
        let g = goal.to_lowercase();

        // Goals that require open-ended investigation are inherently uncertain.
        let complex_keywords = [
            "diagnos",
            "root cause",
            "investigate",
            "debug",
            "hypothes",
            "meta-learn",
            "repeated failure",
            "dead job",
        ];
        if complex_keywords.iter().any(|kw| g.contains(kw)) {
            return 0.40;
        }

        // Long chains have more steps that can fail.
        if steps.len() > 5 {
            return 0.45;
        }
        if steps.len() > 3 {
            return 0.65;
        }

        0.85
    }

    /// Build a lightweight 3-step diagnosis chain for goals that fall below
    /// the confidence threshold.  The original chain is not executed — this
    /// gathers context and stores a reflection note for the next scheduler tick.
    fn build_diagnosis_chain(goal: &str, task_id: &str) -> Vec<ChainStep> {
        let question = format!(
            "What are the prerequisites, known risks, missing context, and potential failure modes \
             for this goal? Be specific and actionable.\n\nGOAL: {goal}"
        );
        vec![
            ChainStep {
                tool_name: "search_notes".to_string(),
                arguments: Some(json!({ "query": goal, "limit": 8 })),
                priority: Some(1),
                max_attempts: Some(3),
                provider_hint: Some("ollama".to_string()),
                description: Some("Guardrail: gather context before dispatch".to_string()),
                ..Default::default()
            },
            ChainStep {
                tool_name: "reason".to_string(),
                arguments: Some(json!({ "question": question, "store_inference": true })),
                priority: Some(1),
                max_attempts: Some(3),
                provider_hint: Some("ollama".to_string()),
                description: Some("Guardrail: assess prerequisites and risks".to_string()),
                ..Default::default()
            },
            ChainStep {
                tool_name: "update_task".to_string(),
                arguments: Some(json!({
                    "task_id": task_id,
                    "status": "blocked",
                    "note": "Guardrail halted dispatch: goal requires prerequisites assessment. \
                                    See inference note for details. Unblock manually once ready.".to_string()
                })),
                priority: Some(1),
                max_attempts: Some(3),
                provider_hint: Some("ollama".to_string()),
                description: Some("Guardrail: mark task blocked pending diagnosis".to_string()),
                ..Default::default()
            },
        ]
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
                // local_config was None — seed it from the configured background
                // backend (LOCAL_LLM_BACKEND), then apply the requested model.
                let mut seeded = agent_brain_llm::llm::build_local_llm_config();
                seeded.model = model.clone();
                *guard = Some(seeded);
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
                "next_tick_at": st.next_tick_at,
            }
        })
    }

    /// Signal the background loop to stop cleanly.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
        self.wakeup.notify_one();
    }

    /// Execute a tick now and record it: the background loop's ticks and
    /// `scheduler_control tick` both come through here, so both update the
    /// counters, persist the last tick and spend the error budget alike.
    pub async fn run_tick(&self) -> Result<TickResult, String> {
        self.state.write().await.is_running = true;
        let now = Utc::now().to_rfc3339();
        let outcome = self.do_tick().await;
        let error = outcome.as_ref().err().cloned();
        let consecutive = {
            let mut st = self.state.write().await;
            st.is_running = false;
            st.last_run_at = Some(now.clone());
            st.last_error = error.clone();
            match &outcome {
                Ok(result) => {
                    st.consecutive_errors = 0;
                    st.tasks_dispatched += result.tasks_dispatched as u64;
                }
                Err(_) => st.consecutive_errors += 1,
            }
            st.consecutive_errors
        };
        self.persist_tick(&now, error.as_deref()).await;
        match &outcome {
            Ok(result) => info!(
                tasks_found = result.tasks_found,
                dispatched = result.tasks_dispatched,
                skipped = result.skipped,
                new_tasks = result.new_tasks_created,
                "Scheduler tick completed"
            ),
            Err(e) => {
                warn!(error = %e, consecutive, "Scheduler tick failed");
                if consecutive >= self.config.read().await.error_budget {
                    self.config.write().await.enabled = false;
                    warn!(
                        consecutive,
                        "Scheduler auto-paused after exhausting error budget"
                    );
                }
            }
        }
        outcome
    }
}

/// Failures per tool from outcome-note contents, which read
/// `"Tool: <name> | Success: false\n..."`. Anything else is ignored.
fn count_tool_failures(contents: impl IntoIterator<Item = String>) -> HashMap<String, u32> {
    let mut failures: HashMap<String, u32> = HashMap::new();
    for content in contents {
        if let Some(rest) = content.strip_prefix("Tool: ")
            && let Some(tool_name) = rest.split(" | ").next()
        {
            *failures.entry(tool_name.trim().to_string()).or_insert(0) += 1;
        }
    }
    failures
}

#[cfg(test)]
mod count_tool_failures_tests {
    use super::*;

    #[test]
    fn counts_each_tool_and_ignores_other_content() {
        let failures = count_tool_failures(vec![
            "Tool: search_web | Success: false\nquota".to_string(),
            "Tool: search_web | Success: false\nquota".to_string(),
            "Tool: ingest_media | Success: false".to_string(),
            "unrelated outcome text".to_string(),
        ]);
        assert_eq!(failures.get("search_web"), Some(&2));
        assert_eq!(failures.get("ingest_media"), Some(&1));
        assert_eq!(failures.len(), 2);
    }
}

#[cfg(test)]
mod adversarial_prepend_tests {
    use super::*;

    fn step(tool: &str) -> ChainStep {
        ChainStep {
            tool_name: tool.into(),
            ..Default::default()
        }
    }

    /// The review is told which task it judges, so its note can be linked to
    /// that task (`REFLECTS_ON`) — the schema has always promised the link.
    #[test]
    fn the_prepended_review_names_its_task() {
        let steps = SchedulerService::maybe_prepend_adversarial(
            vec![step("search_web")],
            "task-123",
            "learn about X",
            Some("a cited summary exists"),
            None,
        );
        assert_eq!(steps[0].tool_name, "adversarial_plan_review");
        assert_eq!(steps[0].arguments.as_ref().unwrap()["task_id"], "task-123");
        assert_eq!(steps[0].adversarial_task_id.as_deref(), Some("task-123"));
    }

    #[test]
    fn no_success_criteria_means_no_review() {
        let steps = SchedulerService::maybe_prepend_adversarial(
            vec![step("search_web")],
            "task-123",
            "learn about X",
            None,
            None,
        );
        assert_eq!(steps.len(), 1);
    }
}
