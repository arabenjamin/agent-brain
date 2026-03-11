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
use serde_json::{json, Value};
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
        info!(interval_secs = interval, enabled = enabled, "SchedulerService started");
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
                if s.is_sleeping { c.sleep_interval_secs } else { c.interval_secs }
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

        let tasks = self
            .neo4j
            .list_tasks(Some("created"), 20)
            .await
            .map_err(|e| e.to_string())?;

        let tasks_found = tasks.len();
        let mut tasks_dispatched = 0usize;

        for task in tasks.iter().take(max_tasks) {
            let task_id = task["id"].as_str().unwrap_or("").to_string();
            let goal = task["goal"].as_str().unwrap_or("").to_string();

            if task_id.is_empty() || goal.is_empty() {
                continue;
            }

            let mut steps = Self::goal_to_steps(&goal, &task_id);

            // Auto-assign a context profile to all steps if context_builder is available.
            if let Some(cb) = &self.context_builder {
                let profile = cb.auto_assign(&goal).await;
                for step in &mut steps {
                    if step.context_profile.is_none() {
                        step.context_profile = Some(profile.clone());
                    }
                }
            }

            match self.queue.enqueue_chain(&steps, session_id.as_deref()).await {
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

        if let Err(e) = self.queue.enqueue_chain(&steps, session_id.as_deref()).await {
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

        let rows = self.neo4j
            .execute(neo4rs::query(cypher))
            .await
            .map_err(|e| e.to_string())?;

        // Count failures per tool name (content format: "Tool: <name> | Success: false\n...")
        let mut tool_failures: HashMap<String, u32> = HashMap::new();
        for row in &rows {
            if let Ok(content) = row.get::<String>("content") {
                if let Some(rest) = content.strip_prefix("Tool: ") {
                    if let Some(tool_name) = rest.split(" | ").next() {
                        *tool_failures.entry(tool_name.trim().to_string()).or_insert(0) += 1;
                    }
                }
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

            let existing: i64 = self.neo4j
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
                match self.neo4j.create_task(
                    &goal,
                    Some("Auto-generated by proactive perception scan"),
                ).await {
                    Ok(_) => {
                        created += 1;
                        info!(tool = %tool, failures = count, "Perception scan created failure analysis task");
                    }
                    Err(e) => warn!(tool = %tool, error = %e, "Failed to create perception task"),
                }
            }
        }

        // Helper: check if an open consolidation task already exists.
        let open_consolidation_exists = || async {
            let q = neo4rs::query(
                "MATCH (t:Task) \
                 WHERE t.goal CONTAINS 'consolidat' \
                   AND t.status IN ['created', 'in_progress'] \
                 RETURN count(t) AS cnt",
            );
            self.neo4j
                .execute(q)
                .await
                .ok()
                .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
                .unwrap_or(0) > 0
        };

        // Trigger 1: many overdue spaced-repetition notes.
        let due_check = neo4rs::query(
            "MATCH (n:Note) \
             WHERE n.next_review_at <= datetime() \
               AND n.note_type <> 'consolidated' \
             RETURN count(n) AS cnt",
        );
        let due_count: i64 = self.neo4j
            .execute(due_check)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
            .unwrap_or(0);

        if due_count >= 10 && !open_consolidation_exists().await {
            let goal = format!(
                "Consolidate {} overdue spaced-repetition notes into long-term memory",
                due_count
            );
            if self.neo4j.create_task(
                &goal,
                Some("Auto-generated by proactive perception scan (spaced-repetition backlog)"),
            ).await.is_ok() {
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
                    warn!("Failed to advance overdue note dates after scheduling consolidation: {e}");
                }

                info!(due_count = due_count, "Perception scan created consolidation task (overdue notes)");
            }
        }

        // Trigger 2: high episodic note volume (sleep-cycle analogue).
        let episodic_check = neo4rs::query(
            "MATCH (n:Note) WHERE n.note_type = 'episodic' RETURN count(n) AS cnt",
        );
        let episodic_count: i64 = self.neo4j
            .execute(episodic_check)
            .await
            .ok()
            .and_then(|rows| rows.first().and_then(|r| r.get::<i64>("cnt").ok()))
            .unwrap_or(0);

        if episodic_count >= 50 && !open_consolidation_exists().await {
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

        Ok(created)
    }

    // =========================================================================
    // Goal → chain-step mapper
    // =========================================================================

    fn goal_to_steps(goal: &str, task_id: &str) -> Vec<ChainStep> {
        let g = goal.to_lowercase();

        let mut steps = if g.contains("document") || g.contains("current state") {
            // Document / capture state: search knowledge, then consolidate
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "consolidate_memories".to_string(),
                    arguments: Some(json!({ "topic": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
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
                    provider_hint: None,
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
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
                    provider_hint: None,
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
                    provider_hint: None,
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
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
                    provider_hint: None,
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
                    provider_hint: None,
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
                    provider_hint: None,
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
                    .skip(1)  // skip "Consolidate"
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
                    provider_hint: None,
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
                    provider_hint: None,
                    context_profile: None,
                },
                // Mark the parent task completed so open_consolidation_exists() stays accurate.
                ChainStep {
                    tool_name: "update_task".to_string(),
                    arguments: Some(json!({ "task_id": task_id, "status": "completed" })),
                    priority: Some(1),
                    max_attempts: Some(1),
                    provider_hint: None,
                    context_profile: None,
                },
            ]
        } else if g.contains("failure") || g.contains("root cause") || g.contains("debug") || g.contains("error pattern") {
            // Failure analysis: search for error context, diagnose, document findings.
            // Matches goals auto-generated by perception_scan: "Analyze repeated failures for 'X'...".
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 15 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
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
                    provider_hint: None,
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
                    provider_hint: None,
                    context_profile: None,
                },
            ]
        } else if g.contains("search web") || g.contains("web search") || g.contains("look up") || (g.contains("find") && g.contains("recent")) {
            // Web research: fetch live information and store findings.
            vec![
                ChainStep {
                    tool_name: "search_web".to_string(),
                    arguments: Some(json!({ "query": goal, "count": 5 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
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
                    provider_hint: None,
                    context_profile: None,
                },
            ]
        } else if g.contains("learn") || g.contains("research") || g.contains("study") || g.contains("understand") {
            // Learning / research: search existing knowledge, reason, persist new knowledge.
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
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
                    provider_hint: None,
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
                    provider_hint: None,
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
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
                    provider_hint: None,
                    context_profile: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
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
                    provider_hint: None,
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
            interval_changed = interval_secs.map_or(false, |v| v != cfg.interval_secs);
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
