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

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use chrono::Utc;
use serde_json::{json, Value};
use tokio::sync::{Notify, RwLock};
use tracing::{debug, info, warn};

use crate::models::TaskStatus;
use crate::repository::Neo4jClient;
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
}

/// Background scheduler service.
pub struct SchedulerService {
    neo4j: Neo4jClient,
    queue: Arc<QueueService>,
    pub config: Arc<RwLock<SchedulerConfig>>,
    pub state: Arc<RwLock<SchedulerState>>,
    shutdown: Arc<AtomicBool>,
    wakeup: Arc<Notify>,
}

impl SchedulerService {
    /// Create and start the scheduler.
    ///
    /// Reads `SCHEDULER_INTERVAL_SECS` and `SCHEDULER_ENABLED` from the environment,
    /// then spawns the background loop immediately.
    pub fn new(neo4j: Neo4jClient, queue: Arc<QueueService>) -> Arc<Self> {
        let svc = Arc::new(Self {
            neo4j,
            queue,
            config: Arc::new(RwLock::new(SchedulerConfig::default())),
            state: Arc::new(RwLock::new(SchedulerState::default())),
            shutdown: Arc::new(AtomicBool::new(false)),
            wakeup: Arc::new(Notify::new()),
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
            // Snapshot interval before sleeping to avoid holding guard across await.
            let interval_secs = self.config.read().await.interval_secs;

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

            let steps = Self::goal_to_steps(&goal, &task_id);

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

        Ok(TickResult {
            tasks_found,
            tasks_dispatched,
            skipped,
        })
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
                },
                ChainStep {
                    tool_name: "consolidate_memories".to_string(),
                    arguments: Some(json!({ "topic": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
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
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
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
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
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
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
                },
            ]
        } else {
            // Default: search context + reason
            vec![
                ChainStep {
                    tool_name: "search_notes".to_string(),
                    arguments: Some(json!({ "query": goal, "limit": 10 })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
                },
                ChainStep {
                    tool_name: "reason".to_string(),
                    arguments: Some(json!({ "question": goal, "store_inference": true })),
                    priority: Some(1),
                    max_attempts: Some(3),
                    provider_hint: None,
                },
            ]
        };

        // Always close the task when the chain finishes successfully.
        steps.push(ChainStep {
            tool_name: "update_task".to_string(),
            arguments: Some(json!({
                "task_id": task_id,
                "status": "completed",
                "note": "Task completed autonomously by scheduler job chain."
            })),
            priority: Some(1),
            max_attempts: Some(3),
            provider_hint: None,
        });

        steps
    }

    // =========================================================================
    // Public control API
    // =========================================================================

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
            },
            "state": {
                "tasks_dispatched": st.tasks_dispatched,
                "consecutive_errors": st.consecutive_errors,
                "last_run_at": st.last_run_at,
                "last_error": st.last_error,
                "is_running": st.is_running,
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
