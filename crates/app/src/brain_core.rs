//! Brain Core — the autonomous agent's runtime engine.
//!
//! [`BrainCore`] owns all the stateful services that make the agent work:
//! storage (Neo4j, DuckDB), background jobs (scheduler + queue), LLM config,
//! and the skill/tool registry.  It exposes a simple control-plane surface:
//!
//! - [`BrainCore::initialize`] — build skills, seed Neo4j, run boot protocol
//! - [`BrainCore::list_tools`] — all registered tool definitions
//! - [`BrainCore::list_tools_filtered`] — tools filtered by a context profile
//! - [`BrainCore::has_tool`] — existence check without locking across an await
//! - [`BrainCore::try_execute_tool`] — dispatch a named tool call
//! - [`BrainCore::notify_scheduler_activity`] — wake the scheduler from sleep
//!
//! Transport adapters (MCP, REST, chat) hold an `Arc<BrainCore>` or a plain
//! `BrainCore` by value and call through these methods; they never reach into
//! brain internals directly.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{Value, json};
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::mcp::tools::{ToolHandler, ToolRegistry};
use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::queue::QueueService;
use crate::services::resource_registry::ResourceRegistry;
use crate::services::{
    AgentRunner, ContextBuilderService, KnowledgeService, LlmConfig, MediaService, SharedLlm,
    SnapshotService,
};
#[cfg(feature = "websocket")]
use crate::skills::ws::WsSkill;
use crate::skills::{
    Skill, agent::AgentSkill, agent_runner::AgentRunnerSkill, claims::ClaimSkill,
    codebase::CodebaseSkill, constructor::ConstructorSkill, context::ContextSkill,
    dynamic::DynamicSkill, exec::ExecSkill, git::GitSkill, http::HttpSkill,
    knowledge::KnowledgeSkill, mcp_client::McpClientSkill, media::MediaSkill, model::ModelSkill,
    query::QuerySkill, resource::ResourceSkill, scheduler::SchedulerSkill, search::SearchSkill,
    sleep::SleepSkill, task::TaskSkill, working_memory::WorkingMemorySkill,
};
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

use crate::services::SchedulerService;

// ============================================================================
// BrainEvent — the internal event bus
// ============================================================================

/// Events emitted by the brain core and broadcast to all subscribers.
///
/// Transport adapters (HTTP, REST) subscribe via [`BrainCore::subscribe`] and
/// route events to their own notification channels (e.g. SSE push to client).
#[derive(Debug, Clone)]
pub enum BrainEvent {
    /// A background job completed successfully.
    JobCompleted {
        job_id: String,
        tool_name: String,
        session_id: Option<String>,
        result_preview: Option<String>,
    },
    /// A background job failed but may be retried.
    JobFailed {
        job_id: String,
        tool_name: String,
        session_id: Option<String>,
        error: String,
    },
    /// A background job exhausted all retries and is permanently dead.
    JobDead {
        job_id: String,
        tool_name: String,
        session_id: Option<String>,
        error: String,
    },
    /// The brain created a proactive notification for the user.
    AgentChatInitiated {
        notification_id: String,
        message: String,
        /// Optional session the brain wants to continue (for context).
        related_session_id: Option<String>,
    },
    /// The scheduler completed one autonomous tick.
    SchedulerTick { tasks_dispatched: usize },
    /// The scheduler entered idle/sleep mode.
    SchedulerSleepEntered,
    /// The scheduler woke up from idle/sleep mode.
    SchedulerSleepExited,
}

/// Broadcast channel capacity for [`BrainEvent`].
const BRAIN_EVENT_CAPACITY: usize = 256;

/// Whether the `run_agent` background tool is registered. Reuses
/// `SUBAGENT_ENABLED` — the same switch that enables chat `delegate` — because
/// sub-agents are the mechanism, and the runner shares the chat loop engine.
/// Default OFF, like the chat spike, until it is measured (plan §7).
fn agent_runner_enabled() -> bool {
    std::env::var("SUBAGENT_ENABLED")
        .map(|v| matches!(v.trim(), "1" | "true" | "yes"))
        .unwrap_or(false)
}

// ============================================================================
// Sub-configs (grouped brain resources)
// ============================================================================

/// Storage-related services (database, telemetry, credentials, data directory).
pub struct StorageConfig {
    pub neo4j: Option<Neo4jClient>,
    pub telemetry: Option<TelemetryClient>,
    pub dataset_dir: PathBuf,
    /// Directory for context profile YAML files.
    pub contexts_dir: PathBuf,
    /// Directory for knowledge graph snapshots.
    pub snapshot_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            neo4j: None,
            telemetry: None,
            dataset_dir: std::env::var("DATASET_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./datasets")),
            contexts_dir: std::env::var("CONTEXTS_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./contexts")),
            snapshot_dir: std::env::var("KNOWLEDGE_SNAPSHOT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./snapshots")),
        }
    }
}

/// Web search API keys.
pub struct SearchConfig {
    pub brave_key: Option<String>,
    pub google_key: Option<String>,
    pub google_cx: Option<String>,
    pub serpapi_key: Option<String>,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            brave_key: std::env::var("BRAVE_API_KEY").ok(),
            google_key: std::env::var("GOOGLE_API_KEY").ok(),
            google_cx: std::env::var("GOOGLE_CX").ok(),
            serpapi_key: std::env::var("SERPAPI_KEY").ok(),
        }
    }
}

/// Codebase self-analysis config (read-only access to the agent's own source).
pub struct CodebaseConfig {
    /// Root directory of the codebase. Auto-detected from `Cargo.toml` walk-up if unset.
    pub codebase_dir: Option<PathBuf>,
    /// Writable workspace for generated code, scripts, and experiments. Set via `WORKSPACE_DIR`.
    pub workspace_dir: Option<PathBuf>,
    /// Directory where the brain writes structured fix proposals. Defaults to `./proposals`.
    pub proposals_dir: Option<PathBuf>,
}

impl Default for CodebaseConfig {
    fn default() -> Self {
        let codebase_dir = std::env::var("CODEBASE_DIR")
            .map(PathBuf::from)
            .ok()
            .or_else(crate::skills::codebase::detect_repo_root);
        let workspace_dir = std::env::var("WORKSPACE_DIR").map(PathBuf::from).ok();
        let proposals_dir = std::env::var("PROPOSALS_DIR")
            .map(PathBuf::from)
            .ok()
            .or_else(|| Some(PathBuf::from("./proposals")));
        Self {
            codebase_dir,
            workspace_dir,
            proposals_dir,
        }
    }
}

/// Background job services (queue + scheduler).
pub struct JobServices {
    pub queue: Arc<RwLock<Option<Arc<QueueService>>>>,
    pub scheduler: Arc<RwLock<Option<Arc<SchedulerService>>>>,
}

impl Default for JobServices {
    fn default() -> Self {
        Self {
            queue: Arc::new(RwLock::new(None)),
            scheduler: Arc::new(RwLock::new(None)),
        }
    }
}

// ============================================================================
// BrainCore
// ============================================================================

/// The autonomous agent's runtime engine.
///
/// Owns all stateful brain resources: storage, LLM config, skill/tool
/// registry, background jobs, and context profiles.  Transport adapters
/// (MCP server, REST API, chat service) receive a reference to this and
/// call through the control-plane methods rather than reaching into internals.
pub struct BrainCore {
    // --- tool registry / dispatch ---
    pub(crate) tool_registry: Arc<RwLock<ToolRegistry>>,
    pub(crate) tool_handler: Arc<RwLock<Option<ToolHandler>>>,

    // --- storage, LLM, search ---
    pub(crate) storage: StorageConfig,
    pub(crate) llm_config: Arc<RwLock<Option<LlmConfig>>>,
    search: SearchConfig,
    codebase: CodebaseConfig,

    // --- background services ---
    pub(crate) jobs: JobServices,

    // --- model catalog / system prompt ---
    /// Active system prompt loaded from models.yaml at startup (currently
    /// stored but not yet consumed inside BrainCore; clients may read it).
    pub system_prompt: String,
    /// Path to models.yaml, forwarded to ModelSkill for hot-reload.
    pub(crate) catalog_path: PathBuf,

    // --- context profiles ---
    pub(crate) context_builder_svc: Arc<RwLock<Option<Arc<ContextBuilderService>>>>,

    // --- shared agent-execution primitive ---
    /// The shared [`AgentRunner`] backing the `run_agent` background tool.
    /// Populated when the `ChatService` (its loop engine) is created —
    /// empty until then, and on deployments without a chat service (stdio),
    /// where `run_agent` reports itself unavailable. See
    /// `project-docs/UNIFIED_AGENT_EXECUTION_PLAN.md` Phase 1.
    pub(crate) agent_runner: Arc<RwLock<Option<Arc<AgentRunner>>>>,

    // --- event bus ---
    /// Broadcast sender for internal brain events.  Subscribers receive a
    /// cloned [`broadcast::Receiver`] via [`BrainCore::subscribe`].
    event_tx: broadcast::Sender<BrainEvent>,
}

impl BrainCore {
    /// Create a new BrainCore with defaults read from environment variables.
    pub fn new() -> Self {
        let (event_tx, _) = broadcast::channel(BRAIN_EVENT_CAPACITY);
        Self {
            tool_registry: Arc::new(RwLock::new(ToolRegistry::new())),
            tool_handler: Arc::new(RwLock::new(None)),
            storage: StorageConfig::default(),
            llm_config: Arc::new(RwLock::new(None)),
            search: SearchConfig::default(),
            codebase: CodebaseConfig::default(),
            jobs: JobServices::default(),
            system_prompt: String::new(),
            catalog_path: std::env::var("MODEL_CATALOG_PATH")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("models.yaml")),
            context_builder_svc: Arc::new(RwLock::new(None)),
            agent_runner: Arc::new(RwLock::new(None)),
            event_tx,
        }
    }

    /// Subscribe to brain events.
    ///
    /// Returns a [`broadcast::Receiver`] that receives all [`BrainEvent`]s
    /// emitted after the subscription is created.  Lagging receivers will see
    /// [`broadcast::error::RecvError::Lagged`] if they fall too far behind.
    pub fn subscribe(&self) -> broadcast::Receiver<BrainEvent> {
        self.event_tx.subscribe()
    }

    /// Return a clone of the underlying broadcast sender.
    ///
    /// Callers (e.g. the HTTP transport) can subscribe by calling
    /// `sender.subscribe()` on the returned handle — this way there is no
    /// need to thread a `Receiver` through configuration structs.
    pub fn event_sender(&self) -> broadcast::Sender<BrainEvent> {
        self.event_tx.clone()
    }

    // ========================================================================
    // Builder methods
    // ========================================================================

    pub fn with_neo4j(mut self, neo4j: Neo4jClient) -> Self {
        self.storage.neo4j = Some(neo4j);
        self
    }

    pub fn with_telemetry(mut self, telemetry: TelemetryClient) -> Self {
        self.storage.telemetry = Some(telemetry);
        self
    }

    pub fn with_llm_config(mut self, config: LlmConfig) -> Self {
        self.llm_config = Arc::new(RwLock::new(Some(config)));
        self
    }

    pub fn with_brave_api_key(mut self, key: impl Into<String>) -> Self {
        self.search.brave_key = Some(key.into());
        self
    }

    pub fn with_google_config(mut self, key: impl Into<String>, cx: impl Into<String>) -> Self {
        self.search.google_key = Some(key.into());
        self.search.google_cx = Some(cx.into());
        self
    }

    pub fn with_serpapi_key(mut self, key: impl Into<String>) -> Self {
        self.search.serpapi_key = Some(key.into());
        self
    }

    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = prompt;
        self
    }

    pub fn with_catalog_path(mut self, path: PathBuf) -> Self {
        self.catalog_path = path;
        self
    }

    // ========================================================================
    // Control-plane surface
    // ========================================================================

    /// Returns all registered tool definitions.
    pub async fn list_tools(&self) -> Vec<ToolDefinition> {
        self.tool_registry.read().await.list()
    }

    /// Returns tool definitions filtered to those allowed by the named context
    /// profile.  Falls back to all tools if the profile is not found.
    pub async fn list_tools_filtered(&self, profile_name: &str) -> Vec<ToolDefinition> {
        let all_tools = self.list_tools().await;
        let cb_opt = self.context_builder_svc.read().await.clone();
        if let Some(cb) = cb_opt {
            if let Some(profile) = cb.get_profile(profile_name).await {
                filter_tools_by_names(all_tools, &profile.tools)
            } else {
                tracing::warn!(
                    profile = %profile_name,
                    "list_tools_filtered: profile not found — returning all tools"
                );
                all_tools
            }
        } else {
            all_tools
        }
    }

    /// Returns `true` if a tool with `name` is registered.
    pub async fn has_tool(&self, name: &str) -> bool {
        self.tool_registry.read().await.get(name).is_some()
    }

    /// Execute a tool by name.
    ///
    /// Returns `Err(msg)` if the tool handler has not been initialized yet
    /// (i.e. [`initialize`] has not been called), so the caller can map the
    /// error into whatever error type is appropriate for its protocol layer.
    pub async fn try_execute_tool(
        &self,
        name: &str,
        args: Option<Value>,
    ) -> Result<ToolCallResult, String> {
        let handler = {
            let guard = self.tool_handler.read().await;
            guard.clone()
        };
        match handler {
            Some(h) => Ok(h.execute(name, args).await),
            None => Err("Tool handler not initialized".to_string()),
        }
    }

    /// Notify the scheduler that a tool call just happened.
    /// Wakes the scheduler if it is in sleep/idle mode.
    pub async fn notify_scheduler_activity(&self) {
        if let Some(sched) = self.jobs.scheduler.read().await.as_ref() {
            sched.notify_activity().await;
        }
    }

    /// Return the scheduler `Arc` handle so callers can attach it to transport
    /// config before [`initialize`] is called.  The `Option` becomes `Some`
    /// once `initialize` (and thus `build_skills`) has run.
    pub fn scheduler_handle(&self) -> Arc<RwLock<Option<Arc<SchedulerService>>>> {
        Arc::clone(&self.jobs.scheduler)
    }

    /// Return the context-builder handle.  Same lazy-init pattern as the
    /// scheduler: the inner `Option` is `None` until `build_skills` completes.
    pub fn context_builder_handle(&self) -> Arc<RwLock<Option<Arc<ContextBuilderService>>>> {
        Arc::clone(&self.context_builder_svc)
    }

    /// Return the shared `AgentRunner` slot backing the `run_agent` tool.
    /// Populated once the `ChatService` (the loop engine) is created — see
    /// `McpServerCore::chat_service`. Empty until then, and on deployments
    /// without a chat service.
    pub fn agent_runner_handle(&self) -> Arc<RwLock<Option<Arc<AgentRunner>>>> {
        Arc::clone(&self.agent_runner)
    }

    /// Return the live LLM-config Arc so HTTP transport can serve `/api/models`.
    pub fn llm_config_arc(&self) -> Arc<RwLock<Option<LlmConfig>>> {
        Arc::clone(&self.llm_config)
    }

    /// Return the tool registry Arc so HTTP transport can serve `/api/skills`.
    pub fn tool_registry_handle(&self) -> Arc<RwLock<crate::mcp::tools::ToolRegistry>> {
        Arc::clone(&self.tool_registry)
    }

    /// Return a clone of the telemetry client (if one was configured).
    pub fn telemetry(&self) -> Option<TelemetryClient> {
        self.storage.telemetry.clone()
    }

    // ========================================================================
    // Initialization
    // ========================================================================

    /// Build the skill registry and run the boot protocol.
    ///
    /// This is the main initialization entry-point.  It calls [`build_skills`]
    /// then runs the `boot` context profile protocol (non-fatal if missing).
    pub async fn initialize(&self) {
        self.build_skills().await;
        let cb_opt = self.context_builder_svc.read().await.clone();
        if let Some(ref cb) = cb_opt {
            let handler = Arc::clone(&self.tool_handler);
            if let Err(e) = cb
                .run_protocol("boot", handler, self.storage.neo4j.as_ref())
                .await
            {
                warn!(error = %e, "Boot protocol error (non-fatal)");
            }
        }
    }

    /// Instantiate every skill and populate the tool registry.
    ///
    /// Idempotent — calling it again clears and re-registers all skills, which
    /// is useful after a `use_model` hot-swap.
    pub async fn build_skills(&self) {
        // Seed built-in Neo4j nodes FIRST so DynamicSkill::load_from_neo4j picks them up.
        if let Some(ref neo4j) = self.storage.neo4j {
            Self::seed_built_ins(neo4j).await;
        }

        // Build DynamicSkill (before taking locks) so we can share the Arc.
        // Both the registry clone and the handler original share the same tools_map.
        let dynamic_skill = if let Some(neo4j) = &self.storage.neo4j {
            let d = DynamicSkill::new(
                neo4j.clone(),
                self.tool_handler.clone(),
                Arc::clone(&self.tool_registry),
            );
            d.load_from_neo4j().await;
            Some(d)
        } else {
            None
        };

        // Create (or reuse) QueueService when Neo4j is available.
        let queue_arc: Option<Arc<QueueService>> = if let Some(neo4j) = &self.storage.neo4j {
            let mut qs_guard = self.jobs.queue.write().await;
            if qs_guard.is_none() {
                let qs = Arc::new(QueueService::new(
                    neo4j.clone(),
                    self.tool_handler.clone(),
                    Some(self.event_tx.clone()),
                ));
                qs.recover().await;
                *qs_guard = Some(Arc::clone(&qs));
            }
            qs_guard.as_ref().map(Arc::clone)
        } else {
            None
        };

        // Create SnapshotService when Neo4j is available.
        let snapshot_svc: Option<Arc<SnapshotService>> = self.storage.neo4j.as_ref().map(|db| {
            Arc::new(SnapshotService::new(
                db.clone(),
                self.storage.snapshot_dir.clone(),
            ))
        });

        // Create ContextBuilderService and load profiles (must be before scheduler).
        let context_builder_arc: Option<Arc<ContextBuilderService>> = {
            let svc = Arc::new(ContextBuilderService::new(
                self.storage.neo4j.clone(),
                self.storage.contexts_dir.clone(),
                Arc::clone(&self.llm_config),
            ));
            let n = svc.load_profiles().await.unwrap_or(0);
            info!(count = n, "Loaded context profiles");
            *self.context_builder_svc.write().await = Some(Arc::clone(&svc));
            Some(svc)
        };

        // Local-Ollama config for background jobs — always points to localhost,
        // so maintenance tasks never touch cloud quota even when the active
        // provider is ollama-cloud or anthropic.
        let local_config_arc = {
            // Backend (Ollama vs llama.cpp) is chosen by LOCAL_LLM_BACKEND; the
            // local model needs a far larger request budget than the shared 120s
            // default (see `DEFAULT_LOCAL_TIMEOUT_SECS`) because on a shared
            // consumer GPU a background call takes 54–79s idle and 80–120s under
            // the media-watch burst. Embeddings stay pinned to local Ollama
            // regardless of the generation backend. See `build_local_llm_config`.
            let local_llm_config = crate::services::llm::build_local_llm_config();
            info!(
                provider = %local_llm_config.provider,
                model = %local_llm_config.model,
                base_url = local_llm_config.base_url.as_deref().unwrap_or(""),
                timeout_secs = local_llm_config.timeout.as_secs(),
                "Local background LLM configured"
            );
            Arc::new(RwLock::new(Some(local_llm_config)))
        };

        // Create (or reuse) SchedulerService when Neo4j + Queue are available.
        let scheduler_arc: Option<Arc<SchedulerService>> =
            if let (Some(neo4j), Some(qs)) = (&self.storage.neo4j, &queue_arc) {
                let mut g = self.jobs.scheduler.write().await;
                if g.is_none() {
                    *g = Some(SchedulerService::new_with_context(
                        neo4j.clone(),
                        Arc::clone(qs),
                        context_builder_arc.clone(),
                        Some(Arc::clone(&local_config_arc)),
                    ));
                }
                g.as_ref().map(Arc::clone)
            } else {
                None
            };

        // Local-only LLM for internal knowledge ops (entity extraction, etc.)
        // Always routes to local Ollama regardless of the active provider.
        // It records to the usage ledger like every other LLM path: before
        // 2026-10-01 it had no telemetry sink, so consolidation and entity
        // extraction never appeared in `model_usage` at all.
        let local_llm = SharedLlm::new_with_local(
            Arc::clone(&local_config_arc),
            Arc::clone(&local_config_arc),
            self.storage.telemetry.clone(),
        );

        // Give the queue the local config for distilled chain handoffs. It builds
        // its own client from this so it can raise num_ctx without affecting any
        // other consumer. Re-applied on every build_skills() so a reload refreshes it.
        if let Some(qs) = &queue_arc
            && let Some(cfg) = local_config_arc.read().await.clone()
        {
            qs.set_distill_config(cfg).await;
        }

        // Shared LLM provider (wraps live Arc<RwLock<Option<LlmConfig>>>)
        let shared_llm = SharedLlm::new_with_local(
            Arc::clone(&self.llm_config),
            local_config_arc,
            self.storage.telemetry.clone(),
        );

        // GPU lease (services/gpu_lease.rs): reload a lease that survived a
        // restart, and on release, expiry or a startup with none held, embed and
        // link the notes stored without a vector. Re-run on every build_skills().
        if let Some(neo4j) = &self.storage.neo4j {
            let backfill = Arc::new(KnowledgeService::new(
                neo4j.clone(),
                Some(Arc::clone(&local_llm) as Arc<dyn crate::services::LlmProvider>),
            ));
            crate::services::gpu_lease::init(
                neo4j.clone(),
                Arc::new(move || {
                    let ks = Arc::clone(&backfill);
                    if let Ok(rt) = tokio::runtime::Handle::try_current() {
                        rt.spawn(async move { ks.run_embedding_backfill().await });
                    }
                }),
            )
            .await;
        }

        // ── Build every skill ONCE ─────────────────────────────────────────
        // The registry (tools/list) and the handler (tools/call) each hold an
        // `Arc` of the same instance. They used to be two separately built
        // lists, kept in step by hand: forgetting one side made a tool
        // invisible or undispatchable, and the copies had already drifted (the
        // registry's MediaSkill had no knowledge store, its SchedulerSkill no
        // tool snapshots). Order matters — dispatch is first-match.
        let llm: Arc<dyn crate::services::LlmProvider> = Arc::clone(&shared_llm) as _;
        let local: Arc<dyn crate::services::LlmProvider> = Arc::clone(&local_llm) as _;
        // One plain knowledge store shared by every skill that reads notes;
        // KnowledgeSkill gets its own, snapshot-enabled, because consolidation
        // runs through it and must snapshot first.
        let knowledge: Option<Arc<dyn crate::services::KnowledgeStore>> =
            self.storage.neo4j.as_ref().map(|neo4j| {
                Arc::new(KnowledgeService::new(
                    neo4j.clone(),
                    Some(Arc::clone(&local)),
                )) as _
            });

        // Filled from the finished registry below; the SchedulerSkill reads them
        // per call to validate chain steps against the tools that exist.
        let live_tools_arc: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(Vec::new()));
        let tool_schemas_arc: Arc<RwLock<std::collections::HashMap<String, serde_json::Value>>> =
            Arc::new(RwLock::new(std::collections::HashMap::new()));
        // Object-envelope JSON Schemas from the `schemas/` seed dir (SCHEMAS_DIR,
        // default ./schemas). Loaded once; a missing dir or bad file degrades to
        // no envelope validation (the per-step arg gate still runs). See
        // services/validation.rs + project-docs/VALIDATION_AND_APPLY_PLAN.md.
        let schemas_dir = std::env::var("SCHEMAS_DIR").unwrap_or_else(|_| "./schemas".to_string());
        let envelope_schemas_arc: Arc<std::collections::HashMap<String, serde_json::Value>> =
            Arc::new(crate::services::validation::load_schema_dir(
                std::path::Path::new(&schemas_dir),
            ));

        let mut skills: Vec<Arc<dyn Skill>> = Vec::new();

        if let Some(neo4j) = &self.storage.neo4j {
            let mut ks = KnowledgeService::new(neo4j.clone(), Some(Arc::clone(&local)));
            if let Some(ref snap) = snapshot_svc {
                ks = ks.with_snapshot(Arc::clone(snap), true, true);
            }
            skills.push(Arc::new(KnowledgeSkill::new(
                Arc::new(ks),
                Arc::clone(&llm),
            )));
        }

        let task_store: Option<Arc<dyn crate::services::TaskStore>> = self
            .storage
            .neo4j
            .as_ref()
            .map(|n| Arc::new(n.clone()) as Arc<dyn crate::services::TaskStore>);
        skills.push(Arc::new(TaskSkill::new(
            Arc::clone(&llm),
            task_store,
            queue_arc.clone(),
        )));

        // The knowledge store gives search_web's prior-knowledge probe hybrid
        // recall — see `SearchSkill::with_knowledge`.
        let mut search =
            SearchSkill::new(self.storage.telemetry.clone(), self.storage.neo4j.clone());
        if let Some(k) = &knowledge {
            search = search.with_knowledge(Arc::clone(k));
        }
        skills.push(Arc::new(search));

        // Claims — epistemic status for ingested assertions.
        if let Some(neo4j) = &self.storage.neo4j {
            skills.push(Arc::new(ClaimSkill::new(
                neo4j.clone(),
                Arc::clone(&llm),
                self.storage.telemetry.clone(),
            )));
        }

        // ETL-loaded datasets (GDELT events, ReliefWeb reports) in their own
        // DuckDB file. Absent the file, the tools are not registered.
        if let Some(ds) =
            crate::skills::datasets::DatasetSkill::from_env(self.storage.neo4j.clone())
        {
            skills.push(Arc::new(ds));
        }

        // ACLED conflict-event data. Absent credentials the tool is not
        // registered at all, rather than registered and always failing.
        match crate::skills::acled::AcledSkill::from_env() {
            Some(acled) => skills.push(Arc::new(acled)),
            None => debug!(
                "ACLED not configured (ACLED_EMAIL/ACLED_PASSWORD) — acled_events not registered"
            ),
        }

        // Generic Neo4j + DuckDB primitives.
        skills.push(Arc::new(QuerySkill::new(
            self.storage.neo4j.clone(),
            self.storage.telemetry.clone(),
        )));
        // http_request + ApiContext management.
        skills.push(Arc::new(HttpSkill::new(
            self.storage.neo4j.clone(),
            Some(Arc::clone(&llm)),
        )));
        // Tools on external MCP servers (:McpServer nodes).
        skills.push(Arc::new(McpClientSkill::new(self.storage.neo4j.clone())));
        // Sandboxed code execution. Absent SANDBOX_URL the tool is not
        // registered at all, rather than registered and always failing.
        if let Some(exec_skill) = ExecSkill::from_env() {
            skills.push(Arc::new(exec_skill));
        }

        // Video/podcast ingestion + watchlist (yt-dlp captions, map-reduce
        // summarization inside the tool).
        if let Some(neo4j) = &self.storage.neo4j {
            skills.push(Arc::new(MediaSkill::new(
                Arc::new(MediaService::from_env()),
                Arc::clone(&llm),
                neo4j.clone(),
                knowledge.clone(),
                self.storage.telemetry.clone(),
            )));
        }

        skills.push(Arc::new(CodebaseSkill::new(
            self.codebase.codebase_dir.clone(),
            self.codebase.workspace_dir.clone(),
            self.codebase.proposals_dir.clone(),
            knowledge.clone(),
            self.storage.neo4j.clone(),
        )));
        // Branch/commit/push/PR — always registered; requires CODEBASE_DIR.
        skills.push(Arc::new(GitSkill::new(self.codebase.codebase_dir.clone())));
        // DuckDB-backed model catalog; shares the live LLM config.
        skills.push(Arc::new(ModelSkill::new(
            self.llm_config.clone(),
            self.storage.telemetry.clone(),
            self.catalog_path.clone(),
        )));
        if let Some(telemetry) = &self.storage.telemetry {
            skills.push(Arc::new(SleepSkill::new(
                telemetry.clone(),
                self.storage.dataset_dir.clone(),
            )));
        }

        if let (Some(neo4j), Some(k)) = (&self.storage.neo4j, &knowledge) {
            let wm_store: Arc<dyn crate::services::WorkingMemoryStore> = Arc::new(neo4j.clone());
            skills.push(Arc::new(
                WorkingMemorySkill::new(wm_store, Arc::clone(k), Arc::clone(&llm))
                    .with_notification_support(Arc::new(neo4j.clone()), self.event_tx.clone()),
            ));
        }

        if let Some(qs) = &queue_arc {
            skills.push(Arc::new(AgentSkill::new(Arc::clone(qs))));
        }
        if let Some(sched) = &scheduler_arc {
            skills.push(Arc::new(
                SchedulerSkill::new(Arc::clone(sched), self.storage.neo4j.clone())
                    .with_live_tools(Arc::clone(&live_tools_arc))
                    .with_tool_schemas(Arc::clone(&tool_schemas_arc))
                    .with_envelope_schemas(Arc::clone(&envelope_schemas_arc))
                    .with_telemetry(self.storage.telemetry.clone()),
            ));
        }
        if let Some(cb) = &context_builder_arc {
            skills.push(Arc::new(ContextSkill::new(Arc::clone(cb))));
        }
        // Agent Constructor (needs the self-model in Neo4j + queue for dispatch).
        if let Some(neo4j) = &self.storage.neo4j {
            skills.push(Arc::new(ConstructorSkill::new(
                neo4j.clone(),
                queue_arc.clone(),
                Arc::clone(&llm),
                self.storage.telemetry.clone(),
            )));
        }

        #[cfg(feature = "websocket")]
        skills.push(Arc::new(WsSkill::new()));
        skills.push(Arc::new(ResourceSkill::new(Arc::new(
            ResourceRegistry::new(),
        ))));

        // `run_agent` — gated on SUBAGENT_ENABLED (the same switch that turns on
        // chat `delegate`; sub-agents are the mechanism). The runner slot is
        // empty until the ChatService (its loop engine) is created; the tool
        // reports itself unavailable until then.
        if agent_runner_enabled() {
            skills.push(Arc::new(
                AgentRunnerSkill::new(Arc::clone(&self.agent_runner))
                    .with_codebase_dir(self.codebase.codebase_dir.clone()),
            ));
        }

        // Runtime-defined tools last: a DynamicTool can never shadow a built-in.
        if let Some(d) = dynamic_skill {
            skills.push(Arc::new(d));
        }

        // ── Registry (tools/list) ─────────────────────────────────────────
        let mut registry = self.tool_registry.write().await;
        // Cleared so a reload re-registers rather than duplicating.
        registry.clear();
        for skill in &skills {
            registry.register_skill(Box::new(Arc::clone(skill)));
        }

        // Snapshot each tool's `input_schema`, keyed by name, for the scheduler's
        // step validation (services/validation.rs). FIRST definition wins,
        // because dispatch is first-match too: a DynamicTool reusing a
        // built-in's name never runs, but a last-wins map validated steps
        // against ITS schema — a 09-07 dynamic `ingest_api` (spec_url required)
        // made the audit reject the brain-api-sync schedule's valid call.
        let mut tool_schemas = std::collections::HashMap::new();
        let mut live_tool_names = Vec::new();
        for t in registry.list() {
            live_tool_names.push(t.name.clone());
            if tool_schemas.contains_key(&t.name) {
                warn!(tool = %t.name, "Duplicate tool name in registry — only the first definition dispatches; later ones are shadowed");
                continue;
            }
            tool_schemas.insert(t.name.clone(), t.input_schema.clone());
        }
        drop(registry);
        *live_tools_arc.write().await = live_tool_names;
        *tool_schemas_arc.write().await = tool_schemas;

        // ── Handler (tools/call) ──────────────────────────────────────────
        let mut handler = self.tool_handler.write().await;
        let mut tool_handler = ToolHandler::new(
            skills
                .iter()
                .map(|s| Box::new(Arc::clone(s)) as Box<dyn Skill>)
                .collect(),
        );
        if let Some(ref tel) = self.storage.telemetry {
            tool_handler = tool_handler.with_telemetry(tel.clone());
        }
        *handler = Some(tool_handler);

        // Spawn the queue coordinator now that the tool handler is populated.
        if let Some(qs) = queue_arc {
            QueueService::spawn_coordinator(qs);
        }

        // Sync the self-model meta-graph (ToolDef / ContextProfile / ModelDef)
        // now that the registry is fully populated. Non-fatal: a missing
        // self-model degrades the Agent Constructor, not the brain.
        if let Some(ref neo4j) = self.storage.neo4j {
            let skills_with_tools = self.tool_registry.read().await.skills_with_tools();
            let profiles = match context_builder_arc {
                Some(ref cb) => cb.list_profiles().await,
                None => Vec::new(),
            };
            if let Err(e) = crate::services::self_model::sync_self_model(
                neo4j,
                &skills_with_tools,
                &profiles,
                self.storage.telemetry.as_ref(),
            )
            .await
            {
                warn!(error = %e, "Self-model sync failed (continuing without it)");
            }

            // Record which commit this process is actually running. Same
            // non-fatal contract as the sync above.
            if let Some(ref dir) = self.codebase.codebase_dir
                && let Err(e) = crate::services::self_model::sync_code_version(neo4j, dir).await
            {
                warn!(error = %e, "Code-version sync failed (continuing without it)");
            }

            // The architecture view's "what changed" needs a baseline; take the
            // first snapshot now rather than after the first weekly run.
            match crate::services::architecture::ensure_baseline(neo4j).await {
                Ok(Some(s)) => info!(summary = %s.summary, "Architecture baseline snapshot taken"),
                Ok(None) => {}
                Err(e) => warn!(error = %e, "Architecture baseline snapshot failed (continuing)"),
            }
        }
    }

    // ========================================================================
    // Startup seeding
    // ========================================================================

    /// Seed all built-in Neo4j nodes idempotently at every startup.
    ///
    /// Called at the top of [`build_skills`] — before
    /// `DynamicSkill::load_from_neo4j()` — so every seeded `DynamicTool` is
    /// available in the tool registry on the very first boot.
    ///
    /// All writes use `MERGE … ON CREATE SET` so user-edited nodes survive
    /// restarts.
    pub async fn seed_built_ins(neo4j: &Neo4jClient) {
        let ts = chrono::Utc::now().to_rfc3339();

        // ── ApiContext nodes ──────────────────────────────────────────────
        // GitHub — always update so the standard headers stay current.
        let default_hdrs =
            r#"{"Accept":"application/vnd.github+json","X-GitHub-Api-Version":"2022-11-28"}"#;
        let cypher = "MERGE (c:ApiContext {name: 'github'}) \
                      SET c.base_url        = 'https://api.github.com', \
                          c.auth_scheme     = 'bearer', \
                          c.auth_param      = 'Authorization', \
                          c.auth_env_var    = 'GITHUB_TOKEN', \
                          c.default_headers = $hdrs, \
                          c.description     = 'GitHub REST API v3'";
        if let Err(e) = neo4j
            .run(neo4rs::query(cypher).param("hdrs", default_hdrs))
            .await
        {
            warn!(error = %e, "Failed to seed github ApiContext (non-fatal)");
        }

        // Search engines — ON CREATE only so user overrides survive.
        for (name, base_url, scheme, param, env_var, desc) in [
            (
                "serpapi",
                "https://serpapi.com",
                "query_param",
                "api_key",
                "SERPAPI_KEY",
                "SerpApi search engine",
            ),
            (
                "brave",
                "https://api.search.brave.com",
                "header",
                "X-Subscription-Token",
                "BRAVE_API_KEY",
                "Brave Search API",
            ),
            (
                "google_cse",
                "https://www.googleapis.com/customsearch/v1",
                "query_param",
                "key",
                "GOOGLE_API_KEY",
                "Google Custom Search Engine",
            ),
            (
                "apitube",
                "https://api.apitube.io/v1",
                "header",
                "X-API-Key",
                "APITUBE_API_KEY",
                "ApiTube news API — structured, dated news articles (results[]: title, href, published_at, source.domain, description)",
            ),
        ] {
            let q = "MERGE (c:ApiContext {name: $name}) \
                     ON CREATE SET c.base_url     = $base_url, \
                                   c.auth_scheme  = $scheme, \
                                   c.auth_param   = $param, \
                                   c.auth_env_var = $env_var, \
                                   c.description  = $desc";
            if let Err(e) = neo4j
                .run(
                    neo4rs::query(q)
                        .param("name", name)
                        .param("base_url", base_url)
                        .param("scheme", scheme)
                        .param("param", param)
                        .param("env_var", env_var)
                        .param("desc", desc),
                )
                .await
            {
                warn!(name = name, error = %e, "Failed to seed search ApiContext (non-fatal)");
            }
        }
        if let Ok(cx) = std::env::var("GOOGLE_CX") {
            let _ = neo4j
                .run(
                    neo4rs::query(
                        "MATCH (c:ApiContext {name: 'google_cse'}) SET c.google_cx = $cx",
                    )
                    .param("cx", cx),
                )
                .await;
        }

        // ── Procedure + DynamicTool pairs ─────────────────────────────────
        let tools: &[(&str, &str, &str, &str, &str)] = &[
            (
                "list_sessions",
                "List working-memory sessions ordered by most recent first. \
                 Returns session_id, started_at, message count, and title.",
                r#"{"type":"object","properties":{}}"#,
                "MATCH (w:WorkingMemory) \
                 WITH w.session_id AS sid, toString(min(w.created_at)) AS started_at, count(w) AS msg_count \
                 OPTIONAL MATCH (first:WorkingMemory {session_id: sid, turn_index: 0}) \
                 RETURN sid AS session_id, started_at, msg_count, COALESCE(first.content, sid) AS title \
                 ORDER BY started_at DESC LIMIT 50",
                "List all working-memory sessions with metadata",
            ),
            (
                "get_job_result",
                "Get the full details and result of a background job by its ID.",
                r#"{"type":"object","properties":{"job_id":{"type":"string","description":"Job ID returned by enqueue_jobs"}},"required":["job_id"]}"#,
                "MATCH (j:AgentJob {id: '{{input.job_id}}'}) \
                 RETURN j.id AS id, j.tool_name AS tool_name, j.status AS status, \
                        j.result_json AS result_json, j.error AS error, \
                        j.priority AS priority, j.attempt_count AS attempt_count, \
                        toString(j.created_at) AS created_at",
                "Fetch agent job by ID",
            ),
            (
                "search_procedures",
                "Search stored procedures by name or description using keyword matching.",
                r#"{"type":"object","properties":{"query":{"type":"string","description":"Keyword to search in procedure names and descriptions"}},"required":["query"]}"#,
                "MATCH (p:Procedure) \
                 WHERE toLower(p.name) CONTAINS toLower('{{input.query}}') \
                    OR toLower(p.description) CONTAINS toLower('{{input.query}}') \
                 RETURN p.id AS id, p.name AS name, p.description AS description, \
                        toString(p.created_at) AS created_at \
                 ORDER BY p.name LIMIT 10",
                "Search procedures by keyword",
            ),
            (
                "list_tasks",
                "List tasks, OPEN WORK FIRST (created/in_progress/blocked, then failed, then \
                 completed), including parent_id for sub-tasks. Every row also carries \
                 whole-graph status totals (total_tasks, status_failed, …). This is a 20-row \
                 WINDOW, not the full set — judge overall state from the totals, never from \
                 the rows. For other filtering use neo4j_query directly.",
                r#"{"type":"object","properties":{}}"#,
                // Ordering and totals both exist to prevent one specific failure:
                // asked "where did we leave off", the brain called this tool, got
                // 20 recently-completed rows, and reported a "clean slate" while
                // 210 tasks sat failed. Newest-first hid the open work; no totals
                // meant the window looked like the whole picture.
                "MATCH (t:Task) \
                 WITH count(t) AS total_tasks, \
                      sum(CASE WHEN t.status = 'created'     THEN 1 ELSE 0 END) AS status_created, \
                      sum(CASE WHEN t.status = 'in_progress' THEN 1 ELSE 0 END) AS status_in_progress, \
                      sum(CASE WHEN t.status = 'blocked'     THEN 1 ELSE 0 END) AS status_blocked, \
                      sum(CASE WHEN t.status = 'failed'      THEN 1 ELSE 0 END) AS status_failed, \
                      sum(CASE WHEN t.status = 'completed'   THEN 1 ELSE 0 END) AS status_completed \
                 MATCH (t2:Task) \
                 OPTIONAL MATCH (t2)-[:SUBTASK_OF]->(parent:Task) \
                 WITH total_tasks, status_created, status_in_progress, status_blocked, \
                      status_failed, status_completed, t2, parent \
                 ORDER BY CASE \
                            WHEN t2.status IN ['created', 'in_progress', 'blocked'] THEN 0 \
                            WHEN t2.status = 'failed' THEN 1 \
                            ELSE 2 END, \
                          t2.created_at DESC \
                 LIMIT 20 \
                 RETURN t2.id AS id, t2.goal AS goal, t2.status AS status, \
                        t2.context AS context, toString(t2.created_at) AS created_at, \
                        parent.id AS parent_id, \
                        total_tasks, status_created, status_in_progress, \
                        status_blocked, status_failed, status_completed",
                "List tasks with open work first, plus whole-graph status totals",
            ),
            (
                "list_notes",
                "List recently created notes in reverse-chronological order. \
                 Returns a 200-char content preview. For type filtering use neo4j_query.",
                r#"{"type":"object","properties":{}}"#,
                "MATCH (n:Note) \
                 RETURN n.id AS id, n.note_type AS note_type, \
                        left(n.content, 200) AS content_preview, \
                        toString(n.created_at) AS created_at \
                 ORDER BY n.created_at DESC LIMIT 20",
                "List recent notes ordered by creation date",
            ),
        ];

        for (name, description, schema, query, purpose) in tools {
            let steps = serde_json::to_string(&json!([{
                "tool":    "neo4j_query",
                "args":    { "cypher": query },
                "purpose": purpose,
            }]))
            .unwrap();

            let q1 = "MERGE (p:Procedure {name: $name}) \
                      ON CREATE SET p.id = $id, p.created_at = datetime($ts) \
                      SET p.description = $description, p.steps = $steps";
            if let Err(e) = neo4j
                .run(
                    neo4rs::query(q1)
                        .param("name", *name)
                        .param("id", uuid::Uuid::new_v4().to_string())
                        .param("ts", ts.as_str())
                        .param("description", *description)
                        .param("steps", steps),
                )
                .await
            {
                warn!(name = *name, error = %e, "seed_built_ins: failed to upsert Procedure");
                continue;
            }

            let q2 = "MERGE (d:DynamicTool {name: $name}) \
                      ON CREATE SET d.id = $id, d.created_at = datetime($ts) \
                      SET d.description = $description, d.input_schema = $schema";
            if let Err(e) = neo4j
                .run(
                    neo4rs::query(q2)
                        .param("name", *name)
                        .param("id", uuid::Uuid::new_v4().to_string())
                        .param("ts", ts.as_str())
                        .param("description", *description)
                        .param("schema", *schema),
                )
                .await
            {
                warn!(name = *name, error = %e, "seed_built_ins: failed to upsert DynamicTool");
                continue;
            }

            let q3 = "MATCH (d:DynamicTool {name: $name}), (p:Procedure {name: $name}) \
                      MERGE (d)-[:USES]->(p)";
            if let Err(e) = neo4j.run(neo4rs::query(q3).param("name", *name)).await {
                warn!(name = *name, error = %e, "seed_built_ins: failed to create [:USES] edge");
            } else {
                debug!(
                    name = *name,
                    "seed_built_ins: upserted DynamicTool+Procedure pair"
                );
            }
        }

        // ── Seed SourceLists from sources/*.yaml ──────────────────────────
        // ON CREATE only — the graph owns each list after first creation, so
        // runtime edits via neo4j_query persist across restarts. A missing
        // directory is non-fatal: search_web runs unrestricted when a
        // source_list name does not resolve.
        let sources_dir =
            PathBuf::from(std::env::var("SOURCES_DIR").unwrap_or_else(|_| "sources".into()));
        let n = crate::services::source_seeder::seed_sources_from_dir(neo4j, &sources_dir).await;
        debug!(created = n, "Seeded SourceLists from sources/");

        // ── Seed external MCP servers from mcp-servers/*.yaml ─────────────────
        // ON CREATE only — graph-owned after first creation (like SourceLists).
        // A missing directory is non-fatal: the MCP-client tools just have fewer
        // servers to offer. Secrets stay in env (only the env var name is stored).
        let mcp_servers_dir = PathBuf::from(
            std::env::var("MCP_SERVERS_DIR").unwrap_or_else(|_| "mcp-servers".into()),
        );
        let mcpn =
            crate::services::mcp_server_seeder::seed_mcp_servers_from_dir(neo4j, &mcp_servers_dir)
                .await;
        debug!(created = mcpn, "Seeded McpServers from mcp-servers/");

        // ── Seed MediaSource watchlist from sources-media/*.yaml ───────────
        // ON CREATE only — graph-owned after first creation (same as SourceList).
        // A missing directory is non-fatal.
        let sources_media_dir = PathBuf::from(
            std::env::var("SOURCES_MEDIA_DIR").unwrap_or_else(|_| "sources-media".into()),
        );
        let mn = crate::services::media_source_seeder::seed_media_sources_from_dir(
            neo4j,
            &sources_media_dir,
        )
        .await;
        debug!(created = mn, "Seeded MediaSources from sources-media/");

        // ── Seed built-in ScheduledTasks ──────────────────────────────────
        // schedules/*.yaml is the single source of truth for yaml-owned task
        // definitions. A missing or unreadable directory is a deployment error
        // (broken mount, wrong SCHEDULES_DIR) — fail hard rather than start a
        // brain whose schedule definitions silently stop propagating.
        let schedules_dir =
            PathBuf::from(std::env::var("SCHEDULES_DIR").unwrap_or_else(|_| "schedules".into()));
        if !schedules_dir.exists() {
            error!(
                dir = %schedules_dir.display(),
                "Schedules directory not found — check the volume mount or SCHEDULES_DIR. Refusing to start."
            );
            std::process::exit(1);
        }
        match crate::services::schedule_seeder::seed_schedules_from_dir(neo4j, &schedules_dir).await
        {
            Ok(n) => info!(count = n, "Seeded ScheduledTasks from schedules/"),
            Err(e) => {
                error!(
                    dir = %schedules_dir.display(),
                    error = %e,
                    "Failed to read schedules directory. Refusing to start."
                );
                std::process::exit(1);
            }
        }
    }
}

impl Default for BrainCore {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Helpers
// ============================================================================

/// Filter a tool list to only those whose names appear in `names`.
/// Returns `all` unchanged if `names` is empty.
pub(crate) fn filter_tools_by_names(
    all: Vec<ToolDefinition>,
    names: &[String],
) -> Vec<ToolDefinition> {
    if names.is_empty() {
        return all;
    }
    let allowed: std::collections::HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    all.into_iter()
        .filter(|t| allowed.contains(t.name.as_str()))
        .collect()
}

#[cfg(test)]
mod build_skills_tests {
    use super::*;

    fn text(r: &ToolCallResult) -> String {
        r.content
            .iter()
            .filter_map(|c| match c {
                agent_brain_protocol::Content::Text { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    /// Every skill is built once and shared by the registry and the handler,
    /// so what `tools/list` advertises is what `tools/call` can dispatch.
    #[tokio::test]
    async fn listed_tools_are_dispatchable_without_neo4j() {
        let core = BrainCore::new();
        core.build_skills().await;

        let names: Vec<String> = core
            .tool_registry
            .read()
            .await
            .list()
            .into_iter()
            .map(|t| t.name)
            .collect();
        for expected in [
            "neo4j_query",
            "http_request",
            "search_web",
            "list_capabilities",
        ] {
            assert!(
                names.iter().any(|n| n == expected),
                "{expected} missing: {names:?}"
            );
        }

        let handler = core.tool_handler.read().await;
        let handler = handler.as_ref().expect("handler installed");
        // Dispatched to QuerySkill, which reports the missing database —
        // not the handler's "Unknown tool".
        let reached = handler
            .execute(
                "neo4j_query",
                Some(serde_json::json!({"cypher": "RETURN 1"})),
            )
            .await;
        assert!(
            !text(&reached).contains("Unknown tool"),
            "{}",
            text(&reached)
        );
        let unknown = handler.execute("no_such_tool", None).await;
        assert!(text(&unknown).contains("Unknown tool"));
    }

    /// Rebuilding (a `use_model` reload) replaces the registry, not appends.
    /// Asserted as "no name twice" rather than an equal count: one skill is
    /// gated on SUBAGENT_ENABLED, which other tests in this process toggle.
    #[tokio::test]
    async fn rebuilding_does_not_duplicate_tools() {
        let core = BrainCore::new();
        core.build_skills().await;
        core.build_skills().await;
        let mut names: Vec<String> = core
            .tool_registry
            .read()
            .await
            .list()
            .into_iter()
            .map(|t| t.name)
            .collect();
        let total = names.len();
        names.sort();
        names.dedup();
        assert_eq!(names.len(), total, "a rebuild duplicated tools");
    }
}
