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
| `CLOUD_TIER` | `1` | Cloud autonomy tier for per-step model routing. `0` = local Ollama only; `1` = local + $0-cost Ollama Cloud models (needs `OLLAMA_API_KEY`); `2` = any provider with a configured key ("income mode") |
| `SCHEDULER_INTERVAL_SECS` | `300` | How often the scheduler polls for pending tasks (seconds) |
| `SCHEDULER_ENABLED` | `true` | Set to `false` to start with the autonomous scheduler disabled |
| `CHAINS_DIR` | `./chains` | Directory containing `*.yaml` SchedulerChain definitions. Seeded by `init-db` and force-refreshed on the first scheduler tick after every startup (YAML edits propagate on restart) |
| `SCHEDULES_DIR` | `./schedules` | Directory containing `*.yaml` ScheduledTask definitions. Seeded by `init-db` and on every startup. A missing/unreadable directory is a **fatal startup error**. See "ScheduledTask ownership" below |
| `SOURCES_DIR` | `./sources` | Directory containing `*.yaml` SourceList definitions (approved-domain lists for `search_web`). Seeded **ON CREATE only** — the graph owns each list after first creation; runtime edits via `neo4j_query` persist across restarts. Delete a node to re-seed it from YAML. Missing directory is non-fatal |
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
│       │   │   ├── constructor.rs # Agent Constructor skill (construct_agent)
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
- `ToolDef` / `ContextProfile` / `ModelDef` - **Self-model meta-graph** (Phase 0b of the Agent Constructor plan). Generated by introspection in `services/self_model.rs` at the end of every `build_skills()` — never hand-edit: the tool registry, `contexts/*.yaml`, and the DuckDB model catalog are the sources of truth, and stale nodes are deleted on each sync. `(:ContextProfile)-[:ALLOWS]->(:ToolDef)` edges mirror profile tool allowlists (`allows_all: true` when the profile has no allowlist). Chains/schedules are not duplicated here — `(:SchedulerChain)`/`(:ScheduledTask)` already are the graph representation.

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
- `(:AgentSpec)-[:CONSTRUCTED_FOR]->(:Task)` — a constructed agent linked to the task it was dispatched for
- `(:AgentSpec)-[:PERFORMED {score, passed, at}]->(:Task)` — graded outcome written by the queue's evaluator hook; the constructor's reuse-before-construct prefers specs with avg score ≥ 3.5

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

**Dead-job meta-learning:** when a job exhausts its retries, the coordinator stores a reflection note and — for non-infrastructure tools (`should_meta_learn`) — enqueues an Analyze→Hypothesize→Test→Integrate chain. Two guards keep this from burning cycles: `is_transient_infra_error` skips the chain for quota/rate-limit/timeout/5xx errors the brain can't fix (e.g. SerpApi 429), and `recently_meta_learned` dedupes to at most once per tool per 24h.

### Evaluator Loop (Generator-Evaluator Pattern)

Inspired by the Anthropic harness design article. When a `Task` has a `success_criteria` field set, `goal_to_steps()` automatically appends a `reflect_on_work` evaluator step to the chain. The evaluator step:

1. Calls `reflect_on_work` with the previous step's output as `current_state`
2. `reflect_on_work` outputs a `Score: N/5` line the coordinator parses
3. If score < `min_score` (default 3.5), the coordinator marks the original task `failed` and creates a new `Task` with the critique injected into `context`, so the scheduler re-dispatches it on the next tick
4. If score passes, the chain continues normally
5. **Retry cap:** `handle_evaluator_requeue` counts `"RETRY —"` occurrences already in the task context. If >= 3, it marks the task as terminal failure and stops re-queuing to prevent infinite loops.
6. **Unparseable output = pass:** `parse_evaluator_score` returns `Option<f32>` — `None` when the output has no `Score: N/5` line *and* no verdict keyword (`FULLY/PARTIALLY/NOT MET`). The coordinator treats `None` as a pass and does **not** grade the AgentSpec. Previously it fabricated a 3.0, which sits below the default 3.5 threshold and failed the task on mere format drift from the local model, burning the full retry budget.

`ChainStep` evaluator fields: `is_evaluator: bool`, `min_score: Option<f32>`, `evaluator_task_id: Option<String>`. Evaluator metadata is embedded in the job's `args_json` as `__evaluator_min_score` and `__evaluator_task_id` (serde ignores them in the tool handler).

`(:SchedulerChain)` nodes can carry an `evaluation_rubric` property that overrides `success_criteria` as the evaluator goal text — useful for custom chain-specific grading criteria.

### Adversarial Critic (Pre-flight Plan Review)

When a `Task` has `success_criteria`, `goal_to_steps()` also **prepends** an `adversarial_plan_review` step to the chain (before the main action steps). This is the "Level 5 Resilient Agent" upgrade: the plan is stress-tested before any real work happens.

The adversarial step:
1. Calls `adversarial_plan_review` with the goal + a description of the planned steps + N hypotheses (default 3)
2. The LLM generates N failure scenarios, scores the plan's robustness per scenario (1–5), and returns `overall_robustness`
3. If `overall_robustness < min_robustness` (default 2.5/5), the coordinator cancels all downstream steps and calls `handle_adversarial_requeue()`: marks the task `failed`, creates a new `Task` with the adversarial critique injected as context
4. If robustness passes, the adversarial result (including `adjusted_plan_notes`) flows into the next step via `{{_prev}}`
5. **Abort cap:** `handle_adversarial_requeue` counts `"ADVERSARIAL ABORT"` occurrences in context. At >= 3, marks terminal failure

The adversarial step is **skipped** when:
- The task has no `success_criteria` (not high-stakes)
- The chain YAML has `no_adversarial: true`
- The task context already contains `"ADVERSARIAL ABORT"` (this is a retry from a prior abort — the plan has already been hardened)

The `adversarial_plan_review` tool is also callable directly (in KnowledgeSkill). Each run stores a `semantic` note with `source_context: "adversarial_review"` so future reviews learn from accumulated failure patterns.

`ChainStep` adversarial fields: `is_adversarial: bool`, `n_hypotheses: Option<u8>`, `min_robustness: Option<f32>`, `adversarial_task_id: Option<String>`. Metadata injected as `__adversarial_min_robustness`, `__adversarial_n_hypotheses`, `__adversarial_task_id` in job args.

`parse_adversarial_robustness()` in `queue.rs` parses `overall_robustness` from the JSON blob; falls back to `Robustness: N/5` text pattern; defaults to 3.0.

### Externalized Agent Chains

All scheduler routing chains are defined as YAML files in `chains/` and seeded into Neo4j as `(:SchedulerChain)` nodes by `init-db` and force-refreshed on the first scheduler tick after every startup, so YAML edits propagate on restart. The `goal_to_steps()` function is now ~20 lines — it queries Neo4j first, falls back to `build_diagnosis_chain()` if nothing matches.

**Chain YAML schema** (`chains/*.yaml`):
```yaml
name: my-chain          # MERGE key in Neo4j
pattern: "keyword"      # primary CONTAINS match; use "" for default chain
patterns: [...]         # additional OR patterns
priority: 100           # lower = matched first; default chain uses 9999
no_evaluator: false     # true = skip evaluator even if task has success_criteria
no_adversarial: false   # true = skip adversarial pre-flight even if task has success_criteria
evaluation_rubric: null # overrides task success_criteria in evaluator step
steps: [...]            # array of ChainStep-compatible objects
```

Template variables in step `arguments`: `{{goal}}`, `{{task_id}}`, `{{date}}`, `{{file_slug}}` (slug derived from goal, used by UI chain for workspace file path). Substitution is **value-level, not text-level**: the stored JSON is parsed first, then `substitute_template_vars()` (in `services/queue.rs`) walks the parsed tree and replaces placeholders inside string values only. This keeps substitution quote/backslash/newline-safe — a `{{goal}}` containing `"` can never corrupt the chain JSON. The same primitive backs chain `{{_prev}}`/`{{result}}` resolution.

**Per-step model routing (Phase 1):** a step may declare `required_capabilities: ["reasoning", ...]`. At execution the model router (`services/model_router.rs`) picks the cheapest catalog model satisfying them within `CLOUD_TIER` (ties broken by largest context window) and the job's LLM calls route to it via the `SELECTED_LLM` task-local (precedence: capability-selected > `USE_LOCAL_LLM` background pin > active config). Cloud calls keep the 429→local fallback and land in the usage ledger. If no catalog model qualifies the step silently keeps normal routing. Metadata travels as `__required_capabilities` in job args (serde-ignored by tools).

The **UI chain** (`chains/ui-frontend.yaml`) matches frontend keywords, writes to `workspace/ui/{{file_slug}}.md`, and sets `no_evaluator: true`.

### ScheduledTask Ownership (`managed_by`)

Built-in ScheduledTask definitions live in `schedules/*.yaml` (seeded by `seed_built_ins` via `schedule_seeder`). There is no hardcoded fallback — a missing `schedules/` directory aborts startup (`std::process::exit(1)`). The graph is always the runtime authority (the scheduler only reads `(:ScheduledTask)` nodes); YAML is the definition source for the tasks it owns. Every node carries a `managed_by` property:

- **`yaml`** — owned by a `schedules/*.yaml` file (matched by exact `name`). Steps, description, and interval are force-synced on every startup, so file edits propagate and runtime edits are overwritten. Legacy nodes without `managed_by` that match a YAML name are claimed as `yaml` at seed time.
- **`runtime`** — created at runtime via `manage_scheduled_task` or `POST /api/scheduled-tasks`. The seeder never touches these. Nodes left unclaimed after seeding are backfilled to `runtime`.

Ownership can be transferred explicitly: `manage_scheduled_task` upsert accepts `managed_by` (`runtime` detaches a task from its YAML; `yaml` hands it back). Updating a yaml-owned task without transferring ownership returns a warning that the change will be overwritten on restart. To make a runtime task durable and reviewable, write a `schedules/*.yaml` with the exact same `name` — the seeder claims and syncs it on the next startup.

**`manage_chain` tool** now accepts `name`, `patterns` (list), `no_evaluator`, and `no_adversarial` fields in addition to `pattern`.

### SourceLists (approved-domain lists for `search_web`)

`(:SourceList {name, domains, description})` nodes restrict `search_web` results to approved domains (the tool adds `site:` operators and post-filters results). Definitions live in `sources/*.yaml` (`name`, `description`, `domains`) and are seeded by `source_seeder` **ON CREATE only** — unlike schedules, the graph owns each list after first creation, so runtime edits (`neo4j_query` with `readonly=false`: `MATCH (s:SourceList {name:'news'}) SET s.domains = [...]`) persist across restarts. Delete a node to re-seed it from its YAML. A `source_list` name that doesn't resolve degrades gracefully: the search runs unrestricted. Built-ins: `news` (national/world outlets), `michigan-news` (metro Detroit and Michigan outlets).

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

**Consolidation loop:** Uses `[Memory N]` labels (not `Note N:`), instructs LLM not to echo them. Auto-generated consolidation topics use `"recent experiences and knowledge"`. Source notes get `next_review_at = now + 30 days` after consolidation. Source selection excludes LLM-generated note types (`consolidated`, `reflection`, `news`, `news_raw`, `outcome`, `inference`, `meta_learning_result`) and only considers notes whose `next_review_at` is due — without the due filter the fixed topic embedding deterministically re-selects the same top-K notes every cycle. An empty selection is a no-op success (not an error) so the bedtime chain still reaches `prune_old_notes`.

**`services/mod.rs`:** Must re-export `LlmProviderType`: `pub use llm::{LlmClient, LlmConfig, LlmProviderType};`

**Context Profiles:** YAML files in `contexts/` (CONTEXTS_DIR env var). Loaded by `ContextBuilderService::load_profiles()` in `build_skills()`. Boot protocol (`contexts/boot.yaml`) runs after each `build_skills()`. Init protocol (`contexts/init.yaml`) runs on empty graph. `ContextSkill` registered when `context_builder_arc` is Some.

## Branch Strategy
DO NOT REMOVE THIS LINE:Never write in credidation to LLMs or coding agents or assistants.

- `feature/*` - Feature branches (no CI)
- `dev` - Development (format + unit tests)
- `test` - Testing (full pipeline with integration tests)
- `prod` - Production (full pipeline + Docker build)
- Update the documentation first, the README, claude, plan, markdowns should reflect our changes.
