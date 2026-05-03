# CLAUDE.md

Guidance for working with the Agent Brain codebase.
This repository is forked from the original [Agent Api]("https://github.com/arabenjamin/agent-api") and has diverged significantly in architecture, design, and implementation. Though there are still some leftovers from the original codebase, the majority of the code has been rewritten to support a persistent, self-improving autonomous agent brain with a Neo4j knowledge graph and a pluggable LLM backend.

## Project Overview

Autonomous Agent Brain — A persistent, self-improving MCP server in Rust backed by a Neo4j knowledge graph. Manages long-term memory with hybrid vector+BM25 RAG, executes background jobs in a durable priority queue, reasons over stored knowledge, and runs an autonomous background scheduler that continuously improves itself by dispatching pending tasks as job chains.

## Tech Stack

- **Language:** Rust (Tokio async runtime, Edition 2024)
- **Protocol:** Model Context Protocol (MCP) via stdio or HTTP transport
- **Web Framework:** Axum (HTTP transport with SSE streaming)
- **Database:** Neo4j via `neo4rs` driver
- **AI Model:** Pluggable — Ollama (local), Ollama Cloud, Anthropic, or Gemini

## Build Commands

```bash
cargo build                    # Build the workspace
cargo build --release          # Build optimized release
cargo fmt                      # Format code
cargo clippy                   # Run linter
```

## Test Commands

```bash
cargo test --lib               # Unit tests only (all crates)
cargo test --test '*'          # Integration tests only (requires Neo4j)
cargo test                     # All tests
cargo test -- --nocapture      # Show println output
```

## CLI Commands

```bash
# Run as MCP server (default - stdio transport)
cargo run -- serve
cargo run                      # Same as above

# Run as MCP server with HTTP transport
cargo run -- serve --transport http                           # HTTP on localhost:3000
cargo run -- serve --transport http --bind 0.0.0.0:8080       # Custom bind address
cargo run -- serve --transport http --api-key my-secret-key   # With API key auth

# Initialize database schema
cargo run -- init-db
```

## Environment Variables

Copy `.env.example` to `.env` and configure:

| Variable | Default | Description |
|----------|---------|-------------|
| `NEO4J_URI` | `bolt://localhost:7688` | Neo4j connection URI |
| `NEO4J_USER` | `neo4j` | Neo4j username |
| `NEO4J_PASSWORD` | *required* | Neo4j password |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama API endpoint. Set to `https://ollama.com` for Ollama Cloud |
| `OLLAMA_LOCAL_URL` | `http://localhost:11434` | Local Ollama endpoint. Background scheduler jobs with `provider_hint="ollama"` always use this, never the cloud URL |
| `OLLAMA_MODEL` | `qwen3.5:4b` | LLM model to use for text generation |
| `OLLAMA_EMBED_MODEL` | - | Ollama model for embeddings (e.g. `bge-m3:latest`). Falls back to `OLLAMA_MODEL` if unset |
| `OLLAMA_API_KEY` | - | API key for Ollama Cloud authentication. Get one at `ollama.com/settings/keys` |
| `OLLAMA_LOCAL_MODEL` | `gemma4:latest` | Model used exclusively for all background/scheduled jobs. Always routes to `OLLAMA_LOCAL_URL` — never touches cloud quota |
| `LOG_LEVEL` | `info` | Log level (trace/debug/info/warn/error) |
| `LOG_FORMAT` | `pretty` | Log format (pretty/json) |
| `MCP_TRANSPORT` | `stdio` | MCP transport type (stdio/http) |
| `MCP_HTTP_BIND` | `127.0.0.1:3000` | HTTP bind address (for http transport) |
| `MCP_API_KEY` | - | API key for HTTP transport authentication |
| `SECRET_PROVIDER` | `local` | Secret provider (local/vault/aws/none) |
| `SECRETS_FILE` | `.secrets.enc` | Path to encrypted secrets file (local provider) |
| `SECRETS_ENCRYPTION_KEY` | - | Encryption key for local secrets (required for production) |
| `VAULT_ADDR` | - | HashiCorp Vault server address |
| `VAULT_TOKEN` | - | Vault authentication token |
| `VAULT_MOUNT_PATH` | `secret` | Vault KV mount path |
| `VAULT_NAMESPACE` | - | Vault namespace (enterprise only) |
| `AWS_REGION` | `us-east-1` | AWS region for Secrets Manager |
| `AWS_SECRET_PREFIX` | - | Prefix for AWS secret names |
| `DATASET_DIR` | `./datasets` | Directory for training data export (`digest_experiences`) |
| `TELEMETRY_DB_PATH` | - | Path to DuckDB file for interaction logging (enables `SleepSkill`) |
| `SERPAPI_KEY` | - | SerpApi key for `search_web` tool |
| `BRAVE_API_KEY` | - | Brave Search API key for `search_web` tool |
| `GOOGLE_API_KEY` | - | Google Custom Search API key for `search_web` tool |
| `GOOGLE_CX` | - | Google Custom Search Engine ID for `search_web` tool |
| `SCHEDULER_INTERVAL_SECS` | `300` | How often the scheduler polls for pending tasks (seconds) |
| `SCHEDULER_ENABLED` | `true` | Set to `false` to start with the autonomous scheduler disabled |
| `CODEBASE_DIR` | auto-detected | Root of the codebase for `CodebaseSkill`. Auto-detected by walking up from cwd until `Cargo.toml` is found |
| `WORKSPACE_DIR` | - | Writable workspace directory for generated code, scripts, and experiments. Enables `write_workspace_file` and `list_workspace_files` tools. Injected into Chat Agent system prompt. |
| `GITHUB_TOKEN` | - | GitHub personal access token. Read by the seeded `github` `ApiContext` and auto-injected into `http_request` calls with `context_name="github"` |
| `CHAT_LLM_PROVIDER` | *(same as brain)* | Override the LLM provider for human-facing `/chat` sessions. Accepted values: `ollama`, `ollama-cloud`, `anthropic`, `gemini`. When unset, chat uses the same provider as the brain. |
| `CHAT_LLM_MODEL` | *(same as brain)* | Override the model name for chat (e.g. `claude-opus-4-5`). When unset, chat uses the brain's model. |
| `CHAT_API_KEY` | *(same as brain)* | Override the API key used by the chat LLM. When unset, inherits the brain's key. |
| `CHAT_LLM_BASE_URL` | *(same as brain)* | Override the base URL for the chat LLM endpoint. |

## Local Development

```bash
docker compose up -d       # Start Neo4j and Ollama
cargo run -- init-db       # Initialize schema
cargo run                  # Run MCP server (stdio)
```

## Docker Deployment (HTTP Transport)

```bash
# Build and start all services (Neo4j + MCP Server)
docker compose up -d --build

# With API key authentication
MCP_API_KEY=your-secret-key docker compose up -d --build

# View logs
docker compose logs -f agent-brain

# Health check
curl http://localhost:3000/health
```

**Endpoints:**
- `POST http://localhost:3000/mcp` - JSON-RPC requests
- `GET http://localhost:3000/mcp` - SSE stream
- `GET http://localhost:3000/health` - Health check


## Project Structure

This is a Cargo workspace with four crates:

```
agent-brain/
├── Cargo.toml                    # [workspace] root
├── crates/
│   ├── protocol/                 # agent-brain-protocol: shared MCP types + traits
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── types.rs          # Content, ToolDefinition, ToolCallResult, JSON-RPC types
│   │       ├── skill.rs          # Skill trait
│   │       ├── sse_notifier.rs   # SseNotifier trait (SessionManager implements it)
│   │       └── tool_handler.rs   # ToolHandlerTrait (ToolHandler implements it)
│   ├── models/                   # agent-brain-models: pure data types
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── agent_job.rs      # AgentJob, AgentJobStatus, PrioritizedJob
│   │       ├── model_spec.rs     # ModelSpec
│   │       ├── procedure.rs      # Procedure
│   │       └── task.rs           # Task, TaskStatus
│   ├── repository/               # agent-brain-repository: Neo4j layer
│   │   └── src/
│   │       ├── lib.rs
│   │       ├── client.rs         # Neo4jClient, init_schema
│   │       ├── error.rs          # RepositoryError
│   │       ├── agent_job.rs      # AgentJob CRUD + chain unpark/cancel
│   │       ├── model_spec.rs     # ModelSpec CRUD (upsert by name, usage stats)
│   │       ├── task.rs           # Task CRUD + dependency tracking
│   │       └── telemetry.rs      # TelemetryClient (DuckDB interaction logging)
│   └── app/                      # agent-brain: application
│       ├── src/
│       │   ├── lib.rs            # Library exports (re-exports models + repository)
│       │   ├── main.rs           # CLI entry point
│       │   ├── brain_core.rs     # BrainCore — brain engine (storage, LLM, skills, scheduler)
│       │   ├── cli.rs            # Clap CLI definitions
│       │   ├── config.rs         # Environment configuration (incl. ChatLlmConfig)
│       │   ├── logging.rs        # Tracing setup
│       │   ├── models/           # Re-exported from agent-brain-models
│       │   ├── repository/       # Re-exported from agent-brain-repository
│       │   ├── clients/          # Client adapters (translate client protocols → BrainCore)
│       │   │   └── chat.rs       # ChatService — conversational LLM loop for /chat SSE
│       │   ├── services/         # Brain-internal business logic
│       │   │   ├── knowledge.rs  # Notes/RAG (vector+BM25, entity extraction, spaced rep)
│       │   │   ├── llm.rs        # Multi-provider LLM client (Ollama/Anthropic/Gemini)
│       │   │   ├── model_selector.rs  # Capability filter + cheapest-first model selection
│       │   │   ├── procedure_executor.rs  # Template-substitution procedure step runner
│       │   │   ├── queue.rs      # Priority job queue + coordinator (AgentJob execution)
│       │   │   ├── scheduler.rs  # Autonomous scheduler (self-improvement loop)
│       │   │   ├── sleep.rs      # Experience digestion and training data export
│       │   │   ├── context_builder.rs  # Context profiles (YAML) + boot/init protocols
│       │   │   └── secrets/      # SecretProvider (local AES-GCM / Vault / AWS)
│       │   ├── skills/           # Pluggable MCP skill implementations
│       │   │   ├── mod.rs        # Skill trait definition
│       │   │   ├── agent.rs      # Agent Job Queue skill (8 tools)
│       │   │   ├── dynamic.rs    # Dynamic Tool Builder skill (4 tools + runtime tools)
│       │   │   ├── knowledge.rs  # Knowledge Manager skill (16 tools)
│       │   │   ├── model.rs      # Model Registry skill (5 tools)
│       │   │   ├── procedure.rs  # Procedural Memory skill (2 tools)
│       │   │   ├── scheduler.rs  # Autonomous Scheduler skill (5 tools)
│       │   │   ├── search.rs     # Web Search skill (1 tool)
│       │   │   ├── sleep.rs      # Sleep / Telemetry skill (2 tools)
│       │   │   ├── task.rs       # Task Manager skill (6 tools)
│       │   │   └── working_memory.rs  # Working Memory skill (4 tools)
│       │   └── mcp/              # MCP protocol adapter
│       │       ├── protocol.rs   # Re-export facade (pub use agent_brain_protocol::*)
│       │       ├── transport.rs  # Async stdio transport
│       │       ├── transport_trait.rs  # McpTransport trait abstraction
│       │       ├── http_transport.rs   # Axum-based HTTP+SSE transport
│       │       ├── session.rs    # HTTP session management
│       │       ├── auth.rs       # API key authentication
│       │       ├── tools.rs      # Tool registry (skill-based dispatch)
│       │       └── server.rs     # McpServerCore: MCP adapter + wires brain → chat
│       └── tests/
│           ├── common/mod.rs     # Test utilities
│           ├── http_transport_test.rs  # HTTP transport infrastructure tests
│           └── task_test.rs      # Task model and repository tests
```

## Architecture Summary

See `project-docs/architecture_context.md` for skill registry table, initialization order, and mechanics. See `project-docs/STATUS.md` for current tool counts and feature status.

**Nodes:**
- `Task` - High-level goals with `id`, `goal`, `context`, `success_criteria` (measurable definition of done — used by evaluator step), `status` (created/in_progress/completed/failed/blocked)
- `Note` - Stored text memories with optional vector `embedding`, `access_count`, `last_accessed_at`, `note_type` (`semantic`/`episodic`/`reflection`/`consolidated`/`outcome`/`inference`), `next_review_at`, `review_interval_days`, `source_context`, `event_at`
- `Procedure` - Named multi-step workflows with `id`, `name`, `description`, `steps` (JSON array), `created_at`
- `WorkingMemory` - Session-scoped scratchpad entries with `id`, `session_id`, `content`, `role`, `turn_index`, `created_at`
- `Entity` - Named entities extracted from notes with `id`, `name` (unique, lowercased), `entity_type`, `created_at`
- `DynamicTool` - Runtime-defined MCP tools with `id`, `name` (unique), `description`, `input_schema` (JSON), `created_at`
- `AgentJob` - Background job record with `id`, `tool_name`, `args_json`, `priority` (0-3), `status` (queued/running/completed/failed/dead/parked/cancelled), `attempt_count`, `max_attempts`, `result_json`, `error`, timestamps, `session_id`, `parent_job_id`
- `ModelSpec` - Registered LLM models with capabilities, cost, and usage stats

**Relationships:**
- `(:Note)-[:RELATES_TO {similarity: float}]->(:Note)` — auto-created when similarity ≥ 0.75
- `(:Note)-[:SUMMARIZED_BY]->(:Note)` — source notes pointing to their consolidated summary
- `(:Note)-[:REFLECTS_ON]->(:Task)` — reflection/outcome notes linked to the task they critique
- `(:Note)-[:PART_OF]->(:Note)` — semantic chunk linked to its parent note
- `(:Note)-[:MENTIONS {count}]->(:Entity)` — entity mentions extracted from note content
- `(:Note {note_type:'inference'})-[:DERIVED_FROM]->(:Note)` — inference notes citing their sources
- `(:Task)-[:SUBTASK_OF]->(:Task)` — sub-tasks created by `decompose_goal`
- `(:Task)-[:DEPENDS_ON]->(:Task)` — dependency edges for task ordering
- `(:DynamicTool)-[:USES]->(:Procedure)` — links a dynamic tool to its step definition

**Stdio Transport (Default)**
- Standard input/output for local CLI usage
- Best for MCP clients like Claude Desktop that spawn the server as subprocess

**HTTP Transport**
- Streamable HTTP with Server-Sent Events (SSE) per MCP specification
- POST `/mcp` - JSON-RPC requests, returns JSON or SSE stream
- GET `/mcp` - SSE stream for server-initiated messages
- DELETE `/mcp` - Terminate session
- GET `/health` - Health check endpoint
- Optional API key authentication via Bearer token

```
                         CLI (main.rs)
                              │
               ┌──────────────┴──────────────┐
               │                             │
     ┌─────────▼─────────┐         ┌─────────▼─────────┐
     │  StdioTransport   │         │   HttpTransport   │
     │    (stdio)        │         │   (Axum + SSE)    │
     └─────────┬─────────┘         └─────────┬─────────┘
               │                             │
               └──────────────┬──────────────┘
                              │
     ┌────────────────────────▼────────────────────────┐
     │         McpServerCore  (MCP adapter)            │
     │  MCP JSON-RPC state machine + session manager   │
     │  chat_llm_config (optional, separate from brain)│
     └──────────┬──────────────────────┬───────────────┘
                │ brain:               │ ChatService
                │ BrainCore            │ (clients/chat.rs)
     ┌──────────▼──────────┐  ┌────────▼──────────────┐
     │      BrainCore      │  │     ChatService        │
     │  (brain_core.rs)    │  │  Conversational LLM    │
     │  storage + LLM +    │  │  loop for /chat SSE    │
     │  skill registry +   │  │  (own LLM config)      │
     │  scheduler + queue  │  └────────────────────────┘
     └─────────────────────┘
           │ Skills
     ┌─────▼──────────────────────────────────────────┐
     │    Skill Registry (~85 static + N runtime)     │
     │  KnowledgeSkill  TaskSkill  AgentSkill          │
     │  WorkingMemorySkill  DynamicSkill  ModelSkill   │
     │  SleepSkill  ProcedureSkill  SearchSkill        │
     │  SchedulerSkill  CodebaseSkill  ...             │
     └────────────────────────────────────────────────┘
```

### Self-Improvement Loop

The `SchedulerService` runs a background Tokio task that:
1. Lists Tasks with `status=created`
2. Maps each goal to a chain of tool calls via `goal_to_steps()`
3. Enqueues chains via `QueueService` (priority job queue)
4. Marks tasks `in_progress`
5. After 3 idle ticks (no new tasks dispatched), enters sleep mode: consolidates memories, prunes stale notes, takes a knowledge snapshot

The `QueueService` coordinator runs jobs serially per provider (Ollama/Anthropic/Gemini semaphores), retrying on transient failures, and unparks dependent jobs on success.

### Evaluator Loop (Generator-Evaluator Pattern)

Inspired by the Anthropic harness design article. When a `Task` has a `success_criteria` field set, `goal_to_steps()` automatically appends a `reflect_on_work` evaluator step to the chain. The evaluator step:

1. Calls `reflect_on_work` with the previous step's output as `current_state`
2. `reflect_on_work` outputs a `Score: N/5` line the coordinator parses
3. If score < `min_score` (default 3.5), the coordinator marks the original task `failed` and creates a new `Task` with the critique injected into `context`, so the scheduler re-dispatches it on the next tick
4. If score passes, the chain continues normally

`ChainStep` evaluator fields: `is_evaluator: bool`, `min_score: Option<f32>`, `evaluator_task_id: Option<String>`. Evaluator metadata is embedded in the job's `args_json` as `__evaluator_min_score` and `__evaluator_task_id` (serde ignores them in the tool handler).

`(:SchedulerChain)` nodes can carry an `evaluation_rubric` property that overrides `success_criteria` as the evaluator goal text — useful for custom chain-specific grading criteria.

### Context Profiles

YAML profiles in `contexts/` (default `./contexts`) define tool allowlists and system prompts for different agent personas. `boot.yaml` runs every startup; `init.yaml` runs when the graph is empty. The `ContextBuilderService` loads profiles and supports `auto_assign(goal)` keyword-matching to pick the best profile.

## TODO / Planned Features

See `project-docs/REFACTOR_PLAN.md` for the ongoing structural refactoring roadmap.

- [x] Phase 2: Break MCP/Services circular dependency (extract `agent-brain-protocol` crate)
- [x] Phase 3: Trait abstractions for Storage and LLM (KnowledgeStore, TaskStore, LlmProvider)
- [x] Phase 4: Decompose McpServerCore god object (service containers + builder pattern)
- [x] Phase 5: Split Config struct (DatabaseConfig, LlmProviderConfig, SecretsConfig, etc.)
- [x] Phase 6: DuckDB + YAML model catalog (`models.yaml` → DuckDB sync, ModelSpec removed from Neo4j)
- [x] Phase 7 (7.4): Feature flags — `aws`, `http-transport`, `telemetry`, `websocket` (all on by default)

## Critical Dev Notes

**LlmConfig:** `base_url` is `Option<String>`. Default model: `"qwen3.5:4b"`. Tests: `config.base_url.as_deref()`.

**Skill registration:** Register to BOTH `tool_registry` (for `tools/list`) AND `skills` vec (for `tools/call`). Forgetting either causes invisible tools or dispatch failures.

**`McpServer`** is a thin backward-compatible wrapper around `McpServerCore` (stdio path only).

**HTTP session init:** Always send `notifications/initialized` AFTER `initialize`, or the server stays in `Initializing` state and rejects all tool calls.

**Initialization order:** `SchedulerService::new()` must be called AFTER `QueueService::new()`. `QueueService::spawn_coordinator()` must be called AFTER the tool handler is set (end of `build_skills`).

**Consolidation loop:** Uses `[Memory N]` labels (not `Note N:`), instructs LLM not to echo them. Auto-generated consolidation topics use `"recent experiences and knowledge"`. Source notes get `next_review_at = now + 30 days` after consolidation.

**`services/mod.rs`:** Must re-export `LlmProviderType`: `pub use llm::{LlmClient, LlmConfig, LlmProviderType};`

**Context Profiles:** YAML files in `contexts/` (CONTEXTS_DIR env var). Loaded by `ContextBuilderService::load_profiles()` in `build_skills()`. Boot protocol (`contexts/boot.yaml`) runs after each `build_skills()`. Init protocol (`contexts/init.yaml`) runs on empty graph. `ContextSkill` registered when `context_builder_arc` is Some.

## Branch Strategy
DO NOT REMOVE THIS LINE:Never write in credidation to LLMs or coding agents or assistants.

- `feature/*` - Feature branches (no CI)
- `dev` - Development (format + unit tests)
- `test` - Testing (full pipeline with integration tests)
- `prod` - Production (full pipeline + Docker build)
- Update the documentation first, the README, claude, plan, markdowns should reflect our changes.
