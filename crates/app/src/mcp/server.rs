//! MCP server implementation.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use thiserror::Error;
use tokio::sync::RwLock;
use tracing::{debug, error, info, warn};

use crate::repository::{Neo4jClient, TelemetryClient};
use crate::services::SchedulerService;
use crate::services::queue::QueueService;
use crate::services::resource_registry::ResourceRegistry;
use crate::services::{
    ChatService, ContextBuilderService, KnowledgeService, LlmConfig, SharedLlm, SnapshotService,
};
use crate::skills::{
    Skill, agent::AgentSkill, codebase::CodebaseSkill, context::ContextSkill,
    dynamic::DynamicSkill, http::HttpSkill, knowledge::KnowledgeSkill, model::ModelSkill,
    procedure::ProcedureSkill, query::QuerySkill, resource::ResourceSkill,
    scheduler::SchedulerSkill, search::SearchSkill, sleep::SleepSkill, task::TaskSkill,
    working_memory::WorkingMemorySkill, ws::WsSkill,
};

use super::protocol::{
    IncomingMessage, InitializeParams, InitializeResult, JsonRpcErrorResponse, JsonRpcRequest,
    JsonRpcResponse, MCP_PROTOCOL_VERSION, ServerCapabilities, ServerInfo, ToolCallParams,
    ToolsCapability, ToolsListResult, error_codes,
};
use super::session::{SessionManager, SessionState};
use super::tools::{ToolHandler, ToolRegistry};
use super::transport::{OutgoingMessage, StdioTransport};
use super::transport_trait::{McpTransport, TransportMessage};

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
///
/// GitHub API access no longer lives here — use the generic `http_request` tool
/// with `context_name="github"`. The `github` ApiContext is seeded at boot and
/// reads `GITHUB_TOKEN` from the environment automatically.
pub struct CodebaseConfig {
    /// Root directory of the codebase. Auto-detected from `Cargo.toml` walk-up if unset.
    pub codebase_dir: Option<std::path::PathBuf>,
}

impl Default for CodebaseConfig {
    fn default() -> Self {
        let codebase_dir = std::env::var("CODEBASE_DIR")
            .map(std::path::PathBuf::from)
            .ok()
            .or_else(crate::skills::codebase::detect_repo_root);
        Self { codebase_dir }
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

#[derive(Debug, Error)]
pub enum McpServerError {
    #[error("Transport error: {0}")]
    Transport(String),

    #[error("Protocol error: {0}")]
    Protocol(String),

    #[error("Server not initialized")]
    NotInitialized,

    #[error("Server already initialized")]
    AlreadyInitialized,
}

/// MCP server state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerState {
    /// Waiting for initialize request.
    Created,
    /// Initialize received, waiting for initialized notification.
    Initializing,
    /// Ready to handle requests.
    Running,
    /// Shutdown requested.
    ShuttingDown,
}

/// MCP server configuration.
#[derive(Debug, Clone)]
pub struct McpServerConfig {
    /// Server name.
    pub name: String,
    /// Server version.
    pub version: String,
    /// Server instructions/description.
    pub instructions: Option<String>,
}

impl Default for McpServerConfig {
    fn default() -> Self {
        Self {
            name: "agent-brain".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            instructions: Some(
                "General Intelligence Agent Core with Graph RAG and MCP. \
                 Manage long-term memory, execute tasks, and learn from feedback."
                    .to_string(),
            ),
        }
    }
}

/// Thread-safe MCP server core that works with any transport.
///
/// Fields are grouped into focused service containers:
/// - `storage`  — database, telemetry, dataset directory
/// - `llm_config` — live-swappable LLM config (Arc<RwLock<>> for `use_model`)
/// - `search`   — web search API keys
/// - `jobs`     — background queue + scheduler
pub struct McpServerCore {
    config: McpServerConfig,
    pub(crate) state: Arc<RwLock<ServerState>>,
    tool_registry: Arc<RwLock<ToolRegistry>>,
    tool_handler: Arc<RwLock<Option<ToolHandler>>>,
    session_manager: Option<Arc<SessionManager>>,
    storage: StorageConfig,
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
    search: SearchConfig,
    codebase: CodebaseConfig,
    jobs: JobServices,
    /// System prompt loaded from the model catalog at startup.
    system_prompt: String,
    /// Path to models.yaml, forwarded to ModelSkill for hot-reload.
    catalog_path: PathBuf,
    /// Context builder service (created in build_skills, shared with chat service).
    context_builder_svc: Arc<RwLock<Option<Arc<ContextBuilderService>>>>,
    /// Optional profile name to filter tools/list responses.
    mcp_tool_profile: Option<String>,
}

impl McpServerCore {
    /// Create a new server core with default configuration.
    pub fn new() -> Self {
        Self::with_config(McpServerConfig::default())
    }

    /// Create a new server core with custom configuration.
    pub fn with_config(config: McpServerConfig) -> Self {
        Self {
            config,
            state: Arc::new(RwLock::new(ServerState::Created)),
            tool_registry: Arc::new(RwLock::new(ToolRegistry::new())),
            tool_handler: Arc::new(RwLock::new(None)),
            session_manager: None,
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
            mcp_tool_profile: std::env::var("MCP_TOOL_PROFILE").ok(),
        }
    }

    /// Set the session manager for HTTP transport.
    pub fn with_session_manager(mut self, manager: Arc<SessionManager>) -> Self {
        self.session_manager = Some(manager);
        self
    }

    /// Get the current server state.
    pub async fn get_state(&self) -> ServerState {
        *self.state.read().await
    }

    /// Get the state for a specific session, falling back to global state.
    pub async fn get_session_state(&self, session_id: Option<&str>) -> ServerState {
        if let (Some(id), Some(manager)) = (session_id, &self.session_manager)
            && let Ok(state) = manager.get_session_state(id).await
        {
            return ServerState::from(state);
        }
        *self.state.read().await
    }

    /// Update the state for a specific session and the global state.
    pub async fn update_session_state(&self, session_id: Option<&str>, new_state: ServerState) {
        if let (Some(id), Some(manager)) = (session_id, &self.session_manager) {
            let _ = manager
                .set_session_state(id, SessionState::from(new_state))
                .await;
        }

        // Always update global state for backward compatibility with stdio
        let mut state = self.state.write().await;
        *state = new_state;
    }

    /// Set the Neo4j client for database operations.
    pub fn with_neo4j(mut self, neo4j: Neo4jClient) -> Self {
        self.storage.neo4j = Some(neo4j);
        self
    }

    /// Set the Telemetry client for logging.
    pub fn with_telemetry(mut self, telemetry: TelemetryClient) -> Self {
        self.storage.telemetry = Some(telemetry);
        self
    }

    /// Set the LLM configuration for healing.
    pub fn with_llm_config(mut self, config: LlmConfig) -> Self {
        self.llm_config = Arc::new(RwLock::new(Some(config)));
        self
    }

    /// Set the Brave API Key for searching.
    pub fn with_brave_api_key(mut self, key: impl Into<String>) -> Self {
        self.search.brave_key = Some(key.into());
        self
    }

    /// Set the Google API Key and CX for searching.
    pub fn with_google_config(mut self, key: impl Into<String>, cx: impl Into<String>) -> Self {
        self.search.google_key = Some(key.into());
        self.search.google_cx = Some(cx.into());
        self
    }

    /// Set the SerpApi Key for searching.
    pub fn with_serpapi_key(mut self, key: impl Into<String>) -> Self {
        self.search.serpapi_key = Some(key.into());
        self
    }

    /// Set the active system prompt (from model catalog).
    pub fn with_system_prompt(mut self, prompt: String) -> Self {
        self.system_prompt = prompt;
        self
    }

    /// Set the path to models.yaml (forwarded to ModelSkill for hot-reload).
    pub fn with_catalog_path(mut self, path: PathBuf) -> Self {
        self.catalog_path = path;
        self
    }

    /// Create a [`ChatService`] backed by this server's live tool handler,
    /// registry, and LLM config.
    ///
    /// Safe to call before or after [`build_skills`] — the `Arc` references
    /// will always see the most up-to-date state.
    pub fn chat_service(&self) -> Arc<ChatService> {
        ChatService::with_context_builder(
            Arc::clone(&self.tool_handler),
            Arc::clone(&self.tool_registry),
            Arc::clone(&self.llm_config),
            Arc::clone(&self.context_builder_svc),
        )
    }

    /// Build the skills and initialize the tool handler.
    /// This should be called before running the server.
    pub async fn build_skills(&self) {
        // Seed built-in Neo4j nodes FIRST so DynamicSkill::load_from_neo4j picks them up.
        if let Some(ref neo4j) = self.storage.neo4j {
            Self::seed_built_ins(neo4j).await;
        }

        // Build DynamicSkill (before taking locks) so we can share the Arc.
        // Both the registry clone and the handler original share the same tools_map.
        let dynamic_skill = if let Some(neo4j) = &self.storage.neo4j {
            let d = DynamicSkill::new(neo4j.clone(), self.tool_handler.clone(), Arc::clone(&self.tool_registry));
            d.load_from_neo4j().await;
            Some(d)
        } else {
            None
        };

        // Create (or reuse) QueueService when Neo4j is available.
        let queue_arc: Option<Arc<QueueService>> = if let Some(neo4j) = &self.storage.neo4j {
            let mut qs_guard = self.jobs.queue.write().await;
            if qs_guard.is_none() {
                let sse_notifier: Option<Arc<dyn agent_brain_protocol::SseNotifier>> = self
                    .session_manager
                    .as_ref()
                    .map(|sm| Arc::clone(sm) as Arc<dyn agent_brain_protocol::SseNotifier>);
                let qs = Arc::new(QueueService::new(
                    neo4j.clone(),
                    self.tool_handler.clone(),
                    sse_notifier,
                ));
                qs.recover().await;
                *qs_guard = Some(Arc::clone(&qs));
            }
            qs_guard.as_ref().map(Arc::clone)
        } else {
            None
        };

        // Create SnapshotService when Neo4j is available.
        let _snapshot_svc: Option<Arc<SnapshotService>> = self.storage.neo4j.as_ref().map(|db| {
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

        // Create (or reuse) SchedulerService when Neo4j + Queue are available.
        let scheduler_arc: Option<Arc<SchedulerService>> =
            if let (Some(neo4j), Some(qs)) = (&self.storage.neo4j, &queue_arc) {
                let mut g = self.jobs.scheduler.write().await;
                if g.is_none() {
                    *g = Some(SchedulerService::new_with_context(
                        neo4j.clone(),
                        Arc::clone(qs),
                        context_builder_arc.clone(),
                    ));
                }
                g.as_ref().map(Arc::clone)
            } else {
                None
            };

        // Local-Ollama config for background jobs — always points to localhost,
        // so maintenance tasks (consolidation, health monitor, etc.) never touch
        // cloud quota even when the active provider is ollama-cloud or anthropic.
        let local_llm_config = {
            use crate::services::LlmProviderType;
            let local_url = std::env::var("OLLAMA_LOCAL_URL")
                .unwrap_or_else(|_| "http://localhost:11434".to_string());
            let model = std::env::var("OLLAMA_MODEL")
                .unwrap_or_else(|_| "qwen3.5:4b".to_string());
            LlmConfig::default()
                .with_provider(LlmProviderType::Ollama)
                .with_base_url(local_url.clone())
                .with_model(model)
                .with_embed_base_url(local_url)
        };
        let local_config_arc = Arc::new(RwLock::new(Some(local_llm_config)));

        // Shared LLM provider (wraps live Arc<RwLock<Option<LlmConfig>>>)
        let shared_llm = SharedLlm::new_with_local(
            Arc::clone(&self.llm_config),
            local_config_arc,
            self.storage.telemetry.clone(),
        );

        let mut registry = self.tool_registry.write().await;

        // Clear registry to allow safe re-registration on reload.
        registry.clear();

        // Register Knowledge Skill
        if let Some(neo4j) = &self.storage.neo4j {
            let config = self.llm_config.read().await.clone();
            let llm_client = config.and_then(|c| crate::services::LlmClient::with_config(c).ok());
            let knowledge_svc: Arc<dyn crate::services::KnowledgeStore> =
                Arc::new(KnowledgeService::new(neo4j.clone(), llm_client));
            let knowledge_skill = KnowledgeSkill::new(
                knowledge_svc,
                Arc::clone(&shared_llm) as Arc<dyn crate::services::LlmProvider>,
            );
            registry.register_skill(Box::new(knowledge_skill));
        }

        // Register Task Skill
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

        // Register Procedure Skill
        if let Some(neo4j) = &self.storage.neo4j {
            let proc_store: Arc<dyn crate::services::ProcedureStore> = Arc::new(neo4j.clone());
            let procedure_skill = ProcedureSkill::new(proc_store);
            registry.register_skill(Box::new(procedure_skill));
        }

        // Register Search Skill
        registry.register_skill(Box::new(SearchSkill::new(
            self.storage.telemetry.clone(),
            self.storage.neo4j.clone(),
        )));

        // Register Query Skill (generic Neo4j + DuckDB primitives)
        registry.register_skill(Box::new(QuerySkill::new(
            self.storage.neo4j.clone(),
            self.storage.telemetry.clone(),
        )));

        // Register HTTP Skill (generic http_request + ApiContext management)
        registry.register_skill(Box::new(HttpSkill::new(self.storage.neo4j.clone())));

        // Register Codebase Skill (read-only filesystem + GitHub API)
        {
            let knowledge_store: Option<Arc<dyn crate::services::KnowledgeStore>> =
                if let Some(neo4j) = &self.storage.neo4j {
                    let cfg_cb = self.llm_config.read().await.clone();
                    let llm_cb = cfg_cb.and_then(|c| crate::services::LlmClient::with_config(c).ok());
                    Some(Arc::new(KnowledgeService::new(neo4j.clone(), llm_cb))
                        as Arc<dyn crate::services::KnowledgeStore>)
                } else {
                    None
                };
            let codebase_skill = CodebaseSkill::new(
                self.codebase.codebase_dir.clone(),
                knowledge_store,
            );
            registry.register_skill(Box::new(codebase_skill));
        }

        // Register Model Skill (DuckDB-backed catalog, shares live LLM config Arc)
        let model_skill = ModelSkill::new(
            self.llm_config.clone(),
            self.storage.telemetry.clone(),
            self.catalog_path.clone(),
        );
        registry.register_skill(Box::new(model_skill));

        // Register Sleep Skill (requires telemetry / DuckDB)
        if let Some(ref telemetry) = self.storage.telemetry {
            let sleep_skill = SleepSkill::new(telemetry.clone(), self.storage.dataset_dir.clone());
            registry.register_skill(Box::new(sleep_skill));
        }

        // Register Todo Skill (requires telemetry / DuckDB)
        if let Some(ref telemetry) = self.storage.telemetry {
            use crate::skills::TodoSkill;
            registry.register_skill(Box::new(TodoSkill::new(Arc::new(telemetry.clone()))));
        }

        // Register Working Memory Skill
        if let Some(neo4j) = &self.storage.neo4j {
            let config2 = self.llm_config.read().await.clone();
            let llm_client2 = config2.and_then(|c| crate::services::LlmClient::with_config(c).ok());
            let knowledge_svc2: Arc<dyn crate::services::KnowledgeStore> =
                Arc::new(KnowledgeService::new(neo4j.clone(), llm_client2));
            let wm_store: Arc<dyn crate::services::WorkingMemoryStore> = Arc::new(neo4j.clone());
            let wm_skill = WorkingMemorySkill::new(
                wm_store,
                knowledge_svc2,
                Arc::clone(&shared_llm) as Arc<dyn crate::services::LlmProvider>,
            );
            registry.register_skill(Box::new(wm_skill));
        }

        // Register Agent Skill (queue management)
        if let Some(ref qs) = queue_arc {
            registry.register_skill(Box::new(AgentSkill::new(Arc::clone(qs))));
        }

        // Register Scheduler Skill
        if let Some(ref sched) = scheduler_arc {
            registry.register_skill(Box::new(SchedulerSkill::new(Arc::clone(sched), self.storage.neo4j.clone())));
        }

        // Register Context Skill (profile management)
        if let Some(ref cb) = context_builder_arc {
            registry.register_skill(Box::new(ContextSkill::new(Arc::clone(cb))));
        }

        // Register WebSocket Skill
        registry.register_skill(Box::new(WsSkill::new()));

        // Register Resource Skill (shared Arc so all callers see the same registry)
        let resource_registry = Arc::new(ResourceRegistry::new());
        registry.register_skill(Box::new(ResourceSkill::new(Arc::clone(&resource_registry))));

        // Register DynamicSkill in registry (shared-map clone — registry sees live updates)
        if let Some(ref d) = dynamic_skill {
            registry.register_skill(Box::new(d.clone_shared()));
        }

        drop(registry);

        // Build handler skills list (re-creates non-dynamic skills; DynamicSkill original goes here)
        let mut skills: Vec<Box<dyn Skill>> = Vec::new();

        if let Some(neo4j) = &self.storage.neo4j {
            let config3 = self.llm_config.read().await.clone();
            let llm_client3 = config3.and_then(|c| crate::services::LlmClient::with_config(c).ok());
            let knowledge_svc3: Arc<dyn crate::services::KnowledgeStore> =
                Arc::new(KnowledgeService::new(neo4j.clone(), llm_client3));
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

        if let Some(neo4j) = &self.storage.neo4j {
            let proc_store2: Arc<dyn crate::services::ProcedureStore> = Arc::new(neo4j.clone());
            skills.push(Box::new(ProcedureSkill::new(proc_store2)));
        }

        skills.push(Box::new(SearchSkill::new(
            self.storage.telemetry.clone(),
            self.storage.neo4j.clone(),
        )));

        // Query Skill (handler copy)
        skills.push(Box::new(QuerySkill::new(
            self.storage.neo4j.clone(),
            self.storage.telemetry.clone(),
        )));

        // HTTP Skill (handler copy)
        skills.push(Box::new(HttpSkill::new(self.storage.neo4j.clone())));

        // Codebase Skill (handler copy)
        {
            let knowledge_store2: Option<Arc<dyn crate::services::KnowledgeStore>> =
                if let Some(neo4j) = &self.storage.neo4j {
                    let cfg_cb2 = self.llm_config.read().await.clone();
                    let llm_cb2 = cfg_cb2.and_then(|c| crate::services::LlmClient::with_config(c).ok());
                    Some(Arc::new(KnowledgeService::new(neo4j.clone(), llm_cb2))
                        as Arc<dyn crate::services::KnowledgeStore>)
                } else {
                    None
                };
            skills.push(Box::new(CodebaseSkill::new(
                self.codebase.codebase_dir.clone(),
                knowledge_store2,
            )));
        }

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

        if let Some(ref telemetry) = self.storage.telemetry {
            use crate::skills::TodoSkill;
            skills.push(Box::new(TodoSkill::new(Arc::new(telemetry.clone()))));
        }

        if let Some(neo4j) = &self.storage.neo4j {
            let config4 = self.llm_config.read().await.clone();
            let llm_client4 = config4.and_then(|c| crate::services::LlmClient::with_config(c).ok());
            let knowledge_svc4: Arc<dyn crate::services::KnowledgeStore> =
                Arc::new(KnowledgeService::new(neo4j.clone(), llm_client4));
            let wm_store2: Arc<dyn crate::services::WorkingMemoryStore> = Arc::new(neo4j.clone());
            skills.push(Box::new(WorkingMemorySkill::new(
                wm_store2,
                knowledge_svc4,
                Arc::clone(&shared_llm) as Arc<dyn crate::services::LlmProvider>,
            )));
        }

        // Agent Skill (queue management)
        if let Some(ref qs) = queue_arc {
            skills.push(Box::new(AgentSkill::new(Arc::clone(qs))));
        }

        // Scheduler Skill (autonomous self-improvement loop)
        if let Some(ref sched) = scheduler_arc {
            skills.push(Box::new(SchedulerSkill::new(Arc::clone(sched), self.storage.neo4j.clone())));
        }

        // Context Skill (profile management)
        if let Some(ref cb) = context_builder_arc {
            skills.push(Box::new(ContextSkill::new(Arc::clone(cb))));
        }

        // WebSocket Skill
        skills.push(Box::new(WsSkill::new()));

        // Resource Skill
        skills.push(Box::new(ResourceSkill::new(Arc::clone(&resource_registry))));

        // Push original DynamicSkill to handler (shares tools_map with registry clone)
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

    /// Seed all built-in Neo4j nodes idempotently at every startup.
    ///
    /// Called at the TOP of `build_skills()` — before `DynamicSkill::load_from_neo4j()`
    /// — so every seeded `DynamicTool` is available in the tool registry on the
    /// very first boot.
    ///
    /// All writes use `MERGE … ON CREATE SET` so user-edited nodes survive restarts.
    async fn seed_built_ins(neo4j: &Neo4jClient) {
        let ts = chrono::Utc::now().to_rfc3339();

        // ── ApiContext nodes ──────────────────────────────────────────────────
        // GitHub — always update so the standard headers stay current.
        let default_hdrs = r#"{"Accept":"application/vnd.github+json","X-GitHub-Api-Version":"2022-11-28"}"#;
        let cypher = "MERGE (c:ApiContext {name: 'github'}) \
                      SET c.base_url        = 'https://api.github.com', \
                          c.auth_scheme     = 'bearer', \
                          c.auth_param      = 'Authorization', \
                          c.auth_env_var    = 'GITHUB_TOKEN', \
                          c.default_headers = $hdrs, \
                          c.description     = 'GitHub REST API v3'";
        if let Err(e) = neo4j.run(neo4rs::query(cypher).param("hdrs", default_hdrs)).await {
            warn!(error = %e, "Failed to seed github ApiContext (non-fatal)");
        }

        // Search engines — ON CREATE only so user overrides survive.
        for (name, base_url, scheme, param, env_var, desc) in [
            ("serpapi",    "https://serpapi.com",                        "query_param", "api_key",              "SERPAPI_KEY",    "SerpApi search engine"),
            ("brave",      "https://api.search.brave.com",               "header",      "X-Subscription-Token", "BRAVE_API_KEY",  "Brave Search API"),
            ("google_cse", "https://www.googleapis.com/customsearch/v1", "query_param", "key",                  "GOOGLE_API_KEY", "Google Custom Search Engine"),
        ] {
            let q = "MERGE (c:ApiContext {name: $name}) \
                     ON CREATE SET c.base_url     = $base_url, \
                                   c.auth_scheme  = $scheme, \
                                   c.auth_param   = $param, \
                                   c.auth_env_var = $env_var, \
                                   c.description  = $desc";
            if let Err(e) = neo4j.run(neo4rs::query(q)
                .param("name", name).param("base_url", base_url)
                .param("scheme", scheme).param("param", param)
                .param("env_var", env_var).param("desc", desc)).await
            {
                warn!(name = name, error = %e, "Failed to seed search ApiContext (non-fatal)");
            }
        }
        if let Ok(cx) = std::env::var("GOOGLE_CX") {
            let _ = neo4j.run(neo4rs::query(
                "MATCH (c:ApiContext {name: 'google_cse'}) SET c.google_cx = $cx"
            ).param("cx", cx)).await;
        }

        // ── Procedure + DynamicTool pairs ─────────────────────────────────────
        // Each entry replaces a thin Rust wrapper with a data-driven equivalent.
        // Seeding both nodes together means load_from_neo4j() picks them up immediately.
        //
        // (tool_name, tool_description, input_schema_json, cypher_query, step_purpose)
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

        // Three simple queries per tool — easier to debug than one combined chain.
        // steps and schema are always SET (not ON CREATE) so fixes land on restart.
        // id and created_at are ON CREATE only so they're stable once set.
        for (name, description, schema, query, purpose) in tools {
            // Use serde_json to build steps — no manual JSON string formatting.
            let steps = serde_json::to_string(&json!([{
                "tool":    "neo4j_query",
                "args":    { "cypher": query },
                "purpose": purpose,
            }]))
            .unwrap();

            // 1 — Upsert Procedure node (steps always updated).
            let q1 = "MERGE (p:Procedure {name: $name}) \
                      ON CREATE SET p.id = $id, p.created_at = datetime($ts) \
                      SET p.description = $description, p.steps = $steps";
            if let Err(e) = neo4j.run(neo4rs::query(q1)
                .param("name",        *name)
                .param("id",          uuid::Uuid::new_v4().to_string())
                .param("ts",          ts.as_str())
                .param("description", *description)
                .param("steps",       steps)).await
            {
                warn!(name = *name, error = %e, "seed_built_ins: failed to upsert Procedure");
                continue;
            }

            // 2 — Upsert DynamicTool node (schema always updated).
            let q2 = "MERGE (d:DynamicTool {name: $name}) \
                      ON CREATE SET d.id = $id, d.created_at = datetime($ts) \
                      SET d.description = $description, d.input_schema = $schema";
            if let Err(e) = neo4j.run(neo4rs::query(q2)
                .param("name",        *name)
                .param("id",          uuid::Uuid::new_v4().to_string())
                .param("ts",          ts.as_str())
                .param("description", *description)
                .param("schema",      *schema)).await
            {
                warn!(name = *name, error = %e, "seed_built_ins: failed to upsert DynamicTool");
                continue;
            }

            // 3 — Ensure [:USES] relationship exists.
            let q3 = "MATCH (d:DynamicTool {name: $name}), (p:Procedure {name: $name}) \
                      MERGE (d)-[:USES]->(p)";
            if let Err(e) = neo4j.run(neo4rs::query(q3).param("name", *name)).await {
                warn!(name = *name, error = %e, "seed_built_ins: failed to create [:USES] edge");
            } else {
                debug!(name = *name, "seed_built_ins: upserted DynamicTool+Procedure pair");
            }
        }
    }

    /// Initialize skills and run the boot protocol.
    ///
    /// Call this before using [`chat_service`] or other capabilities outside
    /// of [`run_with_transport`] (which calls this automatically).
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

    /// Check if the server is shutting down.
    pub async fn is_shutting_down(&self) -> bool {
        *self.state.read().await == ServerState::ShuttingDown
    }

    /// Run the server with a specific transport implementation.
    pub async fn run_with_transport<T: McpTransport>(
        &self,
        transport: &T,
    ) -> Result<(), McpServerError> {
        // Ensure skills are built and boot protocol has run.
        self.initialize().await;

        info!(
            name = %self.config.name,
            version = %self.config.version,
            transport = %transport.name(),
            "Starting MCP server"
        );

        let mut rx = transport
            .start()
            .await
            .map_err(|e| McpServerError::Transport(e.to_string()))?;

        // Main message loop
        while let Some(msg) = rx.recv().await {
            match msg {
                TransportMessage::Request {
                    session_id: _,
                    request,
                    response_tx,
                } => {
                    let response = self.handle_request(request).await;
                    // Send response back through the oneshot channel
                    let _ = response_tx.send(response);

                    // Check if we should shut down
                    if self.is_shutting_down().await {
                        info!("Server shutting down");
                        break;
                    }
                }
                TransportMessage::Notification {
                    session_id: _,
                    notification,
                } => {
                    self.handle_notification(&notification.method).await;
                }
            }
        }

        transport
            .shutdown()
            .await
            .map_err(|e| McpServerError::Transport(e.to_string()))?;

        info!("MCP server stopped");
        Ok(())
    }

    /// Handle an incoming JSON-RPC request (thread-safe).
    pub async fn handle_request(&self, request: JsonRpcRequest) -> OutgoingMessage {
        debug!(method = %request.method, id = ?request.id, "Handling request");

        let response = match request.method.as_str() {
            "initialize" => self.handle_initialize(&request).await,
            "shutdown" => self.handle_shutdown(&request).await,
            "tools/list" => self.handle_tools_list(&request).await,
            "tools/call" => self.handle_tools_call(&request).await,
            "ping" => self.handle_ping(&request),
            _ => {
                let state = self.get_state().await;
                if state != ServerState::Running {
                    Err(JsonRpcErrorResponse::new(
                        Some(request.id.clone()),
                        error_codes::INVALID_REQUEST,
                        "Server not initialized",
                    ))
                } else {
                    Err(JsonRpcErrorResponse::method_not_found(
                        request.id.clone(),
                        &request.method,
                    ))
                }
            }
        };

        match response {
            Ok(result) => OutgoingMessage::Response(result),
            Err(error) => OutgoingMessage::Error(error),
        }
    }

    /// Handle a notification (thread-safe, no response expected).
    pub async fn handle_notification(&self, method: &str) {
        debug!(method = %method, "Handling notification");

        match method {
            "notifications/initialized" => {
                let mut state = self.state.write().await;
                // Accept from Initializing (normal flow) OR Created (resurrection
                // after restart when the transport skips the initialize handshake).
                if *state == ServerState::Initializing || *state == ServerState::Created {
                    *state = ServerState::Running;
                    info!("Server initialized and ready");
                }
            }
            "notifications/cancelled" => {
                debug!("Request cancelled");
            }
            _ => {
                debug!(method = %method, "Unknown notification");
            }
        }
    }

    // ========================================================================
    // Request Handlers (thread-safe versions)
    // ========================================================================

    async fn handle_initialize(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        let current_state = {
            let state = self.state.read().await;
            *state
        };

        // Reject only if shutting down.
        if current_state == ServerState::ShuttingDown {
            return Err(JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INVALID_REQUEST,
                "Server is shutting down",
            ));
        }

        // Parse initialize params
        let params: InitializeParams = request
            .params
            .as_ref()
            .map(|p| serde_json::from_value(p.clone()))
            .transpose()
            .map_err(|e| {
                JsonRpcErrorResponse::invalid_params(
                    request.id.clone(),
                    format!("Invalid initialize params: {}", e),
                )
            })?
            .ok_or_else(|| {
                JsonRpcErrorResponse::invalid_params(
                    request.id.clone(),
                    "Missing initialize params",
                )
            })?;

        info!(
            client = %params.client_info.name,
            protocol_version = %params.protocol_version,
            already_running = (current_state == ServerState::Running),
            "Client connecting"
        );

        // Advance to Initializing only from Created state; already-running server
        // stays Running so existing sessions are not disrupted.
        if current_state == ServerState::Created {
            let mut state = self.state.write().await;
            *state = ServerState::Initializing;
        }

        let result = InitializeResult {
            protocol_version: MCP_PROTOCOL_VERSION.to_string(),
            capabilities: ServerCapabilities {
                tools: Some(ToolsCapability {
                    list_changed: false,
                }),
                resources: None,
                prompts: None,
            },
            server_info: ServerInfo {
                name: self.config.name.clone(),
                version: self.config.version.clone(),
            },
            instructions: self.config.instructions.clone(),
        };

        Ok(JsonRpcResponse::new(
            request.id.clone(),
            serde_json::to_value(result).unwrap(),
        ))
    }

    async fn handle_shutdown(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        info!("Shutdown requested");
        {
            let mut state = self.state.write().await;
            *state = ServerState::ShuttingDown;
        }
        Ok(JsonRpcResponse::new(request.id.clone(), json!(null)))
    }

    async fn handle_tools_list(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        let state = self.get_state().await;
        if state != ServerState::Running {
            return Err(JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INVALID_REQUEST,
                "Server not initialized",
            ));
        }

        let all_tools = {
            let registry = self.tool_registry.read().await;
            registry.list()
        };

        // If MCP_TOOL_PROFILE is set, filter to only that profile's allowed tools.
        let tools = if let Some(profile_name) = &self.mcp_tool_profile {
            let cb_opt = self.context_builder_svc.read().await.clone();
            if let Some(cb) = cb_opt {
                if let Some(profile) = cb.get_profile(profile_name).await {
                    filter_tools_by_names(all_tools, &profile.tools)
                } else {
                    tracing::warn!(profile = %profile_name, "MCP_TOOL_PROFILE not found — returning all tools");
                    all_tools
                }
            } else {
                all_tools
            }
        } else {
            all_tools
        };

        let result = ToolsListResult { tools };

        Ok(JsonRpcResponse::new(
            request.id.clone(),
            serde_json::to_value(result).unwrap(),
        ))
    }

    async fn handle_tools_call(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        let state = self.get_state().await;
        if state != ServerState::Running {
            return Err(JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INVALID_REQUEST,
                "Server not initialized",
            ));
        }

        // Parse tool call params
        let params: ToolCallParams = request
            .params
            .as_ref()
            .map(|p| serde_json::from_value(p.clone()))
            .transpose()
            .map_err(|e| {
                JsonRpcErrorResponse::invalid_params(
                    request.id.clone(),
                    format!("Invalid tool call params: {}", e),
                )
            })?
            .ok_or_else(|| {
                JsonRpcErrorResponse::invalid_params(request.id.clone(), "Missing tool call params")
            })?;

        // Check if tool exists (release lock before await)
        {
            let registry = self.tool_registry.read().await;
            if registry.get(&params.name).is_none() {
                return Err(JsonRpcErrorResponse::invalid_params(
                    request.id.clone(),
                    format!("Unknown tool: {}", params.name),
                ));
            }
        }

        // Clone handler to avoid holding the lock across the await
        let handler = {
            let guard = self.tool_handler.read().await;
            guard.clone()
        };

        let handler = handler.ok_or_else(|| {
            JsonRpcErrorResponse::new(
                Some(request.id.clone()),
                error_codes::INTERNAL_ERROR,
                "Tool handler not initialized",
            )
        })?;

        // Execute the tool (lock is released)
        let result = handler.execute(&params.name, params.arguments).await;

        // Notify the scheduler that activity occurred — wakes sleep mode if active.
        if let Some(sched) = self.jobs.scheduler.read().await.as_ref() {
            sched.notify_activity().await;
        }

        Ok(JsonRpcResponse::new(
            request.id.clone(),
            serde_json::to_value(result).unwrap(),
        ))
    }

    fn handle_ping(
        &self,
        request: &JsonRpcRequest,
    ) -> Result<JsonRpcResponse, JsonRpcErrorResponse> {
        Ok(JsonRpcResponse::new(request.id.clone(), json!({})))
    }
}

impl Default for McpServerCore {
    fn default() -> Self {
        Self::new()
    }
}

// ============================================================================
// Legacy McpServer — wraps McpServerCore for stdio backward compatibility
// ============================================================================

/// MCP server for the API Knowledge Graph.
///
/// This is a thin wrapper around `McpServerCore` maintained for backward
/// compatibility with the legacy stdio transport path. For new code, use
/// `McpServerCore` directly with an explicit transport.
pub struct McpServer {
    core: McpServerCore,
}

impl McpServer {
    /// Create a new MCP server with default configuration.
    pub fn new() -> Self {
        Self {
            core: McpServerCore::new(),
        }
    }

    /// Create a new MCP server with custom configuration.
    pub fn with_config(config: McpServerConfig) -> Self {
        Self {
            core: McpServerCore::with_config(config),
        }
    }

    /// Set the Neo4j client for database operations.
    pub fn with_neo4j(mut self, neo4j: Neo4jClient) -> Self {
        self.core = self.core.with_neo4j(neo4j);
        self
    }

    /// Set the LLM configuration for healing.
    pub fn with_llm_config(mut self, config: LlmConfig) -> Self {
        self.core = self.core.with_llm_config(config);
        self
    }

    /// Run the MCP server with stdio transport.
    pub async fn run(self) -> Result<(), McpServerError> {
        // Build skills first
        self.core.build_skills().await;

        info!(
            name = %self.core.config.name,
            version = %self.core.config.version,
            "Starting MCP server (stdio)"
        );

        let (transport, mut rx) = StdioTransport::new();

        // Main message loop
        while let Some(result) = rx.recv().await {
            match result {
                Ok(message) => {
                    if let Some(response) = self.handle_message(message).await
                        && transport.send(response).await.is_err()
                    {
                        error!("Failed to send response - transport closed");
                        break;
                    }

                    // Check if we should shut down
                    if self.core.is_shutting_down().await {
                        info!("Server shutting down");
                        break;
                    }
                }
                Err(error) => {
                    warn!(error = ?error, "Received malformed message");
                    if transport.send(OutgoingMessage::Error(error)).await.is_err() {
                        break;
                    }
                }
            }
        }

        info!("MCP server stopped");
        Ok(())
    }

    /// Handle an incoming message and optionally return a response.
    async fn handle_message(&self, message: IncomingMessage) -> Option<OutgoingMessage> {
        match message {
            IncomingMessage::Request(request) => Some(self.core.handle_request(request).await),
            IncomingMessage::Notification(notification) => {
                self.core.handle_notification(&notification.method).await;
                None
            }
        }
    }
}

impl Default for McpServer {
    fn default() -> Self {
        Self::new()
    }
}

/// Filter a tool list to only those whose names appear in `names`.
/// Returns `all` unchanged if `names` is empty.
fn filter_tools_by_names(
    all: Vec<super::protocol::ToolDefinition>,
    names: &[String],
) -> Vec<super::protocol::ToolDefinition> {
    if names.is_empty() {
        return all;
    }
    let allowed: std::collections::HashSet<&str> = names.iter().map(|s| s.as_str()).collect();
    all.into_iter()
        .filter(|t| allowed.contains(t.name.as_str()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_server_config_default() {
        let config = McpServerConfig::default();
        assert_eq!(config.name, "agent-brain");
        assert!(config.instructions.is_some());
    }

    #[test]
    fn test_server_creation() {
        let _server = McpServer::new();
    }

    #[test]
    fn test_server_with_config() {
        let config = McpServerConfig {
            name: "test-server".to_string(),
            version: "1.0.0".to_string(),
            instructions: None,
        };
        let server = McpServer::with_config(config);
        assert_eq!(server.core.config.name, "test-server");
    }

    // ========================================================================
    // McpServerCore Tests (thread-safe version)
    // ========================================================================

    #[tokio::test]
    async fn test_server_core_creation() {
        let server = McpServerCore::new();
        assert_eq!(server.get_state().await, ServerState::Created);
    }

    #[tokio::test]
    async fn test_server_core_with_config() {
        let config = McpServerConfig {
            name: "test-server".to_string(),
            version: "1.0.0".to_string(),
            instructions: None,
        };
        let server = McpServerCore::with_config(config);
        assert_eq!(server.config.name, "test-server");
    }

    #[tokio::test]
    async fn test_server_core_state_transitions() {
        let server = McpServerCore::new();
        assert_eq!(server.get_state().await, ServerState::Created);

        // Simulate initialize
        {
            let mut state = server.state.write().await;
            *state = ServerState::Initializing;
        }
        assert_eq!(server.get_state().await, ServerState::Initializing);

        // Simulate initialized notification
        server
            .handle_notification("notifications/initialized")
            .await;
        assert_eq!(server.get_state().await, ServerState::Running);
    }

    #[tokio::test]
    async fn test_server_core_is_shutting_down() {
        let server = McpServerCore::new();
        assert!(!server.is_shutting_down().await);

        {
            let mut state = server.state.write().await;
            *state = ServerState::ShuttingDown;
        }
        assert!(server.is_shutting_down().await);
    }

    #[tokio::test]
    async fn test_server_core_concurrent_state_access() {
        use std::sync::Arc;

        let server = Arc::new(McpServerCore::new());

        // Spawn multiple tasks reading state concurrently
        let mut handles = vec![];
        for _ in 0..10 {
            let server_clone = Arc::clone(&server);
            handles.push(tokio::spawn(async move { server_clone.get_state().await }));
        }

        // All should return Created
        for handle in handles {
            let state = handle.await.expect("Task panicked");
            assert_eq!(state, ServerState::Created);
        }
    }

    #[tokio::test]
    async fn test_server_core_handle_notification_thread_safe() {
        let server = McpServerCore::new();

        // Set state to Initializing
        {
            let mut state = server.state.write().await;
            *state = ServerState::Initializing;
        }

        // Handle notification should transition state
        server
            .handle_notification("notifications/initialized")
            .await;
        assert_eq!(server.get_state().await, ServerState::Running);
    }

    #[tokio::test]
    async fn test_server_core_ping_request() {
        let server = McpServerCore::new();

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id: super::super::protocol::RequestId::Number(1),
            method: "ping".to_string(),
            params: None,
        };

        let response = server.handle_request(request).await;
        match response {
            OutgoingMessage::Response(r) => {
                assert_eq!(r.result, serde_json::json!({}));
            }
            OutgoingMessage::Error(_) => panic!("Expected response, got error"),
        }
    }

    #[tokio::test]
    async fn test_server_core_initialized_notification_only_from_initializing_state() {
        let server = McpServerCore::new();

        // From Created state - transitions to Running (session resurrection support)
        server
            .handle_notification("notifications/initialized")
            .await;
        assert_eq!(server.get_state().await, ServerState::Running);

        // From Running state - should stay Running
        {
            let mut state = server.state.write().await;
            *state = ServerState::Running;
        }
        server
            .handle_notification("notifications/initialized")
            .await;
        assert_eq!(server.get_state().await, ServerState::Running);

        // From Initializing state - should transition to Running
        {
            let mut state = server.state.write().await;
            *state = ServerState::Initializing;
        }
        server
            .handle_notification("notifications/initialized")
            .await;
        assert_eq!(server.get_state().await, ServerState::Running);
    }
}
