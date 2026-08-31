//! Agent job queue — priority-ordered background task executor.
//!
//! # Design
//!
//! - **Durability**: jobs are persisted to Neo4j so they survive server restarts.
//! - **Priority**: an in-memory `BinaryHeap` orders jobs by priority (0–3) then FIFO.
//! - **Concurrency**: a `tokio::sync::Semaphore` limits concurrent executions.
//! - **Wakeup**: a `Notify` wakes the coordinator immediately when a new job arrives or
//!   when a running job finishes and frees a provider concurrency slot.
//! - **Recovery**: on startup, `recover()` resets crashed `running` jobs to `queued`
//!   and reloads all `queued` jobs into the heap.
//!
//! # Resizing concurrency at runtime
//!
//! `update_config()` stores the new `max_concurrent` value but the underlying semaphore
//! is fixed at creation time.  To change effective concurrency, set `enabled = false`,
//! recreate the service, then re-enable.  Phase-2 multi-provider support will introduce
//! per-provider semaphores with dynamic resizing.

use std::collections::{BinaryHeap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

tokio::task_local! {
    /// Set to `true` inside a background job task when `provider_hint == "ollama"`.
    ///
    /// `SharedLlm` reads this flag to route generation calls to the local Ollama
    /// endpoint instead of the active (possibly cloud) model, preventing background
    /// maintenance jobs from consuming cloud quota.
    pub static USE_LOCAL_LLM: bool;

    /// Per-step model override resolved from `ChainStep.required_capabilities`
    /// by the model router (Phase 1 of the Agent Constructor plan).
    /// Takes precedence over `USE_LOCAL_LLM` in `SharedLlm` — a step that
    /// explicitly requires capabilities has earned its routing.
    pub static SELECTED_LLM: Option<crate::services::LlmConfig>;

    /// Name of the tool whose execution this task is running, so `SharedLlm`
    /// can attribute its usage rows to it.
    ///
    /// Without this every background LLM call lands in the ledger with
    /// `tool_name IS NULL`. That was 55% of 30 days of cloud spend — the
    /// majority of it unattributable by the one query you would ask after
    /// exhausting a quota, which is exactly when you need it. The tool name is
    /// known right here at dispatch; it just was not being carried down.
    pub static CURRENT_TOOL: Option<String>;
}

use serde::Deserialize;
use tokio::sync::{Mutex, Notify, RwLock, Semaphore, broadcast};
use tracing::{debug, error, info, warn};

use crate::brain_core::BrainEvent;
use crate::mcp::tools::ToolHandler;
use crate::models::{AgentJob, AgentJobStatus, PrioritizedJob, TaskStatus};
use crate::repository::Neo4jClient;
use agent_brain_protocol::{Content, ToolCallResult};

const DEFAULT_MAX_CONCURRENT: usize = 5;
const DEFAULT_MAX_CONCURRENT_OLLAMA: usize = 2;
const DEFAULT_MAX_CONCURRENT_ANTHROPIC: usize = 2;
const DEFAULT_MAX_CONCURRENT_GEMINI: usize = 5;

/// Progress tuple: (percent, message, updated_at).
pub type JobProgressTuple = (u8, Option<String>, Option<String>);

/// Runtime configuration for the queue coordinator.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Global maximum number of jobs executing concurrently (informational).
    pub max_concurrent: usize,
    /// Concurrency limit for Ollama (local) jobs.
    pub max_concurrent_ollama: usize,
    /// Concurrency limit for Anthropic API jobs.
    pub max_concurrent_anthropic: usize,
    /// Concurrency limit for Gemini API jobs.
    pub max_concurrent_gemini: usize,
    /// When `false`, the coordinator will not pick up new jobs.
    pub enabled: bool,
    /// How often (seconds) the coordinator polls Neo4j for jobs that might have
    /// been missed (e.g. added while the heap was empty).
    pub poll_interval_secs: u64,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            max_concurrent: DEFAULT_MAX_CONCURRENT,
            max_concurrent_ollama: DEFAULT_MAX_CONCURRENT_OLLAMA,
            max_concurrent_anthropic: DEFAULT_MAX_CONCURRENT_ANTHROPIC,
            max_concurrent_gemini: DEFAULT_MAX_CONCURRENT_GEMINI,
            enabled: true,
            poll_interval_secs: 30,
        }
    }
}

/// One step in a sequential job chain submitted via `enqueue_chain`.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct ChainStep {
    #[serde(default)]
    pub tool_name: String,
    #[serde(default)]
    pub arguments: Option<serde_json::Value>,
    pub priority: Option<u8>,
    pub max_attempts: Option<u32>,
    pub provider_hint: Option<String>,
    pub context_profile: Option<String>,
    #[serde(default)]
    pub ttl_secs: Option<u64>,
    #[serde(default)]
    pub description: Option<String>,
    /// Minimum confidence score (0.0–1.0) required to execute this chain.
    /// When set on the first step the scheduler evaluates confidence before
    /// dispatching; if the score falls below the threshold the original chain
    /// is replaced with a lightweight diagnosis chain.
    #[serde(default)]
    pub confidence_threshold: Option<f32>,
    /// When `true`, this step is treated as an evaluator: after it completes the
    /// coordinator parses a 1–5 score from the output.  If the score is below
    /// `min_score` the original task is marked failed and re-created so the
    /// scheduler will dispatch a new attempt with the critique as context.
    #[serde(default)]
    pub is_evaluator: bool,
    /// Minimum acceptable score (1–5) from an evaluator step.  Defaults to 3.5.
    #[serde(default)]
    pub min_score: Option<f32>,
    /// Task ID of the parent goal being evaluated.  Stored as `__evaluator_task_id`
    /// in the job args so the coordinator can look up the original goal on re-queue.
    #[serde(default)]
    pub evaluator_task_id: Option<String>,
    /// When `true`, this step runs *before* the main action steps and calls
    /// `adversarial_plan_review`.  If the returned `overall_robustness` falls below
    /// `min_robustness` the chain is aborted and the task re-created with the
    /// adversarial critique injected as context for the next attempt.
    #[serde(default)]
    pub is_adversarial: bool,
    /// Number of failure hypotheses the adversarial reviewer should generate (default 3).
    #[serde(default)]
    pub n_hypotheses: Option<u8>,
    /// Minimum acceptable overall robustness score (1–5) from the adversarial review.
    /// Defaults to 2.5 — plans that can't defend against half the scenarios are aborted.
    #[serde(default)]
    pub min_robustness: Option<f32>,
    /// Task ID passed to the adversarial re-queue handler.  Stored as
    /// `__adversarial_task_id` in job args.
    #[serde(default)]
    pub adversarial_task_id: Option<String>,
    /// Capabilities this step's LLM calls require (e.g. `["reasoning"]`).
    /// When set, the model router picks the cheapest catalog model that
    /// satisfies them within the active `CLOUD_TIER` and the job's LLM calls
    /// route to it. Stored as `__required_capabilities` in the job args.
    /// When nothing qualifies the step falls back to normal routing.
    #[serde(default)]
    pub required_capabilities: Option<Vec<String>>,
    /// When `true`, the previous step's output is compressed by the local model
    /// before it is substituted into this step's `{{_prev}}` / `{{result}}`.
    ///
    /// This is the "distilled handoff": a chain step usually needs the prior
    /// step's *conclusions*, not its full text, but `{{_prev}}` pastes the whole
    /// thing into the prompt.  Opt in on the **consuming** step, since only the
    /// consumer knows whether it can tolerate a lossy handoff.  Never set it on a
    /// step that persists `{{_prev}}` verbatim (`store_note`, `write_workspace_file`) —
    /// that would truncate the durable artifact rather than just the prompt.
    ///
    /// Stored as `__distill_prev` in the job args; tools ignore it via serde.
    #[serde(default)]
    pub distill_prev: bool,
    /// Skip distillation when the previous output is already at or under this many
    /// characters.  Defaults to [`DEFAULT_DISTILL_MAX_CHARS`].
    ///
    /// Also the length budget handed to the distiller, but a **soft** one — models
    /// cannot count characters, and overshoot of ~50% is normal (measured: a 3000
    /// budget produced 4898 chars from a 195 000-char diff, still a 97.5%
    /// reduction).  The only hard guarantee is that a distilled handoff is never
    /// longer than the raw one; otherwise the raw text is used.
    #[serde(default)]
    pub distill_max_chars: Option<usize>,
    /// What the consuming step actually needs kept — e.g. "preserve every source
    /// URL and any numbers".  Injected into the distiller prompt so the compression
    /// is aimed at this step rather than generic.
    #[serde(default)]
    pub distill_focus: Option<String>,
}

/// Default length budget for a distilled handoff, and the threshold under which
/// distillation is skipped entirely.  Sized so a typical multi-paragraph analysis
/// survives untouched and only genuinely bulky payloads (raw SERP JSON, git diffs,
/// full transcript summaries) pay for a compression call.
pub const DEFAULT_DISTILL_MAX_CHARS: usize = 2000;

/// Hard cap on how much text is fed *into* the distiller.  Input above this is
/// reduced to a head + tail window: conclusions usually live at the end, so
/// head-only truncation would discard exactly what the next step needs.
///
/// Sized to fit inside [`DISTILL_NUM_CTX`] with room for the instructions and the
/// generated output (~4 chars/token).
const DISTILL_INPUT_CAP_CHARS: usize = 24_000;

/// Context window requested for the distillation call.
///
/// Ollama serves 4096 tokens by default *regardless of the model's real limit*,
/// and silently truncates beyond it — a 24 000-char payload sent at the default
/// arrives as a fragment with the trailing instruction cut off, and the model
/// answers the fragment instead of compressing it. Distillation is the one call
/// whose purpose is reading a large payload, so it asks for a real window.
const DISTILL_NUM_CTX: u32 = 16_384;

impl ChainStep {
    pub fn new(tool_name: impl Into<String>) -> Self {
        Self {
            tool_name: tool_name.into(),
            ..Default::default()
        }
    }
}

/// Priority job queue with Neo4j-backed persistence and Tokio worker coordination.
pub struct QueueService {
    neo4j: Neo4jClient,
    tool_handler: Arc<RwLock<Option<ToolHandler>>>,
    heap: Arc<Mutex<BinaryHeap<PrioritizedJob>>>,
    notify: Arc<Notify>,
    semaphore_ollama: Arc<RwLock<Arc<Semaphore>>>,
    semaphore_anthropic: Arc<RwLock<Arc<Semaphore>>>,
    semaphore_gemini: Arc<RwLock<Arc<Semaphore>>>,
    pub config: Arc<RwLock<WorkerConfig>>,
    cancelled_ids: Arc<Mutex<HashSet<String>>>,
    /// Brain event bus — emits `JobCompleted / JobFailed / JobDead` events that
    /// transport adapters (e.g. HTTP SSE) can subscribe to and forward to clients.
    event_tx: Option<broadcast::Sender<BrainEvent>>,
    /// Set to `true` by `run_coordinator` on every heartbeat tick; proves the task is alive.
    coordinator_alive: Arc<AtomicBool>,
    /// Unix timestamp (seconds) of the last coordinator heartbeat, or -1 if never set.
    coordinator_last_heartbeat: Arc<AtomicI64>,
    /// Result of the last orphan-chain audit: count of orphaned parked jobs found (and cancelled).
    last_orphan_audit_count: Arc<AtomicI64>,
    /// Local-Ollama config used to compress chain handoffs (`distill_prev`).
    ///
    /// Held as a config rather than a provider because distillation needs its own
    /// `num_ctx` — it is the one call in the brain whose whole point is reading a
    /// large payload, and the shared local provider is pinned to Ollama's 4096
    /// default. `None` disables distillation rather than failing the step.
    distill_config: Arc<RwLock<Option<crate::services::LlmConfig>>>,
}

impl QueueService {
    pub fn new(
        neo4j: Neo4jClient,
        tool_handler: Arc<RwLock<Option<ToolHandler>>>,
        event_tx: Option<broadcast::Sender<BrainEvent>>,
    ) -> Self {
        Self {
            neo4j,
            tool_handler,
            heap: Arc::new(Mutex::new(BinaryHeap::new())),
            notify: Arc::new(Notify::new()),
            semaphore_ollama: Arc::new(RwLock::new(Arc::new(Semaphore::new(
                DEFAULT_MAX_CONCURRENT_OLLAMA,
            )))),
            semaphore_anthropic: Arc::new(RwLock::new(Arc::new(Semaphore::new(
                DEFAULT_MAX_CONCURRENT_ANTHROPIC,
            )))),
            semaphore_gemini: Arc::new(RwLock::new(Arc::new(Semaphore::new(
                DEFAULT_MAX_CONCURRENT_GEMINI,
            )))),
            config: Arc::new(RwLock::new(WorkerConfig::default())),
            cancelled_ids: Arc::new(Mutex::new(HashSet::new())),
            event_tx,
            coordinator_alive: Arc::new(AtomicBool::new(false)),
            coordinator_last_heartbeat: Arc::new(AtomicI64::new(-1)),
            last_orphan_audit_count: Arc::new(AtomicI64::new(-1)),
            distill_config: Arc::new(RwLock::new(None)),
        }
    }

    /// Install the LLM config used for distilled handoffs.
    ///
    /// Takes `&self` (not `self`) because the queue is already behind an `Arc` by
    /// the time the local config is built, and is re-applied on every
    /// `build_skills()` so a reload refreshes it.  Pass the *local* config:
    /// distillation is an optimization and must never spend cloud quota.
    pub async fn set_distill_config(&self, config: crate::services::LlmConfig) {
        *self.distill_config.write().await = Some(config);
    }

    // =========================================================================
    // Startup
    // =========================================================================

    /// Reset crashed jobs and reload the heap from Neo4j.
    pub async fn recover(&self) {
        match self.neo4j.reset_running_agent_jobs().await {
            Ok(n) if n > 0 => info!(count = n, "Reset crashed AgentJobs to queued"),
            Ok(_) => {}
            Err(e) => warn!("Failed to reset running jobs: {}", e),
        }

        // Cancel parked jobs whose parent is now terminal — these accumulated during
        // crashes or explicit cancellations and can never be unparked.
        match self.neo4j.cancel_orphaned_parked_jobs().await {
            Ok(n) if n > 0 => info!(count = n, "Cancelled orphaned parked AgentJobs on recovery"),
            Ok(_) => {}
            Err(e) => warn!("Failed to cancel orphaned parked jobs on recovery: {}", e),
        }

        match self.neo4j.list_queued_agent_jobs().await {
            Ok(jobs) => {
                let mut heap = self.heap.lock().await;
                for job in jobs {
                    heap.push(PrioritizedJob {
                        priority: job.priority,
                        created_at: job.created_at.clone(),
                        job,
                    });
                }
                let n = heap.len();
                if n > 0 {
                    info!(count = n, "Reloaded queued AgentJobs into heap");
                    self.notify.notify_one();
                }
            }
            Err(e) => warn!("Failed to load queued jobs on startup: {}", e),
        }
    }

    // =========================================================================
    // Public queue API
    // =========================================================================

    /// Submit a new job.  Persists to Neo4j, pushes to in-memory heap, and
    /// notifies the coordinator.  Returns the new job ID.
    #[allow(clippy::too_many_arguments)]
    pub async fn enqueue(
        &self,
        tool_name: &str,
        arguments: Option<&serde_json::Value>,
        priority: u8,
        max_attempts: u32,
        session_id: Option<&str>,
        parent_job_id: Option<&str>,
        provider_hint: Option<&str>,
    ) -> Result<String, String> {
        let job = self
            .neo4j
            .create_agent_job(
                tool_name,
                arguments,
                priority,
                max_attempts,
                session_id,
                parent_job_id,
                provider_hint,
                None,
                None,
                None,
            )
            .await
            .map_err(|e| e.to_string())?;

        // create_agent_job returns the full record — no reload needed.
        let id = job.id.clone();
        self.heap.lock().await.push(PrioritizedJob {
            priority: job.priority,
            created_at: job.created_at.clone(),
            job,
        });
        self.notify.notify_one();

        Ok(id)
    }

    /// Submit a sequential chain of jobs.
    ///
    /// The **first** step is enqueued immediately (`queued`).
    /// Steps 2..N are stored as `parked`, each with `parent_job_id` pointing to the
    /// preceding step.  When a job completes the coordinator automatically promotes
    /// its parked children to `queued`.  If a job fails or is marked dead its parked
    /// children are cancelled.
    ///
    /// Returns the list of job IDs in chain order.
    pub async fn enqueue_chain(
        &self,
        steps: &[ChainStep],
        session_id: Option<&str>,
    ) -> Result<Vec<String>, String> {
        self.enqueue_chain_owned(steps, session_id, None).await
    }

    /// Like [`enqueue_chain`](Self::enqueue_chain), but stamps every job with the
    /// id of the `Task` node that owns the chain (`__owner_task_id` in args,
    /// serde-ignored by tools like the other `__`-prefixed metadata fields).
    ///
    /// When any step later dies, the coordinator reads this id and marks the
    /// owning task `failed` with the real error — so a scheduled run no longer
    /// gets stuck `in_progress` until the stale reaper flips it with no reason.
    /// Callers with no owning task (chat-dispatched chains, bedtime chains) use
    /// [`enqueue_chain`](Self::enqueue_chain) which passes `None`.
    pub async fn enqueue_chain_owned(
        &self,
        steps: &[ChainStep],
        session_id: Option<&str>,
        owner_task_id: Option<&str>,
    ) -> Result<Vec<String>, String> {
        if steps.is_empty() {
            return Err("Chain must contain at least one step".to_string());
        }

        let mut ids: Vec<String> = Vec::with_capacity(steps.len());
        let mut prev_id: Option<String> = None;
        let mut first_job: Option<AgentJob> = None;

        for (i, step) in steps.iter().enumerate() {
            let priority = step.priority.unwrap_or(1);
            let max_attempts = step.max_attempts.unwrap_or(3);

            // For evaluator steps, inject metadata fields into the args JSON so
            // execute_job can parse them without needing extra AgentJob columns.
            // The tool handler ignores unknown fields via serde default behaviour.
            let effective_args: Option<serde_json::Value> = if step.is_evaluator {
                let mut a = step.arguments.clone().unwrap_or(serde_json::json!({}));
                if let serde_json::Value::Object(ref mut m) = a {
                    m.insert(
                        "__evaluator_min_score".to_string(),
                        serde_json::json!(step.min_score.unwrap_or(3.5)),
                    );
                    if let Some(tid) = &step.evaluator_task_id {
                        m.insert("__evaluator_task_id".to_string(), serde_json::json!(tid));
                    }
                }
                Some(a)
            } else if step.is_adversarial {
                let mut a = step.arguments.clone().unwrap_or(serde_json::json!({}));
                if let serde_json::Value::Object(ref mut m) = a {
                    m.insert(
                        "__adversarial_min_robustness".to_string(),
                        serde_json::json!(step.min_robustness.unwrap_or(2.5)),
                    );
                    m.insert(
                        "__adversarial_n_hypotheses".to_string(),
                        serde_json::json!(step.n_hypotheses.unwrap_or(3)),
                    );
                    if let Some(tid) = &step.adversarial_task_id {
                        m.insert("__adversarial_task_id".to_string(), serde_json::json!(tid));
                    }
                }
                Some(a)
            } else {
                step.arguments.clone()
            };

            // Inject required_capabilities for any step kind — the model router
            // reads it at execution time; tools ignore it via serde defaults.
            let effective_args: Option<serde_json::Value> = match &step.required_capabilities {
                Some(caps) if !caps.is_empty() => {
                    let mut a = effective_args.unwrap_or(serde_json::json!({}));
                    if let serde_json::Value::Object(ref mut m) = a {
                        m.insert(
                            "__required_capabilities".to_string(),
                            serde_json::json!(caps),
                        );
                    }
                    Some(a)
                }
                _ => effective_args,
            };

            // Inject distilled-handoff metadata. Read by execute_job before
            // {{_prev}} substitution; tools ignore it via serde defaults.
            let effective_args: Option<serde_json::Value> = if step.distill_prev {
                let mut a = effective_args.unwrap_or(serde_json::json!({}));
                if let serde_json::Value::Object(ref mut m) = a {
                    m.insert("__distill_prev".to_string(), serde_json::json!(true));
                    m.insert(
                        "__distill_max_chars".to_string(),
                        serde_json::json!(
                            step.distill_max_chars.unwrap_or(DEFAULT_DISTILL_MAX_CHARS)
                        ),
                    );
                    if let Some(focus) = &step.distill_focus {
                        m.insert("__distill_focus".to_string(), serde_json::json!(focus));
                    }
                }
                Some(a)
            } else {
                effective_args
            };

            // Stamp the owning task id onto every step so the coordinator can
            // attribute a chain death back to its Task and fail it with a reason.
            let effective_args: Option<serde_json::Value> = match owner_task_id {
                Some(tid) => {
                    let mut a = effective_args.unwrap_or(serde_json::json!({}));
                    if let serde_json::Value::Object(ref mut m) = a {
                        m.insert("__owner_task_id".to_string(), serde_json::json!(tid));
                    }
                    Some(a)
                }
                None => effective_args,
            };

            let id = if i == 0 {
                let job = self
                    .neo4j
                    .create_agent_job(
                        &step.tool_name,
                        effective_args.as_ref(),
                        priority,
                        max_attempts,
                        session_id,
                        None,
                        step.provider_hint.as_deref(),
                        step.context_profile.as_deref(),
                        step.description.as_deref(),
                        step.ttl_secs,
                    )
                    .await
                    .map_err(|e| e.to_string())?;
                let id = job.id.clone();
                first_job = Some(job); // reuse the created record for the heap push below
                id
            } else {
                self.neo4j
                    .create_agent_job_parked(
                        &step.tool_name,
                        effective_args.as_ref(),
                        priority,
                        max_attempts,
                        session_id,
                        prev_id.as_deref().unwrap(),
                        step.provider_hint.as_deref(),
                        step.context_profile.as_deref(),
                        step.description.as_deref(),
                        step.ttl_secs,
                    )
                    .await
                    .map_err(|e| e.to_string())?
            };

            prev_id = Some(id.clone());
            ids.push(id);
        }

        // Push the first job to the in-memory heap — no reload needed.
        if let Some(job) = first_job {
            self.heap.lock().await.push(PrioritizedJob {
                priority: job.priority,
                created_at: job.created_at.clone(),
                job,
            });
            self.notify.notify_one();
        }

        info!(steps = ids.len(), "Enqueued job chain");
        Ok(ids)
    }

    /// Cancel a job by ID.  Returns `true` if the job was found and cancelled.
    pub async fn cancel(&self, job_id: &str) -> Result<bool, String> {
        let job = self
            .neo4j
            .get_agent_job(job_id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(job) = job else { return Ok(false) };

        if matches!(
            job.status,
            AgentJobStatus::Completed | AgentJobStatus::Dead | AgentJobStatus::Cancelled
        ) {
            return Ok(false);
        }

        self.neo4j
            .update_agent_job_status(job_id, AgentJobStatus::Cancelled)
            .await
            .map_err(|e| e.to_string())?;

        // Cancel any parked chain children — they can never run without this parent.
        let _ = self.neo4j.cancel_parked_children(job_id).await;

        // Lazy removal from heap via tombstone.
        self.cancelled_ids.lock().await.insert(job_id.to_string());
        Ok(true)
    }

    /// Retry a failed, dead, or cancelled job.
    pub async fn retry(&self, job_id: &str) -> Result<bool, String> {
        let job = self
            .neo4j
            .get_agent_job(job_id)
            .await
            .map_err(|e| e.to_string())?;
        let Some(job) = job else { return Ok(false) };

        if !matches!(
            job.status,
            AgentJobStatus::Failed | AgentJobStatus::Dead | AgentJobStatus::Cancelled
        ) {
            return Ok(false);
        }

        self.neo4j
            .retry_agent_job(job_id)
            .await
            .map_err(|e| e.to_string())?;

        // Remove from tombstone set if it was there.
        self.cancelled_ids.lock().await.remove(job_id);

        if let Ok(Some(refreshed)) = self.neo4j.get_agent_job(job_id).await {
            self.heap.lock().await.push(PrioritizedJob {
                priority: refreshed.priority,
                created_at: refreshed.created_at.clone(),
                job: refreshed,
            });
            self.notify.notify_one();
        }
        Ok(true)
    }

    /// Cancel all queued jobs. Returns the number cancelled.
    ///
    /// Drains the in-memory heap *and* cancels any `queued` job in Neo4j that was never
    /// loaded into the heap (e.g. added directly to the DB) — otherwise the next periodic
    /// reload would resurrect it right after a drain.
    pub async fn drain(&self) -> Result<usize, String> {
        // 1. Drain the heap and tombstone those ids so any in-flight pop skips them.
        let jobs: Vec<AgentJob> = {
            let mut heap = self.heap.lock().await;
            heap.drain().map(|pj| pj.job).collect()
        };
        {
            let mut set = self.cancelled_ids.lock().await;
            for job in &jobs {
                set.insert(job.id.clone());
            }
        }

        // 2. Cancel every queued job in Neo4j (covers heap jobs and any queued job missing
        //    from the heap), collecting ids so their parked chain children can be cancelled.
        let now = chrono::Utc::now().to_rfc3339();
        let cypher = "MATCH (j:AgentJob {status: 'queued'}) \
                      SET j.status = 'cancelled', j.updated_at = datetime($now) \
                      RETURN collect(j.id) AS ids";
        let cancelled_ids: Vec<String> = match self
            .neo4j
            .execute(neo4rs::query(cypher).param("now", now))
            .await
        {
            Ok(rows) => rows
                .first()
                .and_then(|r| r.get::<Vec<String>>("ids").ok())
                .unwrap_or_default(),
            Err(e) => return Err(e.to_string()),
        };

        // 3. Cascade-cancel parked children of every cancelled job.
        for id in &cancelled_ids {
            let _ = self.neo4j.cancel_parked_children(id).await;
        }

        Ok(cancelled_ids.len())
    }

    /// Fetch a single job record from Neo4j.
    pub async fn get_job(&self, id: &str) -> Option<AgentJob> {
        self.neo4j.get_agent_job(id).await.ok().flatten()
    }

    /// Update the runtime worker configuration.  Returns the new config.
    ///
    /// Per-provider semaphore sizes are updated by swapping in a new semaphore with
    /// the requested capacity.  Jobs already holding a permit from the old semaphore
    /// continue unaffected; new jobs pick up the replacement.
    pub async fn update_config(
        &self,
        max_concurrent: Option<usize>,
        max_concurrent_ollama: Option<usize>,
        max_concurrent_anthropic: Option<usize>,
        max_concurrent_gemini: Option<usize>,
        enabled: Option<bool>,
        poll_interval_secs: Option<u64>,
    ) -> WorkerConfig {
        let mut cfg = self.config.write().await;
        if let Some(v) = max_concurrent {
            cfg.max_concurrent = v;
        }
        if let Some(v) = max_concurrent_ollama {
            cfg.max_concurrent_ollama = v;
            *self.semaphore_ollama.write().await = Arc::new(Semaphore::new(v));
        }
        if let Some(v) = max_concurrent_anthropic {
            cfg.max_concurrent_anthropic = v;
            *self.semaphore_anthropic.write().await = Arc::new(Semaphore::new(v));
        }
        if let Some(v) = max_concurrent_gemini {
            cfg.max_concurrent_gemini = v;
            *self.semaphore_gemini.write().await = Arc::new(Semaphore::new(v));
        }
        if let Some(v) = enabled {
            cfg.enabled = v;
            if v {
                // Re-enable: wake coordinator in case there are queued jobs.
                self.notify.notify_one();
            }
        }
        if let Some(v) = poll_interval_secs {
            cfg.poll_interval_secs = v;
        }
        cfg.clone()
    }

    /// Return queue statistics (in-memory + Neo4j).
    pub async fn stats(&self) -> serde_json::Value {
        let db_stats = self
            .neo4j
            .get_queue_stats()
            .await
            .unwrap_or(serde_json::json!({}));
        let provider_stats = self
            .neo4j
            .get_provider_stats()
            .await
            .unwrap_or(serde_json::json!({}));
        let heap_len = self.heap.lock().await.len();
        let cfg = self.config.read().await;

        let avail_ollama = self.semaphore_ollama.read().await.available_permits();
        let avail_anthropic = self.semaphore_anthropic.read().await.available_permits();
        let avail_gemini = self.semaphore_gemini.read().await.available_permits();
        let running_ollama = cfg.max_concurrent_ollama.saturating_sub(avail_ollama);
        let running_anthropic = cfg.max_concurrent_anthropic.saturating_sub(avail_anthropic);
        let running_gemini = cfg.max_concurrent_gemini.saturating_sub(avail_gemini);

        let coordinator_alive = self.coordinator_alive.load(Ordering::Relaxed);
        let hb_ts = self.coordinator_last_heartbeat.load(Ordering::Relaxed);
        let coordinator_last_heartbeat = if hb_ts >= 0 {
            chrono::DateTime::from_timestamp(hb_ts, 0)
                .map(|dt: chrono::DateTime<chrono::Utc>| dt.to_rfc3339())
        } else {
            None
        };
        let orphan_audit = self.last_orphan_audit_count.load(Ordering::Relaxed);

        serde_json::json!({
            "coordinator": {
                "alive": coordinator_alive,
                "last_heartbeat": coordinator_last_heartbeat,
            },
            "orphan_audit": {
                "last_cancelled": if orphan_audit >= 0 { serde_json::json!(orphan_audit) } else { serde_json::Value::Null },
            },
            "in_memory_pending": heap_len,
            "running_now": running_ollama + running_anthropic + running_gemini,
            "max_concurrent": cfg.max_concurrent,
            "enabled": cfg.enabled,
            "poll_interval_secs": cfg.poll_interval_secs,
            "per_provider": {
                "ollama": { "running": running_ollama, "max": cfg.max_concurrent_ollama },
                "anthropic": { "running": running_anthropic, "max": cfg.max_concurrent_anthropic },
                "gemini": { "running": running_gemini, "max": cfg.max_concurrent_gemini },
            },
            "by_status": db_stats,
            "provider_stats": provider_stats,
        })
    }

    /// List agent jobs from Neo4j, optionally filtered by status.
    /// Returns up to `limit` jobs ordered by created_at DESC.
    pub async fn list_jobs(
        &self,
        status: Option<&str>,
        limit: usize,
    ) -> Result<Vec<crate::models::AgentJob>, crate::repository::RepositoryError> {
        self.neo4j.list_agent_jobs(status, limit).await
    }

    // =========================================================================
    // Progress tracking
    // =========================================================================

    /// Update progress for a running job.
    pub async fn update_progress(
        &self,
        job_id: &str,
        percent: u8,
        message: Option<&str>,
        metadata: Option<&serde_json::Value>,
    ) -> Result<(), String> {
        self.neo4j
            .update_job_progress(job_id, percent, message, metadata)
            .await
            .map_err(|e| e.to_string())
    }

    /// Get progress for a job.
    pub async fn get_job_progress(&self, job_id: &str) -> Result<Option<JobProgressTuple>, String> {
        self.neo4j
            .get_job_progress(job_id)
            .await
            .map_err(|e| e.to_string())
    }

    // =========================================================================
    // TTL and expiration
    // =========================================================================

    /// Expire jobs that have exceeded their TTL.
    pub async fn expire_jobs(&self) -> Result<usize, String> {
        self.neo4j.expire_jobs().await.map_err(|e| e.to_string())
    }

    // =========================================================================
    // Dead Letter Queue
    // =========================================================================

    /// List jobs in the dead letter queue.
    pub async fn list_dead_letter(
        &self,
        limit: usize,
    ) -> Result<Vec<crate::models::AgentJob>, String> {
        self.neo4j
            .list_dead_letter(limit)
            .await
            .map_err(|e| e.to_string())
    }

    /// Retry a job from the dead letter queue.
    pub async fn retry_dead_letter(&self, job_id: &str) -> Result<bool, String> {
        self.neo4j
            .retry_dead_letter(job_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// Permanently delete a dead letter entry.
    pub async fn delete_dead_letter(&self, job_id: &str) -> Result<bool, String> {
        self.neo4j
            .delete_dead_letter(job_id)
            .await
            .map_err(|e| e.to_string())
    }

    /// Get dead letter queue statistics.
    pub async fn get_dead_letter_stats(&self) -> Result<serde_json::Value, String> {
        self.neo4j
            .get_dead_letter_stats()
            .await
            .map_err(|e| e.to_string())
    }

    // =========================================================================
    // Cleanup
    // =========================================================================

    /// Clean up old completed, failed, dead, and dead-letter jobs.
    pub async fn cleanup_old_jobs(&self) -> Result<usize, String> {
        // Default: keep completed for 1 day; failed/dead for 7 days.
        let mut total = self
            .neo4j
            .cleanup_old_jobs(24 * 3600, 7 * 24 * 3600)
            .await
            .map_err(|e| e.to_string())?;
        // Dead-letter entries are kept longer (30 days) since they carry
        // failure forensics, but must not accumulate forever either.
        total += self
            .neo4j
            .cleanup_old_dead_letter(30 * 24 * 3600)
            .await
            .map_err(|e| e.to_string())?;
        Ok(total)
    }

    // =========================================================================
    // Coordinator
    // =========================================================================

    /// Spawn the background coordinator task.
    /// Must be called **after** `tool_handler` has been populated (i.e. after
    /// `McpServerCore::build_skills()`).
    pub fn spawn_coordinator(queue: Arc<QueueService>) {
        let queue_coordinator = Arc::clone(&queue);
        tokio::spawn(async move {
            queue_coordinator.run_coordinator().await;
        });
        // Spawn periodic TTL expiration check (every 60 seconds).
        let queue_ttl = Arc::clone(&queue);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(60));
            loop {
                interval.tick().await;
                if let Err(e) = queue_ttl.expire_jobs().await {
                    warn!("TTL expiration check failed: {}", e);
                }
            }
        });
        // Spawn periodic cleanup (every 5 minutes).
        let queue_cleanup = Arc::clone(&queue);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                if let Err(e) = queue_cleanup.cleanup_old_jobs().await {
                    warn!("Periodic cleanup failed: {}", e);
                }
            }
        });
        // Spawn periodic orphan-chain audit (every 5 minutes).
        // Cancels any parked jobs whose parent is terminal/missing, and records the count.
        let queue_orphan = Arc::clone(&queue);
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(300));
            loop {
                interval.tick().await;
                match queue_orphan.neo4j.cancel_orphaned_parked_jobs().await {
                    Ok(n) => {
                        queue_orphan
                            .last_orphan_audit_count
                            .store(n as i64, Ordering::Relaxed);
                        if n > 0 {
                            warn!(count = n, "Orphan-chain audit cancelled stuck parked jobs");
                        }
                    }
                    Err(e) => warn!("Orphan-chain audit failed: {}", e),
                }
            }
        });
    }

    async fn run_coordinator(self: Arc<Self>) {
        info!("AgentJob coordinator started");
        self.coordinator_alive.store(true, Ordering::Relaxed);
        loop {
            self.coordinator_last_heartbeat
                .store(chrono::Utc::now().timestamp(), Ordering::Relaxed);
            let poll_secs = self.config.read().await.poll_interval_secs;

            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(poll_secs)) => {
                    // Periodic sync: pick up any jobs added directly to Neo4j.
                    self.reload_from_neo4j().await;
                }
            }

            // Drain the heap while capacity is available. Jobs whose provider semaphore is
            // saturated are set aside (not pushed back immediately) so a job for a different
            // provider queued behind them can still be dispatched this pass — no head-of-line
            // blocking across providers. Set-aside jobs return to the heap after the scan.
            let mut blocked: Vec<PrioritizedJob> = Vec::new();
            loop {
                if !self.config.read().await.enabled {
                    break;
                }

                let pjob = { self.heap.lock().await.pop() };
                let Some(pjob) = pjob else { break };

                // Skip tombstoned (cancelled) jobs.
                {
                    let mut set = self.cancelled_ids.lock().await;
                    if set.remove(&pjob.job.id) {
                        continue;
                    }
                }

                // Pick the semaphore based on provider_hint.
                // Read the current inner Arc so that runtime config changes
                // (semaphore swaps) take effect on the next job dispatch.
                let semaphore: Arc<Semaphore> = {
                    let lock = match pjob.job.provider_hint.as_deref() {
                        Some("anthropic") => self.semaphore_anthropic.read().await,
                        Some("gemini") => self.semaphore_gemini.read().await,
                        _ => self.semaphore_ollama.read().await,
                    };
                    Arc::clone(&*lock)
                };

                // Try to acquire a concurrency slot (non-blocking).
                let permit = match semaphore.try_acquire_owned() {
                    Ok(p) => p,
                    Err(_) => {
                        // This provider is at capacity — set the job aside and keep scanning
                        // so other providers with free slots are not blocked behind it.
                        blocked.push(pjob);
                        continue;
                    }
                };

                let svc = Arc::clone(&self);
                tokio::spawn(async move {
                    let notify = Arc::clone(&svc.notify);
                    let permit_guard = permit; // holds the concurrency slot
                    svc.execute_job(pjob.job).await;
                    // Release the slot, then wake the coordinator so a queued job can take the
                    // freed slot immediately instead of waiting for the poll interval.
                    drop(permit_guard);
                    notify.notify_one();
                });
            }

            // Return set-aside jobs to the heap for the next wakeup / permit release.
            if !blocked.is_empty() {
                let mut heap = self.heap.lock().await;
                for pjob in blocked {
                    heap.push(pjob);
                }
            }
        }
    }

    /// Reload queued jobs from Neo4j that are not already in the heap.
    async fn reload_from_neo4j(self: &Arc<Self>) {
        match self.neo4j.list_queued_agent_jobs().await {
            Ok(jobs) if !jobs.is_empty() => {
                let mut heap = self.heap.lock().await;
                let existing: HashSet<_> = heap.iter().map(|pj| pj.job.id.clone()).collect();
                let mut added = 0usize;
                for job in jobs {
                    if !existing.contains(&job.id) {
                        heap.push(PrioritizedJob {
                            priority: job.priority,
                            created_at: job.created_at.clone(),
                            job,
                        });
                        added += 1;
                    }
                }
                if added > 0 {
                    debug!(count = added, "Reloaded missed jobs from Neo4j");
                }
            }
            Ok(_) => {}
            Err(e) => warn!("Periodic Neo4j reload failed: {}", e),
        }
    }

    // =========================================================================
    // Job execution
    // =========================================================================

    /// Emit a brain event for a job status change.
    ///
    /// Ignores send errors — having no subscribers is fine.
    fn emit_job_event(&self, event: BrainEvent) {
        if let Some(tx) = &self.event_tx {
            let _ = tx.send(event);
        }
    }

    /// Promote any parked children of `parent_id` to queued and push them onto the heap.
    /// `prev_result_text` is the plain-text output of the completing job; it is stamped
    /// onto each child so `{{_prev}}` can be resolved when the child executes.
    /// `prev_result_raw` is the structured envelope the plain-text extraction
    /// discarded, so `{{_prev.<path>}}` can reach it; `""` when there is none.
    async fn unpark_and_enqueue_children(
        self: &Arc<Self>,
        parent_id: &str,
        prev_result_text: &str,
        prev_result_raw: &str,
    ) {
        match self
            .neo4j
            .unpark_children(parent_id, prev_result_text, prev_result_raw)
            .await
        {
            Ok(children) if !children.is_empty() => {
                let mut heap = self.heap.lock().await;
                for child in children {
                    heap.push(PrioritizedJob {
                        priority: child.priority,
                        created_at: child.created_at.clone(),
                        job: child,
                    });
                }
                self.notify.notify_one();
            }
            Ok(_) => {}
            Err(e) => warn!(parent = %parent_id, "Failed to unpark chain children: {}", e),
        }
    }

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

    /// Raise a user-facing notification that an external API's quota is spent.
    ///
    /// Deduped to once per tool per 24h against existing `:AgentNotification`
    /// nodes — the daily news chain alone would otherwise fire eight identical
    /// alerts the moment a search quota dies.
    async fn notify_quota_exhausted(&self, tool_name: &str, error_text: &str) {
        let context = format!("quota_exhausted:{tool_name}");
        let dedupe = "MATCH (n:AgentNotification) \
                      WHERE n.context = $ctx \
                        AND datetime(n.created_at) >= datetime() - duration({hours: 24}) \
                      RETURN count(n) AS c";
        let already = match self
            .neo4j
            .execute(neo4rs::query(dedupe).param("ctx", context.clone()))
            .await
        {
            Ok(rows) => {
                rows.first()
                    .and_then(|r| r.get::<i64>("c").ok())
                    .unwrap_or(0)
                    > 0
            }
            Err(e) => {
                warn!("quota notification dedupe check failed (notifying anyway): {e}");
                false
            }
        };
        if already {
            debug!(tool = %tool_name, "Quota-exhaustion notification already raised in the last 24h");
            return;
        }

        let message = format!(
            "⚠️ External API quota exhausted — `{tool_name}` is failing and dependent \
             scheduled work (news briefs, research chains) will keep dying until it is \
             restored.\n\nLast error:\n{}\n\nRun `get_search_usage` to see per-engine burn \
             rate, then either restore the quota or reorder `SEARCH_ENGINE_ORDER`.",
            error_text.chars().take(500).collect::<String>()
        );
        let id = uuid::Uuid::new_v4().to_string();
        if let Err(e) = self
            .neo4j
            .create_notification(
                &id,
                &message,
                Some(&context),
                None,
                &chrono::Utc::now().to_rfc3339(),
            )
            .await
        {
            warn!(tool = %tool_name, error = %e, "Failed to create quota-exhaustion notification");
            return;
        }
        warn!(tool = %tool_name, notification_id = %id, "Raised quota-exhaustion notification");
        if let Some(ref tx) = self.event_tx {
            let _ = tx.send(BrainEvent::AgentChatInitiated {
                notification_id: id,
                message,
                related_session_id: None,
            });
        }
    }

    /// Compress the previous step's output into a compact handoff for this step,
    /// or return `None` to use the raw text unchanged.
    ///
    /// Every failure mode falls back to the raw text: distillation is a token
    /// optimization, and losing it must never fail a job. Runs on the local model
    /// only, so the compression call itself is free.
    async fn maybe_distill_prev(&self, job: &AgentJob, prev_text: &str) -> Option<String> {
        let args = job.arguments.as_ref()?;
        if args.get("__distill_prev").and_then(|v| v.as_bool()) != Some(true) {
            return None;
        }

        let max_chars = args
            .get("__distill_max_chars")
            .and_then(|v| v.as_u64())
            .map(|n| n as usize)
            .unwrap_or(DEFAULT_DISTILL_MAX_CHARS);

        // Character counts throughout — byte length would misjudge non-ASCII
        // transcripts, which this path sees regularly via the media chains.
        let prev_chars = prev_text.chars().count();

        // Already compact enough — don't spend a generation call to save nothing.
        if prev_chars <= max_chars {
            return None;
        }

        let cfg = self.distill_config.read().await.clone()?;
        let llm = match crate::services::LlmClient::with_config(
            cfg.with_num_ctx(DISTILL_NUM_CTX).with_temperature(0.2),
        ) {
            Ok(c) => c,
            Err(e) => {
                warn!(job_id = %job.id, "Distiller client build failed — passing raw output: {}", e);
                return None;
            }
        };

        // Cap the distiller's own input. Keep the head and the tail: conclusions
        // usually sit at the end, so head-only truncation drops the payload.
        let input: String = if prev_chars > DISTILL_INPUT_CAP_CHARS {
            let head_len = DISTILL_INPUT_CAP_CHARS * 2 / 3;
            let tail_len = DISTILL_INPUT_CAP_CHARS - head_len;
            let head: String = prev_text.chars().take(head_len).collect();
            let tail: String = prev_text
                .chars()
                .skip(prev_chars.saturating_sub(tail_len))
                .collect();
            format!("{head}\n\n[… middle omitted …]\n\n{tail}")
        } else {
            prev_text.to_string()
        };

        let focus = args
            .get("__distill_focus")
            .and_then(|v| v.as_str())
            .unwrap_or("the findings, claims, and specifics the next step must reason over");

        let system = "You are a text compressor in a data pipeline. You are not a \
chat assistant and you are not talking to a person. You receive one pipeline \
step's output and emit a shorter version of that same content for the next step \
to read. Keep concrete specifics — names, numbers, URLs, identifiers, file \
paths, error strings — verbatim; never round or paraphrase them. Drop preamble, \
repetition, and formatting scaffolding. Never add facts or commentary that are \
not in the input. Never address the reader, never describe what you did, and \
never ask a question. Emit only the compressed content.";

        // The instruction is repeated *after* the payload as well as before it.
        // A small local model that reads a large blob first will otherwise answer
        // the blob instead of compressing it — observed as a chatty "I have
        // processed the updates, how can I help?" response during development.
        let prompt = format!(
            "Compress the following pipeline output to at most {max_chars} characters.\n\
Keep only what the next step (`{tool}`) needs: {focus}\n\n\
--- BEGIN PIPELINE OUTPUT ---\n{input}\n--- END PIPELINE OUTPUT ---\n\n\
Now emit the compressed version of the text between the markers above, in at \
most {max_chars} characters, keeping: {focus}\n\
Output the compressed content only — no greeting, no preamble, no offer to help.",
            tool = job.tool_name,
        );

        let distilled = match llm.generate_with_system(&prompt, Some(system)).await {
            Ok(r) => r.text.trim().to_string(),
            Err(e) => {
                warn!(job_id = %job.id, "Handoff distillation failed — passing raw output: {}", e);
                return None;
            }
        };

        // A distiller that returned nothing, or that somehow grew the payload,
        // has produced no saving worth the fidelity loss.
        let distilled_chars = distilled.chars().count();
        if distilled.is_empty() || distilled_chars >= prev_chars {
            debug!(job_id = %job.id, "Handoff distillation produced no saving — passing raw output");
            return None;
        }

        info!(
            job_id = %job.id,
            tool = %job.tool_name,
            before = prev_chars,
            after = distilled_chars,
            "Distilled chain handoff"
        );
        Some(distilled)
    }

    async fn execute_job(self: Arc<Self>, job: AgentJob) {
        info!(job_id = %job.id, tool = %job.tool_name, priority = job.priority, "Executing AgentJob");

        match self.neo4j.set_job_started(&job.id).await {
            Ok(true) => {}
            Ok(false) => {
                // The job left 'queued' between heap-pop and start — cancelled, or already
                // taken by a concurrent path. Do not execute it.
                info!(job_id = %job.id, "Job no longer queued (cancelled or already started) — skipping execution");
                return;
            }
            Err(e) => {
                error!(job_id = %job.id, "Failed to mark job running: {}", e);
                return;
            }
        }

        let handler_guard = self.tool_handler.read().await;
        let Some(ref handler) = *handler_guard else {
            warn!(job_id = %job.id, "No tool handler — job cannot execute");
            let _ = self
                .neo4j
                .set_job_failed(&job.id, "Tool handler not available")
                .await;
            return;
        };

        // Resolve {{_prev}} / {{result}} in arguments if the job carries a prior step result.
        // {{result}} is treated as an alias for {{_prev}} — brain-generated chains often use
        // the more natural name.
        //
        // Two forms, resolved in order of specificity:
        //   {{_prev.id}}  — a path into the previous output parsed as JSON
        //   {{_prev}}     — the previous output pasted whole
        // The two can never collide: "{{_prev}}" is not a substring of
        // "{{_prev.id}}" (the char after `_prev` is `.`, not `}`).
        let resolved_args = match &job.prev_result {
            Some(prev_text) => {
                let (wants_whole, wants_path) = job
                    .arguments
                    .as_ref()
                    .map(|a| {
                        let s = a.to_string();
                        (
                            s.contains("{{_prev}}") || s.contains("{{result}}"),
                            s.contains("{{_prev.") || s.contains("{{result."),
                        )
                    })
                    .unwrap_or((false, false));

                let mut args = job.arguments.clone();

                // Paths first, and always against the structured envelope, never
                // the extracted text: `{{_prev}}` is the *unwrapped* result (the
                // note body), so the sibling fields a path wants — `id` above all
                // — exist only in `prev_result_raw`. Distillation is skipped here
                // too, since it rewrites prose and would destroy the structure.
                if wants_path {
                    let root = job.prev_result_raw.as_deref().unwrap_or(prev_text);
                    args = args.map(|a| substitute_prev_paths(&a, root));
                }

                // Distilled handoff: when the step opted in, compress the upstream
                // output down to what this step needs before it lands in the prompt.
                // Returns None (use the raw text) whenever distillation is off, the
                // payload is already small, or the compression call fails.
                if wants_whole {
                    let distilled = self.maybe_distill_prev(&job, prev_text).await;
                    let effective_prev = distilled.as_deref().unwrap_or(prev_text.as_str());
                    args = args.map(|a| substitute_prev(&a, effective_prev));
                }

                args
            }
            None => job.arguments.clone(),
        };

        // Per-step model routing: when the step declared required_capabilities,
        // resolve a model within the active cloud tier. Falls back to normal
        // routing (None) when nothing qualifies or telemetry is unavailable.
        let selected_llm: Option<crate::services::LlmConfig> = job
            .arguments
            .as_ref()
            .and_then(|a| a.get("__required_capabilities"))
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|c| c.as_str().map(String::from))
                    .collect::<Vec<String>>()
            })
            .filter(|caps| !caps.is_empty())
            .and_then(|caps| {
                handler.telemetry().and_then(|tc| {
                    // Base config only contributes timeout/temperature defaults;
                    // provider, model, URL, and key are set by the router.
                    crate::services::model_router::resolve_model_config(
                        tc,
                        &caps,
                        &crate::services::LlmConfig::default(),
                    )
                })
            });

        // Run the tool call inside task-local scopes so `SharedLlm` can route:
        // SELECTED_LLM (capability-resolved) takes precedence over USE_LOCAL_LLM
        // (background default), which takes precedence over the active config.
        let use_local = job.provider_hint.as_deref() == Some("ollama");
        let result = SELECTED_LLM
            .scope(
                selected_llm,
                USE_LOCAL_LLM.scope(
                    use_local,
                    CURRENT_TOOL.scope(
                        Some(job.tool_name.clone()),
                        handler.execute(&job.tool_name, resolved_args),
                    ),
                ),
            )
            .await;
        // Drop the read lock before any awaits below.
        drop(handler_guard);

        let is_error = result.is_error.unwrap_or(false);
        let result_json = serde_json::to_string(&result).unwrap_or_default();

        // Extract plain text from the result to pass to child steps via {{_prev}}.
        let result_text = extract_result_text(&result);

        if !is_error {
            match self.neo4j.set_job_completed(&job.id, &result_json).await {
                Err(e) => {
                    error!(job_id = %job.id, "Failed to store completed result: {}", e);
                }
                Ok(false) => {
                    // The job was cancelled while executing: set_job_completed's
                    // status='running' guard matched nothing. Do NOT unpark chain children
                    // or emit a completion event for a job the user cancelled.
                    info!(job_id = %job.id, "Job cancelled during execution — chain children not promoted");
                }
                Ok(true) => {
                    info!(job_id = %job.id, "AgentJob completed");

                    // Evaluator step: parse score and re-queue the parent task if below threshold.
                    // When score fails, cancel parked children (e.g. update_task) so the task is
                    // not prematurely marked completed — the retry task created by
                    // handle_evaluator_requeue will drive the next attempt.
                    let evaluator_blocked = if let Some(min_score) = job
                        .arguments
                        .as_ref()
                        .and_then(|a| a.get("__evaluator_min_score"))
                        .and_then(|v| v.as_f64())
                    {
                        let task_id = job
                            .arguments
                            .as_ref()
                            .and_then(|a| a.get("__evaluator_task_id"))
                            .and_then(|v| v.as_str())
                            .map(String::from);

                        match parse_evaluator_score(&result_text) {
                            None => {
                                // No parseable score or verdict keyword. Treating this as a
                                // failing 3.0 (the old behaviour) burned up to 3 retry chains on
                                // mere format drift from the local model. Treat unparseable output
                                // as a pass, and do NOT grade the AgentSpec with an invented number.
                                warn!(
                                    job_id = %job.id,
                                    "Evaluator output unparseable (no Score line or verdict) — treating as pass"
                                );
                                false
                            }
                            Some(score) => {
                                // Phase 3 learning edge: if this task was built by the Agent
                                // Constructor, record the graded outcome on its AgentSpec —
                                // pass AND fail (failures are the more valuable signal).
                                if let Some(tid) = &task_id {
                                    match self
                                        .neo4j
                                        .record_agent_spec_performance(
                                            tid,
                                            score as f64,
                                            score >= min_score as f32,
                                        )
                                        .await
                                    {
                                        Ok(true) => {
                                            info!(task_id = %tid, score = score, "Recorded AgentSpec PERFORMED edge")
                                        }
                                        Ok(false) => {} // not a constructed task — nothing to learn onto
                                        Err(e) => {
                                            warn!(task_id = %tid, error = %e, "Failed to record AgentSpec performance")
                                        }
                                    }
                                }

                                if score < min_score as f32 {
                                    warn!(
                                        job_id = %job.id,
                                        score = score,
                                        min_score = min_score,
                                        "Evaluator: score below threshold — cancelling downstream steps and re-queuing task"
                                    );
                                    let _ = self.neo4j.cancel_parked_children(&job.id).await;
                                    if let Some(tid) = &task_id {
                                        self.handle_evaluator_requeue(tid, score, &result_text)
                                            .await;
                                    }
                                    true
                                } else {
                                    info!(job_id = %job.id, score = score, "Evaluator: score passed");
                                    false
                                }
                            }
                        }
                    } else {
                        false
                    };

                    // Adversarial pre-flight: parse overall_robustness and abort the chain
                    // if the plan is too risky.  Mirrors the evaluator gate but fires on
                    // the first step rather than the last.
                    let adversarial_blocked = if let Some(min_robustness) = job
                        .arguments
                        .as_ref()
                        .and_then(|a| a.get("__adversarial_min_robustness"))
                        .and_then(|v| v.as_f64())
                    {
                        let robustness = parse_adversarial_robustness(&result_text);
                        let task_id = job
                            .arguments
                            .as_ref()
                            .and_then(|a| a.get("__adversarial_task_id"))
                            .and_then(|v| v.as_str())
                            .map(String::from);

                        if robustness < min_robustness as f32 {
                            warn!(
                                job_id = %job.id,
                                robustness,
                                min_robustness,
                                "Adversarial: robustness below threshold — cancelling chain and re-queuing task"
                            );
                            let _ = self.neo4j.cancel_parked_children(&job.id).await;
                            if let Some(tid) = &task_id {
                                self.handle_adversarial_requeue(tid, robustness, &result_text)
                                    .await;
                            }
                            true
                        } else {
                            info!(job_id = %job.id, robustness, "Adversarial: robustness passed — proceeding");
                            false
                        }
                    } else {
                        false
                    };

                    // Promote any chained children waiting on this job, unless the evaluator
                    // or adversarial gate already cancelled them.
                    if !evaluator_blocked && !adversarial_blocked {
                        let raw = structured_prev_to_preserve(&result, &result_text);
                        self.unpark_and_enqueue_children(&job.id, &result_text, &raw)
                            .await;
                    }
                    self.emit_job_event(BrainEvent::JobCompleted {
                        job_id: job.id.clone(),
                        tool_name: job.tool_name.clone(),
                        session_id: job.session_id.clone(),
                        result_preview: Some(result_text.chars().take(200).collect()),
                    });
                }
            }
        } else {
            let error_text = result
                .content
                .first()
                .and_then(|c| {
                    if let Content::Text { text } = c {
                        Some(text.clone())
                    } else {
                        None
                    }
                })
                .unwrap_or_else(|| "Unknown error".to_string());

            // Re-fetch to get the updated attempt_count (set by set_job_started).
            let (attempt, max) = if let Ok(Some(updated)) = self.neo4j.get_agent_job(&job.id).await
            {
                (updated.attempt_count, updated.max_attempts)
            } else {
                (job.attempt_count + 1, job.max_attempts)
            };

            if attempt >= max {
                match self.neo4j.set_job_dead(&job.id, &error_text).await {
                    Ok(true) => {}
                    Ok(false) => {
                        // Cancelled while executing: the status='running' guard matched
                        // nothing. Don't dead-letter, reflect, or meta-learn a cancelled job.
                        info!(job_id = %job.id, "Job cancelled during execution — not dead-lettering");
                        return;
                    }
                    Err(e) => {
                        warn!(job_id = %job.id, error = %e, "Failed to mark job dead");
                        return;
                    }
                }
                warn!(job_id = %job.id, attempts = attempt, "AgentJob exhausted retries → dead");
                // Parent chain is broken — cancel any waiting children.
                let _ = self.neo4j.cancel_parked_children(&job.id).await;

                // If this job belongs to a scheduler-dispatched task chain, fail
                // that Task now with the real error instead of leaving it stuck
                // in_progress until the 6-hour stale reaper flips it blind. This
                // is the concrete failure signal capability-mining reasons over.
                if let Some(owner_task_id) = job
                    .arguments
                    .as_ref()
                    .and_then(|a| a.get("__owner_task_id"))
                    .and_then(|v| v.as_str())
                {
                    let reason = format!(
                        "Chain step '{}' died after {}/{} attempts. Last error: {}",
                        job.tool_name,
                        attempt,
                        max,
                        error_text.chars().take(500).collect::<String>(),
                    );
                    match self
                        .neo4j
                        .fail_task_with_reason(owner_task_id, &reason)
                        .await
                    {
                        Ok(true) => {
                            info!(task_id = %owner_task_id, tool = %job.tool_name, "Marked owning task failed with error context")
                        }
                        // Already terminal (e.g. completed, or failed via evaluator) — leave it.
                        Ok(false) => {}
                        Err(e) => {
                            warn!(task_id = %owner_task_id, error = %e, "Failed to record chain-death reason on owning task")
                        }
                    }
                }
                // Store a reflection note so the brain can learn from this failure.
                let reflection_content = format!(
                    "Dead job: tool '{}' (job_id: {}) exhausted {}/{} attempts and was marked dead.\n\
                     Last error: {}\n\
                     This is an automated failure record. Investigate the tool definition, \
                     its input arguments, or any external dependencies to prevent recurrence.",
                    job.tool_name, job.id, attempt, max, error_text
                );
                if let Err(e) = self
                    .neo4j
                    .store_reflection_note(&reflection_content, None)
                    .await
                {
                    warn!(job_id = %job.id, error = %e, "Failed to store dead-job reflection note");
                } else {
                    debug!(job_id = %job.id, tool = %job.tool_name, "Stored dead-job reflection note");
                }

                // Enqueue a targeted meta-learning chain for non-infrastructure tools.
                // This triggers the Analyze→Hypothesize→Test→Integrate cycle immediately
                // rather than waiting for perception_scan to accumulate 3+ failures.
                // Skip it for transient/infra errors (quota, rate limits, timeouts) the
                // brain can't fix, and dedupe to at most once per tool per 24h.
                // A spent quota is transient for meta-learning purposes but not
                // for the operator: it needs a human to restore or reroute it,
                // so it gets a notification even though the chain below skips.
                if is_quota_exhausted_error(&error_text) {
                    self.notify_quota_exhausted(&job.tool_name, &error_text)
                        .await;
                }

                let do_meta_learn = should_meta_learn(&job.tool_name)
                    && if is_transient_infra_error(&error_text) {
                        info!(job_id = %job.id, tool = %job.tool_name, "Dead job is a transient/infra error — skipping meta-learning");
                        false
                    } else if self.recently_meta_learned(&job.tool_name).await {
                        info!(job_id = %job.id, tool = %job.tool_name, "Meta-learning already ran for this tool in the last 24h — skipping");
                        false
                    } else {
                        true
                    };
                if do_meta_learn {
                    let search_query = format!("failure {} root cause error", job.tool_name);
                    let hypothesis_question = format!(
                        "You are a meta-learning system. Respond in English only.\n\
                         A job running '{}' just died after {} \
                         attempts with error: {}\n\n\
                         Based on any related notes above:\n\
                         1. ANALYZE: What is causing this failure?\n\
                         2. HYPOTHESIZE: Form a specific, testable hypothesis.\n\
                         3. TEST: Propose a concrete test to confirm/refute the hypothesis.\n\
                         4. INTEGRATE: What single change would prevent this failure from recurring?",
                        job.tool_name, attempt, error_text
                    );
                    let meta_steps = vec![
                        crate::services::queue::ChainStep {
                            tool_name: "search_notes".to_string(),
                            arguments: Some(serde_json::json!({
                                "query": search_query,
                                "limit": 6
                            })),
                            priority: Some(0),
                            max_attempts: Some(2),
                            provider_hint: Some("ollama".to_string()),
                            description: Some(format!(
                                "Meta-learn: gather evidence for '{}' failure",
                                job.tool_name
                            )),
                            ..Default::default()
                        },
                        crate::services::queue::ChainStep {
                            tool_name: "reason".to_string(),
                            arguments: Some(serde_json::json!({
                                "question": hypothesis_question,
                                "store_inference": true
                            })),
                            priority: Some(0),
                            max_attempts: Some(2),
                            provider_hint: Some("ollama".to_string()),
                            description: Some(format!(
                                "Meta-learn: hypothesize root cause for '{}'",
                                job.tool_name
                            )),
                            ..Default::default()
                        },
                        crate::services::queue::ChainStep {
                            tool_name: "store_note".to_string(),
                            arguments: Some(serde_json::json!({
                                "content": "{{_prev}}",
                                "note_type": "meta_learning_result",
                                "source_context": format!("dead_job:{}", job.tool_name),
                                "provenance": "synthesis_inference"
                            })),
                            priority: Some(0),
                            max_attempts: Some(2),
                            provider_hint: Some("ollama".to_string()),
                            description: Some(format!(
                                "Meta-learn: store result for '{}'",
                                job.tool_name
                            )),
                            ..Default::default()
                        },
                    ];
                    if let Err(e) = self
                        .enqueue_chain(&meta_steps, job.session_id.as_deref())
                        .await
                    {
                        warn!(job_id = %job.id, error = %e, "Failed to enqueue meta-learning chain for dead job");
                    } else {
                        info!(job_id = %job.id, tool = %job.tool_name, "Enqueued meta-learning chain for dead job");
                    }
                }

                self.emit_job_event(BrainEvent::JobDead {
                    job_id: job.id.clone(),
                    tool_name: job.tool_name.clone(),
                    session_id: job.session_id.clone(),
                    error: error_text.clone(),
                });
            } else {
                // Re-queue for automatic retry: set status back to 'queued' so the
                // coordinator picks it up again.  Children remain parked and will be
                // unparked when the retry eventually succeeds.
                match self.neo4j.requeue_for_retry(&job.id, &error_text).await {
                    Ok(true) => {
                        warn!(job_id = %job.id, attempt = attempt, max = max, "AgentJob failed — re-queued for retry");
                        self.notify.notify_one(); // wake coordinator immediately
                        self.emit_job_event(BrainEvent::JobFailed {
                            job_id: job.id.clone(),
                            tool_name: job.tool_name.clone(),
                            session_id: job.session_id.clone(),
                            error: error_text.clone(),
                        });
                    }
                    Ok(false) => {
                        // Cancelled while executing — leave it cancelled, don't re-queue.
                        info!(job_id = %job.id, "Job cancelled during execution — not re-queuing for retry");
                    }
                    Err(e) => {
                        warn!(job_id = %job.id, error = %e, "Failed to re-queue job for retry");
                    }
                }
            }
        }
    }

    // =========================================================================
    // Evaluator re-queue
    // =========================================================================

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

        match self
            .neo4j
            .create_task(
                &task.goal,
                Some(&retry_context),
                task.success_criteria.as_deref(),
            )
            .await
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
}

// ---------------------------------------------------------------------------
// Adversarial re-queue
// ---------------------------------------------------------------------------

impl QueueService {
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

        match self
            .neo4j
            .create_task(
                &task.goal,
                Some(&retry_context),
                task.success_criteria.as_deref(),
            )
            .await
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

// ---------------------------------------------------------------------------
// Evaluator helpers
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Adversarial helpers
// ---------------------------------------------------------------------------

/// Parse `overall_robustness` (1.0–5.0) from `adversarial_plan_review` JSON output.
///
/// Tries the JSON field first; falls back to a `Robustness: N/5` text pattern;
/// defaults to 3.0 (neutral) if neither is found.
fn parse_adversarial_robustness(text: &str) -> f32 {
    // Try to parse the JSON blob returned by the tool.
    let trimmed = text.trim();
    let json_start = trimmed.find('{').unwrap_or(0);
    let json_end = trimmed.rfind('}').map(|i| i + 1).unwrap_or(trimmed.len());
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&trimmed[json_start..json_end])
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

// ---------------------------------------------------------------------------
// Meta-learning helpers
// ---------------------------------------------------------------------------

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
        "record_outcome",
        "get_task",
        "list_tasks",
    ];
    !EXCLUDED.contains(&tool_name)
}

/// Returns `true` if a dead-job error looks like a transient or infrastructure failure
/// (rate limit, quota exhaustion, timeout, upstream 5xx) rather than a logic error the
/// brain could fix. Meta-learning on these just burns LLM cycles hypothesising about a
/// billing limit or a flaky network — e.g. repeated "SerpApi 429: run out of searches".
fn is_transient_infra_error(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    const NEEDLES: &[&str] = &[
        "429",
        "too many requests",
        "rate limit",
        "quota",
        "run out of searches",
        "timed out",
        "timeout",
        "connection refused",
        "502",
        "503",
        "504",
        "service unavailable",
        "temporarily unavailable",
    ];
    NEEDLES.iter().any(|n| lower.contains(n))
}

/// Returns `true` if a dead-job error means an external API's quota is spent.
///
/// This is a strict subset of `is_transient_infra_error`, and the two serve
/// opposite purposes. Meta-learning is still skipped — the brain cannot reason
/// its way out of a billing cap — but a spent quota is *not* the kind of blip
/// that should be swallowed silently. An exhausted SerpApi free tier stopped
/// the daily news brief for two days while the coordinator logged nothing but
/// "skipping meta-learning", because "run out of searches" was classified as
/// transient and therefore ignorable. It needs a human.
fn is_quota_exhausted_error(error_text: &str) -> bool {
    let lower = error_text.to_lowercase();
    const NEEDLES: &[&str] = &[
        "run out of searches",
        "quota",
        "insufficient credits",
        "billing",
        "exceeded your current",
    ];
    NEEDLES.iter().any(|n| lower.contains(n))
}

// ---------------------------------------------------------------------------
// {{_prev}} template substitution helpers
// ---------------------------------------------------------------------------

/// Extract the plain-text content from a ToolCallResult for use as {{_prev}}.
/// Tries `content[0].text` first (standard ToolCallResult shape). When the text is
/// a JSON object with an `"answer"` field (the standard `reason` tool output shape),
/// returns just the answer string so downstream `store_note` steps receive clean
/// markdown instead of a JSON wrapper. Falls back to the full serialised result so
/// `{{_prev}}` is never left unreplaced when a tool returns structured data without
/// a human-readable answer field (e.g. `duckdb_query`, `search_web`).
/// Unwrap a `{"count": N, "rows": [{"col": "…"}, …]}` query result into its
/// concatenated column values, when every row has exactly one column.
///
/// This is the "banking idiom" payoff: chains stash intermediate output in a
/// `WorkingMemory` session and reassemble it with
/// `neo4j_query … RETURN w.content AS content ORDER BY w.turn_index`, because
/// `{{_prev}}` carries only the previous step's output. Without unwrapping, the
/// *envelope* is what flows onward — `chains/video-learning.yaml` stored notes
/// that began `{"count":2,"rows":[{"content":"## VIDEO SUMMARY\n{…escaped…}"`,
/// so the brain's durable semantic knowledge was JSON scaffolding with the real
/// content escaped inside it, then chunked and embedded that way.
///
/// Multi-column results are left alone: there the shape is the information
/// (a table of tasks, models, usage rows), and flattening it would destroy the
/// association between columns.
pub fn unwrap_single_column_rows(parsed: &serde_json::Value) -> Option<String> {
    let rows = parsed.get("rows")?.as_array()?;
    if rows.is_empty() {
        return None;
    }
    let mut out: Vec<String> = Vec::with_capacity(rows.len());
    for row in rows {
        let obj = row.as_object()?;
        if obj.len() != 1 {
            return None;
        }
        let value = obj.values().next()?;
        match value {
            // Strings pass through as-is — this is the content case.
            serde_json::Value::String(s) => out.push(s.clone()),
            // A single non-string column (counts, ids) is still more useful
            // unwrapped than wrapped, but must not lose its representation.
            serde_json::Value::Null => out.push(String::new()),
            other => out.push(other.to_string()),
        }
    }
    Some(out.join("\n\n"))
}

/// The structured envelope worth carrying alongside `extract_result_text`'s output.
///
/// `extract_result_text` is deliberately lossy: it unwraps `{"id":…,"answer":…}`
/// down to the answer so `{{_prev}}` yields clean markdown rather than JSON
/// scaffolding. That is right for the common case and wrong for the one where a
/// later step needs a sibling field — `store_note` returns the note's id next to
/// its content, and discarding it is why chain-extracted claims had no
/// `ASSERTED_IN` edge back to their source.
///
/// Returns `""` (store nothing) when there is nothing to recover: a non-JSON
/// result, or one where extraction changed nothing. That keeps the duplicate
/// payload off every job in the queue and confines it to the envelope case.
fn structured_prev_to_preserve(result: &ToolCallResult, extracted: &str) -> String {
    let text = result
        .content
        .first()
        .and_then(|c| {
            if let Content::Text { text } = c {
                Some(text.as_str())
            } else {
                None
            }
        })
        .unwrap_or("");

    if text.is_empty() || text == extracted {
        return String::new();
    }
    // Only JSON objects are worth keeping — paths cannot index anything else.
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(serde_json::Value::Object(_)) => text.to_string(),
        _ => String::new(),
    }
}

fn extract_result_text(result: &ToolCallResult) -> String {
    let text = result
        .content
        .first()
        .and_then(|c| {
            if let Content::Text { text } = c {
                Some(text.as_str())
            } else {
                None
            }
        })
        .unwrap_or("");

    if !text.is_empty() {
        if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(text) {
            // When the text is a JSON object with an "answer" key (reason tool output),
            // return just the answer so {{_prev}} in store_note steps gets clean markdown.
            if let Some(answer) = parsed.get("answer").and_then(|a| a.as_str())
                && !answer.is_empty()
            {
                return answer.to_string();
            }
            // Single-column query results unwrap to their concatenated values.
            if let Some(unwrapped) = unwrap_single_column_rows(&parsed) {
                return unwrapped;
            }
        }
        text.to_string()
    } else {
        // Fallback: serialise the whole result so downstream steps always receive data.
        serde_json::to_string(result).unwrap_or_default()
    }
}

/// The subject of a goal, with any routing prefix (`"fill knowledge gap:"`,
/// `"watch video:"`, …) stripped — exposed to chains as `{{goal_topic}}`.
///
/// Routing prefixes exist to match a chain, not to be searched for. Passing the
/// raw `{{goal}}` to `search_web` produced queries like *"fill knowledge gap:
/// Research the specific impact of ALPRs…"*, where the first three words are
/// noise that no search engine can do anything useful with.
///
/// Conservative by construction: it strips only up to the *first* colon, only
/// when that colon appears early enough to be a prefix rather than punctuation
/// mid-sentence, and only when something non-empty follows. Anything else is
/// returned unchanged, so a goal that is an ordinary sentence is never mangled.
pub fn goal_topic(goal: &str) -> &str {
    /// A prefix is a short label. Beyond this, a colon is sentence punctuation.
    const MAX_PREFIX_LEN: usize = 40;

    match goal.find(':') {
        Some(idx) if idx < MAX_PREFIX_LEN => {
            let rest = goal[idx + 1..].trim();
            // A URL ("https://…") colon must not be treated as a prefix, and a
            // trailing colon with nothing after it leaves nothing to search.
            if rest.is_empty() || rest.starts_with("//") {
                goal.trim()
            } else {
                rest
            }
        }
        _ => goal.trim(),
    }
}

/// Recursively replace `{{key}}` placeholders in every string value of a JSON Value tree.
///
/// Operating at the Value level (rather than on serialized JSON text) means replacement
/// values containing quotes, backslashes, or newlines are inserted safely and can never
/// corrupt the surrounding JSON structure.  This is the substitution primitive shared by
/// `substitute_prev` (chain `{{_prev}}` results) and the scheduler's chain/scheduled-task
/// template expansion (`{{goal}}`, `{{task_id}}`, `{{date}}`, `{{file_slug}}`,
/// `{{goal_topic}}`).
pub fn substitute_template_vars(
    val: &serde_json::Value,
    vars: &[(&str, &str)],
) -> serde_json::Value {
    match val {
        serde_json::Value::String(s) => {
            let mut out = s.clone();
            for (key, replacement) in vars {
                let placeholder = ["{{", key, "}}"].concat();
                out = out.replace(&placeholder, replacement);
            }
            serde_json::Value::String(out)
        }
        serde_json::Value::Object(obj) => serde_json::Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), substitute_template_vars(v, vars)))
                .collect(),
        ),
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| substitute_template_vars(v, vars))
                .collect(),
        ),
        other => other.clone(),
    }
}

/// Recursively replace `{{_prev}}` and its alias `{{result}}` in all string values of a JSON
/// Value tree.  Operates at the Value level so there is no risk of JSON injection.
fn substitute_prev(val: &serde_json::Value, prev_text: &str) -> serde_json::Value {
    substitute_template_vars(val, &[("_prev", prev_text), ("result", prev_text)])
}

/// Resolve a dotted path against a JSON value: `id`, `answer`, `rows.0.content`.
///
/// Numeric segments index arrays; everything else is an object key.  Returns
/// `None` the moment a segment does not resolve, so a wrong path yields nothing
/// rather than a partial match.
fn lookup_json_path<'a>(root: &'a serde_json::Value, path: &str) -> Option<&'a serde_json::Value> {
    let mut cur = root;
    for seg in path.split('.') {
        if seg.is_empty() {
            return None;
        }
        cur = match cur {
            serde_json::Value::Object(map) => map.get(seg)?,
            serde_json::Value::Array(arr) => arr.get(seg.parse::<usize>().ok()?)?,
            _ => return None,
        };
    }
    Some(cur)
}

/// Render a resolved value for insertion into a string argument.
///
/// Strings are inserted raw — a `{{_prev.answer}}` holding a note body must not
/// arrive wrapped in JSON quotes with its newlines escaped.
fn render_prev_path_value(val: &serde_json::Value) -> String {
    match val {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Replace `{{_prev.<path>}}` / `{{result.<path>}}` occurrences in one string.
///
/// Placeholders that are not path forms (`{{_prev}}`, `{{goal}}`, …) are copied
/// through untouched for the later substitution passes to handle.
fn substitute_prev_paths_in_str(s: &str, root: Option<&serde_json::Value>) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;

    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            // Unclosed placeholder — emit the remainder verbatim.
            out.push_str(&rest[start..]);
            return out;
        };

        let token = after[..end].trim();
        match token
            .strip_prefix("_prev.")
            .or_else(|| token.strip_prefix("result."))
        {
            Some(path) => {
                // An unresolvable path becomes the empty string, never the
                // literal placeholder: a tool receiving "{{_prev.id}}" as an id
                // would treat it as a real one, while an empty value is simply
                // absent and the optional link is skipped.
                let resolved = root
                    .and_then(|r| lookup_json_path(r, path))
                    .map(render_prev_path_value)
                    .unwrap_or_default();
                out.push_str(&resolved);
            }
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }

    out.push_str(rest);
    out
}

/// Recursively resolve `{{_prev.<path>}}` / `{{result.<path>}}` against the
/// previous step's output parsed as JSON.
///
/// This exists because `{{_prev}}` can only paste a step's *entire* output, and
/// most tool results are JSON envelopes. `store_note` returns
/// `{"id":…,"answer":…}`, so a downstream step could get the note's text or its
/// id but never pick one out — which is why chain-extracted claims carried
/// provenance labels but no `ASSERTED_IN` edge to the note they came from.
///
/// Resolution runs against the RAW previous output, never a distilled one:
/// distillation rewrites prose and would destroy the JSON structure paths need.
/// When the output is not JSON, `root` is `None` and every path resolves empty.
fn substitute_prev_paths(val: &serde_json::Value, prev_text: &str) -> serde_json::Value {
    let root = serde_json::from_str::<serde_json::Value>(prev_text).ok();
    substitute_prev_paths_inner(val, root.as_ref())
}

fn substitute_prev_paths_inner(
    val: &serde_json::Value,
    root: Option<&serde_json::Value>,
) -> serde_json::Value {
    match val {
        serde_json::Value::String(s) => {
            serde_json::Value::String(substitute_prev_paths_in_str(s, root))
        }
        serde_json::Value::Object(obj) => serde_json::Value::Object(
            obj.iter()
                .map(|(k, v)| (k.clone(), substitute_prev_paths_inner(v, root)))
                .collect(),
        ),
        serde_json::Value::Array(arr) => serde_json::Value::Array(
            arr.iter()
                .map(|v| substitute_prev_paths_inner(v, root))
                .collect(),
        ),
        other => other.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn single_column_rows_unwrap_to_their_content() {
        // The banking idiom: neo4j_query reassembles WorkingMemory into one
        // column. Storing the envelope is what polluted video_learning notes.
        let v = serde_json::json!({
            "count": 2,
            "rows": [
                {"content": "## VIDEO SUMMARY\nsomething"},
                {"content": "## NEW vs KNOWN\nsomething else"}
            ]
        });
        assert_eq!(
            unwrap_single_column_rows(&v).unwrap(),
            "## VIDEO SUMMARY\nsomething\n\n## NEW vs KNOWN\nsomething else"
        );
    }

    #[test]
    fn multi_column_rows_are_left_as_json() {
        // Here the shape IS the information — flattening would sever the
        // association between a task's id, goal and status.
        let v = serde_json::json!({
            "count": 1,
            "rows": [{"id": "abc", "goal": "do a thing", "status": "created"}]
        });
        assert!(unwrap_single_column_rows(&v).is_none());
    }

    #[test]
    fn empty_or_shapeless_results_are_left_alone() {
        assert!(unwrap_single_column_rows(&serde_json::json!({"count":0,"rows":[]})).is_none());
        assert!(unwrap_single_column_rows(&serde_json::json!({"answer":"hi"})).is_none());
        assert!(unwrap_single_column_rows(&serde_json::json!([1, 2, 3])).is_none());
    }

    #[test]
    fn extract_result_text_unwraps_a_reassembly_envelope() {
        let payload = serde_json::json!({
            "count": 1,
            "rows": [{"content": "clean prose"}]
        })
        .to_string();
        let result = ToolCallResult::success_text(payload);
        assert_eq!(extract_result_text(&result), "clean prose");
    }

    #[test]
    fn extract_result_text_still_prefers_the_answer_field() {
        let payload = serde_json::json!({"answer": "the answer", "rows": [{"c": "x"}]}).to_string();
        let result = ToolCallResult::success_text(payload);
        assert_eq!(extract_result_text(&result), "the answer");
    }

    #[test]
    fn goal_topic_strips_a_routing_prefix() {
        assert_eq!(
            goal_topic("fill knowledge gap: Research the impact of ALPRs on privacy"),
            "Research the impact of ALPRs on privacy"
        );
        assert_eq!(goal_topic("watch video: some talk"), "some talk");
    }

    #[test]
    fn goal_topic_leaves_an_ordinary_sentence_alone() {
        let goal = "Analyze the status of the deal between Iran and Oman";
        assert_eq!(goal_topic(goal), goal);
    }

    #[test]
    fn goal_topic_ignores_a_colon_that_is_sentence_punctuation() {
        // Past the prefix-length bound, a colon introduces a clause rather than
        // labelling the goal — stripping there would discard the actual subject.
        let goal = "Compare the two leading approaches to caching and explain: \
                    which one wins under write-heavy load";
        assert_eq!(goal_topic(goal), goal);
    }

    #[test]
    fn goal_topic_does_not_decapitate_a_bare_url() {
        // "https://…" — the scheme colon is at index 5, well inside the prefix
        // bound, so the naive rule would hand a search engine "//youtu.be/x".
        let goal = "https://youtu.be/abc123";
        assert_eq!(goal_topic(goal), goal);
    }

    #[test]
    fn goal_topic_keeps_the_goal_when_nothing_follows_the_colon() {
        assert_eq!(goal_topic("fill knowledge gap:"), "fill knowledge gap:");
    }

    #[test]
    fn spent_search_quota_is_both_transient_and_alertable() {
        // These two classifiers deliberately overlap: meta-learning is skipped
        // (the brain cannot fix a billing cap) but the operator is still told.
        let err = "SerpApi failed: 429 Too Many Requests - \
                   {\"error\": \"Your account has run out of searches.\"}";
        assert!(is_transient_infra_error(err));
        assert!(is_quota_exhausted_error(err));
    }

    #[test]
    fn ordinary_transient_errors_do_not_raise_a_quota_alert() {
        // A timeout or a 503 resolves on its own; waking the operator for one
        // would make the quota alert worthless through noise.
        for err in [
            "Request timed out after 30s",
            "Upstream returned 503 Service Unavailable",
            "connection refused",
        ] {
            assert!(is_transient_infra_error(err), "{err} should be transient");
            assert!(
                !is_quota_exhausted_error(err),
                "{err} should not alert as a spent quota"
            );
        }
    }

    /// Every step in every seeded `chains/*.yaml` and `schedules/*.yaml` must
    /// deserialize into `ChainStep`. The seeder stores steps as opaque JSON and
    /// they are only parsed at dispatch time, so a typo in a step key would
    /// otherwise surface as a runtime chain failure hours later.
    #[test]
    fn seeded_yaml_steps_deserialize_as_chain_steps() {
        #[derive(serde::Deserialize)]
        struct StepsOnly {
            steps: Vec<serde_json::Value>,
        }

        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .expect("workspace root");

        let mut checked = 0;
        for dir in ["chains", "schedules"] {
            let path = root.join(dir);
            let entries = std::fs::read_dir(&path).unwrap_or_else(|e| panic!("{dir}: {e}"));
            for entry in entries.flatten() {
                let p = entry.path();
                if p.extension().and_then(|e| e.to_str()) != Some("yaml") {
                    continue;
                }
                let text = std::fs::read_to_string(&p).expect("read yaml");
                let file: StepsOnly =
                    serde_yaml::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", p.display()));
                for (i, step) in file.steps.iter().enumerate() {
                    serde_json::from_value::<ChainStep>(step.clone())
                        .unwrap_or_else(|e| panic!("{} step {i}: {e}", p.display()));
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "no steps found to validate");
    }

    #[test]
    fn chain_step_distill_fields_default_off() {
        // Existing chain YAML has no distill keys — they must default to "off"
        // so seeded chains keep passing {{_prev}} through verbatim.
        let step: ChainStep = serde_yaml::from_str(
            r#"
tool_name: reason
arguments:
  context: "{{_prev}}"
"#,
        )
        .expect("must deserialize");
        assert!(!step.distill_prev);
        assert!(step.distill_max_chars.is_none());
        assert!(step.distill_focus.is_none());
    }

    #[test]
    fn chain_step_parses_distill_fields() {
        let step: ChainStep = serde_yaml::from_str(
            r#"
tool_name: reason
distill_prev: true
distill_max_chars: 1200
distill_focus: "keep every source URL"
"#,
        )
        .expect("must deserialize");
        assert!(step.distill_prev);
        assert_eq!(step.distill_max_chars, Some(1200));
        assert_eq!(step.distill_focus.as_deref(), Some("keep every source URL"));
    }

    #[test]
    fn substitute_template_vars_preserves_quotes_and_backslashes() {
        // A goal containing a quote, backslash, and newline must not corrupt the JSON:
        // substitution happens on the parsed Value, so these are inserted verbatim.
        let raw = json!([{
            "tool_name": "reason",
            "arguments": { "question": "{{goal}}" }
        }]);
        let goal = "say \"hello\" \\ world\nnext line";
        let out = substitute_template_vars(&raw, &[("goal", goal)]);
        let steps: Vec<ChainStep> = serde_json::from_value(out).expect("must deserialize");
        assert_eq!(steps.len(), 1);
        let q = steps[0]
            .arguments
            .as_ref()
            .unwrap()
            .get("question")
            .unwrap()
            .as_str()
            .unwrap();
        assert_eq!(q, goal);
    }

    #[test]
    fn substitute_template_vars_replaces_multiple_keys_and_nested() {
        let raw = json!({
            "a": "{{task_id}}",
            "b": ["{{goal}}", { "c": "{{date}}" }],
            "d": 42
        });
        let out = substitute_template_vars(
            &raw,
            &[("task_id", "t-1"), ("goal", "do X"), ("date", "2026-08-03")],
        );
        assert_eq!(out["a"], json!("t-1"));
        assert_eq!(out["b"][0], json!("do X"));
        assert_eq!(out["b"][1]["c"], json!("2026-08-03"));
        assert_eq!(out["d"], json!(42)); // non-strings untouched
    }

    #[test]
    fn substitute_prev_handles_both_aliases() {
        let raw = json!({ "x": "{{_prev}}", "y": "{{result}}" });
        let out = substitute_prev(&raw, "PREV");
        assert_eq!(out["x"], json!("PREV"));
        assert_eq!(out["y"], json!("PREV"));
    }

    /// The ASSERTED_IN case: `store_note` returns an envelope, and the claim
    /// step needs the body from one field and the note id from another.
    /// The envelope is preserved exactly when extraction was lossy — this is
    /// the half of the ASSERTED_IN fix that runs at completion time.
    #[test]
    fn structured_prev_kept_only_when_extraction_lost_something() {
        // store_note: extraction unwraps to `answer`, so the id must be kept.
        let envelope = json!({ "id": "note-1", "answer": "body text" }).to_string();
        let result = ToolCallResult::success_text(envelope.clone());
        let extracted = extract_result_text(&result);
        assert_eq!(extracted, "body text");
        assert_eq!(structured_prev_to_preserve(&result, &extracted), envelope);

        // A plain-text result loses nothing — storing a copy would be waste.
        let result = ToolCallResult::success_text("just prose");
        let extracted = extract_result_text(&result);
        assert_eq!(structured_prev_to_preserve(&result, &extracted), "");

        // JSON with no `answer` passes through extraction unchanged: identical
        // text, nothing to recover.
        let result = ToolCallResult::success_text(r#"{"stored":3}"#);
        let extracted = extract_result_text(&result);
        assert_eq!(structured_prev_to_preserve(&result, &extracted), "");

        // A top-level array cannot be path-indexed by field name; not kept.
        let result = ToolCallResult::success_text(r#"[{"answer":"x"}]"#);
        let extracted = extract_result_text(&result);
        assert_eq!(structured_prev_to_preserve(&result, &extracted), "");
    }

    #[test]
    fn prev_paths_pick_fields_out_of_a_store_note_envelope() {
        // r##"…"## because the payload itself contains the sequence `"#`.
        let prev = r##"{"success":true,"id":"note-123","links_created":2,
                       "answer":"# Report\n\nLine two.","message":"Note stored successfully"}"##;
        let raw = json!({
            "text": "{{_prev.answer}}",
            "source_note_id": "{{_prev.id}}",
            "source_context": "slm_benchmark_watch"
        });
        let out = substitute_prev_paths(&raw, prev);
        // Strings are inserted raw — not re-quoted, newlines not escaped.
        assert_eq!(out["text"], json!("# Report\n\nLine two."));
        assert_eq!(out["source_note_id"], json!("note-123"));
        assert_eq!(out["source_context"], json!("slm_benchmark_watch"));
    }

    #[test]
    fn prev_paths_leave_the_whole_output_placeholder_alone() {
        // "{{_prev}}" must survive the path pass so substitute_prev can handle
        // it — the two forms run in sequence over the same arguments.
        let raw = json!({ "a": "{{_prev}}", "b": "{{_prev.id}}", "c": "{{goal}}" });
        let out = substitute_prev_paths(&raw, r#"{"id":"x1"}"#);
        assert_eq!(out["a"], json!("{{_prev}}"));
        assert_eq!(out["b"], json!("x1"));
        assert_eq!(out["c"], json!("{{goal}}"));
    }

    #[test]
    fn prev_paths_resolve_empty_rather_than_leaking_the_placeholder() {
        // A tool handed the literal "{{_prev.id}}" would treat it as a real id.
        // Non-JSON output, a missing key, and a wrong type must all yield "".
        for prev in [
            "not json at all",
            r#"{"other":"field"}"#,
            r#"{"id":{"nested":"object"}}"#,
        ] {
            let out = substitute_prev_paths(&json!({ "k": "{{_prev.id.deep}}" }), prev);
            assert_eq!(out["k"], json!(""), "prev={prev}");
        }
    }

    #[test]
    fn prev_paths_index_arrays_and_render_non_strings() {
        let prev = r#"{"rows":[{"content":"first"},{"content":"second"}],"count":7}"#;
        let out = substitute_prev_paths(
            &json!({ "a": "{{_prev.rows.1.content}}", "b": "n={{_prev.count}}" }),
            prev,
        );
        assert_eq!(out["a"], json!("second"));
        assert_eq!(out["b"], json!("n=7"));
    }

    #[test]
    fn prev_paths_alias_and_malformed_placeholders() {
        let prev = r#"{"id":"abc"}"#;
        // `result.` is the documented alias for `_prev.`.
        let out = substitute_prev_paths(&json!({ "a": "{{result.id}}" }), prev);
        assert_eq!(out["a"], json!("abc"));
        // An unclosed placeholder is emitted verbatim, not silently swallowed.
        let out = substitute_prev_paths(&json!({ "a": "x {{_prev.id" }), prev);
        assert_eq!(out["a"], json!("x {{_prev.id"));
    }

    #[test]
    fn prev_path_values_cannot_corrupt_surrounding_json() {
        // Substitution is value-level, so a field holding quotes/backslashes/
        // newlines lands intact instead of breaking the argument structure.
        let prev = json!({ "answer": "say \"hi\" \\ then\nnewline" }).to_string();
        let out = substitute_prev_paths(&json!({ "text": "{{_prev.answer}}" }), &prev);
        assert_eq!(out["text"], json!("say \"hi\" \\ then\nnewline"));
    }

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

    #[test]
    fn transient_infra_errors_detected() {
        assert!(is_transient_infra_error(
            "SerpApi failed: 429 Too Many Requests - Your account has run out of searches."
        ));
        assert!(is_transient_infra_error(
            "upstream returned 503 Service Unavailable"
        ));
        assert!(is_transient_infra_error("request timed out after 120s"));
        assert!(is_transient_infra_error("RATE LIMIT exceeded"));
    }

    #[test]
    fn logic_errors_are_not_transient() {
        assert!(!is_transient_infra_error(
            "tool 'decompose_goal' returned invalid JSON: missing field `steps`"
        ));
        assert!(!is_transient_infra_error("task not found"));
    }
}
