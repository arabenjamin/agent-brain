//! Agent job queue — priority-ordered background task executor.
//!
//! # Design
//!
//! - **Durability**: jobs are persisted to Neo4j so they survive server restarts.
//! - **Priority**: an in-memory `BinaryHeap` orders jobs by priority (0–3) then FIFO.
//! - **Concurrency**: per-`provider_hint` semaphores cap how many JOBS run at
//!   once. They are not the provider limit: a job also searches and writes the
//!   graph, and its hint is not always the provider it calls. How many requests a
//!   provider may serve at once is enforced per call by `services/provider_gate.rs`,
//!   never here — a job holding a provider slot that then called the model would
//!   take the slot twice and deadlock a one-slot provider.
//! - **Wakeup**: a `Notify` wakes the coordinator immediately when a new job arrives or
//!   when a running job finishes and frees a provider concurrency slot.
//! - **Recovery**: on startup, `recover()` resets crashed `running` jobs to `queued`
//!   and reloads all `queued` jobs into the heap.
//!
//! # Parts
//!
//! This file holds the job types, the coordinator and running one job. The rest
//! is in `queue/`: `templates` (step argument substitution), `gates` (evaluator
//! and adversarial checks, meta-learning), `ops` (cancel, retry, drain, stats).
//!
//! # Resizing concurrency at runtime
//!
//! `update_config()` swaps in a new job semaphore of the requested size; jobs
//! already holding a permit from the old one finish on it.

use std::collections::{BinaryHeap, HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::time::Duration;

pub use agent_brain_llm::ctx::{
    CURRENT_OWNER_TASK, CURRENT_TOOL, SELECTED_LLM, USE_LOCAL_LLM, current_owner_task,
};

use serde::Deserialize;
use serde_json::Value;
use tokio::sync::{Mutex, Notify, RwLock, Semaphore, broadcast};
use tracing::{debug, error, info, warn};

use crate::events::BrainEvent;
use crate::tools::ToolHandler;
use agent_brain_models::{AgentJob, AgentJobStatus, PrioritizedJob};
use agent_brain_protocol::{Content, ToolCallResult};
use agent_brain_repository::Neo4jClient;

const DEFAULT_MAX_CONCURRENT: usize = 5;
const DEFAULT_MAX_CONCURRENT_LOCAL: usize = 2;
const DEFAULT_MAX_CONCURRENT_ANTHROPIC: usize = 2;
const DEFAULT_MAX_CONCURRENT_GEMINI: usize = 5;

mod gates;
mod ops;
mod templates;
pub use templates::*;

/// Runtime configuration for the queue coordinator.
#[derive(Debug, Clone)]
pub struct WorkerConfig {
    /// Global maximum number of jobs executing concurrently (informational).
    pub max_concurrent: usize,
    /// Concurrency limit for local jobs.
    pub max_concurrent_local: usize,
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
            max_concurrent_local: DEFAULT_MAX_CONCURRENT_LOCAL,
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
    /// Names this step so a later one can use its output as `{{steps.<id>}}`
    /// (or `{{steps.<id>.<path>}}`), not only the next step via `{{_prev}}`.
    pub id: Option<String>,
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
    /// A model this step must run on, by catalog `model` id or `name`
    /// (`models.yaml`). Takes precedence over `required_capabilities`, still
    /// judged by the routing policy, and never a preference: an unknown or
    /// refused model fails the job with the reason instead of running on
    /// something else. The step's `provider_hint` is derived from the model's
    /// provider at enqueue, so the two cannot disagree. Stored as `__model` in
    /// the job args; tools ignore it via serde.
    #[serde(default)]
    pub model: Option<String>,
    /// Who may see this step's prompts (`services/routing.rs`): `private`
    /// (the default when absent) only reaches `no_log` providers; `public` also
    /// admits free tiers that may train on prompts. A chain or schedule file's
    /// top-level `data_class` is stamped onto steps that do not set their own.
    /// Stored as `__data_class` in the job args; tools ignore it via serde.
    #[serde(default)]
    pub data_class: Option<agent_brain_llm::routing::DataClass>,
    /// The owner approved this workflow for paid models (`paid: true` on the
    /// workflow, stamped onto each step). Without it a queued step is the
    /// brain's own work and never reaches a `billing: paid` provider.
    /// Stored as `__paid` in the job args; tools ignore it via serde.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paid: bool,
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
    /// When `true`, this step dying does not end the chain: it is still
    /// dead-lettered (and reflected on), but the next step runs with an
    /// explicit "optional step failed" note as its `{{_prev}}`, and the owning
    /// Task is not failed. For enrichment a chain can do without — the weather
    /// fetch that, failing during a brief internet outage, cancelled the whole
    /// 2026-09-30 daily news brief. Stored as `__optional` in the job args.
    #[serde(default)]
    pub optional: bool,
    /// Run this step only when it renders non-empty: `"{{inputs.session}}"`
    /// keeps the step in a run that was given `session` and leaves it out of
    /// one that was not (`workflows::render_steps`). Decided when the run
    /// starts, so it may use only run-start variables (inputs, task id, goal,
    /// date), never `{{_prev}}` or `{{steps.*}}`. For a step a workflow's
    /// usual runs skip: the chat hand-off steps on video-learning, which the
    /// media watcher's ~20 runs a day must not pay for.
    #[serde(default)]
    pub when: Option<String>,
}

/// The `{{_prev}}` a step receives when the optional step before it died.
fn optional_step_failed_note(tool: &str, attempt: u32, max: u32, error: &str) -> String {
    let error: String = error.chars().take(300).collect();
    format!(
        "[OPTIONAL STEP FAILED — '{tool}' died after {attempt}/{max} attempts: {error}. \
         The chain continued without its result; say this section is unavailable.]"
    )
}

/// The `{{_prev}}` a step receives when the optional step before it was
/// skipped because its tool is not registered.
fn optional_step_skipped_note(tool: &str, reason: &str) -> String {
    format!(
        "[OPTIONAL STEP SKIPPED — '{tool}' did not run: {reason}. \
         The chain continued without its result; say this section is unavailable.]"
    )
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
    semaphore_local: Arc<RwLock<Arc<Semaphore>>>,
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
    /// Local-pinned jobs set aside on the last coordinator pass because a GPU
    /// lease is held (`services/gpu_lease.rs`).
    gpu_deferred: Arc<AtomicI64>,
    /// Local-engine config used to compress chain handoffs (`distill_prev`).
    /// `None` disables distillation rather than failing the step.
    distill_config: Arc<RwLock<Option<agent_brain_llm::LlmConfig>>>,
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
            semaphore_local: Arc::new(RwLock::new(Arc::new(Semaphore::new(
                DEFAULT_MAX_CONCURRENT_LOCAL,
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
            gpu_deferred: Arc::new(AtomicI64::new(0)),
            distill_config: Arc::new(RwLock::new(None)),
        }
    }

    /// Install the LLM config used for distilled handoffs.
    ///
    /// Takes `&self` (not `self`) because the queue is already behind an `Arc` by
    /// the time the local config is built, and is re-applied on every
    /// `build_skills()` so a reload refreshes it.  Pass the *local* config:
    /// distillation is an optimization and must never spend cloud quota.
    pub async fn set_distill_config(&self, config: agent_brain_llm::LlmConfig) {
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
        let mut step_job_ids = serde_json::Map::new();
        let mut prev_id: Option<String> = None;
        let mut first_job: Option<AgentJob> = None;

        // The catalog, read once, only when some step pins a model.
        let catalog: Vec<agent_brain_llm::model_router::CatalogModel> =
            if steps.iter().any(|s| pinned_model(s).is_some()) {
                let handler = self.tool_handler.read().await;
                handler
                    .as_ref()
                    .and_then(|h| h.telemetry())
                    .map(agent_brain_llm::model_router::catalog_models)
                    .unwrap_or_default()
            } else {
                Vec::new()
            };

        for (i, step) in steps.iter().enumerate() {
            let priority = step.priority.unwrap_or(1);
            let max_attempts = step.max_attempts.unwrap_or(3);
            let provider_hint = step_provider_hint(step, &catalog);

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

            // Inject a pinned model (read by execute_job; tools ignore it).
            let effective_args: Option<serde_json::Value> = match pinned_model(step) {
                Some(model) => {
                    let mut a = effective_args.unwrap_or(serde_json::json!({}));
                    if let serde_json::Value::Object(ref mut m) = a {
                        m.insert("__model".to_string(), serde_json::json!(model));
                    }
                    Some(a)
                }
                None => effective_args,
            };

            // Inject the data class for the routing policy (read by execute_job).
            let effective_args: Option<serde_json::Value> = match step.data_class {
                Some(class) => {
                    let mut a = effective_args.unwrap_or(serde_json::json!({}));
                    if let serde_json::Value::Object(ref mut m) = a {
                        m.insert(
                            "__data_class".to_string(),
                            serde_json::json!(class.as_str()),
                        );
                    }
                    Some(a)
                }
                None => effective_args,
            };

            // Inject the owner's paid-model approval (read by execute_job).
            let effective_args: Option<serde_json::Value> = if step.paid {
                let mut a = effective_args.unwrap_or(serde_json::json!({}));
                if let serde_json::Value::Object(ref mut m) = a {
                    m.insert("__paid".to_string(), serde_json::json!(true));
                }
                Some(a)
            } else {
                effective_args
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

            let effective_args: Option<serde_json::Value> = if step.optional {
                let mut a = effective_args.unwrap_or(serde_json::json!({}));
                if let serde_json::Value::Object(ref mut m) = a {
                    m.insert("__optional".to_string(), serde_json::json!(true));
                }
                Some(a)
            } else {
                effective_args
            };

            // A step that names earlier steps' outputs carries their job ids, so
            // `{{steps.<id>}}` resolves by job id when it runs.
            let effective_args: Option<serde_json::Value> = match effective_args {
                Some(serde_json::Value::Object(mut m))
                    if !step_job_ids.is_empty()
                        && serde_json::Value::Object(m.clone())
                            .to_string()
                            .contains("{{steps.") =>
                {
                    m.insert(
                        "__step_jobs".to_string(),
                        serde_json::Value::Object(step_job_ids.clone()),
                    );
                    Some(serde_json::Value::Object(m))
                }
                other => other,
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
                        provider_hint.as_deref(),
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
                        provider_hint.as_deref(),
                        step.context_profile.as_deref(),
                        step.description.as_deref(),
                        step.ttl_secs,
                    )
                    .await
                    .map_err(|e| e.to_string())?
            };

            if let Some(sid) = step.id.as_deref().filter(|s| !s.is_empty()) {
                step_job_ids.insert(sid.to_string(), serde_json::json!(id));
            }
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

    // =========================================================================
    // Progress tracking
    // =========================================================================

    // =========================================================================
    // TTL and expiration
    // =========================================================================

    // =========================================================================
    // Dead Letter Queue
    // =========================================================================

    // =========================================================================
    // Cleanup
    // =========================================================================

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
            // Read once per pass: a lease taken or released mid-pass applies next pass.
            let gpu_leased = agent_brain_llm::gpu_lease::held().is_some();
            let mut gpu_deferred = 0i64;
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

                // A GPU lease (services/gpu_lease.rs) takes local generation away.
                // Local-pinned jobs wait for it rather than fail against a stopped
                // sidecar or crowd Ollama Cloud's single free slot; they keep their
                // place in the heap and their attempt count.
                if gpu_leased
                    && defers_during_gpu_lease(
                        pjob.job.provider_hint.as_deref(),
                        pjob.job.arguments.as_ref(),
                    )
                    && waits_in_lease_mode(lease_jobs_go_free(), &pjob.job.tool_name)
                {
                    gpu_deferred += 1;
                    blocked.push(pjob);
                    continue;
                }

                // Pick the semaphore based on provider_hint.
                // Read the current inner Arc so that runtime config changes
                // (semaphore swaps) take effect on the next job dispatch.
                let semaphore: Arc<Semaphore> = {
                    let lock = match pjob.job.provider_hint.as_deref() {
                        Some("anthropic") => self.semaphore_anthropic.read().await,
                        Some("gemini") => self.semaphore_gemini.read().await,
                        _ => self.semaphore_local.read().await,
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

            self.gpu_deferred.store(gpu_deferred, Ordering::Relaxed);

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
        let llm = match agent_brain_llm::LlmClient::with_config(cfg.with_temperature(0.2)) {
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

        // An optional step whose tool is not registered (its plugin is off)
        // is skipped at once and the chain carries on. Not a death: retrying
        // cannot register the tool, and a dead job would leave a reflection
        // telling the brain to investigate a tool that is off by choice (seen
        // in Phase 4 pack 6's test run). The job is `cancelled` with the
        // reason; the next step is told plainly that this one did not run.
        if !handler.has_tool(&job.tool_name)
            && job_arg(&job, "__optional")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        {
            drop(handler_guard);
            let reason = format!(
                "tool '{}' is not registered (is its plugin off?); optional step skipped",
                job.tool_name
            );
            info!(job_id = %job.id, tool = %job.tool_name, "Optional step's tool is not registered — skipping");
            // Children first: a parked child of a cancelled parent is reaped.
            let note = optional_step_skipped_note(&job.tool_name, &reason);
            self.unpark_and_enqueue_children(&job.id, &note, "").await;
            if let Err(e) = self.neo4j.set_job_skipped(&job.id, &reason).await {
                warn!(job_id = %job.id, error = %e, "Failed to mark skipped job");
            }
            return;
        }

        let resolved_args = self.resolve_step_args(&job).await;
        let route_ctx = route_context_for(&job);
        let selected_llm = match job_arg(&job, "__model").and_then(Value::as_str) {
            // A pinned model runs as written or not at all: the job dies with
            // the reason (retrying cannot change the catalog or the policy).
            Some(model) => {
                let rows = handler
                    .telemetry()
                    .map(agent_brain_llm::model_router::catalog_models)
                    .unwrap_or_default();
                match agent_brain_llm::model_router::pinned_config(
                    &rows,
                    model,
                    &agent_brain_llm::LlmConfig::default(),
                    route_ctx,
                ) {
                    Ok(cfg) => {
                        info!(job_id = %job.id, tool = %job.tool_name, model = %cfg.model,
                              provider = %cfg.provider_label(), "Step runs on its pinned model");
                        Some(cfg)
                    }
                    Err(reason) => {
                        drop(handler_guard);
                        let error = format!("{STEP_MODEL_ERROR}: {reason}");
                        warn!(job_id = %job.id, tool = %job.tool_name, %error, "Step model cannot run");
                        let attempt = self
                            .neo4j
                            .get_agent_job(&job.id)
                            .await
                            .ok()
                            .flatten()
                            .map(|j| j.attempt_count)
                            .unwrap_or(job.attempt_count + 1);
                        self.on_job_dead(&job, attempt, job.max_attempts, &error)
                            .await;
                        return;
                    }
                }
            }
            None => select_step_model(&job, handler, route_ctx),
        };

        // Run the tool call inside task-local scopes so `SharedLlm` can route:
        // SELECTED_LLM (capability-resolved) takes precedence over USE_LOCAL_LLM
        // (background default), which takes precedence over the active config;
        // ROUTE_CONTEXT lets the routing policy judge every one of them.
        let use_local = agent_brain_llm::model_router::is_local_hint(job.provider_hint.as_deref());
        let result = agent_brain_llm::routing::ROUTE_CONTEXT
            .scope(
                route_ctx,
                SELECTED_LLM.scope(
                    selected_llm,
                    USE_LOCAL_LLM.scope(
                        use_local,
                        CURRENT_TOOL.scope(
                            Some(job.tool_name.clone()),
                            CURRENT_OWNER_TASK.scope(
                                job_arg(&job, "__owner_task_id")
                                    .and_then(Value::as_str)
                                    .map(String::from),
                                handler.execute(&job.tool_name, resolved_args),
                            ),
                        ),
                    ),
                ),
            )
            .await;
        // Drop the read lock before any awaits below.
        drop(handler_guard);

        if result.is_error.unwrap_or(false) {
            self.on_job_failed(&job, &result).await;
        } else {
            self.on_job_succeeded(&job, &result).await;
        }
    }

    /// Resolve `{{_prev}}` / `{{result}}` in the step's arguments from the prior
    /// step's result. `{{result}}` is an alias — brain-generated chains often
    /// use the more natural name.
    ///
    /// Two forms, resolved in order of specificity:
    ///   `{{_prev.id}}` — a path into the previous output parsed as JSON
    ///   `{{_prev}}`    — the previous output pasted whole
    /// They never collide: "{{_prev}}" is not a substring of "{{_prev.id}}".
    async fn resolve_step_args(&self, job: &AgentJob) -> Option<Value> {
        let job = &self.resolve_named_steps(job).await;
        let Some(prev_text) = &job.prev_result else {
            return job.arguments.clone();
        };
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

        // Paths first, and always against the structured envelope, never the
        // extracted text: `{{_prev}}` is the *unwrapped* result (the note body),
        // so the sibling fields a path wants — `id` above all — exist only in
        // `prev_result_raw`. Distillation is skipped here too, since it rewrites
        // prose and would destroy the structure.
        if wants_path {
            let root = job.prev_result_raw.as_deref().unwrap_or(prev_text);
            args = args.map(|a| substitute_prev_paths(&a, root));
        }

        // Distilled handoff: when the step opted in, compress the upstream output
        // down to what this step needs before it lands in the prompt. None (use
        // the raw text) whenever distillation is off, the payload is already
        // small, or the compression call fails.
        if wants_whole {
            let distilled = self.maybe_distill_prev(job, prev_text).await;
            let effective_prev = distilled.as_deref().unwrap_or(prev_text.as_str());
            args = args.map(|a| substitute_prev(&a, effective_prev));
        }

        args
    }

    /// Resolve `{{steps.<id>}}` / `{{steps.<id>.<path>}}` from the jobs named in
    /// `__step_jobs`. Returns the job with those placeholders filled. A step that
    /// did not complete resolves to a marker naming its state, never to silence:
    /// a reader must be able to tell "no news" from "the search failed".
    async fn resolve_named_steps(&self, job: &AgentJob) -> AgentJob {
        let Some(map) = job_arg(job, "__step_jobs").and_then(Value::as_object) else {
            return job.clone();
        };
        let mut outputs: HashMap<String, StepOutput> = HashMap::new();
        for (step_id, job_id) in map {
            let Some(job_id) = job_id.as_str() else {
                continue;
            };
            let out = match self.neo4j.get_agent_job(job_id).await {
                Ok(Some(j)) if j.status == AgentJobStatus::Completed => {
                    // The stored envelope's first text block is all the helpers read.
                    match j
                        .result
                        .as_ref()
                        .and_then(|r| r.pointer("/content/0/text"))
                        .and_then(Value::as_str)
                        .map(|t| ToolCallResult::success_text(t.to_string()))
                    {
                        Some(r) => {
                            let text = extract_result_text(&r);
                            let raw = structured_prev_to_preserve(&r, &text);
                            StepOutput::Done { text, raw }
                        }
                        None => StepOutput::Missing("completed with no result".into()),
                    }
                }
                Ok(Some(j)) => StepOutput::Missing(format!(
                    "{}{}",
                    format!("{:?}", j.status).to_lowercase(),
                    j.error
                        .map(|e| format!(": {}", e.chars().take(300).collect::<String>()))
                        .unwrap_or_default()
                )),
                Ok(None) => StepOutput::Missing("job not found".into()),
                Err(e) => StepOutput::Missing(format!("lookup failed: {e}")),
            };
            outputs.insert(step_id.clone(), out);
        }
        let mut job = job.clone();
        job.arguments = job.arguments.map(|a| substitute_step_refs(&a, &outputs));
        job
    }

    async fn on_job_succeeded(self: &Arc<Self>, job: &AgentJob, result: &ToolCallResult) {
        let result_json = serde_json::to_string(result).unwrap_or_default();
        // Plain text passed to child steps via {{_prev}}.
        let result_text = extract_result_text(result);

        match self.neo4j.set_job_completed(&job.id, &result_json).await {
            Err(e) => {
                error!(job_id = %job.id, "Failed to store completed result: {}", e);
            }
            Ok(false) => {
                // The job was cancelled while executing: set_job_completed's
                // status='running' guard matched nothing. Do NOT unpark chain
                // children or emit a completion event for a job the user cancelled.
                info!(job_id = %job.id, "Job cancelled during execution — chain children not promoted");
            }
            Ok(true) => {
                info!(job_id = %job.id, "AgentJob completed");
                // Both gates run (each records its own outcome); either one
                // blocking stops the chain.
                let evaluator_blocked = self.evaluator_gate(job, &result_text).await;
                let adversarial_blocked = self.adversarial_gate(job, &result_text).await;
                if !evaluator_blocked && !adversarial_blocked {
                    let raw = structured_prev_to_preserve(result, &result_text);
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
    }

    async fn on_job_failed(self: &Arc<Self>, job: &AgentJob, result: &ToolCallResult) {
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
        let (attempt, max) = if let Ok(Some(updated)) = self.neo4j.get_agent_job(&job.id).await {
            (updated.attempt_count, updated.max_attempts)
        } else {
            (job.attempt_count + 1, job.max_attempts)
        };

        if attempt >= max {
            self.on_job_dead(job, attempt, max, &error_text).await;
            return;
        }

        // Re-queue for automatic retry: status back to 'queued' so the
        // coordinator picks it up again. Children stay parked and are unparked
        // when the retry eventually succeeds.
        match self.neo4j.requeue_for_retry(&job.id, &error_text).await {
            Ok(true) => {
                warn!(job_id = %job.id, attempt = attempt, max = max, "AgentJob failed — re-queued for retry");
                self.notify.notify_one(); // wake coordinator immediately
                self.emit_job_event(BrainEvent::JobFailed {
                    job_id: job.id.clone(),
                    tool_name: job.tool_name.clone(),
                    session_id: job.session_id.clone(),
                    error: error_text,
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

    /// Retries are exhausted: dead-letter the job, settle its chain and owning
    /// task, and record what the brain can learn from it.
    async fn on_job_dead(
        self: &Arc<Self>,
        job: &AgentJob,
        attempt: u32,
        max: u32,
        error_text: &str,
    ) {
        match self.neo4j.set_job_dead(&job.id, error_text).await {
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

        let optional = job_arg(job, "__optional")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if optional {
            // An optional step: the chain carries on without its result, told
            // plainly that it is missing.
            let note = optional_step_failed_note(&job.tool_name, attempt, max, error_text);
            info!(job_id = %job.id, tool = %job.tool_name, "Optional chain step died — continuing the chain");
            self.unpark_and_enqueue_children(&job.id, &note, "").await;
        } else {
            // Parent chain is broken — cancel any waiting children.
            let _ = self.neo4j.cancel_parked_children(&job.id).await;
        }

        // If this job belongs to a scheduler-dispatched task chain, fail that
        // Task now with the real error instead of leaving it in_progress until
        // the 6-hour stale reaper flips it blind. This is the concrete failure
        // signal capability-mining reasons over. An optional step's death does
        // not fail the Task.
        if !optional
            && let Some(owner_task_id) = job_arg(job, "__owner_task_id").and_then(Value::as_str)
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

        // A reflection note so the brain can learn from this failure.
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

        // A spent quota is transient for meta-learning purposes but not for the
        // operator: it needs a human to restore or reroute it, so it gets a
        // notification even though meta-learning skips it.
        if is_quota_exhausted_error(error_text) {
            self.notify_quota_exhausted(&job.tool_name, error_text)
                .await;
        }
        self.maybe_meta_learn(job, attempt, error_text).await;

        self.emit_job_event(BrainEvent::JobDead {
            job_id: job.id.clone(),
            tool_name: job.tool_name.clone(),
            session_id: job.session_id.clone(),
            error: error_text.to_string(),
        });
    }
}

/// One of the `__`-prefixed metadata keys the scheduler stamps on a step's
/// arguments (`__owner_task_id`, `__optional`, `__evaluator_min_score`, …).
fn job_arg<'a>(job: &'a AgentJob, key: &str) -> Option<&'a Value> {
    job.arguments.as_ref().and_then(|a| a.get(key))
}

/// The work's route context: a queued step is background work, private unless
/// the step (or its chain/schedule file) declared `data_class: public`, and
/// the brain's own work (no paid model) unless its workflow is `paid: true`.
/// Scoped around the tool call so every LLM call it makes is judged by the
/// routing policy.
fn route_context_for(job: &AgentJob) -> agent_brain_llm::routing::RouteContext {
    use agent_brain_llm::routing::{DataClass, Initiator, Purpose, RouteContext};
    let paid = job_arg(job, "__paid").and_then(Value::as_bool) == Some(true);
    RouteContext::new(
        Purpose::Background,
        job_arg(job, "__data_class")
            .and_then(Value::as_str)
            .map(DataClass::parse_or_private)
            .unwrap_or_default(),
    )
    .with_initiator(if paid {
        Initiator::Approved
    } else {
        Initiator::Brain
    })
}

/// Prefix of the error a job dies with when its pinned model cannot run.
/// Meta-learning skips it: the fix is the step, not anything the brain learns.
pub const STEP_MODEL_ERROR: &str = "Step model cannot run";

/// A step's pinned model, trimmed; `None` when absent or blank.
fn pinned_model(step: &ChainStep) -> Option<&str> {
    step.model
        .as_deref()
        .map(str::trim)
        .filter(|m| !m.is_empty())
}

/// The `provider_hint` a step is enqueued with: derived from its pinned model
/// when the catalog knows the model, else as written. An unknown pin keeps the
/// written hint; the job then dies at execution saying the model is unknown.
fn step_provider_hint(
    step: &ChainStep,
    catalog: &[agent_brain_llm::model_router::CatalogModel],
) -> Option<String> {
    match pinned_model(step).and_then(|m| agent_brain_llm::model_router::find_model(catalog, m)) {
        Some(row) => Some(agent_brain_llm::model_router::provider_hint_for(
            &row.provider,
        )),
        None => step.provider_hint.clone(),
    }
}

/// Per-step model routing: when the step declared `required_capabilities`,
/// the model the routing policy admits for this work. `None` (normal routing)
/// when nothing qualifies or telemetry is unavailable.
fn select_step_model(
    job: &AgentJob,
    handler: &ToolHandler,
    route_ctx: agent_brain_llm::routing::RouteContext,
) -> Option<agent_brain_llm::LlmConfig> {
    // In a GPU lease a local-pinned job that runs (GPU_LEASE_LOCAL_JOBS=free)
    // takes whichever model the policy admits for reasoning, not the stopped engine.
    let leased_local = agent_brain_llm::gpu_lease::held().is_some()
        && defers_during_gpu_lease(job.provider_hint.as_deref(), job.arguments.as_ref());
    let caps: Vec<String> = if leased_local {
        vec!["reasoning".to_string()]
    } else {
        job_arg(job, "__required_capabilities")?
            .as_array()?
            .iter()
            .filter_map(|c| c.as_str().map(String::from))
            .collect()
    };
    if caps.is_empty() {
        return None;
    }
    // The base config only contributes timeout/temperature defaults; provider,
    // model, URL, and key are set by the router.
    agent_brain_llm::model_router::resolve_model_config(
        handler.telemetry()?,
        &caps,
        &agent_brain_llm::LlmConfig::default(),
        route_ctx,
    )
}

// ---------------------------------------------------------------------------
// Telling a real failure from a transient one
// ---------------------------------------------------------------------------

/// Returns `true` if a dead-job error looks like a transient or infrastructure failure
/// (rate limit, quota exhaustion, timeout, upstream 5xx) rather than a logic error the
/// brain could fix. Meta-learning on these just burns LLM cycles hypothesising about a
/// billing limit or a flaky network — e.g. repeated "SerpApi 429: run out of searches".
/// Whether a job waits out a GPU lease: it is pinned to the local model
/// (`provider_hint: local`) and has no capability routing, which would pick a
/// permitted cloud model instead.
fn defers_during_gpu_lease(
    provider_hint: Option<&str>,
    arguments: Option<&serde_json::Value>,
) -> bool {
    agent_brain_llm::model_router::is_local_hint(provider_hint)
        && arguments
            .and_then(|a| a.get("__required_capabilities"))
            .is_none()
}

/// `GPU_LEASE_LOCAL_JOBS`: `free` (default) runs local-pinned jobs on the policy's
/// cloud models during a lease; `defer` makes them wait for its end.
fn lease_jobs_go_free() -> bool {
    std::env::var("GPU_LEASE_LOCAL_JOBS").map_or(true, |v| v.trim() != "defer")
}

/// Tools that need a sidecar the lease stops, whatever model the step gets:
/// `ingest_media` may fall back to Whisper. They still wait.
const WAIT_OUT_GPU_LEASE: &[&str] = &["ingest_media"];

/// Whether a job that `defers_during_gpu_lease` really waits, in this mode.
fn waits_in_lease_mode(free: bool, tool_name: &str) -> bool {
    !free || WAIT_OUT_GPU_LEASE.contains(&tool_name)
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pinned_model_decides_the_provider_hint() {
        use agent_brain_llm::model_router::CatalogModel;
        let catalog = vec![
            CatalogModel {
                name: "gemma4:latest".into(),
                provider: "local".into(),
                model: "gemma4:latest".into(),
            },
            CatalogModel {
                name: "claude-haiku-5-5".into(),
                provider: "anthropic".into(),
                model: "claude-haiku-5-5".into(),
            },
        ];
        let step = |hint: Option<&str>, model: Option<&str>| ChainStep {
            provider_hint: hint.map(String::from),
            model: model.map(String::from),
            ..ChainStep::new("reason")
        };
        // The Zero Trust step: written cloud, pinned to a host-local model.
        let zt = step(Some("ollama-cloud"), Some("gemma4:latest"));
        assert_eq!(step_provider_hint(&zt, &catalog).as_deref(), Some("local"));
        // A cloud pin picks its provider's lane.
        let haiku = step(None, Some("claude-haiku-5-5"));
        assert_eq!(
            step_provider_hint(&haiku, &catalog).as_deref(),
            Some("anthropic")
        );
        // No pin, a blank pin, or an unknown pin keeps the hint as written.
        assert_eq!(
            step_provider_hint(&step(Some("local"), None), &catalog).as_deref(),
            Some("local")
        );
        assert_eq!(step_provider_hint(&step(None, Some("  ")), &catalog), None);
        assert_eq!(
            step_provider_hint(&step(Some("gemini"), Some("nope")), &catalog).as_deref(),
            Some("gemini")
        );
        // The pin travels in the job args deserialised from a stored step.
        let stored: ChainStep = serde_json::from_value(
            serde_json::json!({"tool_name": "reason", "model": "gemma4:latest"}),
        )
        .unwrap();
        assert_eq!(pinned_model(&stored), Some("gemma4:latest"));
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
        for dir in ["plugins/core/workflows"] {
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

#[cfg(test)]
mod gpu_lease_deferral_tests {
    use super::{defers_during_gpu_lease, waits_in_lease_mode};
    use serde_json::json;

    #[test]
    fn free_mode_runs_local_jobs_in_a_lease_but_whisper_work_still_waits() {
        assert!(!waits_in_lease_mode(true, "reason"));
        assert!(!waits_in_lease_mode(true, "neo4j_query"));
        assert!(waits_in_lease_mode(true, "ingest_media"));
        assert!(waits_in_lease_mode(false, "reason"));
    }

    #[test]
    fn only_local_pinned_jobs_without_capability_routing_wait_out_a_lease() {
        assert!(defers_during_gpu_lease(Some("local"), None));
        assert!(
            defers_during_gpu_lease(Some("ollama"), None),
            "a stored job with the old name"
        );
        assert!(defers_during_gpu_lease(
            Some("ollama"),
            Some(&json!({"query": "x"}))
        ));
        assert!(!defers_during_gpu_lease(
            Some("ollama"),
            Some(&json!({"__required_capabilities": ["reasoning"]}))
        ));
        assert!(!defers_during_gpu_lease(Some("ollama-cloud"), None));
        assert!(!defers_during_gpu_lease(None, None));
    }
}
