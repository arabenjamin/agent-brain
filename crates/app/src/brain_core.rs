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

use serde_json::Value;
use tokio::sync::RwLock;
use tokio::sync::broadcast;
use tracing::{debug, error, info, warn};

use crate::mcp::tools::{ToolHandler, ToolRegistry};
use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::queue::QueueService;
use crate::services::{
    AgentRunner, ContextBuilderService, KnowledgeService, LlmConfig, SharedLlm, SnapshotService,
};
use crate::skills::{
    Skill, agent::AgentSkill, agent_runner::AgentRunnerSkill, http::HttpSkill,
    knowledge::KnowledgeSkill, mcp_client::McpClientSkill, model::ModelSkill, query::QuerySkill,
    scheduler::SchedulerSkill, search::SearchSkill, task::TaskSkill,
    working_memory::WorkingMemorySkill,
};
use agent_brain_protocol::{ToolCallResult, ToolDefinition};

use crate::services::SchedulerService;

// ============================================================================
// BrainEvent — the internal event bus
// ============================================================================

pub use agent_brain_work::events::BrainEvent;

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
    /// The core bundle's `contexts/` (profiles are written here).
    pub contexts_dir: PathBuf,
    /// Directory for knowledge graph snapshots.
    pub snapshot_dir: PathBuf,
}

impl Default for StorageConfig {
    fn default() -> Self {
        Self {
            neo4j: None,
            telemetry: None,
            contexts_dir: crate::services::plugins::Plugins::global().core_dir("contexts"),
            snapshot_dir: std::env::var("KNOWLEDGE_SNAPSHOT_DIR")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./snapshots")),
        }
    }
}

/// Codebase self-analysis config (read-only access to the agent's own source).
pub struct CodebaseConfig {
    /// Root directory of the codebase. Auto-detected from `Cargo.toml` walk-up if unset.
    pub codebase_dir: Option<PathBuf>,
    /// Writable workspace for generated code, scripts, and experiments. Set via `WORKSPACE_DIR`.
    pub workspace_dir: Option<PathBuf>,
}

/// Walk up from `current_dir()` to find the repo root (directory containing `Cargo.toml`).
pub fn detect_repo_root() -> Option<PathBuf> {
    let mut dir = std::env::current_dir().ok()?;
    loop {
        if dir.join("Cargo.toml").exists() {
            return Some(dir);
        }
        if !dir.pop() {
            return None;
        }
    }
}

impl Default for CodebaseConfig {
    fn default() -> Self {
        let codebase_dir = std::env::var("CODEBASE_DIR")
            .map(PathBuf::from)
            .ok()
            .or_else(detect_repo_root);
        let workspace_dir = std::env::var("WORKSPACE_DIR").map(PathBuf::from).ok();
        Self {
            codebase_dir,
            workspace_dir,
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

    // --- storage, LLM ---
    pub(crate) storage: StorageConfig,
    pub(crate) llm_config: Arc<RwLock<Option<LlmConfig>>>,
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
        // Seed the graph from the plugin bundles first: workflows, sources,
        // MCP servers and API contexts the skills below read.
        if let Some(ref neo4j) = self.storage.neo4j {
            Self::seed_bundles(neo4j).await;
        }

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
            let plugin_contexts: Vec<(String, PathBuf)> =
                crate::services::plugins::Plugins::global()
                    .dirs("contexts")
                    .into_iter()
                    .filter(|(p, _)| p != crate::services::plugins::CORE)
                    .collect();
            let svc = Arc::new(
                ContextBuilderService::new(
                    self.storage.neo4j.clone(),
                    self.storage.contexts_dir.clone(),
                    Arc::clone(&self.llm_config),
                )
                .with_plugin_dirs(plugin_contexts),
            );
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
        // Object-envelope JSON Schemas from every loaded bundle's `schemas/`
        // (core first; a later bundle cannot replace one). Loaded once; a
        // missing dir or bad file degrades to no envelope validation (the
        // per-step arg gate still runs). See services/validation.rs.
        let envelope_schemas_arc: Arc<std::collections::HashMap<String, serde_json::Value>> = {
            let mut all = std::collections::HashMap::new();
            for (plugin, dir) in crate::services::plugins::Plugins::global().dirs("schemas") {
                for (name, schema) in crate::services::validation::load_schema_dir(&dir) {
                    if all.contains_key(&name) {
                        warn!(%plugin, schema = %name, "schema already declared by an earlier bundle — skipped");
                        continue;
                    }
                    all.insert(name, schema);
                }
            }
            Arc::new(all)
        };

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
        skills.push(Arc::new(TaskSkill::new(Arc::clone(&llm), task_store)));

        // The knowledge store gives search_web's prior-knowledge probe hybrid
        // recall — see `SearchSkill::with_knowledge`.
        let mut search =
            SearchSkill::new(self.storage.telemetry.clone(), self.storage.neo4j.clone());
        if let Some(k) = &knowledge {
            search = search.with_knowledge(Arc::clone(k));
        }
        skills.push(Arc::new(search));

        // Generic Neo4j + DuckDB primitives.
        let query_skill = Arc::new(QuerySkill::new(
            self.storage.neo4j.clone(),
            self.storage.telemetry.clone(),
        ));
        skills.push(query_skill.clone());
        // http_request + ApiContext management.
        skills.push(Arc::new(HttpSkill::new(
            self.storage.neo4j.clone(),
            Some(Arc::clone(&llm)),
        )));
        // Tools on external MCP servers (:McpServer nodes).
        skills.push(Arc::new(McpClientSkill::new(self.storage.neo4j.clone())));
        skills.push(Arc::new(ModelSkill));

        if let Some(neo4j) = &self.storage.neo4j {
            let wm_store: Arc<dyn crate::services::WorkingMemoryStore> = Arc::new(neo4j.clone());
            skills.push(Arc::new(
                WorkingMemorySkill::new(wm_store)
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
        // Brain-drafted plugin bundles (PLUGINS_RUNTIME_DIR), checked like a save.
        if let Some(neo4j) = &self.storage.neo4j {
            skills.push(Arc::new(
                crate::skills::plugin_manager::PluginManagerSkill::new(
                    neo4j.clone(),
                    Arc::clone(&live_tools_arc),
                    Arc::clone(&tool_schemas_arc),
                    self.storage.telemetry.clone(),
                ),
            ));
        }
        // The user's items (todos), readable and writable by the brain.
        if let Some(neo4j) = &self.storage.neo4j {
            skills.push(Arc::new(crate::skills::todo::TodoSkill::new(
                neo4j.clone(),
                context_builder_arc.clone(),
                queue_arc.clone(),
                knowledge.clone(),
            )));
        }
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

        // In-process plugins whose bundles loaded (services/plugins.rs). They
        // come after every core skill and before the declared tools; a plugin that
        // would take a name already registered is refused whole. Each gets the
        // host's services through its context (services/plugin_host.rs).
        let accepted_plugins = {
            let taken: Vec<String> = skills
                .iter()
                .flat_map(|s| s.tools().into_iter().map(|t| t.name))
                .collect();
            let host = crate::services::plugin_host::Host {
                neo4j: self.storage.neo4j.clone(),
                knowledge: knowledge.clone(),
                llm: Some(Arc::clone(&llm)),
                telemetry: self.storage.telemetry.clone(),
                queue: queue_arc.clone(),
                codebase_dir: self.codebase.codebase_dir.clone(),
                workspace_dir: self.codebase.workspace_dir.clone(),
                router_source: crate::mcp::ROUTER_SOURCE,
            };
            let (plugin_skills, accepted) = crate::services::plugins::plugin_skills(
                &crate::services::plugins::Plugins::global(),
                &crate::services::plugins::compiled(),
                &taken,
                Arc::new(|k: &str| std::env::var(k).ok()),
                &host,
            );
            skills.extend(plugin_skills);
            accepted
        };

        // `lookup` runs the bundles' views (skills/views.rs); before the
        // declared tools, so none can take its name.
        skills.push(Arc::new(crate::skills::views::LookupSkill::load(
            &crate::services::plugins::Plugins::global(),
            Arc::clone(&query_skill),
        )));

        // Declared tools last (skills/bundle_tools.rs): one can never shadow a built-in.
        {
            let taken: Vec<String> = skills
                .iter()
                .flat_map(|s| s.tools().into_iter().map(|t| t.name))
                .collect();
            let (declared, status) = crate::skills::bundle_tools::BundleToolSkill::load(
                &crate::services::plugins::Plugins::global(),
                &taken,
                query_skill,
            );
            crate::services::plugins::set_declared_status(status);
            skills.push(Arc::new(declared));
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
        // because dispatch is first-match too: a later tool reusing a
        // built-in's name never runs, but a last-wins map validated steps
        // against ITS schema — a 09-07 dynamic `ingest_api` (spec_url required)
        // made the audit reject the brain-api-sync schedule's valid call.
        let mut tool_schemas = std::collections::HashMap::new();
        let mut live_tool_names = Vec::new();
        let mut tool_index: Vec<(String, String)> = Vec::new();
        for t in registry.list() {
            live_tool_names.push(t.name.clone());
            tool_index.push((t.name.clone(), t.description.clone()));
            if tool_schemas.contains_key(&t.name) {
                warn!(tool = %t.name, "Duplicate tool name in registry — only the first definition dispatches; later ones are shadowed");
                continue;
            }
            tool_schemas.insert(t.name.clone(), t.input_schema.clone());
        }
        drop(registry);
        // Plugins' start hooks (their schema, their seeds), now that their
        // tools are registered.
        crate::services::plugins::start_plugins(&accepted_plugins).await;
        // A workflow naming a tool, MCP server or plugin that is not here is
        // marked unavailable: refused at dispatch, skipped by its timer,
        // counted in the Feed. Never run broken, never silently dropped.
        if let Some(ref neo4j) = self.storage.neo4j {
            match crate::services::plugins::mark_unavailable_workflows(
                neo4j,
                &live_tool_names,
                &crate::services::plugins::Plugins::global(),
            )
            .await
            {
                Ok(0) => {}
                Ok(n) => warn!(
                    count = n,
                    "Workflows that cannot run (see GET /api/plugins)"
                ),
                Err(e) => warn!(error = %e, "Could not check workflow availability"),
            }
        }
        *live_tools_arc.write().await = live_tool_names;
        // Profiles render their sections and tool catalogue against this.
        if let Some(ref cb) = context_builder_arc {
            cb.set_registry(tool_index).await;
        }
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
        // self-model degrades self-inspection, not the brain.
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
            // After the sync, which writes the :RemovedTool tombstones.
            if let Some(ref cb) = context_builder_arc {
                let n = cb.audit_profiles().await;
                if n > 0 {
                    warn!(
                        count = n,
                        "Profiles name tools that do not exist (GET /api/contexts)"
                    );
                }
            }

            // Record which commit this process is actually running. Same
            // non-fatal contract as the sync above.
            if let Some(ref dir) = self.codebase.codebase_dir
                && let Err(e) = crate::services::self_model::sync_code_version(neo4j, dir).await
            {
                warn!(error = %e, "Code-version sync failed (continuing without it)");
            }
        }
    }

    // ========================================================================
    // Startup seeding
    // ========================================================================

    /// Seed the graph from the plugin bundles (`services/plugins.rs`):
    /// source lists, media sources, MCP servers and API contexts (ON CREATE),
    /// then workflows. Refuses to start without the core bundle.
    pub async fn seed_bundles(neo4j: &Neo4jClient) {
        // ── Plugin bundles (services/plugins.rs) ──────────────────────────
        // The core bundle must load: without it there are no workflows,
        // profiles or schemas, and a brain that starts anyway runs blind.
        let plugins = crate::services::plugins::Plugins::global();
        if let Err(reason) = plugins.core_loaded() {
            error!(
                dir = %plugins.dir.display(),
                %reason,
                "The core plugin bundle did not load — check the volume mount or PLUGINS_DIR. Refusing to start."
            );
            std::process::exit(1);
        }
        // Source lists, media sources, MCP servers: ON CREATE only, so the
        // graph owns each after first creation and runtime edits persist.
        let n = crate::services::plugins::seed_on_create(neo4j, &plugins).await;
        debug!(
            created = n,
            "Seeded source lists, media sources and MCP servers"
        );

        // Workflows: each file owns its yaml-managed workflow.
        match crate::services::workflow_seeder::seed_workflows(neo4j, &plugins).await {
            Ok(n) => info!(count = n, "Seeded workflows from the plugin bundles"),
            Err(e) => {
                error!(error = %e, "Cannot seed workflows. Refusing to start.");
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
