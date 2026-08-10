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
| `OLLAMA_KEEP_ALIVE` | - | How long Ollama keeps a model resident in VRAM after a request, as a **Go duration** (`30m`, `2h`, `1h30m`). A negative duration (`-1m`) pins it indefinitely. Unset leaves Ollama's own 5m default. Sent on every generate/chat/**embeddings** request; ignored by non-Ollama providers. Compose sets `30m`. Malformed values are dropped with a warning (`validate_go_duration` in `config.rs`) rather than 400-ing every LLM call |
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
| `SEARCH_ENGINE_ORDER` | `searxng,google,serpapi,brave` | Comma-separated `search_web` failover ladder, most-preferred first. Each engine is tried until one answers, so a single exhausted quota no longer fails the call. Reorder to change preference without a rebuild |
| `SEARXNG_URL` | `http://searxng:8080` | Base URL of the self-hosted SearXNG metasearch sidecar — the default first rung of the ladder. No API key, no quota. Also readable from a `searxng` `ApiContext` node (`base_url` field) |
| `SERPAPI_KEY` | - | SerpApi key for `search_web` tool. Free tier is **100 searches/month** — far below the brain's ~15/day, so this is a backstop, not a primary |
| `BRAVE_API_KEY` | - | Brave Search API key for `search_web` tool. Brave **killed its free tier in Feb 2026** (card required, metered, no spending cap) — last rung by default |
| `GOOGLE_API_KEY` | - | Google Custom Search API key for `search_web` tool. Free tier is 100 queries/day. Requires the *Custom Search API* to be **enabled** in the GCP project or every call 403s |
| `GOOGLE_CX` | - | Google Custom Search Engine ID for `search_web` tool |
| `CLOUD_TIER` | `1` | Cloud autonomy tier for per-step model routing. `0` = local Ollama only; `1` = local + $0-cost Ollama Cloud models (needs `OLLAMA_API_KEY`); `2` = any provider with a configured key ("income mode") |
| `SCHEDULER_INTERVAL_SECS` | `300` | How often the scheduler polls for pending tasks (seconds) |
| `SCHEDULER_ENABLED` | `true` | Set to `false` to start with the autonomous scheduler disabled |
| `CHAINS_DIR` | `./chains` | Directory containing `*.yaml` SchedulerChain definitions. Seeded by `init-db` and force-refreshed on the first scheduler tick after every startup (YAML edits propagate on restart) |
| `SCHEDULES_DIR` | `./schedules` | Directory containing `*.yaml` ScheduledTask definitions. Seeded by `init-db` and on every startup. A missing/unreadable directory is a **fatal startup error**. See "ScheduledTask ownership" below |
| `SOURCES_DIR` | `./sources` | Directory containing `*.yaml` SourceList definitions (approved-domain lists for `search_web`). Seeded **ON CREATE only** — the graph owns each list after first creation; runtime edits via `neo4j_query` persist across restarts. Delete a node to re-seed it from YAML. Missing directory is non-fatal |
| `SOURCES_MEDIA_DIR` | `./sources-media` | Directory containing `*.yaml` MediaSource watchlist definitions (channels/playlists to watch). Seeded **ON CREATE only** — graph-owned after first creation, like SourceLists. Missing directory is non-fatal |
| `YT_DLP_PATH` | `yt-dlp` | Path to the `yt-dlp` binary used by the Media Learning skill for metadata + caption extraction |
| `FFMPEG_PATH` | `ffmpeg` | Path to `ffmpeg` (audio extraction for the Whisper fallback — Phase 4, not yet wired) |
| `MEDIA_CAPTION_LANG` | `en` | Preferred caption language for `ingest_media` / `fetch_transcript` |
| `MEDIA_MAX_DURATION_SECS` | `10800` | Skip videos longer than this (cost guard; `0` = unlimited) |
| `MEDIA_WATCH_ENABLED` | `false` | Enable autonomous channel polling. When unset/false, `poll_media_sources` is a no-op (the `media-watch` schedule is inert) |
| `MEDIA_WATCH_MAX_PER_SOURCE` | `3` | Max new videos enqueued per source per poll (first-poll stampede guard). A fresh channel's RSS carries ~15 recent uploads; this caps how many become tasks at once |
| `MEDIA_DISCOVERY_ENABLED` | `false` | Enable autonomous watchlist curation. When unset/false, `discover_media_sources` is a no-op (the `discover-media-sources` weekly schedule is inert) |
| `MEDIA_DISCOVERY_MAX` | `3` | Max new sources staged per discovery run. Discovered channels/podcasts are added **inactive** (`active:false`) for human review — never polled until activated |
| `WHISPER_PROVIDER` | `none` | Whisper backend for caption-less media. `none` = disabled (videos without captions error cleanly). `http`/`local`/`openai` = POST audio to an **OpenAI-compatible** `/audio/transcriptions` endpoint at `WHISPER_BASE_URL` (intended for a self-hosted server — the `whisper` compose sidecar). Phase 4 — implemented |
| `WHISPER_BASE_URL` | - | Base URL of the self-hosted Whisper server (e.g. `http://whisper:8000/v1`). Required when `WHISPER_PROVIDER` is enabled |
| `WHISPER_MODEL` | `Systran/faster-whisper-base` | Whisper model name sent to the endpoint |
| `WHISPER_API_KEY` | - | Optional bearer token for the Whisper endpoint (the self-hosted sidecar needs none) |
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
│       │   │   ├── media.rs      # Media Learning skill (7 tools: ingest_media, fetch_transcript, list_channel_videos, poll_media_sources, manage_media_source, spawn_gap_tasks, discover_media_sources)
│       │   │   ├── model.rs      # Model Registry skill (5 tools)
│       │   │   ├── procedure.rs  # Procedural Memory skill (2 tools)
│       │   │   ├── scheduler.rs  # Autonomous Scheduler skill (5 tools)
│       │   │   ├── search.rs     # Web Search skill (2 tools: search_web, get_search_usage)
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
- `Note` - Stored text memories with optional vector `embedding`, `access_count`, `last_accessed_at`, `note_type` (`semantic`/`episodic`/`reflection`/`consolidated`/`outcome`/`inference`/`claim`/`source_record`/`news`), `next_review_at`, `review_interval_days`, `source_context`, `event_at`
- `Procedure` - Named multi-step workflows with `id`, `name`, `description`, `steps` (JSON array), `created_at`
- `WorkingMemory` - Session-scoped scratchpad entries with `id`, `session_id`, `content`, `role`, `turn_index`, `created_at`
- `Entity` - Named entities extracted from notes with `id`, `name` (unique, lowercased), `entity_type`, `created_at`
- `DynamicTool` - Runtime-defined MCP tools with `id`, `name` (unique), `description`, `input_schema` (JSON), `created_at`
- `AgentJob` - Background job record with `id`, `tool_name`, `args_json`, `priority` (0-3), `status` (queued/running/completed/failed/dead/parked/cancelled), `attempt_count`, `max_attempts`, `result_json`, `error`, timestamps, `session_id`, `parent_job_id`
- `ModelSpec` - Registered LLM models with capabilities, cost, and usage stats
- `ToolDef` / `ContextProfile` / `ModelDef` - **Self-model meta-graph** (Phase 0b of the Agent Constructor plan). Generated by introspection in `services/self_model.rs` at the end of every `build_skills()` — never hand-edit: the tool registry, `contexts/*.yaml`, and the DuckDB model catalog are the sources of truth, and stale nodes are deleted on each sync. `(:ContextProfile)-[:ALLOWS]->(:ToolDef)` edges mirror profile tool allowlists (`allows_all: true` when the profile has no allowlist). Chains/schedules are not duplicated here — `(:SchedulerChain)`/`(:ScheduledTask)` already are the graph representation.
- `BrainVersion` - **Singleton `{id:'current'}`** recording the commit this process is running (`sha`, `subject`, `branch`, `dirty`, `seen_at`, `deployed_at`). Written by `sync_code_version()` in `services/self_model.rs`, right after the meta-graph sync. When the sha differs from the stored one it also writes ONE episodic note (`source_context: "code_version <sha>"`) listing the changed files; an unchanged sha is a no-op, so restarts cost nothing.

  This replaced a `post-commit` → `scripts/self_update.py` round-trip (deleted 2026-08-10). That script re-derived over HTTP what the process already knows, and got it wrong: it parsed `analyze_own_structure`'s **Markdown** output as JSON inside a bare `except: pass`, printing `tools at runtime: 0` on every commit since March, and it indexed `result[0]` on a dict. It also fired on `git commit` — before the rebuild it triggered had finished — so the note described code that was not yet running, and it only ever covered a local `git commit` (not `docker compose restart`, CI, or a pull on another machine). Reading HEAD in-process at startup reports **what is actually loaded**. The hook is now just `docker compose up -d --build`.

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

**Chain-death task attribution:** scheduler-dispatched chains are enqueued via `enqueue_chain_owned(steps, session_id, Some(&task_id))`, which stamps `__owner_task_id` (serde-ignored, like the other `__`-prefixed job metadata) onto every step. When a step dies, the coordinator reads it and calls `fail_task_with_reason(task_id, reason)` — flipping the run Task to `failed` immediately with a `[FAILURE] Chain step '<tool>' died … Last error: …` line appended to its `context`. Previously a died chain left the run stuck `in_progress` until the 6-hour stale reaper (`reset_stale_in_progress_tasks`) flipped it with no diagnosis, so `capability-mining` reasoned over boilerplate context and free-associated generic advice. The write is guarded to `status IN ['created','in_progress']`, so it never clobbers a task already resolved (completed, or failed via the evaluator/adversarial path). `enqueue_chain` remains the no-owner wrapper used by chat/bedtime/meta-learning chains.

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

**Priority is the only tiebreak between overlapping patterns, and matching is plain CONTAINS** — so a chain with a broad substring pattern silently hijacks goals meant for a chain with a specific prefix if it sorts first. This was live for months: `chains/learn.yaml` (pattern `"research"`, priority 50) beat `chains/fill-knowledge-gap.yaml` (pattern `"fill knowledge gap:"`, then priority 100) on every goal phrased *"fill knowledge gap: **Research** …"*, so the curiosity engine mostly ran someone else's chain. When adding a chain, check that no broader existing pattern outranks it; specific prefixes belong at low priority numbers.

Template variables in step `arguments`: `{{goal}}`, `{{goal_topic}}`, `{{task_id}}`, `{{date}}`, `{{file_slug}}` (slug derived from goal, used by UI chain for workspace file path). `{{goal_topic}}` is the goal with its routing prefix stripped (`"fill knowledge gap: Research X"` → `"Research X"`) — **use it for any step that feeds the goal to a search engine**, since the prefix exists to match a chain and is pure noise inside a query. `goal_topic()` in `services/queue.rs` is conservative: it strips only up to the first colon, only within the first 40 chars, and never on a bare URL or an empty remainder, so ordinary sentence goals pass through untouched. Substitution is **value-level, not text-level**: the stored JSON is parsed first, then `substitute_template_vars()` (in `services/queue.rs`) walks the parsed tree and replaces placeholders inside string values only. This keeps substitution quote/backslash/newline-safe — a `{{goal}}` containing `"` can never corrupt the chain JSON. The same primitive backs chain `{{_prev}}`/`{{result}}` resolution.

**Per-step model routing (Phase 1):** a step may declare `required_capabilities: ["reasoning", ...]`. At execution the model router (`services/model_router.rs`) picks the cheapest catalog model satisfying them within `CLOUD_TIER` (ties broken by largest context window) and the job's LLM calls route to it via the `SELECTED_LLM` task-local (precedence: capability-selected > `USE_LOCAL_LLM` background pin > active config). Cloud calls keep the 429→local fallback and land in the usage ledger. If no catalog model qualifies the step silently keeps normal routing. Metadata travels as `__required_capabilities` in job args (serde-ignored by tools).

**Distilled handoffs (`distill_prev`):** `{{_prev}}` pastes the *entire* previous step's output into the next step's prompt, but a step usually needs the prior step's conclusions, not its full text — raw SERP payloads, git diffs, and transcript summaries are mostly boilerplate. A step may declare:

```yaml
distill_prev: true
distill_max_chars: 3000        # default 2000; also the skip threshold
distill_focus: "every claim with its source URL, plus dates and figures"
```

Before substitution, `maybe_distill_prev` (in `services/queue.rs`) compresses the upstream output on the **local** model, so the compression itself is free. Metadata travels as `__distill_prev` / `__distill_max_chars` / `__distill_focus` in job args (serde-ignored by tools), like the evaluator/adversarial fields.

`distill_max_chars` is a **soft** budget — models can't count characters and overshoot ~50% routinely (measured: a 3000 budget produced 4898 chars from a 195 000-char git diff — still 97.5% off the handoff). The only hard guarantee is that the distilled text is never longer than the raw text.

The distiller builds its **own** `LlmClient` from the local config with `num_ctx` raised to 16 384, rather than reusing the shared local provider — see the `num_ctx` note under Critical Dev Notes: at Ollama's 4096 default the payload is silently truncated and the model answers the fragment instead of compressing it. Its prompt repeats the instruction *after* the payload for the same reason.

Rules that make it safe:
- **Opt in on the *consuming* step**, never globally. Only the consumer knows whether it tolerates a lossy handoff.
- **Never on a step that persists `{{_prev}}` verbatim** (`store_note`, `write_workspace_file`, `create_task`) — that truncates the durable artifact rather than just a prompt. The self-improvement loop also *reads* intermediate text (the evaluator regexes `Score: N/5`, `parse_adversarial_robustness` scrapes JSON), so evaluator/adversarial steps must stay raw.
- Every failure path falls back to the raw text: distillation off, payload already ≤ `distill_max_chars`, no LLM installed, generation error, empty result, or a result no shorter than the input. It can never fail a job.
- Distiller input is capped at 24 000 chars as a head+tail window — conclusions sit at the end, so head-only truncation would drop what the next step needs.

Wired into `chains/learn.yaml` (SERP → cloud reasoning step), `chains/git-history.yaml` (10-commit diff → reason), and `chains/video-learning.yaml` (transcript summary → the chain's one cloud-routed step; the durable note re-reassembles from working memory, so stored fidelity is untouched).

The **UI chain** (`chains/ui-frontend.yaml`) matches frontend keywords, writes to `workspace/ui/{{file_slug}}.md`, and sets `no_evaluator: true`.

### Self-knowledge: making the brain able to see its own running work

A chat session on 2026-08-10 asked "where are we on project X" and was told the project was an unbuilt "conceptual blueprint" — while three ScheduledTasks implementing it had been running for two days. The same session reported a "clean slate" with 210 tasks in `failed`. Neither was a model-quality problem; four separate mechanisms each hid state, and every one failed *silently*, which is what turns a gap into a confident false statement.

- **`manage_scheduled_task(action=list)`** — listing had been REST-only (`GET /api/scheduled-tasks`), which the chat agent cannot call, so its only reflex for a project question was `search_notes`. Notes record what was *decided*; schedules are what is *running*. The payload is deliberately lean (name/enabled/`every`/`last_run`/`next_run`, dates truncated to day) with `verbose: true` for the rest — chat caps tool results at 2000–6000 chars and the repository orders by `next_run_at ASC`, so long-cadence schedules (exactly the strategic, project-level ones) sort last and are cut first.
- **Empty-filter guardrail.** `filter` matches name and description. When a filter matches nothing but schedules exist, the tool returns **every** name plus an explicit warning instead of an empty list — because the failure mode is filtering on the user's phrase ("Niche Intelligence Agency") when the work is filed under an internal name ("hardware tripwire"), then reporting live schedules as nonexistent.
- **`list_tasks` returns totals and leads with open work** (created/in_progress/blocked → failed → completed). It previously returned 20 newest rows with no counts; the newest rows were completed, so the window read as "all done." The rows are a window; the totals are the truth.
- **Truncated tool results are now marked.** `truncate_tool_result` in `clients/chat.rs` appends a `[TRUNCATED: … INCOMPLETE …]` notice. Silent truncation is worse than a short answer: the model cannot tell "the list ends here" from "the list was cut here," and states the former.
- **Failed protocol steps log at WARN.** `ProtocolStep::ToolCall` discarded results and logged `is_error` at `debug!` (invisible at `LOG_LEVEL=info`). `contexts/boot.yaml` called `scheduler_control{action:"status"}` — an action that does not exist — on every startup for months while the surrounding `log` steps printed "Scheduler status obtained." Grep for `tool_call FAILED` after editing any protocol.

The behavioural half lives in the `PROJECT-STATUS RULE` in `contexts/general.yaml`: check schedules *and* tasks *and* notes, treat an alias found in a note as a lead to re-query rather than an answer, and report running work in the present tense.

### ScheduledTask Ownership (`managed_by`)

Built-in ScheduledTask definitions live in `schedules/*.yaml` (seeded by `seed_built_ins` via `schedule_seeder`). There is no hardcoded fallback — a missing `schedules/` directory aborts startup (`std::process::exit(1)`). The graph is always the runtime authority (the scheduler only reads `(:ScheduledTask)` nodes); YAML is the definition source for the tasks it owns. Every node carries a `managed_by` property:

- **`yaml`** — owned by a `schedules/*.yaml` file (matched by exact `name`). Steps, description, and interval are force-synced on every startup, so file edits propagate and runtime edits are overwritten. Legacy nodes without `managed_by` that match a YAML name are claimed as `yaml` at seed time.
- **`runtime`** — created at runtime via `manage_scheduled_task` or `POST /api/scheduled-tasks`. The seeder never touches these. Nodes left unclaimed after seeding are backfilled to `runtime`.

Ownership can be transferred explicitly: `manage_scheduled_task` upsert accepts `managed_by` (`runtime` detaches a task from its YAML; `yaml` hands it back). Updating a yaml-owned task without transferring ownership returns a warning that the change will be overwritten on restart. To make a runtime task durable and reviewable, write a `schedules/*.yaml` with the exact same `name` — the seeder claims and syncs it on the next startup.

**`manage_chain` tool** now accepts `name`, `patterns` (list), `no_evaluator`, and `no_adversarial` fields in addition to `pattern`.

**A ScheduledTask's steps are enqueued verbatim.** `dispatch_one_scheduled_task` creates its run-record Task with `success_criteria: None` and enqueues `st.steps` directly — the chain never passes through `goal_to_steps()`, so it gets **no pattern matching, no adversarial pre-flight, and no evaluator**. Two consequences when writing `schedules/*.yaml`: nothing grades the output, so the step prompts must be correct on their own; and nothing will add retrieval for you, so a schedule needing fresh external information must include its own `search_web` steps. (Contrast a one-shot `Task`, which *does* route through chain matching and picks up both loops when it has `success_criteria`.)

**One-shot Task vs recurring ScheduledTask.** `create_task` makes a `(:Task)` that the scheduler dispatches on the *next tick* and never again — cadence words inside the goal string ("Weekly …", "Monthly …") are inert text. Recurrence requires `manage_scheduled_task(action=upsert, interval_seconds=…)` or a `schedules/*.yaml`. A batch of one-shot Tasks named for a cadence all fire at once, immediately, then never again. `contexts/general.yaml` carries this distinction in its prompt because the chat persona previously hit exactly this failure.

**Reading prior runs back out of the graph:** `store_note` chunks long content into `(:Note)-[:PART_OF]->(:Note)` children, and **chunks inherit `source_context` from their parent**. Any Cypher that retrieves a previous run's report by `source_context` must therefore exclude them with `AND NOT (n)-[:PART_OF]->()`, or it can return a few-hundred-character fragment of one section instead of the whole report. The three Critical Tech Dependency schedules below all do this.

**Critical Tech Dependency Mapping schedules** — a three-tier standing investigation, each storing under a stable `source_context` that the next run (and the monthly audit) retrieves by:

| File | Interval | `source_context` |
|------|----------|------------------|
| `schedules/hardware-tripwire.yaml` | weekly (604800) | `hardware_tripwire` |
| `schedules/slm-benchmark-watch.yaml` | bi-weekly (1209600) | `slm_benchmark_watch` |
| `schedules/tech-dependency-synthesis.yaml` | monthly (2592000) | `tech_dependency_synthesis` |

The two watch schedules each run three `search_web` sweeps, bank every result into a `WorkingMemory` scratch session, and reassemble with `neo4j_query` before the `reason` step — `{{_prev}}` carries only the *previous* step's output, so consecutive searches would otherwise discard all but the last (the `chains/video-learning.yaml` idiom). Step 1 of each pulls the prior run's note so `reason` performs a delta rather than re-describing the world; the retrieval also matches the original hand-written `BASELINE: …` notes by content prefix. The monthly synthesis performs **no** web search by design — it correlates what the other two accumulated, issues a SUPPORTED/NOT SUPPORTED/INSUFFICIENT EVIDENCE verdict, and spawns a `fill knowledge gap:` Task from its weakest link.

### Curiosity engine (`chains/fill-knowledge-gap.yaml`)

Gap tasks (`"fill knowledge gap: …"`) are spawned by `spawn_gap_tasks` from a video's `## FOLLOW UP` section and by the daily news-analysis schedule. The chain searches **both** the graph (`search_notes`) and the web (`search_web`), banks each result set into a `gap-{{task_id}}` WorkingMemory session, reassembles them with `neo4j_query`, and answers the gap from the union under three headings: `## ANSWER` (cited), `## WHAT THIS ADDS` (vs. what was already stored), `## STILL UNKNOWN` (specific enough to re-search).

Before 2026-08-10 this chain searched *only* internal notes, which cannot fill a knowledge gap by construction — a gap is precisely what the graph does not contain. It produced self-referential "Gap synthesis" notes that re-chewed prior notes and drifted into meta-commentary about the source material. Two changes fixed it: adding the `search_web` step, and dropping the chain's priority to 15 so it stops losing its own tasks to `learn.yaml` (see the pattern-priority note above). Search steps use `{{goal_topic}}`, not `{{goal}}`.

### Epistemics: claims, source records, and retrieval labelling

The brain ingests from sources of wildly varying reliability. Until 2026-08-10 it stored them identically — a CRS report and a late-night cable segment both became `semantic` notes — so retrieval handed them to `reason` indistinguishably and an assertion made once on a talk show came back out phrased as established fact. Observed directly: asked to summarise the evidence on a UAP topic, `reason` answered *"clear evidence of government agencies… deeply engaged"*.

The response is deliberately **not** to filter sources. Dropping fringe material at ingest also destroys the record needed to notice a narrative being pushed — you cannot detect a coordinated shift in messaging you never stored. Instead the type system distinguishes three things it previously conflated:

| Type | Means |
|---|---|
| `semantic` | knowledge the brain has established |
| `source_record` | a record of what an external source *said* (video summaries, and `news` for briefs) |
| `claim` | a single checkable proposition, with evidentiary status |

- **`(:Note {note_type:'claim', claim_status, asserted_by, asserted_at})`** with `-[:ASSERTED_IN]->`, `-[:CORROBORATED_BY]->`, `-[:CONTRADICTED_BY]->` edges. Status is **derived** from the edges (`recompute_status`), never asserted, so it cannot drift from its evidence. Verification never edits a claim — evidence is attached and status recomputed; the assertion is preserved exactly as made. Support *and* contradiction stays `disputed` rather than collapsing to a verdict, and absence of evidence leaves a claim `unverified`, never refuted.
- **Retrieval labelling is the load-bearing piece.** `label_claims` in `services/knowledge.rs` prefixes claims with `[CLAIM · status · asserted by X · date]` and source records with `[SOURCE RECORD — what a source said, not verified · … ]`. It runs in **both** retrieval paths — miss one and the unlabelled copy of the same assertion still reaches the context window, which is exactly what happened when only claims were labelled and `reason` kept reading the unlabelled video note. After labelling both, the same query returned *"highly speculative"*, *"the narrative suggests"*.
- **`claim` and `source_record` are excluded from consolidation source selection.** Consolidation rewrites its sources into a settled summary, which would strip the status and launder an unverified assertion into semantic knowledge.
- **Corroboration is described, not ranked.** `classify_domains` splits corroborating domains into `primary sources` (institutional — `.gov`/`.mil`/`.edu`/`.gov.uk`, decided mechanically from the domain, no editorial judgement), `established sources` (on a curated `:SourceList`), and `unclassified`. The tier rides in the label: `[CLAIM · corroborated · primary sources · …]` vs `[CLAIM · corroborated · unclassified sources only · …]`. Verification is **never gated** on tier — doing so would encode "mainstream equals true" and make niche-but-accurate sources permanently unverifiable, which is its own censorship. `unclassified` means "not on our list", not "unreliable". Note the first version tiered on `:SourceList` alone and labelled a Congressional-hearing claim corroborated by `congress.gov`/`house.gov` as "unclassified sources only" — the lists were curated for *search restriction*, not classification, and a mislabel like that is worse than no label.
- **Independence is checked, not assumed.** `check_independence` requires `MIN_INDEPENDENT_DOMAINS` (2) distinct non-self-referential domains before support counts. It rejects the circular case — a claim about Skywatcher "corroborated" by `skywatcher.ai`. **It does not solve source independence**: five topic-aligned outlets republishing one origin pass any count-based test, observed live. Corroborating domains are recorded on the edge so this stays inspectable rather than hidden behind a status word. Contradiction is deliberately *not* gated — gating it would bias the system toward belief.

Known gaps: extraction does not distinguish "X was asserted" from "X is true"; chain-extracted claims carry `source_context`/`asserted_by` but no `ASSERTED_IN` edge (`store_note` hands content, not its id, to `{{_prev}}`); `learn_chain` notes (248) are still typed `semantic`.

### Web Search: engine failover ladder + usage ledger

`search_web` does not have "a search engine" — it has an ordered **ladder** of them (`SEARCH_ENGINE_ORDER`, default `searxng,google,serpapi,brave`). It walks the ladder until one engine answers, so a single dead provider degrades quality instead of failing the call.

This exists because of a concrete outage: on 2026-08-08 the SerpApi free tier hit `429 "Your account has run out of searches."` and, because `search_web` hard-failed on its one default engine, it took **39 jobs and 38 tasks** with it and stopped the daily news brief for two days. The arithmetic was never survivable — `schedules/daily-news.yaml` alone issues 8 searches/day (~240/month) against SerpApi's 100/month cap, and measured total volume is 11–20/day.

- **`searxng` leads by design.** A [SearXNG](https://docs.searxng.org/dev/search_api.html) sidecar (compose service `searxng`, config in `searxng/settings.yml`) aggregates ~70 upstream engines behind one JSON API with **no key and no quota** — the only rung that cannot exhaust. It is deliberately **not** published to a host port; it lives on `brain-internal` only, which is what makes the disabled bot `limiter` in its settings safe. Do not add a `ports:` mapping without re-enabling the limiter. The settings file must keep `json` in `search.formats` — the upstream default enables `html` only, and its absence makes `?format=json` answer 403.
- **Requested engine ≠ only engine.** Passing `engine: "serpapi"` promotes it to the head of the ladder but does not truncate the rest: a caller wants an answer more than it wants a specific provider.
- **Quota cooldown.** An engine that reported `quota_exhausted` within `QUOTA_COOLDOWN_HOURS` (6) is moved to the *back* of the ladder rather than dropped — a daily quota recovers overnight on its own, but must not cost a wasted round-trip on all eight searches of the news chain until it does. Ordering rules are pure (`order_engines` in `skills/search.rs`) and unit-tested.
- **Result normalisation.** SearXNG's `url`/`content` fields are mapped to the `link`/`snippet` shape SerpApi and Google CSE already emit, so downstream `reason` steps see one schema regardless of which rung answered. (Brave still emits `url`/`description` — the `source_list` post-filter accepts both.)
- **Usage ledger.** Every engine *attempt* writes a row to the DuckDB `search_usage` table (engine, query, success, result count, duration, `error_kind`) — a failover that tries two engines writes two rows, because quota accounting needs to know what each engine was actually asked to do. `get_search_usage` reports per-engine totals plus a per-day breakdown; default window is 720h (~30 days) because monthly caps are the ones that bite.
- **Quota exhaustion now alerts.** `is_quota_exhausted_error` in `services/queue.rs` is a deliberate subset of `is_transient_infra_error`: meta-learning is still skipped (the brain cannot reason its way out of a billing cap) but the coordinator now raises a deduped `:AgentNotification` (once per tool per 24h) instead of logging "skipping meta-learning" and going quiet. Silence is what turned a spent quota into a two-day outage.

### SourceLists (approved-domain lists for `search_web`)

`(:SourceList {name, domains, description})` nodes restrict `search_web` results to approved domains (the tool adds `site:` operators and post-filters results). Definitions live in `sources/*.yaml` (`name`, `description`, `domains`) and are seeded by `source_seeder` **ON CREATE only** — unlike schedules, the graph owns each list after first creation, so runtime edits (`neo4j_query` with `readonly=false`: `MATCH (s:SourceList {name:'news'}) SET s.domains = [...]`) persist across restarts. Delete a node to re-seed it from its YAML. A `source_list` name that doesn't resolve degrades gracefully: the search runs unrestricted. Built-ins: `news` (national/world outlets), `michigan-news` (metro Detroit and Michigan outlets).

### Media Learning (watch & summarize videos)

The brain watches and summarizes videos to learn new concepts and stay current on topics it already knows. `MediaSkill` (`skills/media.rs`) + `MediaService` (`services/media.rs`) own the pipeline; `project-docs/VIDEO_LEARNING_PLAN.md` is the full spec.

- **Transcript acquisition (captions-first):** `MediaService` shells out to `yt-dlp -J` for metadata, then lets **yt-dlp download the caption file itself** (`--write-subs --write-auto-subs --sub-langs … --sub-format json3` into a scratch dir) and parses the `json3` to plain text. (Fetching the timedtext URL directly via reqwest was tried and abandoned — it works for manual subs but fails for auto-captions, which need yt-dlp's session.) **Whisper fallback (Phase 4, implemented):** when a video has no captions and `WHISPER_PROVIDER` is set, `MediaService` downloads best audio (`yt-dlp -f bestaudio`, no ffmpeg) and POSTs it to a self-hosted **OpenAI-compatible** Whisper endpoint (`WHISPER_BASE_URL`) via `services/transcribe.rs` (`Transcriber` trait → `HttpTranscriber`). The `whisper` compose service runs `faster-whisper-server` on GPU (`:latest-cuda`, `float16`) — host driver 535.261.03 (CUDA 12.2) satisfies the image's 12.2 runtime on the RTX 3060 Ti; the base model uses ~380 MiB VRAM and unloads on idle TTL. Fall back to `:latest-cpu` + `WHISPER__INFERENCE_DEVICE=cpu` if the driver ever lags the image's CUDA version. `WHISPER_PROVIDER=none` (the code default) keeps caption-less videos erroring cleanly. Subprocess safety: `yt-dlp` is always invoked with an **arg array**, and URLs are scheme-validated (`http`/`https` only).
- **Map-reduce summarization happens *inside* `ingest_media`,** not as chain steps — chains are fixed-length but transcripts aren't. Short transcripts are single-pass; long ones are chunked on sentence boundaries ("map"), then synthesized via `generate_json` into `{summary, key_concepts}` ("reduce").
- **On-demand:** `ingest_media(url)` (accepts a bare URL or a goal like `"watch video: <url>"`) and `fetch_transcript(url)`. A Task whose goal starts with `watch video:` routes to `chains/video-learning.yaml` (ingest → bank/reassemble in a `video-{{task_id}}` WorkingMemory session → new-vs-known `reason` → `store_note` → cleanup).
- **Autonomous watch:** `poll_media_sources` iterates active `:MediaSource` nodes, lists new videos from YouTube's **free per-channel RSS feed** (`youtube.com/feeds/videos.xml?channel_id=…`), and fans each new upload out into a `"watch video: <url>"` Task (chains can't loop a dynamic list, so we create Tasks). Gated by `MEDIA_WATCH_ENABLED` — a no-op when unset, so `schedules/media-watch.yaml` (6h) is safe to seed everywhere. Dedup: a video is skipped if a `:Media` node exists **or** an open Task already targets it.
- **Watchlist ownership:** `(:MediaSource {name, kind, ref, description, active, managed_by})` — `kind` ∈ `youtube_channel`/`youtube_playlist`/`podcast_rss`. Seeded from `sources-media/*.yaml` **ON CREATE only** (`managed_by='yaml'`), graph-owned afterwards; `manage_media_source` upserts set `managed_by='runtime'`. Mirrors the SourceList model.
- **Autonomous watchlist curation:** `discover_media_sources` lets the brain grow its own watchlist. The weekly `schedules/discover-media-sources.yaml` reasons about the topics the brain studies → `search_web` for channels/podcasts on them → `discover_media_sources` uses `generate_json` to pick candidates, resolves each (YouTube channel URL → `channel_id` via `MediaService::resolve_youtube_channel_id`; podcast feed validated by a trial `list_feed_videos`), dedups against existing sources, and upserts each as an **inactive** `MediaSource` (`active:false`) — staged for human review, never polled until activated via `manage_media_source(action=upsert, active=true)`. Gated by `MEDIA_DISCOVERY_ENABLED` (no-op otherwise), capped by `MEDIA_DISCOVERY_MAX`.
- **Phase status:** Phases 1–5 implemented. Phases 1–4: captions, on-demand, autonomous YouTube watch, learning loop, and self-hosted Whisper fallback via `services/transcribe.rs` + the `whisper` sidecar. **Phase 5 (podcast RSS + local files):** `fetch_transcript` classifies its input via a `MediaInput` enum and dispatches — yt-dlp URLs (captions-first), **direct audio URLs** (a podcast enclosure or any http(s) `.mp3`/`.mp4`/… → downloaded via reqwest and Whisper-transcribed, no captions), and **local files** (`file://` canonicalized and confined to `MEDIA_DIR`, symlink-escape safe → Whisper). `feed_url`/`list_feed_videos` handle `kind: podcast_rss` (the `ref` *is* the feed URL; `parse_rss_feed` reads RSS 2.0 `<item><enclosure url>`), so the autonomous watch fans podcast episodes out into `watch video:` Tasks exactly like YouTube uploads. Dedup id for non-yt-dlp media is a stable FNV-1a hash of the URL/path (`pod-…`/`file-…`), computed identically at poll time and ingest time. `MEDIA_MAX_DURATION_SECS` does not apply to direct/local input (no duration up front).

New nodes: `:MediaSource` (watchlist) and `:Media` (`{id, url, title, channel, channel_id, published_at, duration_secs, transcript_source, ingested_at, source_media_name}` — dedup/provenance ledger; `id` is the platform video id). New relationships: `(:Media)-[:FROM_SOURCE]->(:MediaSource)`, `(:Note)-[:SUMMARIZES]->(:Media)` (created on the `ingest_media` `store=true` path), and the reused `(:Note)-[:MENTIONS]->(:Entity)` from `store_note` so video concepts become entities automatically.

### Context Profiles

YAML profiles in `contexts/` (default `./contexts`) define tool allowlists and system prompts for different agent personas. `boot.yaml` runs every startup; `init.yaml` runs when the graph is empty. The `ContextBuilderService` loads profiles and supports `auto_assign(goal)` keyword-matching to pick the best profile.

**A profile applies three layers to a `/chat` turn** (`clients/chat.rs`): its `tools` allowlist filters the registry, its `system_prompt` is appended after the shared base prompt, and whatever `pre_load_query` returns is appended last under a `LIVE SELF-STATE` heading. `build_system_prompt(profile_prompt, pre_loaded)` composes them once per turn and every provider loop receives the result. Ordering is deliberate: base rules → profile guidance → live state, so the most specific layer sits closest to the conversation. The `thinking` diagnostic event reports which layers landed (`prompt=profile|base | preload=N`). Previously only the tool allowlist was applied — the prompt and pre-load were computed and discarded, so every profile's `system_prompt` was dead code for chat.

**`pre_load_query` accepts Cypher or a keyword.** When the string opens with a read clause (`MATCH`/`OPTIONAL`/`WITH`/`UNWIND`/`CALL`/`RETURN`) and contains no write clause, it executes verbatim and every `content` column it returns is injected; otherwise it falls back to the legacy behaviour of keyword-matching against note content. `is_read_cypher()` rejects any query containing a write token (`CREATE`/`MERGE`/`DELETE`/`DETACH`/`SET`/`REMOVE`/`DROP`/`FOREACH`) because profiles are editable at runtime via the `context` tool — a pre-load must never mutate the graph. The check is deliberately blunt: a false negative degrades to "no pre-load", which is safe. A failing query warns and continues rather than breaking the profile.

**`contexts/self-analyst.yaml`** is the self-inspection persona. `auto_assign` routes goals about the brain's own architecture, models, cost, or performance to it. Its `pre_load_query` injects live self-state every turn — the `(:ModelDef)` catalog (the only models it can actually run), the `(:SchedulerChain)`/`(:ScheduledTask)` inventory (so it stops proposing autonomy that already exists), and `(:ToolDef)` counts per skill. Its prompt enforces a MEASURE → LOCATE → COMPARE → PROPOSE order and forbids answering self-questions from general knowledge. `contexts/general.yaml` carries a smaller version of the same idea plus the read-only introspection tools (`neo4j_query`, `duckdb_query`, `get_usage`, `context`, `analyze_own_structure`, `get_file_tree`, `get_git_log`). Note `MCP_TOOL_PROFILE=general` in compose means the general allowlist also bounds what `tools/list` exposes to MCP clients.

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

**Structured LLM output:** For tool outputs that must be strict JSON, call `LlmProvider::generate_json(prompt, system, required_keys, max_retries)` (default method on the trait in `services/traits.rs`) instead of hand-rolling `generate` + `extract_json` + `serde_json::from_str().unwrap_or_else(fallback)`. It runs the "targeted self-correction" loop: on a parse error or a missing required key it re-prompts the model with the specific error, up to `max_retries` extra attempts. Wired in `reason` `clarify` and `reason_structured`. `extract_json` (in `services/llm.rs`) now picks the **earliest-opening** delimiter, so a top-level `[{...}]` array is no longer mis-extracted to its first object — pass `&[]` for `required_keys` to accept any valid JSON (arrays/scalars).

**Skill registration:** Register to BOTH `tool_registry` (for `tools/list`) AND `skills` vec (for `tools/call`). Forgetting either causes invisible tools or dispatch failures.

**Ollama serves a 4096-token context by default, whatever the model claims.** `gemma4:latest` advertises `gemma4.context_length = 131072` via `/api/show`, but Ollama serves it at `num_ctx = 4096` unless the request sets `options.num_ctx` — and input past the window is **truncated silently**, with no error and no warning. Measured: a 24 000-char prompt came back with `prompt_eval_count: 4096` and the model answered the surviving fragment (a chatty "I have processed the updates, how can I help?") instead of following the instruction, which had been cut off the end. Two consequences: (1) any prompt built from a large `{{_prev}}`, RAG context, or map-reduce chunk may be silently losing its tail; (2) put the operative instruction **before and after** a large payload, never only after. `LlmConfig::num_ctx` (`Option<u32>`, default `None` = leave Ollama's default) sets it per config; only the distiller raises it today (`DISTILL_NUM_CTX = 16384`), since a bigger window costs VRAM on a shared GPU.

**Embedding can fail per-input — never let it be fatal.** Ollama returns `500 … {"error":"failed to encode response: json: unsupported value: NaN"}` when the embedding model emits NaN into the vector (Go's `encoding/json` refuses to marshal it). It is **deterministic for that exact string**, not flaky: reproduced 5/5 on `bge-m3:latest` under Ollama 0.20.7, with a hard boundary at 97 characters for one goal string while unrelated 100+ character strings embed fine. Retrying cannot clear it, so a hard failure here burns all `max_attempts`, dead-letters the job, and takes the owning chain and its Task down via chain-death attribution. Both vector-search entry points in `services/knowledge.rs` therefore degrade to the BM25 `note_content_fulltext` index instead of propagating the error: `search_notes_inner` (`if let Ok(embedding) = …`) and `fetch_similar_notes` (used by `synthesize_knowledge`). Any new embed call site must do the same. The BM25 fallback must run its query through `sanitize_lucene_query` — an unescaped `:` or `/` makes `queryNodes` throw, turning the fallback into a second failure, and goal strings routinely contain colons.

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
