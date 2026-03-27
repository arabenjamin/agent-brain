# Refactoring Plan: Monolith to Workspace

Status as of 2026-03-27. This plan addresses structural/architectural issues
in the codebase. It is independent of feature work tracked in `ROADMAP.md`.

**Decision: Incremental refactor, not a rewrite.**

The codebase is ~30K LOC of working, tested Rust. The problems are structural
(god objects, tight coupling, shared mutable state) — not fundamental design
flaws. Every phase below preserves existing functionality and tests.

---

## Current Problems

| # | Problem | Severity | Where |
|---|---------|----------|-------|
| 1 | `McpServerCore` is a god object (15+ fields, all responsibilities) | HIGH | `mcp/server.rs` |
| 2 | `Config` is a catch-all (Neo4j + 3 LLM providers + 3 secret backends + logging + search keys) | MEDIUM | `config.rs` |
| 3 | MCP and Services have a circular import (services use `mcp::protocol::Content`, MCP uses services) | HIGH | `services/queue.rs`, `mcp/server.rs` |
| 4 | No trait abstractions — `Neo4jClient` and `LlmConfig` passed as concrete types everywhere | MEDIUM | All skills, all services |
| 5 | `Arc<RwLock<Option<LlmConfig>>>` shared across all skills — no isolation | MEDIUM | `mcp/server.rs`, all skills |
| 6 | `build_skills()` has fragile order-dependent initialization | HIGH | `mcp/server.rs:253-471` |
| 7 | `DynamicSkill` shares mutable `HashMap` via `Arc<RwLock>` with tool handler | MEDIUM | `skills/dynamic.rs` |
| 8 | Single crate with 37 runtime dependencies — slow compilation, no modularity | LOW | `Cargo.toml` |

---

## Phase 1: Cargo Workspace + Extract Pure Crates

**Risk:** Low — mechanical extraction, no logic changes
**Estimated scope:** ~200 LOC of `Cargo.toml` / `mod.rs` changes, zero logic changes

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

## Phase 2: Break the MCP/Services Circular Dependency

**Risk:** Medium — requires moving types between modules
**Estimated scope:** ~300 LOC moved/changed

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

## Phase 3: Trait Abstractions for Storage and LLM

**Risk:** Medium — changes function signatures across many files
**Estimated scope:** ~500-800 LOC changed across services and skills

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

## Phase 6: Optional Further Extraction

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
Phase 1 ─── Phase 2 ─── Phase 3 ─── Phase 4
  │                        │
  │                        └── Phase 5 (can run in parallel with 4)
  │
  └── Phase 6 (after 4 is done, pick what you need)
```

| Phase | Depends On | Can Break Existing Tests? | Estimated LOC Changed |
|-------|-----------|--------------------------|----------------------|
| 1 | Nothing | No | ~200 (Cargo.toml / mod.rs only) |
| 2 | Phase 1 | No (type moves only) | ~300 |
| 3 | Phase 2 | Temporarily (signature changes) | ~500-800 |
| 4 | Phase 3 | Temporarily (wiring changes) | ~400-600 |
| 5 | Nothing (can run any time) | No | ~200 |
| 6 | Phase 4 | No | Varies |

---

## What NOT To Do

- **Don't split into microservices.** This is one logical application. Multiple
  binaries communicating over the network would add complexity for zero benefit.

- **Don't extract skills into separate crates yet.** They share too much state
  via the service containers. Fix the abstractions first (Phase 3), then skills
  naturally become extractable if needed.

- **Don't rewrite.** You'd rebuild the same 30K lines, making the same decisions,
  and lose all the battle-tested edge-case handling in the existing code.

- **Don't do all phases at once.** Each phase is independently shippable. Ship
  Phase 1, validate, then move to Phase 2. If you stop after Phase 2, you've
  still made meaningful progress.

---

## Success Criteria

After all phases are complete:

1. **Compilation:** Each crate compiles independently. Changing `models` doesn't
   recompile `mcp`. Changing a skill doesn't recompile `repository`.

2. **Testing:** Skills can be unit-tested with mock storage and mock LLM
   providers. No Neo4j needed for skill-level tests.

3. **Readability:** A new contributor can understand a skill by reading just
   that skill file + the trait definitions. They don't need to understand
   `McpServerCore` or the initialization order.

4. **Extensibility:** Adding a new LLM provider means implementing one trait.
   Adding a new storage backend means implementing three traits. Neither
   requires touching skills or MCP code.

5. **Build times:** Incremental builds only recompile the crate that changed
   and its dependents, not the entire 30K LOC monolith.
