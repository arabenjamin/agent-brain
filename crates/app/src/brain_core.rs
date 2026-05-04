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
use tracing::{debug, info, warn};

use crate::mcp::tools::{ToolHandler, ToolRegistry};
use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::queue::QueueService;
use crate::services::resource_registry::ResourceRegistry;
use crate::services::{
    ContextBuilderService, KnowledgeService, LlmConfig, LlmProviderType, SharedLlm, SnapshotService,
};
use crate::skills::{
    Skill, agent::AgentSkill, codebase::CodebaseSkill, context::ContextSkill,
    dynamic::DynamicSkill, git::GitSkill, http::HttpSkill, knowledge::KnowledgeSkill,
    model::ModelSkill, query::QuerySkill, resource::ResourceSkill, scheduler::SchedulerSkill,
    search::SearchSkill, sleep::SleepSkill, task::TaskSkill, working_memory::WorkingMemorySkill,
    ws::WsSkill,
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

    /// Emit a brain event to all current subscribers.
    ///
    /// Ignores send errors (no subscribers is fine).
    #[allow(dead_code)]
    pub(crate) fn emit(&self, event: BrainEvent) {
        let _ = self.event_tx.send(event);
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
            let local_url = std::env::var("OLLAMA_LOCAL_URL")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            let model =
                std::env::var("OLLAMA_LOCAL_MODEL").unwrap_or_else(|_| "gemma4:latest".to_string());
            let mut local_llm_config = LlmConfig::default()
                .with_provider(LlmProviderType::Ollama)
                .with_base_url(local_url.clone())
                .with_model(model)
                .with_embed_base_url(local_url);
            // Pin the embedding model so local knowledge ops use bge-m3 (or whatever
            // OLLAMA_EMBED_MODEL is set to) instead of falling back to the generation model.
            if let Ok(em) = std::env::var("OLLAMA_EMBED_MODEL") {
                local_llm_config = local_llm_config.with_embed_model(em);
            }
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
        let local_llm = SharedLlm::new(Arc::clone(&local_config_arc));

        // Shared LLM provider (wraps live Arc<RwLock<Option<LlmConfig>>>)
        let shared_llm = SharedLlm::new_with_local(
            Arc::clone(&self.llm_config),
            local_config_arc,
            self.storage.telemetry.clone(),
        );

        let mut registry = self.tool_registry.write().await;

        // Clear registry to allow safe re-registration on reload.
        registry.clear();

        // ── Register all skills in the tool registry ──────────────────────

        // Knowledge Skill
        if let Some(neo4j) = &self.storage.neo4j {
            let mut ks = KnowledgeService::new(
                neo4j.clone(),
                Some(Arc::clone(&local_llm) as Arc<dyn crate::services::LlmProvider>),
            );
            if let Some(ref snap) = snapshot_svc {
                ks = ks.with_snapshot(Arc::clone(snap), true, true);
            }
            let knowledge_svc: Arc<dyn crate::services::KnowledgeStore> = Arc::new(ks);
            let knowledge_skill = KnowledgeSkill::new(
                knowledge_svc,
                Arc::clone(&shared_llm) as Arc<dyn crate::services::LlmProvider>,
            );
            registry.register_skill(Box::new(knowledge_skill));
        }

        // Task Skill
        let task_store: Option<Arc<dyn crate::services::TaskStore>> = self
            .storage
            .neo4j
            .as_ref()
            .map(|n| Arc::new(n.clone()) as Arc<dyn crate::services::TaskStore>);
        let task_skill = TaskSkill::new(
            Arc::clone(&shared_llm) as Arc<dyn crate::services::LlmProvider>,
            task_store.clone(),
            queue_arc.as_ref().map(Arc::clone),
        );
        registry.register_skill(Box::new(task_skill));

        // Search Skill
        registry.register_skill(Box::new(SearchSkill::new(
            self.storage.telemetry.clone(),
            self.storage.neo4j.clone(),
        )));

        // Query Skill (generic Neo4j + DuckDB primitives)
        registry.register_skill(Box::new(QuerySkill::new(
            self.storage.neo4j.clone(),
            self.storage.telemetry.clone(),
        )));

        // HTTP Skill (generic http_request + ApiContext management)
        registry.register_skill(Box::new(HttpSkill::new(self.storage.neo4j.clone())));

        // Codebase Skill
        {
            let knowledge_store: Option<Arc<dyn crate::services::KnowledgeStore>> =
                if let Some(neo4j) = &self.storage.neo4j {
                    Some(Arc::new(KnowledgeService::new(
                        neo4j.clone(),
                        Some(Arc::clone(&local_llm) as Arc<dyn crate::services::LlmProvider>),
                    ))
                        as Arc<dyn crate::services::KnowledgeStore>)
                } else {
                    None
                };
            let codebase_skill = CodebaseSkill::new(
                self.codebase.codebase_dir.clone(),
                self.codebase.workspace_dir.clone(),
                self.codebase.proposals_dir.clone(),
                knowledge_store,
                self.storage.neo4j.clone(),
            );
            registry.register_skill(Box::new(codebase_skill));
        }

        // Git Skill (branch/commit/push/PR — always registered; requires CODEBASE_DIR)
        registry.register_skill(Box::new(GitSkill::new(self.codebase.codebase_dir.clone())));

        // Model Skill (DuckDB-backed catalog, shares live LLM config Arc)
        let model_skill = ModelSkill::new(
            self.llm_config.clone(),
            self.storage.telemetry.clone(),
            self.catalog_path.clone(),
        );
        registry.register_skill(Box::new(model_skill));

        // Sleep Skill (requires telemetry / DuckDB)
        if let Some(ref telemetry) = self.storage.telemetry {
            let sleep_skill = SleepSkill::new(telemetry.clone(), self.storage.dataset_dir.clone());
            registry.register_skill(Box::new(sleep_skill));
        }

        // Working Memory Skill
        if let Some(neo4j) = &self.storage.neo4j {
            let knowledge_svc2: Arc<dyn crate::services::KnowledgeStore> =
                Arc::new(KnowledgeService::new(
                    neo4j.clone(),
                    Some(Arc::clone(&local_llm) as Arc<dyn crate::services::LlmProvider>),
                ));
            let wm_store: Arc<dyn crate::services::WorkingMemoryStore> = Arc::new(neo4j.clone());
            let wm_skill = WorkingMemorySkill::new(
                wm_store,
                knowledge_svc2,
                Arc::clone(&shared_llm) as Arc<dyn crate::services::LlmProvider>,
            )
            .with_notification_support(Arc::new(neo4j.clone()), self.event_tx.clone());
            registry.register_skill(Box::new(wm_skill));
        }

        // Agent Skill (queue management)
        if let Some(ref qs) = queue_arc {
            registry.register_skill(Box::new(AgentSkill::new(Arc::clone(qs))));
        }

        // Scheduler Skill
        if let Some(ref sched) = scheduler_arc {
            registry.register_skill(Box::new(SchedulerSkill::new(
                Arc::clone(sched),
                self.storage.neo4j.clone(),
            )));
        }

        // Context Skill (profile management)
        if let Some(ref cb) = context_builder_arc {
            registry.register_skill(Box::new(ContextSkill::new(Arc::clone(cb))));
        }

        // WebSocket Skill
        registry.register_skill(Box::new(WsSkill::new()));

        // Resource Skill
        let resource_registry = Arc::new(ResourceRegistry::new());
        registry.register_skill(Box::new(ResourceSkill::new(Arc::clone(&resource_registry))));

        // DynamicSkill — registry clone shares tools_map with the handler original
        if let Some(ref d) = dynamic_skill {
            registry.register_skill(Box::new(d.clone_shared()));
        }

        // Snapshot live tool names for the scheduler audit action.
        let live_tool_names: Vec<String> = registry.list().iter().map(|t| t.name.clone()).collect();
        let live_tools_arc: Arc<RwLock<Vec<String>>> = Arc::new(RwLock::new(live_tool_names));

        drop(registry);

        // ── Build the ToolHandler skill list ──────────────────────────────
        // Re-creates non-dynamic skills; the DynamicSkill *original* goes here
        // so it holds the live tools_map that the registry clone already references.

        let mut skills: Vec<Box<dyn Skill>> = Vec::new();

        if let Some(neo4j) = &self.storage.neo4j {
            let mut ks3 = KnowledgeService::new(
                neo4j.clone(),
                Some(Arc::clone(&local_llm) as Arc<dyn crate::services::LlmProvider>),
            );
            if let Some(ref snap) = snapshot_svc {
                ks3 = ks3.with_snapshot(Arc::clone(snap), true, true);
            }
            let knowledge_svc3: Arc<dyn crate::services::KnowledgeStore> = Arc::new(ks3);
            skills.push(Box::new(KnowledgeSkill::new(
                knowledge_svc3,
                Arc::clone(&shared_llm) as Arc<dyn crate::services::LlmProvider>,
            )));
        }

        skills.push(Box::new(TaskSkill::new(
            Arc::clone(&shared_llm) as Arc<dyn crate::services::LlmProvider>,
            task_store,
            queue_arc.as_ref().map(Arc::clone),
        )));

        skills.push(Box::new(SearchSkill::new(
            self.storage.telemetry.clone(),
            self.storage.neo4j.clone(),
        )));

        skills.push(Box::new(QuerySkill::new(
            self.storage.neo4j.clone(),
            self.storage.telemetry.clone(),
        )));

        skills.push(Box::new(HttpSkill::new(self.storage.neo4j.clone())));

        {
            let knowledge_store2: Option<Arc<dyn crate::services::KnowledgeStore>> =
                if let Some(neo4j) = &self.storage.neo4j {
                    Some(Arc::new(KnowledgeService::new(
                        neo4j.clone(),
                        Some(Arc::clone(&local_llm) as Arc<dyn crate::services::LlmProvider>),
                    ))
                        as Arc<dyn crate::services::KnowledgeStore>)
                } else {
                    None
                };
            skills.push(Box::new(CodebaseSkill::new(
                self.codebase.codebase_dir.clone(),
                self.codebase.workspace_dir.clone(),
                self.codebase.proposals_dir.clone(),
                knowledge_store2,
                self.storage.neo4j.clone(),
            )));
        }

        skills.push(Box::new(GitSkill::new(self.codebase.codebase_dir.clone())));

        skills.push(Box::new(ModelSkill::new(
            self.llm_config.clone(),
            self.storage.telemetry.clone(),
            self.catalog_path.clone(),
        )));

        if let Some(ref telemetry) = self.storage.telemetry {
            skills.push(Box::new(SleepSkill::new(
                telemetry.clone(),
                self.storage.dataset_dir.clone(),
            )));
        }

        if let Some(neo4j) = &self.storage.neo4j {
            let knowledge_svc4: Arc<dyn crate::services::KnowledgeStore> =
                Arc::new(KnowledgeService::new(
                    neo4j.clone(),
                    Some(Arc::clone(&local_llm) as Arc<dyn crate::services::LlmProvider>),
                ));
            let wm_store2: Arc<dyn crate::services::WorkingMemoryStore> = Arc::new(neo4j.clone());
            skills.push(Box::new(
                WorkingMemorySkill::new(
                    wm_store2,
                    knowledge_svc4,
                    Arc::clone(&shared_llm) as Arc<dyn crate::services::LlmProvider>,
                )
                .with_notification_support(Arc::new(neo4j.clone()), self.event_tx.clone()),
            ));
        }

        if let Some(ref qs) = queue_arc {
            skills.push(Box::new(AgentSkill::new(Arc::clone(qs))));
        }

        if let Some(ref sched) = scheduler_arc {
            skills.push(Box::new(
                SchedulerSkill::new(Arc::clone(sched), self.storage.neo4j.clone())
                    .with_live_tools(Arc::clone(&live_tools_arc)),
            ));
        }

        if let Some(ref cb) = context_builder_arc {
            skills.push(Box::new(ContextSkill::new(Arc::clone(cb))));
        }

        skills.push(Box::new(WsSkill::new()));
        skills.push(Box::new(ResourceSkill::new(Arc::clone(&resource_registry))));

        // Original DynamicSkill goes to the handler (shares tools_map with registry clone).
        if let Some(d) = dynamic_skill {
            skills.push(Box::new(d));
        }

        let mut handler = self.tool_handler.write().await;
        let mut tool_handler = ToolHandler::new(skills);
        if let Some(ref tel) = self.storage.telemetry {
            tool_handler = tool_handler.with_telemetry(tel.clone());
        }
        *handler = Some(tool_handler);

        // Spawn the queue coordinator now that the tool handler is populated.
        if let Some(qs) = queue_arc {
            QueueService::spawn_coordinator(qs);
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
                "List tasks from the graph ordered by creation date, including parent_id for sub-tasks. \
                 For status filtering use neo4j_query directly.",
                r#"{"type":"object","properties":{}}"#,
                "MATCH (t:Task) \
                 OPTIONAL MATCH (t)-[:SUBTASK_OF]->(parent:Task) \
                 RETURN t.id AS id, t.goal AS goal, t.status AS status, \
                        t.context AS context, toString(t.created_at) AS created_at, \
                        parent.id AS parent_id \
                 ORDER BY t.created_at DESC LIMIT 20",
                "List all tasks ordered by creation date",
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

        // ── Seed built-in SourceList for news ────────────────────────────
        let news_domains: Vec<String> = vec![
            "apnews.com".into(),
            "reuters.com".into(),
            "bbc.com".into(),
            "bbc.co.uk".into(),
            "theguardian.com".into(),
            "nytimes.com".into(),
            "washingtonpost.com".into(),
            "wsj.com".into(),
            "ft.com".into(),
            "economist.com".into(),
            "bloomberg.com".into(),
            "politico.com".into(),
            "techcrunch.com".into(),
            "wired.com".into(),
            "arstechnica.com".into(),
            "theatlantic.com".into(),
            "axios.com".into(),
            "npr.org".into(),
            "pbs.org".into(),
            "aljazeera.com".into(),
        ];
        if let Err(e) = neo4j
            .upsert_source_list(
                "news",
                &news_domains,
                "Approved news sources for scheduled news briefings. \
                 Edit via neo4j_query: MATCH (s:SourceList {name:'news'}) SET s.domains = [...]",
            )
            .await
        {
            warn!(error = %e, "Failed to upsert news SourceList");
        } else {
            debug!("Upserted news SourceList ({} domains)", news_domains.len());
        }

        // ── Seed built-in ScheduledTasks ──────────────────────────────────
        let daily_news_steps = serde_json::json!([
            {"tool_name":"search_web","arguments":{"query":"top world news headlines {{date}}","count":10,"source_list":"news"},"priority":1,"max_attempts":3,"provider_hint":"ollama"},
            {"tool_name":"search_web","arguments":{"query":"AI technology science news {{date}}","count":10,"source_list":"news"},"priority":1,"max_attempts":3,"provider_hint":"ollama"},
            {"tool_name":"search_web","arguments":{"query":"business economy politics news {{date}}","count":10,"source_list":"news"},"priority":1,"max_attempts":3,"provider_hint":"ollama"},
            {"tool_name":"reason","arguments":{"question":"Write a structured daily news brief for {{date}} with the following sections:\n## EXECUTIVE SUMMARY\n## WORLD NEWS\n## TECHNOLOGY & AI\n## BUSINESS & POLITICS\n## STORY TO WATCH\n\n3-4 bullets per section. Cite sources with URLs. Flag notable spin or framing with ⚠️.","context":"{{_prev}}","store_inference":false},"priority":2,"max_attempts":3,"provider_hint":"ollama"},
            {"tool_name":"notify_user","arguments":{"message":"{{_prev}}","context":"Daily News Brief {{date}}","related_session_id":"news-{{date}}"},"priority":2,"max_attempts":2,"provider_hint":"ollama"},
            {"tool_name":"push_context","arguments":{"session_id":"news-{{date}}","content":"{{_prev}}","role":"assistant"},"priority":1,"max_attempts":2,"provider_hint":"ollama"},
            {"tool_name":"store_note","arguments":{"content":"{{_prev}}","note_type":"news","source_context":"scheduled_daily_news_brief"},"priority":1,"max_attempts":2,"provider_hint":"ollama"}
        ]);
        let health_monitor_steps = serde_json::json!([
            {"tool_name":"neo4j_query","arguments":{"cypher":"MATCH (j:AgentJob) WHERE j.created_at >= toString(datetime() - duration('P1D')) RETURN j.status, count(j) AS cnt ORDER BY cnt DESC"},"priority":1,"max_attempts":2,"provider_hint":"ollama"},
            {"tool_name":"dead_letter","arguments":{"action":"list","limit":20},"priority":1,"max_attempts":2,"provider_hint":"ollama"},
            {"tool_name":"list_tasks","arguments":{"status":"failed","limit":10},"priority":1,"max_attempts":2,"provider_hint":"ollama"},
            {"tool_name":"reason","arguments":{"question":"Job status counts from the past 24 h, dead-letter queue, and recent failed tasks: {{_prev}}\n\nSummarise the current health of the brain. Note any failure patterns, queue backlogs, or regressions compared to previous health snapshots.","store_inference":true},"priority":1,"max_attempts":3,"provider_hint":"ollama"},
            {"tool_name":"store_note","arguments":{"content":"Health monitor cycle complete — see inference note for analysis.","note_type":"outcome","source_context":"health_monitor"},"priority":1,"max_attempts":2,"provider_hint":"ollama"}
        ]);
        let weekly_news_steps = serde_json::json!([
            {"tool_name":"search_web","arguments":{"query":"major world news events this week AP Reuters BBC","count":10},"priority":1,"max_attempts":3,"provider_hint":"ollama"},
            {"tool_name":"search_web","arguments":{"query":"AI technology breakthroughs this week TechCrunch Wired","count":10},"priority":1,"max_attempts":3,"provider_hint":"ollama"},
            {"tool_name":"search_web","arguments":{"query":"business economy politics week Financial Times Bloomberg","count":10},"priority":1,"max_attempts":3,"provider_hint":"ollama"},
            {"tool_name":"reason","arguments":{"question":"Write a weekly news synthesis with top 3 stories per category (world, technology, business/politics), emerging themes across categories, and one trend to watch next week. Cite sources with URLs.","context":"{{_prev}}","store_inference":false},"priority":2,"max_attempts":3,"provider_hint":"ollama"},
            {"tool_name":"store_note","arguments":{"content":"{{_prev}}","note_type":"news","source_context":"scheduled_weekly_news_brief"},"priority":1,"max_attempts":2,"provider_hint":"ollama"}
        ]);

        // Todo review: query outstanding todos → reason over them → open a dedicated chat
        // session pre-loaded with the agent's message → notify the user to join.
        //
        // Step flow and {{_prev}} chain:
        //   1. neo4j_query  → raw todo rows JSON
        //   2. reason       → agent opening message (markdown)     [returns {"answer":"..."}]
        //   3. notify_user  → delivers message, opens "todos-{{date}}" session
        //                     [returns {"answer":"[msg]"} so next step gets clean text]
        //   4. push_context → seeds "todos-{{date}}" session with the agent message
        //                     so it appears as chat history when user clicks "Continue"
        let todo_review_steps = serde_json::json!([
            {
                "tool_name": "neo4j_query",
                "arguments": {
                    "cypher": "MATCH (t:Todo) WHERE t.status IN ['pending','in_progress'] RETURN t.id AS id, t.title AS title, t.description AS description, t.priority AS priority, t.status AS status, t.due_at AS due_at ORDER BY t.priority ASC, t.created_at ASC LIMIT 15"
                },
                "priority": 1, "max_attempts": 2, "provider_hint": "ollama"
            },
            {
                "tool_name": "reason",
                "arguments": {
                    "question": "You are reviewing the user's outstanding todos for {{date}}. Here is the current list:\n\n{{_prev}}\n\nWrite a friendly, concise message to the user (under 250 words) that:\n1. Opens with a brief summary of what's on their plate (number of items, any urgent/overdue ones).\n2. Picks the 1-2 highest-priority or most unclear todos and asks a specific clarifying question for each — e.g. what does completion look like, are there blockers, should you start on it now?\n3. Closes with an offer to help with any of them.\n\nIf there are no outstanding todos, say so warmly and ask if the user wants to add anything.\n\nWrite directly to the user (second person). Do not use bullet-point headers for the questions — keep it conversational.",
                    "context": "{{_prev}}",
                    "store_inference": false
                },
                "priority": 2, "max_attempts": 3, "provider_hint": "ollama"
            },
            {
                "tool_name": "notify_user",
                "arguments": {
                    "message": "{{_prev}}",
                    "context": "Todo Review {{date}}",
                    "related_session_id": "todos-{{date}}"
                },
                "priority": 2, "max_attempts": 2, "provider_hint": "ollama"
            },
            {
                "tool_name": "push_context",
                "arguments": {
                    "session_id": "todos-{{date}}",
                    "content": "{{_prev}}",
                    "role": "assistant"
                },
                "priority": 1, "max_attempts": 2, "provider_hint": "ollama"
            }
        ]);

        let seeds: &[(&str, Option<&str>, i64, &serde_json::Value)] = &[
            (
                "Daily news aggregation and briefing: aggregate headlines from world, tech, and business, then write and store a daily briefing",
                Some(
                    "Aggregates headlines from multiple sources daily and stores a structured news briefing.",
                ),
                86400,
                &daily_news_steps,
            ),
            (
                "Brain health monitor: review scheduler state, queue metrics, and failure patterns",
                Some(
                    "Periodic brain health check — reviews scheduler state, queue depth, and failure trends.",
                ),
                43200,
                &health_monitor_steps,
            ),
            (
                "Weekly news briefing and analysis: synthesize major world, tech, business, and international stories from the past week",
                Some(
                    "Weekly synthesis of major news themes across world, technology, and business.",
                ),
                604800,
                &weekly_news_steps,
            ),
            (
                "Daily todo review: check outstanding todos and open a chat session to discuss progress, blockers, and next steps with the user",
                Some(
                    "Reviews pending todos daily, asks the user clarifying questions, and opens a dedicated chat session for follow-up.",
                ),
                86400,
                &todo_review_steps,
            ),
        ];

        for (name, description, interval, steps) in seeds {
            let steps_str = serde_json::to_string(steps).unwrap_or_default();
            match neo4j
                .seed_scheduled_task_if_absent(name, *description, *interval, &steps_str)
                .await
            {
                Ok((id, true)) => info!(name = *name, id = %id, "Seeded ScheduledTask"),
                Ok((_, false)) => {
                    debug!(name = *name, "ScheduledTask already exists — skipped");
                    // Force-update steps for tasks whose chain definition has changed.
                    // This patches live tasks that were seeded before the change.
                    let steps_str2 = serde_json::to_string(steps).unwrap_or_default();
                    match neo4j.update_scheduled_task_steps(name, &steps_str2).await {
                        Ok(true) => debug!(name = *name, "Updated ScheduledTask steps"),
                        Ok(false) => {}
                        Err(e) => {
                            warn!(name = *name, error = %e, "Failed to update ScheduledTask steps")
                        }
                    }
                }
                Err(e) => warn!(name = *name, error = %e, "Failed to seed ScheduledTask"),
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
