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
use crate::repository::Neo4jClient;
use crate::services::context_builder::ContextBuilderService;
use crate::services::queue::{ChainStep, QueueService};

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
}

impl SchedulerService {
    /// Create and start the scheduler.
    ///
    /// Reads `SCHEDULER_INTERVAL_SECS` and `SCHEDULER_ENABLED` from the environment,
    /// then spawns the background loop immediately.
    pub fn new(neo4j: Neo4jClient, queue: Arc<QueueService>) -> Arc<Self> {
        Self::new_with_context(neo4j, queue, None)
    }

    /// Create and start the scheduler with an optional context builder for profile auto-assignment.
    pub fn new_with_context(
        neo4j: Neo4jClient,
        queue: Arc<QueueService>,
        context_builder: Option<Arc<ContextBuilderService>>,
    ) -> Arc<Self> {
        let svc = Arc::new(Self {
            neo4j,
            queue,
            config: Arc::new(RwLock::new(SchedulerConfig::default())),
            state: Arc::new(RwLock::new(SchedulerState::default())),
            shutdown: Arc::new(AtomicBool::new(false)),
            wakeup: Arc::new(Notify::new()),
            context_builder,
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

        let tasks = self
            .neo4j
            .list_tasks(Some("created"), 20)
            .await
            .map_err(|e| e.to_string())?;

        let tasks_found = tasks.len();
        let mut tasks_dispatched = 0usize;

        // Dedup: for self-scheduling task types, keep only the first 'created' instance per tick.
        // Mark extras as Completed so chains never multiply.
        let mut skip_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        {
            let mut health_seen = false;
            let mut daily_news_seen = false;
            let mut weekly_news_seen = false;
            for task in &tasks {
                let goal_lower = task["goal"].as_str().unwrap_or("").to_lowercase();
                let is_health = goal_lower.contains("health monitor") || goal_lower.contains("health check");
                let is_daily_news = goal_lower.contains("daily news")
                    || goal_lower.contains("news aggregation")
                    || goal_lower.contains("news briefing");
                let is_weekly_news = goal_lower.contains("weekly news")
                    || goal_lower.contains("weekly briefing");

                let is_dup = (is_health && health_seen)
                    || (is_daily_news && daily_news_seen)
                    || (is_weekly_news && weekly_news_seen);

                if is_dup {
                    let dup_id = task["id"].as_str().unwrap_or("").to_string();
                    if !dup_id.is_empty() {
                        if let Err(e) = self
                            .neo4j
                            .update_task_status(&dup_id, TaskStatus::Completed)
                            .await
                        {
                            warn!(task_id = %dup_id, error = %e, "Failed to dedup self-scheduling task");
                        } else {
                            warn!(task_id = %dup_id, goal = %goal_lower, "Deduped duplicate self-scheduling task");
                            skip_ids.insert(dup_id);
                        }
                    }
                } else {
                    if is_health       { health_seen       = true; }
                    if is_daily_news   { daily_news_seen   = true; }
                    if is_weekly_news  { weekly_news_seen  = true; }
                }
            }
        }

        // Cooldown: skip daily news if one completed within the last 20 hours (once-per-day cadence).
        // The in_progress check is time-bounded so stale stuck tasks don't permanently block runs.
        let recent_daily_news_exists = {
            let q = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE (t.goal CONTAINS 'daily news' \
                     OR t.goal CONTAINS 'news aggregation' \
                     OR t.goal CONTAINS 'news briefing') \
                   AND ((t.status = 'in_progress' \
                         AND t.updated_at >= datetime() - duration({hours: 20})) \
                     OR (t.status = 'completed' \
                         AND t.updated_at >= datetime() - duration({hours: 20}))) \
                 RETURN count(t) AS cnt",
            );
            self.neo4j
                .execute(q)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                .unwrap_or(0)
                > 0
        };

        // Cooldown: skip weekly news if one completed within the last 6 days.
        // The in_progress check is time-bounded so stale stuck tasks don't permanently block runs.
        let recent_weekly_news_exists = {
            let q = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE (t.goal CONTAINS 'weekly news' \
                     OR t.goal CONTAINS 'weekly briefing') \
                   AND ((t.status = 'in_progress' \
                         AND t.updated_at >= datetime() - duration({days: 6})) \
                     OR (t.status = 'completed' \
                         AND t.updated_at >= datetime() - duration({days: 6}))) \
                 RETURN count(t) AS cnt",
            );
            self.neo4j
                .execute(q)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                .unwrap_or(0)
                > 0
        };

        for task in tasks.iter().take(max_tasks) {
            let task_id = task["id"].as_str().unwrap_or("").to_string();
            let goal = task["goal"].as_str().unwrap_or("").to_string();

            if task_id.is_empty() || goal.is_empty() {
                continue;
            }

            if skip_ids.contains(&task_id) {
                continue;
            }

            let goal_lower = goal.to_lowercase();

            // Skip daily news tasks if one ran recently (20-hour cooldown).
            if recent_daily_news_exists
                && (goal_lower.contains("daily news")
                    || goal_lower.contains("news aggregation")
                    || goal_lower.contains("news briefing"))
            {
                debug!(task_id = %task_id, "Skipping daily news task — ran within last 20 hours");
                continue;
            }

            // Skip weekly news tasks if one ran recently (6-day cooldown).
            if recent_weekly_news_exists
                && (goal_lower.contains("weekly news")
                    || goal_lower.contains("weekly briefing"))
            {
                debug!(task_id = %task_id, "Skipping weekly news task — ran within last 6 days");
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

        let skipped = tasks_found - tasks_dispatched;

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

    /// Enqueue a low-priority bedtime chain: consolidate → prune → snapshot → store note.
    ///
    /// Called once when the scheduler transitions into sleep mode after `idle_sleep_after_ticks`
    /// consecutive idle ticks.
    async fn enter_sleep(&self) {
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
                provider_hint: None,
                context_profile: None,
            },
            ChainStep {
                tool_name: "prune_old_notes".to_string(),
                arguments: Some(json!({ "dry_run": false })),
                priority: Some(0),
                max_attempts: Some(2),
                provider_hint: None,
                context_profile: None,
            },
            ChainStep {
                tool_name: "snapshot_knowledge".to_string(),
                arguments: Some(json!({ "label": "sleep" })),
                priority: Some(0),
                max_attempts: Some(2),
                provider_hint: None,
                context_profile: None,
            },
            ChainStep {
                tool_name: "store_note".to_string(),
                arguments: Some(json!({
                    "content": format!(
                        "Brain entering sleep mode at {timestamp}. Idle cleanup complete."
                    ),
                    "note_type": "outcome"
                })),
                priority: Some(0),
                max_attempts: Some(2),
                provider_hint: None,
                context_profile: None,
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

        // Helper: check if a consolidation task is active OR was completed recently (within 2h).
        // The 2-hour cooldown prevents perception_scan from re-queuing consolidation every tick
        // when the previous run finishes in under 5 minutes.
        let open_consolidation_exists = || async {
            let q = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE t.goal CONTAINS 'consolidat' \
                   AND (t.status IN ['created', 'in_progress'] \
                     OR (t.status = 'completed' \
                         AND t.created_at >= datetime() - duration({hours: 2}))) \
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

        // Raised threshold from 10 → 25: 10 was too easily hit by normal note accumulation,
        // causing a new consolidation task every scheduler tick.
        if due_count >= 25 && !open_consolidation_exists().await {
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

        if episodic_count >= 75 && !open_consolidation_exists().await {
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

        // Trigger 4: daily news cycle self-healing — if no news note has been stored in the last
        // 26 hours (slightly beyond the 20-hour cooldown) and no daily news task is active or
        // recently completed, the cycle has broken (e.g. chain died, self-scheduling step was
        // cancelled).  Auto-create a new task so the cycle recovers without manual intervention.
        let news_cycle_check = neo4rs::query(
            "MATCH (n:Note) \
             WHERE n.note_type = 'news' \
               AND n.created_at >= datetime() - duration({hours: 26}) \
             RETURN count(n) AS cnt",
        );
        let recent_news_count: i64 = self
            .neo4j
            .execute(news_cycle_check)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
            .unwrap_or(1); // default to 1 so we don't fire if the query fails

        if recent_news_count == 0 {
            let check_active = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE (t.goal CONTAINS 'daily news' \
                     OR t.goal CONTAINS 'news aggregation' \
                     OR t.goal CONTAINS 'news briefing') \
                   AND t.status IN ['created', 'in_progress'] \
                 RETURN count(t) AS cnt",
            );
            let active: i64 = self
                .neo4j
                .execute(check_active)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                .unwrap_or(1);

            if active == 0 {
                let goal = "Daily news aggregation and briefing: aggregate headlines from world, tech, and business, then write and store a daily briefing";
                if self
                    .neo4j
                    .create_task(
                        goal,
                        Some(
                            "Auto-generated by perception scan: daily news cycle recovery \
                             (no news note in the last 26 hours and no active news task)",
                        ),
                    )
                    .await
                    .is_ok()
                {
                    created += 1;
                    info!("Perception scan created daily news recovery task (cycle was broken)");
                }
            }
        }

        // Trigger 5 (was 4): no codebase self-analysis note exists — bootstrap self-knowledge.
        // Only fires once (or after all codebase notes are pruned).
        let codebase_check = neo4rs::query(
            "MATCH (n:Note) \
             WHERE n.source_context = 'codebase_self_analysis' \
             RETURN count(n) AS cnt",
        );
        let codebase_note_count: i64 = self
            .neo4j
            .execute(codebase_check)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
            .unwrap_or(0);

        if codebase_note_count == 0 {
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
                let goal =
                    "Analyze own codebase structure and store self-knowledge in graph";
                if self
                    .neo4j
                    .create_task(
                        goal,
                        Some("Auto-generated: no codebase self-analysis note found in graph"),
                    )
                    .await
                    .is_ok()
                {
                    created += 1;
                    info!("Perception scan created codebase self-analysis task");
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
    async fn try_load_chain_from_neo4j(
        &self,
        goal: &str,
        task_id: &str,
    ) -> Option<Vec<ChainStep>> {
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

        // Health monitor is checked first — its goal contains "failure" and "review" which
        // would otherwise be swallowed by later generic branches.
        let mut steps = if g.contains("health monitor") || g.contains("health check") {
            // Recurring brain health monitor: gather metrics, reason, store snapshot, re-queue.
            let next_goal =
                "Brain health monitor: review scheduler state, queue metrics, and failure patterns";
            vec![
                ChainStep {
                    tool_name: "get_scheduler_status".to_string(),
                    arguments: Some(json!({})),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "queue_status".to_string(),
                    arguments: Some(json!({ "limit": 20 })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "list_tasks".to_string(),
                    arguments: Some(json!({ "status": "failed", "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "duckdb_query".to_string(),
                    arguments: Some(json!({
                        "sql": "SELECT model_name, \
                                       COUNT(*) AS total, \
                                       SUM(CASE WHEN success THEN 1 ELSE 0 END) AS successes, \
                                       SUM(CASE WHEN NOT success THEN 1 ELSE 0 END) AS failures, \
                                       AVG(duration_ms) AS avg_duration_ms, \
                                       SUM(tokens_in) AS total_tokens_in, \
                                       SUM(tokens_out) AS total_tokens_out \
                                FROM model_usage \
                                GROUP BY model_name \
                                ORDER BY total DESC"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({
                        "question": "Model usage stats from the previous step: {{_prev}}\n\n\
                                     Based on the above model stats, plus the scheduler state, \
                                     queue metrics, and recent failed tasks collected earlier \
                                     in this chain: what is the current health of the brain? \
                                     Are there any regressions, bottlenecks, or patterns \
                                     emerging compared to previous health snapshots?",
                        "store_inference": true
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "store_note".to_string(),
                    arguments: Some(json!({
                        "content": "Health monitor snapshot stored — see inference note for analysis.",
                        "note_type": "outcome",
                        "source_context": "health_monitor"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // Self-scheduling: re-create the task so the next cycle runs automatically.
                ChainStep {
                    tool_name: "create_task".to_string(),
                    arguments: Some(json!({
                        "goal": next_goal,
                        "context": "Recurring autonomous health check — self-scheduled"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]
        } else if g.contains("daily news")
            || g.contains("news aggregation")
            || g.contains("news briefing")
        {
            // Daily news aggregation:
            //   - 4 search categories (world, tech/AI, business/politics, international)
            //   - Outlet names embedded in queries to steer results toward quality sources
            //   - reason prompt performs bias labelling (L/R/C/I), propaganda flagging, and
            //     produces a ## CURIOUS THREADS section for follow-up research
            //   - Stores briefing as note_type="news"; creates a follow-up research task;
            //     self-schedules tomorrow's run.
            let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
            let session_id = format!("news-{}", date);
            let next_daily_goal = "Daily news aggregation and briefing: aggregate headlines from world, tech, and business, then write and store a daily briefing";
            let follow_up_goal = format!("Follow-up research from {} news briefing: investigate curious threads and store findings", date);
            let store_context = format!("daily_news_briefing_{}", date);
            vec![
                // --- World & international news (AP, Reuters, BBC, Guardian) ---
                ChainStep {
                    tool_name: "search_web".to_string(),
                    arguments: Some(json!({
                        "query": format!("top world news {} AP Reuters BBC Guardian Associated Press", date),
                        "count": 10
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "push_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "content": "WORLD NEWS:\n{{_prev}}",
                        "role": "observation"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Technology, AI & science (TechCrunch, Wired, Ars Technica, MIT Tech Review) ---
                ChainStep {
                    tool_name: "search_web".to_string(),
                    arguments: Some(json!({
                        "query": format!("AI technology science news {} TechCrunch Wired Ars Technica MIT Technology Review", date),
                        "count": 10
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "push_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "content": "TECHNOLOGY & AI NEWS:\n{{_prev}}",
                        "role": "observation"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Business, economy & politics (FT, WSJ, Bloomberg, Politico) ---
                ChainStep {
                    tool_name: "search_web".to_string(),
                    arguments: Some(json!({
                        "query": format!("business economy politics news {} Financial Times WSJ Bloomberg Politico", date),
                        "count": 10
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "push_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "content": "BUSINESS, ECONOMY & POLITICS:\n{{_prev}}",
                        "role": "observation"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Non-Western & international perspectives (Al Jazeera, Der Spiegel, Le Monde, South China Morning Post) ---
                ChainStep {
                    tool_name: "search_web".to_string(),
                    arguments: Some(json!({
                        "query": format!("international news perspectives {} Al Jazeera Der Spiegel Le Monde South China Morning Post", date),
                        "count": 10
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "push_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "content": "INTERNATIONAL PERSPECTIVES:\n{{_prev}}",
                        "role": "observation"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Retrieve all accumulated results ---
                ChainStep {
                    tool_name: "get_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "limit": 30
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Synthesise the daily briefing with bias analysis ---
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({
                        "question": format!(
                            "You are a senior news analyst compiling the daily intelligence briefing for {}.\n\n\
                             Below are search results from four news categories \
                             (world, technology/AI, business/politics, international perspectives):\n\n\
                             {{{{_prev}}}}\n\n\
                             Produce a comprehensive daily briefing structured EXACTLY as follows:\n\n\
                             ## EXECUTIVE SUMMARY\n\
                             One paragraph covering the 3 most consequential stories of the day.\n\n\
                             ## WORLD NEWS\n\
                             3-5 stories. For each story: 2-3 sentence summary + source link.\n\
                             After each story headline add a bias label in brackets: [LEFT], [RIGHT], [CENTER], or [INDIFFERENT].\n\
                             If a story shows signs of spin, selective framing, or propaganda add ⚠️ SPIN FLAG: <one-sentence explanation>.\n\n\
                             ## TECHNOLOGY & AI\n\
                             3-5 stories. Same format: summary + link + bias label + spin flag if warranted.\n\
                             Flag stories that appear to be PR/marketing disguised as news.\n\n\
                             ## BUSINESS, ECONOMY & POLITICS\n\
                             3-5 stories. Same format. Flag partisan framing or motivated reasoning.\n\n\
                             ## INTERNATIONAL PERSPECTIVES\n\
                             2-3 stories specifically from non-Western outlets. Note how they frame \
                             events differently from Western sources where relevant.\n\n\
                             ## CURIOUS THREADS\n\
                             List exactly 3 topics from today's news that are:\n\
                             - Potentially significant for AI, software engineering, or distributed systems\n\
                             - Underreported or framed too narrowly\n\
                             - Relevant to autonomous agents, LLMs, or Rust/systems development\n\
                             Format each as: **[Topic]**: one-sentence explanation of why it warrants investigation.\n\n\
                             ## STORY TO WATCH\n\
                             One sentence on the story most likely to develop further in the next 48 hours.\n\n\
                             Rules:\n\
                             - Preserve ALL URLs exactly as they appear in the search results.\n\
                             - Be factual and analytical, not opinionated.\n\
                             - Cross-reference the same story across multiple sources when possible.",
                            date
                        ),
                        "store_inference": true
                    })),
                    priority: Some(2),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Persist as a daily news briefing note ---
                ChainStep {
                    tool_name: "store_note".to_string(),
                    arguments: Some(json!({
                        "content": "{{_prev}}",
                        "note_type": "news",
                        "source_context": store_context
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Create follow-up research task for today's curious threads ---
                ChainStep {
                    tool_name: "create_task".to_string(),
                    arguments: Some(json!({
                        "goal": follow_up_goal,
                        "context": format!("Research the CURIOUS THREADS topics identified in the {} daily news briefing. Search for more context on each topic and store findings.", date)
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Self-schedule for next daily run ---
                ChainStep {
                    tool_name: "create_task".to_string(),
                    arguments: Some(json!({
                        "goal": next_daily_goal,
                        "context": "Recurring daily news aggregation — self-scheduled"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]
        } else if g.contains("weekly news") || g.contains("weekly briefing") {
            // Weekly news synthesis:
            //   - Broader "this week" queries across the same 4 categories
            //   - reason prompt focuses on week-level themes, trend arcs, and pattern shifts
            //   - 6-day self-scheduling cadence (cooldown enforced in do_tick)
            let week_start = {
                use chrono::Datelike;
                let now = chrono::Utc::now();
                let days_since_monday = now.weekday().num_days_from_monday();
                (now - chrono::Duration::days(days_since_monday as i64))
                    .format("%Y-%m-%d")
                    .to_string()
            };
            let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
            let session_id = format!("weekly-news-{}", week_start);
            let next_weekly_goal = "Weekly news briefing and analysis: synthesize major world, tech, business, and international stories from the past week";
            let store_context = format!("weekly_news_briefing_{}", week_start);
            vec![
                // --- World news this week ---
                ChainStep {
                    tool_name: "search_web".to_string(),
                    arguments: Some(json!({
                        "query": format!("major world news events week of {} AP Reuters BBC", week_start),
                        "count": 10
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "push_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "content": "WORLD NEWS THIS WEEK:\n{{_prev}}",
                        "role": "observation"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Technology & AI this week ---
                ChainStep {
                    tool_name: "search_web".to_string(),
                    arguments: Some(json!({
                        "query": format!("AI technology breakthroughs this week {} TechCrunch Wired Ars Technica", week_start),
                        "count": 10
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "push_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "content": "TECHNOLOGY & AI THIS WEEK:\n{{_prev}}",
                        "role": "observation"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Business & politics this week ---
                ChainStep {
                    tool_name: "search_web".to_string(),
                    arguments: Some(json!({
                        "query": format!("business economy politics developments week of {} Financial Times Bloomberg Politico", week_start),
                        "count": 10
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "push_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "content": "BUSINESS & POLITICS THIS WEEK:\n{{_prev}}",
                        "role": "observation"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- International perspectives this week ---
                ChainStep {
                    tool_name: "search_web".to_string(),
                    arguments: Some(json!({
                        "query": format!("international news analysis week {} Al Jazeera Der Spiegel Le Monde South China Morning Post", week_start),
                        "count": 10
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "push_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "content": "INTERNATIONAL PERSPECTIVES THIS WEEK:\n{{_prev}}",
                        "role": "observation"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Also pull any daily briefings already stored for this week ---
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({
                        "query": format!("daily news briefing {}", week_start),
                        "limit": 7
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "push_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "content": "STORED DAILY BRIEFINGS THIS WEEK:\n{{_prev}}",
                        "role": "observation"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Retrieve all accumulated context ---
                ChainStep {
                    tool_name: "get_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "limit": 40
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Synthesise the weekly briefing ---
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({
                        "question": format!(
                            "You are a senior intelligence analyst compiling the weekly briefing for the week of {}.\n\n\
                             Below are search results and any stored daily briefings from this week:\n\n\
                             {{{{_prev}}}}\n\n\
                             Produce a comprehensive weekly intelligence briefing structured EXACTLY as follows:\n\n\
                             ## WEEK IN REVIEW — {}\n\
                             Two-paragraph executive summary of the week's most consequential developments \
                             and any emergent patterns or inflection points.\n\n\
                             ## MAJOR WORLD EVENTS\n\
                             Top 5 world stories of the week. For each: 3-4 sentence summary + source link.\n\
                             Add bias label [LEFT/RIGHT/CENTER/INDIFFERENT] and ⚠️ SPIN FLAG where warranted.\n\n\
                             ## TECHNOLOGY & AI DEVELOPMENTS\n\
                             Top 5 tech/AI stories. Same format. Flag PR-driven stories explicitly.\n\
                             Note any stories with direct implications for autonomous agents or LLMs.\n\n\
                             ## BUSINESS, ECONOMY & POLITICS\n\
                             Top 5 stories. Same format. Note any regulatory, economic, or geopolitical \
                             shifts that could affect the technology sector.\n\n\
                             ## INTERNATIONAL PERSPECTIVES\n\
                             3 stories from non-Western sources. Highlight framing differences from Western coverage.\n\n\
                             ## TREND ARCS\n\
                             3 multi-day trends that developed or accelerated this week. \
                             For each: what drove it, where it's heading, why it matters.\n\n\
                             ## CURIOUS THREADS\n\
                             3 underreported or technically significant topics worth deeper investigation. \
                             Format: **[Topic]**: why it matters for AI/systems/agents.\n\n\
                             ## STORY TO WATCH NEXT WEEK\n\
                             One sentence on the most likely developing story to follow.\n\n\
                             Rules: preserve ALL URLs. Be analytical, not opinionated. Cross-reference sources.",
                            week_start, week_start
                        ),
                        "store_inference": true
                    })),
                    priority: Some(2),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Persist as a weekly news note ---
                ChainStep {
                    tool_name: "store_note".to_string(),
                    arguments: Some(json!({
                        "content": "{{_prev}}",
                        "note_type": "news",
                        "source_context": store_context
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Self-schedule for next week ---
                ChainStep {
                    tool_name: "create_task".to_string(),
                    arguments: Some(json!({
                        "goal": next_weekly_goal,
                        "context": format!("Recurring weekly news briefing — self-scheduled after {}", date)
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]

        } else if g.contains("follow-up research from")
            || g.contains("research curious threads")
            || g.contains("investigate curious")
        {
            // Follow-up research: find the relevant news briefing, pull the CURIOUS THREADS
            // topics, search the web for each, and store consolidated findings.
            // Triggered by the daily/weekly news chain's follow-up task.
            let date = chrono::Utc::now().format("%Y-%m-%d").to_string();
            let session_id = format!("research-{}", date);
            let store_context = format!("news_research_{}", date);
            vec![
                // --- Find today's (or most recent) news briefing ---
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({
                        "query": format!("daily news briefing CURIOUS THREADS {}", date),
                        "limit": 3
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "push_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "content": "NEWS BRIEFING CONTEXT:\n{{_prev}}",
                        "role": "observation"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Search for more context on the curious threads topics ---
                ChainStep {
                    tool_name: "search_web".to_string(),
                    arguments: Some(json!({
                        "query": format!("AI autonomous agents LLM systems developments research {}", date),
                        "count": 8
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "push_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "content": "FOLLOW-UP SEARCH RESULTS:\n{{_prev}}",
                        "role": "observation"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Retrieve all context ---
                ChainStep {
                    tool_name: "get_context".to_string(),
                    arguments: Some(json!({
                        "session_id": session_id,
                        "limit": 20
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Synthesise research findings ---
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({
                        "question": format!(
                            "You are a research analyst doing follow-up investigation on topics \
                             flagged as CURIOUS THREADS in today's ({}) news briefing.\n\n\
                             Below is the briefing context and additional search results:\n\n\
                             {{{{_prev}}}}\n\n\
                             Produce a research findings report:\n\
                             1. For each CURIOUS THREAD topic found in the briefing, provide 2-3 \
                                paragraphs of deeper context and analysis.\n\
                             2. Identify any connections between these topics and autonomous agents, \
                                LLMs, Rust/systems programming, or distributed systems.\n\
                             3. Note any actionable implications: things to build, patterns to adopt, \
                                risks to be aware of, or areas to monitor.\n\
                             4. Include all relevant source URLs.\n\n\
                             Be concise and technically precise.",
                            date
                        ),
                        "store_inference": true
                    })),
                    priority: Some(2),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // --- Store research findings ---
                ChainStep {
                    tool_name: "store_note".to_string(),
                    arguments: Some(json!({
                        "content": "{{_prev}}",
                        "note_type": "semantic",
                        "source_context": store_context
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]

        } else if g.contains("add news source")
            || g.starts_with("news source:")
            || g.contains("register news source")
        {
            // News source management: store a new source as a news_source note.
            // The note content is JSON: { name, url, bias, scope, description }.
            // Bias values: "left" | "right" | "center" | "indifferent"
            // Scope values: "world" | "tech" | "business" | "international" | "financial" | "general"
            // Both humans and the LLM can add sources by creating tasks with this goal pattern.
            vec![
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({
                        "question": format!(
                            "Extract news source details from this task goal and format as JSON.\n\
                             Task goal: \"{}\"\n\n\
                             Return ONLY a JSON object with these fields:\n\
                             - name: outlet name (string)\n\
                             - url: domain or URL (string)\n\
                             - bias: political leaning — one of: \"left\", \"right\", \"center\", \"indifferent\"\n\
                             - scope: coverage focus — one of: \"world\", \"tech\", \"business\", \"international\", \"financial\", \"general\"\n\
                             - description: one sentence describing the outlet (string)\n\n\
                             Example: {{\"name\":\"Reuters\",\"url\":\"reuters.com\",\"bias\":\"center\",\"scope\":\"world\",\"description\":\"International wire service known for factual reporting.\"}}\n\n\
                             If you cannot determine bias from the goal, use \"indifferent\". \
                             If you cannot determine scope, use \"general\".",
                            goal
                        ),
                        "store_inference": false
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "store_note".to_string(),
                    arguments: Some(json!({
                        "content": "{{_prev}}",
                        "note_type": "news_source",
                        "source_context": "news_source_registry"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]

        } else if g.contains("document") || g.contains("current state") {
            // Document / capture state: search knowledge, then consolidate
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "consolidate_memories".to_string(),
                    arguments: Some(json!({ "topic": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
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
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "store_note".to_string(),
                    arguments: Some(json!({
                        "content": format!("Planning result for: {goal}"),
                        "note_type": "semantic"
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]
        } else if g.contains("improve") || g.contains("execute") {
            // Improvement / execution: search, reason, reflect
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reflect_on_work".to_string(),
                    arguments: Some(json!({
                        "goal": goal,
                        "current_state": "Executing task via autonomous scheduler",
                        "task_id": task_id
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]
        } else if g.contains("identify") || g.contains("opportunit") {
            // Opportunity identification: reason directly, persist finding
            vec![
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "store_note".to_string(),
                    arguments: Some(json!({
                        "content": format!("Opportunity analysis: {goal}"),
                        "note_type": "semantic"
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]
        } else if g.contains("consolidat") {
            // Memory consolidation: run consolidate_memories then prune stale notes.
            // Auto-generated goals (containing "overdue" or "episodic") use a broad topic;
            // manually created consolidation goals extract the meaningful subject after the verb.
            let topic = if g.contains("overdue") || g.contains("episodic") {
                "recent experiences and knowledge".to_string()
            } else {
                // e.g. "Consolidate robotics knowledge" → "robotics knowledge"
                goal.split_whitespace()
                    .skip(1) // skip "Consolidate"
                    .collect::<Vec<_>>()
                    .join(" ")
                    .trim()
                    .to_string()
            };
            vec![
                ChainStep {
                    tool_name: "consolidate_memories".to_string(),
                    arguments: Some(json!({ "topic": topic, "limit": 15 })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "prune_old_notes".to_string(),
                    arguments: Some(json!({
                        "score_threshold": 0.05,
                        "lambda": 0.1,
                        "dry_run": false
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                // Mark the parent task completed so open_consolidation_exists() stays accurate.
                ChainStep {
                    tool_name: "update_task".to_string(),
                    arguments: Some(json!({ "task_id": task_id, "status": "completed" })),
                    priority: Some(1),
                    max_attempts: Some(1),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]
        } else if g.contains("failure")
            || g.contains("root cause")
            || g.contains("debug")
            || g.contains("error pattern")
        {
            // Failure analysis: search for error context, diagnose, document findings.
            // Matches goals auto-generated by perception_scan: "Analyze repeated failures for 'X'...".
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 15 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "store_note".to_string(),
                    arguments: Some(json!({
                        "content": format!("Failure analysis outcome: {goal}"),
                        "note_type": "semantic"
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reflect_on_work".to_string(),
                    arguments: Some(json!({
                        "goal": goal,
                        "current_state": "Completed failure analysis and root-cause reasoning",
                        "task_id": task_id
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]
        } else if g.contains("search web")
            || g.contains("web search")
            || g.contains("look up")
            || (g.contains("find") && g.contains("recent"))
        {
            // Web research: fetch live information and store findings.
            vec![
                ChainStep {
                    tool_name: "search_web".to_string(),
                    arguments: Some(json!({ "query": goal, "count": 5 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "store_note".to_string(),
                    arguments: Some(json!({
                        "content": format!("Web research finding for: {goal}"),
                        "note_type": "semantic"
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
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
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "store_note".to_string(),
                    arguments: Some(json!({
                        "content": format!("Learning outcome: {goal}"),
                        "note_type": "semantic"
                    })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
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
                },
                ChainStep {
                    tool_name: "get_git_log".to_string(),
                    arguments: Some(json!({ "n": 10 })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
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
                },
                ChainStep {
                    tool_name: "get_git_diff".to_string(),
                    arguments: Some(json!({ "from_ref": "HEAD~10" })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "store_note".to_string(),
                    arguments: Some(json!({
                        "content": format!("Git history analysis: {goal}"),
                        "note_type": "episodic"
                    })),
                    priority: Some(1),
                    max_attempts: Some(2),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]
        } else if g.contains("review") || g.contains("analyz") || g.contains("source") {
            // Review / analysis: search context, reason over it
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
            ]
        } else {
            // Default: search context, reason, and reflect on the outcome.
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: Some("ollama".to_string()),
                    context_profile: None,
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
            provider_hint: None,
            context_profile: None,
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
            cfg.clone()
        };

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
