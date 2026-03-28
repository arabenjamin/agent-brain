# Refactoring Plan: Monolith to Workspace

Status as of 2026-03-27. This plan addresses structural/architectural issues
in the codebase. It is independent of feature work tracked in `ROADMAP.md`.

**Decision: Incremental refactor, not a rewrite.**

**Progress: Phase 0 ✅ complete (2026-03-27) | Phase 1 ✅ complete (2026-03-27)**

The codebase is ~30K LOC of working, tested Rust. The problems are structural
(god objects, tight coupling, shared mutable state) — not fundamental design
flaws. Every phase below preserves existing functionality and tests.

---

## Current Problems

| # | Problem | Severity | Where |
|---|---------|----------|-------|
| 1 | 40% of the codebase (~11K LOC) is agent-api legacy with no brain purpose | **CRITICAL** | services/, skills/api.rs, models/, repository/ |
| 2 | 12 direct "agent-api" references in names, strings, and config | HIGH | cli.rs, main.rs, docker-compose, services/ |
| 3 | `McpServerCore` is a god object (15+ fields, all responsibilities) | HIGH | `mcp/server.rs` |
| 4 | `Config` is a catch-all (Neo4j + 3 LLM providers + 3 secret backends + logging + search keys) | MEDIUM | `config.rs` |
| 5 | MCP and Services have a circular import (services use `mcp::protocol::Content`, MCP uses services) | HIGH | `services/queue.rs`, `mcp/server.rs` |
| 6 | No trait abstractions — `Neo4jClient` and `LlmConfig` passed as concrete types everywhere | MEDIUM | All skills, all services |
| 7 | `Arc<RwLock<Option<LlmConfig>>>` shared across all skills — no isolation | MEDIUM | `mcp/server.rs`, all skills |
| 8 | `build_skills()` has fragile order-dependent initialization | HIGH | `mcp/server.rs:253-471` |
| 9 | `DynamicSkill` shares mutable `HashMap` via `Arc<RwLock>` with tool handler | MEDIUM | `skills/dynamic.rs` |
| 10 | Single crate with 37 runtime dependencies — slow compilation, no modularity | LOW | `Cargo.toml` |
| 11 | Model context stored in Neo4j (heavy), system prompt hardcoded, no declarative config | MEDIUM | `models/model_spec.rs`, `services/chat.rs` |

---

## Phase 0: Shed the Agent-API Identity ✅ COMPLETE

**Risk:** Low-Medium — large deletion, but cleanly bounded code
**Estimated scope:** ~11,161 LOC removed, ~200 LOC changed
**Actual:** ~15K LOC removed across 49 files, 107 unit tests passing

This project was forked from `agent-api`, an OpenAPI management tool. Agent-brain
has since grown into an autonomous knowledge-graph agent, but 40% of the codebase
is still dedicated to OpenAPI ingestion, API healing, endpoint management, and
spec export — none of which serve the brain's core mission. This phase removes
that legacy so subsequent refactoring phases operate on a leaner, focused codebase.

### 0.1 What Gets Removed

**By the numbers:**
- 14 MCP tools (ApiSkill) → removed
- 6 services (~5,465 LOC) → removed
- 1 export sub-module (4 files, ~2,037 LOC) → removed
- 6 models (~512 LOC) → removed
- 6 repository files (~1,153 LOC) → removed
- 6 Neo4j node types (Resource, Endpoint, Schema, Parameter, HealingEvent, ApiCredential) → removed
- 7 CLI commands (ingest, query, execute, export, diff, stats, embed) → removed
- 1 CI workflow (api-contract.yml, 205 LOC) → removed

**Files to delete:**

```
# Skills (1,783 LOC)
src/skills/api.rs

# Services (5,465 LOC)
src/services/openapi.rs          # OpenAPI 3.0 parser
src/services/healing.rs          # Self-healing orchestrator
src/services/discovery.rs        # OpenAPI auto-discovery
src/services/docgen.rs           # Doc→OpenAPI generator
src/services/repo.rs             # Repo→OpenAPI generator
src/services/http.rs             # HTTP executor for API calls

# Export module (2,037 LOC)
src/services/export/mod.rs
src/services/export/builder.rs
src/services/export/exporter.rs
src/services/export/differ.rs
src/services/export/report.rs

# Context store (API-specific cache)
src/services/context.rs

# Models (512 LOC)
src/models/endpoint.rs
src/models/schema.rs
src/models/parameter.rs
src/models/resource.rs
src/models/healing.rs
src/models/credential.rs
src/models/http.rs               # HttpMethod enum (only used by endpoints)

# Repository (1,153 LOC)
src/repository/endpoint.rs
src/repository/schema.rs
src/repository/parameter.rs
src/repository/resource.rs
src/repository/healing.rs
src/repository/credential.rs

# CI workflow
.github/workflows/api-contract.yml
```

### 0.2 What Gets Modified

**Remove ApiSkill registration:**
- `src/skills/mod.rs` — remove `pub mod api;` and `ApiSkill` export
- `src/mcp/server.rs` — remove ApiSkill from `build_skills()`, remove
  `context_store` and `credential_manager` fields from McpServerCore

**Remove API-related CLI commands:**
- `src/cli.rs` — remove `ingest`, `query`, `execute`, `export`, `diff`,
  `stats`, `embed` subcommands
- `src/main.rs` — remove match arms for deleted subcommands

**Remove API models/repository from mod.rs files:**
- `src/models/mod.rs` — remove re-exports for deleted types
- `src/repository/mod.rs` — remove re-exports for deleted modules
- `src/services/mod.rs` — remove re-exports for deleted modules

**Remove API graph schema from init-db:**
- `src/repository/client.rs` — remove index/constraint creation for
  Resource, Endpoint, Schema, Parameter, HealingEvent, ApiCredential nodes;
  remove `endpoint_embeddings` vector index

**Remove API-related config fields:**
- `src/config.rs` — no API-specific config fields exist (credentials
  managed via tools), but verify

**Update AdminSkill:**
- `src/skills/admin.rs` — remove `delete_api`, `purge_duplicate_endpoints`,
  `purge_orphaned_schemas`, `reset_graph`, `backfill_endpoint_embeddings`
  (all API-graph-specific). AdminSkill itself may be deleted or reduced to
  a single general-purpose tool.

**Remove secrets/credential integration (if solely API-serving):**
- Evaluate whether `services/secrets/manager.rs` (`CredentialManager`) is
  used by anything other than API credential injection. If not, remove it.
  The `SecretProvider` trait and implementations (local/vault/aws) may still
  be useful for general secrets — keep those.

### 0.3 Fix Direct "agent-api" References

These 12 locations still reference the old project name:

| File | Line | Current | Change To |
|------|------|---------|-----------|
| `src/cli.rs` | 5 | `#[command(name = "agent-api")]` | `"agent-brain"` |
| `src/main.rs` | 48 | `"Starting agent-api"` | `"Starting agent-brain"` |
| `src/services/repo.rs` | 489 | User-agent `"agent-api/0.1.0"` | **file deleted** |
| `src/services/discovery.rs` | 109 | User-agent `"agent-api/0.1.0"` | **file deleted** |
| `src/services/docgen.rs` | 315 | User-agent `"agent-api/0.1.0"` | **file deleted** |
| `src/services/http.rs` | 148 | User-agent `"agent-api/{version}"` | **file deleted** |
| `src/services/secrets/local.rs` | 19 | Salt `b"agent-api-secrets-v1"` | `b"agent-brain-secrets-v1"` |
| `src/services/secrets/aws.rs` | 19,323,326 | Prefix `/agent-api` | `/agent-brain` |
| `docker-compose.yml` | 8,29,34 | Network `agent-api_default` | `agent-brain_default` |
| `.github/workflows/api-contract.yml` | 79 | `"Build agent-api"` | **file deleted** |

**Note on salt change:** Changing the salt in `local.rs` will invalidate
existing encrypted secrets. Document this as a breaking change and provide
a migration path (decrypt with old salt, re-encrypt with new).

### 0.4 Remove Unused Dependencies

After deletion, these Cargo.toml dependencies become unused:

| Dependency | Used By | Action |
|------------|---------|--------|
| `openapiv3` | `services/openapi.rs`, `services/export/` | Remove |
| `scraper` | `services/discovery.rs`, `services/docgen.rs` | Remove (unless used elsewhere) |
| `glob` | `services/repo.rs` (file discovery) | Remove (unless used elsewhere) |
| `tempfile` | `services/repo.rs` (repo cloning) | Check — also used in tests |
| `tokio-tungstenite` | `services/repo.rs` (WebSocket for git) | Remove |

This reduces the dependency tree and compile times.

### 0.5 Update Documentation

- `CLAUDE.md` — remove all references to API tools, update project structure,
  update tool count, remove API node types from graph schema
- `docs/PLAN.md` — mark as historical (original agent-api build plan)
- `ROADMAP.md` — remove any API-specific items
- `README.md` — rewrite project description to focus on brain/knowledge/autonomy

### 0.6 Update Tests

- Delete `tests/context_tools_test.rs` (API context management tests)
- Delete `tests/discovery_test.rs` (API discovery tests)
- Delete `tests/docgen_test.rs` (doc→OpenAPI tests)
- Delete `tests/repo_analyzer_test.rs` (repo→OpenAPI tests)
- Delete `tests/fixtures/petstore.json` (OpenAPI fixture)
- Keep `tests/repository_test.rs` (verify it doesn't test API entities)
- Keep `tests/http_transport_test.rs` (MCP transport, not API-specific)

### 0.7 Verify

- [ ] `cargo build` succeeds
- [ ] `cargo test --lib` passes
- [ ] `cargo test --test '*'` passes
- [ ] `cargo clippy` clean
- [ ] No references to `agent-api` remain (`grep -r "agent.api" src/`)
- [ ] No references to deleted modules remain
- [ ] Docker build works
- [ ] Tool count drops from 69 to ~50 (14 API tools + ~5 admin tools removed)
- [ ] Binary still starts and responds to MCP initialize

### 0.8 What This Unlocks

After this phase, the codebase drops from ~30K to ~19K LOC. The remaining
code is 100% brain-focused:
- Knowledge graph (notes, entities, RAG, reasoning)
- Task management (goals, decomposition, reflection)
- Job queue and scheduler (autonomous execution)
- Working memory (session scratchpads)
- Procedural memory (stored workflows, dynamic tools)
- LLM integration (multi-provider, model selection)
- MCP transport (stdio + HTTP/SSE)

Every subsequent refactoring phase operates on a smaller, more coherent codebase.

---

## Phase 1: Cargo Workspace + Extract Pure Crates ✅ COMPLETE

**Risk:** Low — mechanical extraction, no logic changes
**Estimated scope:** ~200 LOC of `Cargo.toml` / `mod.rs` changes, zero logic changes
**Actual:** 3-crate workspace (agent-brain-models, agent-brain-repository, agent-brain app), 107 unit tests passing

### 1.1 Convert to Cargo Workspace

Create a workspace root `Cargo.toml` and move the application into `crates/app/`:

```
agent-brain/
├── Cargo.toml                    # [workspace] members = ["crates/*"]
├── crates/
│   ├── models/                   # New crate: agent-brain-models
│   │   ├── Cargo.toml            # deps: serde, chrono, uuid, schemars
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── resource.rs
│   │       ├── endpoint.rs
│   │       ├── schema.rs
│   │       ├── parameter.rs
│   │       ├── healing.rs
│   │       ├── http.rs
│   │       ├── credential.rs
│   │       ├── task.rs
│   │       ├── agent_job.rs
│   │       ├── model_spec.rs
│   │       └── procedure.rs
│   ├── repository/               # New crate: agent-brain-repository
│   │   ├── Cargo.toml            # deps: agent-brain-models, neo4rs, chrono, uuid, thiserror
│   │   └── src/
│   │       ├── lib.rs            # Re-exports Neo4jClient, TelemetryClient, error types
│   │       ├── client.rs
│   │       ├── error.rs
│   │       ├── resource.rs
│   │       ├── endpoint.rs
│   │       ├── schema.rs
│   │       ├── parameter.rs
│   │       ├── healing.rs
│   │       ├── credential.rs
│   │       ├── admin.rs
│   │       ├── agent_job.rs
│   │       ├── model_spec.rs
│   │       └── task.rs
│   └── app/                      # Existing application (everything else)
│       ├── Cargo.toml            # deps: agent-brain-models, agent-brain-repository, + rest
│       └── src/
│           ├── main.rs
│           ├── lib.rs
│           ├── cli.rs
│           ├── config.rs
│           ├── logging.rs
│           ├── services/
│           ├── skills/
│           └── mcp/
├── tests/                        # Integration tests (stay at workspace root)
├── docker-compose.yml
└── .github/
```

### 1.2 Extract `agent-brain-models`

**Precondition:** Models have zero internal dependencies (verified).

Steps:
1. Create `crates/models/Cargo.toml` with only: `serde`, `serde_json`, `chrono`, `uuid`, `schemars`
2. Move all files from `src/models/` to `crates/models/src/`
3. In the app crate, replace `mod models` with `use agent_brain_models as models` (or alias)
4. Run `cargo test --lib` — everything should compile unchanged

### 1.3 Extract `agent-brain-repository`

**Precondition:** Repository depends only on models (verified).

Steps:
1. Create `crates/repository/Cargo.toml` with: `agent-brain-models`, `neo4rs`, `chrono`, `uuid`, `thiserror`, `serde_json`, `tracing`
2. Move all files from `src/repository/` to `crates/repository/src/`
3. In the app crate, replace `mod repository` with `use agent_brain_repository as repository`
4. Run `cargo test` — all tests should pass

### 1.4 Verify

- [ ] `cargo build` succeeds
- [ ] `cargo test --lib` passes (unit tests)
- [ ] `cargo test --test '*'` passes (integration tests)
- [ ] `cargo clippy` clean
- [ ] Docker build works
- [ ] CI pipeline passes

---

## Phase 2: Break the MCP/Services Circular Dependency ✅ COMPLETE

**Risk:** Medium — requires moving types between modules
**Estimated scope:** ~300 LOC moved/changed
**Actual:** 483 LOC added to new crates/protocol/, 350 LOC changed in app; 107 tests passing

### 2.1 Extract `agent-brain-protocol` Crate

The circular dependency exists because:
- `services/queue.rs` imports `mcp::protocol::Content` and `mcp::tools::ToolHandler`
- `mcp/server.rs` imports services to build skills

The fix: extract protocol types into their own crate that both can depend on.

Create `crates/protocol/`:
```
crates/protocol/
├── Cargo.toml          # deps: serde, serde_json, schemars
└── src/
    ├── lib.rs
    ├── jsonrpc.rs       # JsonRpcRequest, JsonRpcResponse, JsonRpcError
    ├── messages.rs      # InitializeParams, InitializeResult, etc.
    ├── content.rs       # Content, TextContent, ImageContent, EmbeddedResource
    └── tools.rs         # ToolDefinition, ToolCallResult (data types only)
```

Move from `mcp/protocol.rs`:
- All JSON-RPC message types
- `Content` enum and variants
- `ToolDefinition` struct
- `ToolCallResult` struct

These are pure data types with no behavior — safe to extract.

### 2.2 Extract `ToolHandler` Trait

Currently `ToolHandler` is a concrete struct in `mcp/tools.rs`. Extract the trait:

```rust
// In crates/protocol/src/tools.rs
#[async_trait]
pub trait ToolHandler: Send + Sync {
    async fn call(&self, tool_name: &str, arguments: serde_json::Value) -> ToolCallResult;
    fn list_tools(&self) -> Vec<ToolDefinition>;
}
```

The concrete implementation stays in `mcp/tools.rs` but implements this trait.
Services depend on the trait (from the protocol crate), not the concrete type.

### 2.3 Update Import Paths

After extraction:
- `services/queue.rs`: `use agent_brain_protocol::{Content, ToolHandler}` (no MCP dependency)
- `mcp/server.rs`: `use agent_brain_protocol::*` + still imports services (one-way now)
- `skills/*.rs`: `use agent_brain_protocol::{ToolDefinition, ToolCallResult}`

Dependency graph becomes strictly acyclic:
```
protocol  <--  models
   ^             ^
   |             |
   +-- repository (+ models)
   |
   +-- services  (+ models, repository, protocol)
   |
   +-- skills    (+ models, repository, services, protocol)
   |
   +-- mcp/app   (+ everything above)
```

### 2.4 Move `Skill` Trait to Protocol Crate

The `Skill` trait definition (currently `skills/mod.rs`) should also live in
the protocol crate since it's the interface contract, not an implementation:

```rust
// In crates/protocol/src/skill.rs
#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &str;
    fn tools(&self) -> Vec<ToolDefinition>;
    async fn execute(&self, tool_name: &str, args: serde_json::Value) -> ToolCallResult;
}
```

### 2.5 Verify

- [ ] `cargo build` succeeds
- [ ] No module in `crates/` imports from `mcp/` except the app crate
- [ ] `services/` has zero imports from `mcp/`
- [ ] All tests pass

---

## Phase 3: Trait Abstractions for Storage and LLM ✅ COMPLETE

**Risk:** Medium — changes function signatures across many files
**Estimated scope:** ~500-800 LOC changed across services and skills
**Actual:** 777 LOC net change; 5 new traits, SharedLlm wrapper, 4 skills fully decoupled; 107 tests passing

### 3.1 Define Core Traits

Create `crates/protocol/src/traits.rs` (or a new `crates/core/` crate):

```rust
/// Storage abstraction — replaces direct Neo4jClient usage
#[async_trait]
pub trait KnowledgeStore: Send + Sync {
    async fn store_note(&self, content: &str, note_type: &str, ...) -> Result<String>;
    async fn search_notes(&self, query: &str, limit: usize) -> Result<Vec<Note>>;
    async fn get_note(&self, id: &str) -> Result<Option<Note>>;
    // ... other knowledge operations
}

#[async_trait]
pub trait TaskStore: Send + Sync {
    async fn create_task(&self, goal: &str, context: Option<&str>) -> Result<String>;
    async fn update_task(&self, id: &str, status: &str) -> Result<()>;
    async fn list_tasks(&self, status: Option<&str>, limit: usize) -> Result<Vec<Task>>;
    // ...
}

#[async_trait]
pub trait EndpointStore: Send + Sync {
    async fn query_endpoints(&self, query: &str) -> Result<Vec<Endpoint>>;
    async fn store_healing_event(&self, ...) -> Result<()>;
    // ...
}

/// LLM abstraction — replaces direct LlmClient/LlmConfig usage
#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn generate(&self, prompt: &str, system: Option<&str>) -> Result<String>;
    async fn embed(&self, text: &str) -> Result<Vec<f32>>;
    fn model_name(&self) -> &str;
}
```

### 3.2 Implement Traits on Existing Types

`Neo4jClient` already has the methods — wrap them:

```rust
// In crates/repository/src/knowledge_store.rs
impl KnowledgeStore for Neo4jClient {
    async fn store_note(&self, ...) -> Result<String> {
        // delegate to existing self.create_note(...)
    }
}
```

`LlmClient` already has `.generate()` and `.embed()` — implement `LlmProvider`.

### 3.3 Update Skill Signatures

Before:
```rust
pub struct KnowledgeSkill {
    neo4j: Neo4jClient,
    llm_config: Arc<RwLock<Option<LlmConfig>>>,
}
```

After:
```rust
pub struct KnowledgeSkill {
    store: Arc<dyn KnowledgeStore>,
    llm: Arc<dyn LlmProvider>,
}
```

### 3.4 Benefits

- Skills become testable with mock implementations
- Can swap Neo4j for another backend without touching skills
- Can swap LLM providers per-skill
- Eliminates the `Arc<RwLock<Option<LlmConfig>>>` pattern

### 3.5 Verify

- [ ] All skills accept trait objects instead of concrete types
- [ ] Unit tests can use mock implementations
- [ ] No skill directly imports `neo4rs` or `Neo4jClient`
- [ ] All tests pass

---

## Phase 4: Decompose McpServerCore

**Risk:** Medium-High — the core wiring changes
**Estimated scope:** ~400-600 LOC refactored in `mcp/server.rs`

### 4.1 Identify Responsibility Groups

Current `McpServerCore` fields fall into these groups:

| Group | Fields | Used By |
|-------|--------|---------|
| **Storage** | `neo4j`, `telemetry_client` | All skills |
| **LLM** | `llm_config` | Most skills |
| **API Management** | `context_store`, `credential_manager` | ApiSkill |
| **Job System** | `queue_service`, `scheduler_service` | AgentSkill, SchedulerSkill |
| **Search** | `serpapi_key`, `brave_api_key`, `google_api_key`, `google_cx` | SearchSkill |
| **Transport** | `session_manager` | HTTP transport only |
| **Tools** | `tool_registry`, `tool_handler` | MCP dispatch |
| **Config** | `dataset_dir` | SleepSkill |

### 4.2 Create Service Containers

Replace the god object with focused containers:

```rust
/// Injected into skills that need storage
pub struct StorageServices {
    pub knowledge: Arc<dyn KnowledgeStore>,
    pub tasks: Arc<dyn TaskStore>,
    pub endpoints: Arc<dyn EndpointStore>,
    pub telemetry: Option<TelemetryClient>,
}

/// Injected into skills that need LLM
pub struct LlmServices {
    pub provider: Arc<dyn LlmProvider>,
}

/// Injected into ApiSkill
pub struct ApiServices {
    pub context: Arc<ContextStore>,
    pub credentials: Arc<CredentialManager>,
}

/// Injected into AgentSkill / SchedulerSkill
pub struct JobServices {
    pub queue: Arc<QueueService>,
    pub scheduler: Arc<SchedulerService>,
}
```

### 4.3 Skill Construction with Builder

Replace the monolithic `build_skills()` with a builder:

```rust
impl McpServerCore {
    pub fn builder() -> McpServerBuilder { ... }
}

struct McpServerBuilder {
    storage: Option<StorageServices>,
    llm: Option<LlmServices>,
    api: Option<ApiServices>,
    jobs: Option<JobServices>,
    // ...
}

impl McpServerBuilder {
    pub fn with_storage(mut self, s: StorageServices) -> Self { ... }
    pub fn with_llm(mut self, l: LlmServices) -> Self { ... }
    pub fn build(self) -> Result<McpServerCore> { ... }
}
```

### 4.4 Fix Temporal Coupling

The current initialization requires:
1. Create queue with `None` tool handler
2. Build skills
3. Populate tool handler
4. Start queue coordinator

Fix: use a two-phase init or pass a `Lazy<ToolHandler>` that resolves on first use.

```rust
// Option A: Lazy initialization
pub struct LazyToolHandler {
    inner: Arc<OnceLock<ToolHandler>>,
}

impl LazyToolHandler {
    pub fn new() -> (Self, LazyToolHandlerSetter) { ... }
}

// Option B: Builder finalizes in correct order
impl McpServerBuilder {
    pub fn build(self) -> Result<McpServerCore> {
        // 1. Build tool handler from skills
        // 2. Create queue with populated handler
        // 3. Create scheduler with queue
        // 4. Return fully initialized core
    }
}
```

### 4.5 Verify

- [ ] `McpServerCore` has < 8 fields
- [ ] No skill receives more than 2 service containers
- [ ] `build_skills()` is replaced by builder pattern
- [ ] Temporal coupling is eliminated
- [ ] All tests pass

---

## Phase 5: Split Config

**Risk:** Low — straightforward struct decomposition
**Estimated scope:** ~200 LOC

### 5.1 Decompose Config Struct

Before (one flat struct):
```rust
pub struct Config {
    pub neo4j_uri: String,
    pub neo4j_user: String,
    pub neo4j_password: String,
    pub ollama_url: String,
    pub ollama_model: String,
    pub anthropic_api_key: Option<String>,
    pub gemini_api_key: Option<String>,
    pub secret_provider: String,
    pub vault_addr: Option<String>,
    pub aws_region: String,
    pub log_level: String,
    pub serpapi_key: Option<String>,
    // ... 20+ fields
}
```

After (domain-grouped):
```rust
pub struct Config {
    pub database: DatabaseConfig,
    pub llm: LlmConfig,
    pub secrets: SecretConfig,
    pub logging: LogConfig,
    pub transport: TransportConfig,
    pub search: SearchConfig,
    pub scheduler: SchedulerConfig,
}

pub struct DatabaseConfig {
    pub uri: String,
    pub user: String,
    pub password: String,
}

pub struct LlmConfig {
    pub provider: LlmProvider,
    pub ollama: Option<OllamaConfig>,
    pub anthropic: Option<AnthropicConfig>,
    pub gemini: Option<GeminiConfig>,
}
// ... etc.
```

### 5.2 Verify

- [ ] Each sub-config is independently validatable
- [ ] `Config::from_env()` still works
- [ ] All tests pass

---

## Phase 6: DuckDB + YAML for Lean Model Context

**Risk:** Medium — migrates model management from Neo4j to DuckDB + flat files
**Estimated scope:** ~600-800 LOC changed/added
**Prerequisite:** Phase 0 complete (cleaner codebase to work with)

### Problem

Model context is currently managed through a heavy stack:
1. `ModelSpec` nodes stored in **Neo4j** (a graph DB — overkill for flat config)
2. `register_model` tool requires manual MCP calls to define each model
3. System prompt is **hardcoded** in `services/chat.rs` line 21
4. No declarative way to define model catalogs or per-model prompts
5. LLM config lives in env vars, model specs in Neo4j, system prompts in
   source code — three different places for related concerns

### Solution: YAML Declaration + DuckDB Runtime

**YAML** for human-readable, git-versioned model definitions (source of truth).
**DuckDB** for fast runtime queries (already integrated for telemetry).

### 6.1 Create `models.yaml` Config File

```yaml
# models.yaml — Model catalog and context configuration
# This file is the single source of truth for model definitions.
# Loaded into DuckDB at startup for fast runtime queries.

defaults:
  temperature: 0.7
  max_tokens: 4096
  timeout_secs: 120

models:
  granite4:
    provider: ollama
    model: "granite4:latest"
    context_window: 8192
    cost_per_1k_input: 0.0        # local, no cost
    cost_per_1k_output: 0.0
    capabilities:
      - reasoning
      - code
    system_prompt: |
      You are agent-brain, an autonomous AI agent backed by a persistent
      Neo4j knowledge graph. You can search notes, manage tasks, reason
      over stored knowledge, and use many other tools. Think step-by-step
      before acting and use available tools for grounded answers.

  claude-sonnet:
    provider: anthropic
    model: "claude-sonnet-4-20250514"
    context_window: 200000
    cost_per_1k_input: 0.003
    cost_per_1k_output: 0.015
    capabilities:
      - reasoning
      - code
      - vision
      - fast
    temperature: 0.5
    system_prompt: |
      You are agent-brain running on Claude Sonnet. Prioritize efficiency
      and tool use. Be concise.

  gemini-flash:
    provider: gemini
    model: "gemini-2.0-flash"
    context_window: 1048576
    cost_per_1k_input: 0.0001
    cost_per_1k_output: 0.0004
    capabilities:
      - reasoning
      - code
      - fast
    system_prompt: null            # falls back to defaults.system_prompt

# Default system prompt used when a model doesn't define its own
default_system_prompt: |
  You are agent-brain, an autonomous AI agent backed by a persistent
  knowledge graph. Think step-by-step and use available tools.
```

### 6.2 Extend DuckDB Schema

Add a `model_registry` table alongside the existing `interactions` and
`knowledge_gaps` tables:

```sql
CREATE TABLE IF NOT EXISTS model_registry (
    name           TEXT PRIMARY KEY,
    provider       TEXT NOT NULL,       -- 'ollama', 'anthropic', 'gemini'
    model          TEXT NOT NULL,       -- actual model identifier
    context_window INTEGER NOT NULL,
    cost_input     DOUBLE NOT NULL,     -- per 1K tokens
    cost_output    DOUBLE NOT NULL,     -- per 1K tokens
    capabilities   TEXT NOT NULL,       -- JSON array: '["reasoning","code"]'
    system_prompt  TEXT,                -- per-model system prompt (nullable)
    temperature    DOUBLE,
    max_tokens     INTEGER,
    timeout_secs   INTEGER,
    loaded_at      TIMESTAMP DEFAULT current_timestamp
);

-- Usage tracking (replaces Neo4j-based get_model_stats)
CREATE TABLE IF NOT EXISTS model_usage (
    id             TEXT PRIMARY KEY,
    model_name     TEXT NOT NULL,
    tool_name      TEXT,
    success        BOOLEAN,
    duration_ms    INTEGER,
    tokens_in      INTEGER,
    tokens_out     INTEGER,
    cost           DOUBLE,             -- computed from registry rates
    created_at     TIMESTAMP DEFAULT current_timestamp
);
```

### 6.3 YAML Loader Service

New file: `src/services/model_config.rs`

```rust
use serde::Deserialize;
use std::path::Path;

#[derive(Debug, Deserialize)]
pub struct ModelCatalog {
    pub defaults: ModelDefaults,
    pub models: HashMap<String, ModelEntry>,
    pub default_system_prompt: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct ModelEntry {
    pub provider: String,
    pub model: String,
    pub context_window: u32,
    pub cost_per_1k_input: f64,
    pub cost_per_1k_output: f64,
    pub capabilities: Vec<String>,
    pub system_prompt: Option<String>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub timeout_secs: Option<u64>,
}

impl ModelCatalog {
    /// Load from YAML file, falling back to embedded defaults
    pub fn load(path: &Path) -> Result<Self> {
        let content = std::fs::read_to_string(path)?;
        Ok(serde_yaml::from_str(&content)?)
    }

    /// Sync all entries into DuckDB model_registry table
    pub fn sync_to_duckdb(&self, db: &TelemetryClient) -> Result<usize> {
        // TRUNCATE + INSERT (idempotent reload)
        // Returns count of models loaded
    }
}
```

### 6.4 Startup Flow

In `main.rs`, after DuckDB init and before MCP server start:

```rust
// 1. Load model catalog from YAML
let catalog_path = config.model_catalog_path  // env: MODEL_CATALOG_PATH
    .unwrap_or_else(|| "models.yaml".into());
let catalog = ModelCatalog::load(&catalog_path)?;

// 2. Sync to DuckDB for runtime queries
catalog.sync_to_duckdb(&telemetry_client)?;
info!("Loaded {} models from {}", catalog.models.len(), catalog_path);

// 3. Resolve active model's system prompt
let system_prompt = catalog.resolve_system_prompt(&config.active_model);
```

### 6.5 Update ModelSkill

**Remove:**
- `register_model` tool — models are now defined in YAML, not via MCP calls

**Keep (reimplemented against DuckDB):**
- `list_models` — queries `model_registry` table instead of Neo4j
- `select_model` — queries DuckDB with capability/cost filters
- `use_model` — switches active model, loads system prompt from DuckDB
- `get_model_stats` — queries `model_usage` table instead of AgentJob nodes

**Add:**
- `reload_models` — re-reads `models.yaml` and syncs to DuckDB (hot reload)

### 6.6 Update Chat Service

Replace the hardcoded system prompt:

```rust
// Before (services/chat.rs)
const CHAT_SYSTEM_PROMPT: &str = "You are agent-brain...";

// After
pub struct ChatService {
    system_prompt: String,  // loaded from DuckDB for active model
}

impl ChatService {
    pub fn with_system_prompt(prompt: String) -> Self { ... }
}
```

### 6.7 Remove ModelSpec from Neo4j

- Delete `src/models/model_spec.rs`
- Delete `src/repository/model_spec.rs`
- Remove ModelSpec constraint/index from `repository/client.rs` init_schema
- Remove `ModelSpec` node type from graph schema documentation

### 6.8 New Environment Variable

| Variable | Default | Description |
|----------|---------|-------------|
| `MODEL_CATALOG_PATH` | `models.yaml` | Path to YAML model catalog |

### 6.9 Why This Approach

| Concern | Before (Neo4j) | After (YAML + DuckDB) |
|---------|----------------|----------------------|
| **Define a model** | MCP tool call at runtime | Edit YAML, restart (or `reload_models`) |
| **Version control** | Not tracked | Git-versioned YAML |
| **Query models** | Cypher over Neo4j | SQL over DuckDB (faster, lighter) |
| **System prompts** | Hardcoded in source | Per-model in YAML |
| **Usage tracking** | Derived from AgentJob nodes | Dedicated `model_usage` table |
| **Add new model** | `register_model` MCP call | Add entry to YAML |
| **Cost tracking** | Manual | Auto-computed from registry rates |

### 6.10 Verify

- [ ] `models.yaml` loads and parses correctly
- [ ] `model_registry` table populated on startup
- [ ] `list_models` returns data from DuckDB
- [ ] `select_model` filters by capabilities and cost via SQL
- [ ] `use_model` switches active model and loads correct system prompt
- [ ] `get_model_stats` queries `model_usage` table
- [ ] `reload_models` hot-reloads YAML without restart
- [ ] No `ModelSpec` references remain in Neo4j code
- [ ] Chat service uses per-model system prompt
- [ ] All tests pass

---

## Phase 7: Optional Crate Extraction

**Risk:** Low — only do what adds value
**Prerequisite:** Phases 1-4 complete

These are optional and should only be done if there's a concrete need:

### 6.1 Extract `crates/llm/`

Move `services/llm.rs` + `services/llm_providers/` into a standalone crate.
Dependencies: `reqwest`, `serde`, `serde_json`, `tokio`, `tracing`.
Useful if you want to reuse the multi-provider LLM client in other projects.

### 6.2 Extract `crates/secrets/`

Move `services/secrets/` into a standalone crate.
Dependencies: `aes-gcm`, `rand`, `reqwest`, `aws-config`, `aws-sdk-secretsmanager`.
Useful to isolate the heavy AWS SDK dependency behind a feature flag.

### 6.3 Extract `crates/queue/`

Move `services/queue.rs` + `services/scheduler.rs` into a standalone crate.
Depends on: `protocol` (for `ToolHandler` trait), `repository` (for job persistence).

### 6.4 Feature-Flag Heavy Dependencies

Some dependencies are only needed for specific features:

```toml
[features]
default = ["ollama", "local-secrets"]
ollama = []
anthropic = ["dep:reqwest"]
gemini = ["dep:reqwest"]
vault = ["dep:reqwest"]
aws = ["dep:aws-config", "dep:aws-sdk-secretsmanager"]
http-transport = ["dep:axum", "dep:axum-extra", "dep:tower", "dep:tower-http"]
telemetry = ["dep:duckdb"]
```

This would significantly reduce compile times for developers who only need
a subset of features.

---

## Phase Ordering and Dependencies

```
Phase 0 (shed agent-api) ───┐
                             ├── Phase 1 ─── Phase 2 ─── Phase 3 ─── Phase 4
                             │                              │
Phase 5 (split config) ─────┘                              └── Phase 7 (optional)
                             │
Phase 6 (YAML+DuckDB) ──────┘
```

**Phase 0 is the recommended starting point.** It removes 11K LOC of legacy code,
making every subsequent phase smaller and simpler. Phases 5 and 6 are independent
of the workspace/abstraction track and can run whenever convenient.

| Phase | What | Depends On | Breaks Tests? | LOC Changed |
|-------|------|-----------|---------------|-------------|
| **0** | Shed agent-api identity | Nothing | Yes (tests deleted too) | ~11K removed, ~200 changed |
| **1** | Cargo workspace + extract models/repository | Phase 0 | No | ~200 |
| **2** | Extract protocol crate, break circular dep | Phase 1 | No | ~300 |
| **3** | Trait abstractions (storage, LLM) | Phase 2 | Temporarily | ~500-800 |
| **4** | Decompose McpServerCore | Phase 3 | Temporarily | ~400-600 |
| **5** | Split Config struct | Nothing | No | ~200 |
| **6** | YAML + DuckDB model context | Phase 0 | Temporarily | ~600-800 |
| **7** | Optional crate extraction | Phase 4 | No | Varies |

---

## What NOT To Do

- **Don't split into microservices.** This is one logical application. Multiple
  binaries communicating over the network would add complexity for zero benefit.

- **Don't extract skills into separate crates yet.** They share too much state
  via the service containers. Fix the abstractions first (Phase 3), then skills
  naturally become extractable if needed.

- **Don't rewrite.** After Phase 0, the remaining ~19K lines are focused and
  working. Rewriting would just rebuild the same logic from scratch.

- **Don't do all phases at once.** Each phase is independently shippable. Ship
  Phase 0, validate, then move to Phase 1. If you stop after Phase 2, you've
  still made meaningful progress.

- **Don't keep API code "just in case."** The agent-api functionality can be
  recovered from git history if ever needed. Dead code is worse than no code —
  it misleads contributors and bloats compile times.

- **Don't store model config in Neo4j.** Graph databases are for relationships.
  Model specs are flat config — YAML + DuckDB is the right tool for the job.

---

## Success Criteria

After all phases are complete:

1. **Identity:** The codebase is 100% agent-brain. No API management remnants,
   no "agent-api" references. The project description, tools, and CLI all
   reflect an autonomous knowledge-graph agent.

2. **Compilation:** Each crate compiles independently. Changing `models` doesn't
   recompile `mcp`. Changing a skill doesn't recompile `repository`.

3. **Testing:** Skills can be unit-tested with mock storage and mock LLM
   providers. No Neo4j needed for skill-level tests.

4. **Readability:** A new contributor can understand a skill by reading just
   that skill file + the trait definitions. They don't need to understand
   `McpServerCore` or the initialization order.

5. **Extensibility:** Adding a new LLM provider means implementing one trait.
   Adding a new storage backend means implementing three traits. Adding a new
   model means adding an entry to `models.yaml`. None require touching skills
   or MCP code.

6. **Build times:** Incremental builds only recompile the crate that changed
   and its dependents, not the entire ~19K LOC codebase.

7. **Configuration:** Models are declared in git-versioned YAML, queryable
   via DuckDB at runtime. System prompts are per-model and configurable.
   No config lives in source code constants.
