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
| `TZ` | *(unset ⇒ UTC)* | IANA zone the container runs in (`America/Detroit`). Everything the brain **says** about time is local; everything it **stores** stays UTC. Unset means the brain's "today" is UTC's, which is wrong for part of every day outside UTC — see "The brain's sense of time" below |
| `BRAIN_TIMEZONE` | *(falls back to `TZ`)* | Overrides `TZ` for the brain's own clock only, when the image's zone must differ from the brain's |
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
| `SANDBOX_URL` | - | Base URL of the code-execution sandbox (compose: `http://sandbox:8000`). **Unset ⇒ `execute_code` is not registered at all**, so a deployment without the sandbox service lacks the tool rather than failing every call |
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
│       │   │   ├── exec.rs       # Code Execution skill (execute_code → sandbox sidecar)
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
     │    Skill Registry (79 static + N runtime)      │
     │  KnowledgeSkill  TaskSkill  AgentSkill          │
     │  WorkingMemorySkill  DynamicSkill  ModelSkill   │
     │  SleepSkill  ProcedureSkill  SearchSkill        │
     │  SchedulerSkill  CodebaseSkill  ExecSkill  ...  │
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

Template variables in step `arguments`: `{{goal}}`, `{{goal_topic}}`, `{{task_id}}`, `{{date}}`, `{{now}}`, `{{weekday}}`, `{{file_slug}}`. `{{date}}` is the **local** calendar date, `{{now}}` a fully qualified local instant (`Wednesday 2026-08-12 21:22 America/Detroit (UTC-04:00)`), `{{weekday}}` the local day name — see "The brain's sense of time" for why these are local while stored timestamps are UTC. `{{file_slug}}` is a slug derived from goal, used by the UI chain for its workspace file path. `{{goal_topic}}` is the goal with its routing prefix stripped (`"fill knowledge gap: Research X"` → `"Research X"`) — **use it for any step that feeds the goal to a search engine**, since the prefix exists to match a chain and is pure noise inside a query. `goal_topic()` in `services/queue.rs` is conservative: it strips only up to the first colon, only within the first 40 chars, and never on a bare URL or an empty remainder, so ordinary sentence goals pass through untouched. Substitution is **value-level, not text-level**: the stored JSON is parsed first, then `substitute_template_vars()` (in `services/queue.rs`) walks the parsed tree and replaces placeholders inside string values only. This keeps substitution quote/backslash/newline-safe — a `{{goal}}` containing `"` can never corrupt the chain JSON. The same primitive backs chain `{{_prev}}`/`{{result}}` resolution.

**`{{_prev}}` vs `{{_prev.<path>}}`.** `{{_prev}}` pastes the previous step's output as the coordinator extracted it — `extract_result_text` unwraps a JSON envelope's `answer` field and single-column `rows`, so a step after `store_note` receives clean markdown rather than JSON scaffolding. That unwrapping is lossy by design, and `{{_prev.<path>}}` is the escape hatch: a dotted path (`{{_prev.id}}`, `{{_prev.rows.0.content}}`, alias `{{result.id}}`) resolved against the *pre-unwrap* envelope kept in `prev_result_raw`. Rules:

- Both forms may appear in one step; `{{_prev}}` is never a substring of `{{_prev.id}}`, and paths are substituted first.
- Paths resolve against the **raw** envelope and are never distilled — `distill_prev` rewrites prose and would destroy the structure a path indexes.
- An unresolvable path (non-JSON output, missing key, wrong type) becomes the **empty string**, never the literal placeholder — a tool handed `"{{_prev.id}}"` as an id would treat it as a real one. Consumers should read empty as absent; `claim` filters a blank `source_note_id` back to `None`.
- Strings are inserted raw, not re-quoted, so quotes/backslashes/newlines in a field survive intact (same value-level guarantee as the rest of substitution).

**Per-step model routing (Phase 1):** a step may declare `required_capabilities: ["reasoning", ...]`. At execution the model router (`services/model_router.rs`) picks the best catalog model satisfying them within `CLOUD_TIER` and the job's LLM calls route to it via the `SELECTED_LLM` task-local (precedence: capability-selected > `USE_LOCAL_LLM` background pin > active config). Cloud calls keep the 429→local fallback and land in the usage ledger. If no catalog model qualifies the step silently keeps normal routing. Metadata travels as `__required_capabilities` in job args (serde-ignored by tools). Exactly seven steps across `chains/` and `schedules/` declare capabilities; everything else is pinned `provider_hint: ollama`.

**"Best" is `cost ASC, selection_rank ASC, context_window DESC` — and the middle term is the whole decision.** Every local and Ollama-Cloud entry in `models.yaml` costs `0.0`, so cost separates nothing among the models the brain actually routes to, and whatever comes second *is* the selection. Until 2026-08-26 that was `context_window DESC` — a proxy for nothing anyone was optimising.

The consequence was a routing capture. Adding `minimax-m3:cloud` to the catalog on 08-25 with a 524288 window silently gave it **every** `reasoning` step in the system — no config change named it, no log line reported the switch, and the only evidence was in the usage ledger afterwards: background cloud calls moved off `gemma4:31b-cloud` at 15:00 and onto minimax at 17:00, where they ran at **20.2s** against the previous **5.6s**. Nothing was misconfigured; the ordering was answering a different question than the one being asked.

**`minimax-m3:cloud` was removed from the catalog on 2026-08-31 — it left the free tier.** A trial `/api/chat` now answers `402 "this model requires a subscription or extra usage"`, as do `qwen3.5:397b`, `deepseek-v4-pro`, `deepseek-v4-flash`, `mistral-large-3:675b`, and every `glm-5.x` / `kimi-k2`/`k3` name that `/api/tags` advertises. **The seven ollama-cloud entries in `models.yaml` are the whole of what the free tier reaches**, re-verified that day; `/api/tags` listing a model says nothing about whether your key can call it.

Leaving it in was not neutral, and the two defects compounded. `is_subscription_required` matches on `"requires a subscription"`, so a 402 classifies as *unavailable* and falls back to local `gemma4:latest` — the chain **succeeds**, the weaker model writes the report, and it is stored as `source_record` with nothing recording that the intended model was never reached. That is exactly the silent degradation `scripts/pause_cloud_schedules.sh` exists to prevent, arriving through the catalog instead of through an outage. And because the deployed binary predated `selection_rank`, minimax was still the live winner of all seven `required_capabilities: ["reasoning"]` steps — so the *first* thing every resumed schedule would have done is take that path. **A catalog edit needs the binary that reads it: `selection_rank` in the YAML changes nothing until the process parsing it is redeployed.**

`selection_rank` (per model in `models.yaml`, lower wins) makes the choice explicit and reviewable in the file where models are added, rather than emergent from an unrelated number. Bands: `10–99` ollama-cloud (the contested band), `110–199` local (also $0, so they compete and lose at tier 1 by design — ranked anyway because three of them tie at a 128000 window and tier 0 was previously picking between them arbitrarily), `210+` paid providers where cost already separates and rank only breaks same-price ties. Unranked entries sort behind every ranked one and keep the old widest-window order among themselves, so omitting the field degrades to the previous behaviour rather than jumping the queue.

Two things to know before editing a rank. **It is global, not per-capability**, so check everything a model claims: `gemma4:31b-cloud` sits at 30 rather than lower because it is the `vision` winner, and at 30 the two `gpt-oss` entries still take `reasoning` ahead of it — which is what we want, since it is the one cloud model measured to return empty completions. And **a capability with a single holder is only safe while it stays single**: `qwen2.5-coder:7b` holds `computation` alone at rank 150, which loses to every cloud entry, so widening that capability would route `execute_code` steps to a model that narrates arithmetic instead of running it.

Guards, because this was a *catalog* edit whose routing consequence was invisible rather than a code bug: `selection_order_tests` in `repository/src/telemetry.rs` pins the ordering semantics, and `model_config.rs` asserts against the real checked-in `models.yaml` — which model wins `reasoning`/`vision`/`computation` at tier 1, which wins `reasoning` at tier 0, that every entry declares a rank, and that no two share one. Add a model that captures a capability and the build names it before it ever dispatches a job. `resolve_model_config` also logs the winner's `rank` alongside the `runner_up` it beat, so a reorder is greppable at runtime.

**Every LLM call made inside a job is attributed to that job's tool.** `CURRENT_TOOL` (a third task-local in `queue.rs`, scoped alongside `SELECTED_LLM`/`USE_LOCAL_LLM`) carries `job.tool_name` down to `SharedLlm`, which passes it to all three `record_model_usage` call sites. Before this every background call landed with `tool_name IS NULL` — 55% of a 30-day cloud spend, unattributable by exactly the query you run after exhausting a quota. `None` outside a job (a direct MCP call, a startup protocol step) is honest: those rows genuinely have no owning tool.

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
- **A `reason` result's declared limits are hoisted to the front of it.** `annotate_tool_limits` + `reason_limits_marker` (same file) prepend a `[REASON — LIMITS THE TOOL DECLARED ABOUT ITS OWN ANSWER …]` block listing `gaps`, `caveats`, `critic_counter_arguments`, and a `confidence` below 0.5. `prepare_tool_result` is the single entry point that annotates then truncates, and **all four tool-result call sites go through it** — the marker leads, so truncation (which keeps the head) can never drop it.

  Observed 2026-08-23: asked how Nebula fits with Meshtastic and Reticulum, `reason` reported in **five** separate places that it could not establish the integration — `answer`, `caveats`, `gaps`, `critic_counter_arguments`, and `confidence: 0.5`. The reply presented a confident three-tier architecture ending in "Verdict:" and relayed none of them; the architecture was also unbuildable (Nebula is an IP overlay, Meshtastic carries ~1 kbps of non-IP packets, Reticulum has a 500-byte MTU and no IP layer). Adding a `TOOL-RESULT RULE` to `contexts/general.yaml` did **not** fix it — re-tested, the model still discarded all five signals. The information was never missing; it was buried at fields 6 and 9 of a large JSON blob, and the chat model (`gemma4:31b-cloud`) was already reading a 14 k-char, seven-rule system prompt.

  With the marker, the same question produces a reply that opens *"the tool reported that there is no specified technical mechanism for how Nebula integrates with the other two"*, puts its own contribution under a **`Technical Reality (General Knowledge)`** heading, and states the constraint correctly — *"You cannot simply 'run' Nebula over Meshtastic because Nebula's overhead … would exceed Meshtastic's tiny payload capacity."*
- **`search_web` results are prefixed with a numbered source list.** `search_sources_marker` emits `[SEARCH SOURCES — N retrieved …]` followed by `[S1] <title> — <url>` lines. Accepts `link` *or* `url`, since every engine is normalised to `link` except Brave, which still emits `url` — miss that and Brave results are silently uncitable. Capped at 12 sources; an empty result set or one with no links gets no marker, because "nobody has anything on this query" is a legitimate outcome and must not grow an instruction to cite sources that do not exist.

  Same defect, same fix. The CITATION RULE has been in `contexts/general.yaml` in prose since it was written and did not hold: measured 2026-08-24, two `search_web` calls returned directly relevant sources — including `github.com/FreeTAKTeam/Reticulum_Meshtastic_Integration` — and the reply cited **zero** URLs. The marker does more than restate the rule; it makes citing *cheap*, pairing short `[S1]` handles with their URLs so the model copies a token instead of re-extracting a URL from JSON it has scrolled past. Listing them at the head also means the links outlive truncation — under `CLOUD_TOOL_RESULT_CHARS` a long result previously lost its tail entries' URLs entirely, so the sources most likely to be cut were uncitable by construction. After the change the same class of question returns every claim inline-cited (`([okmesh.org](https://…))`), plus an unprompted closing note on what the search did not cover.

  The general lesson, now three times over (truncation, reason limits, citations): **a signal the model must act on belongs at position zero in a loud delimiter, not in a field.** Prompt rules do not reliably beat payload position — and when a rule and the payload disagree about what is salient, the payload wins. The prose rules were kept (they are what make the model search again before declaring the graph empty), but the markers are what make the behaviour survive into the reply. Note the chat loop in `clients/chat.rs` has its own LLM client and does **not** go through `SharedLlm`, so it gets no local fallback — an overloaded cloud model surfaces as an `error` SSE event to the user, which is acceptable for an interactive turn and would not be for a background job.
- **Failed protocol steps log at WARN.** `ProtocolStep::ToolCall` discarded results and logged `is_error` at `debug!` (invisible at `LOG_LEVEL=info`). `contexts/boot.yaml` called `scheduler_control{action:"status"}` — an action that does not exist — on every startup for months while the surrounding `log` steps printed "Scheduler status obtained." Grep for `tool_call FAILED` after editing any protocol.
- **A dropped chat turn is now reported instead of vanishing.** `run()` persisted the assistant reply under `if … && !final_text.is_empty()` with **no else branch**, so a turn that produced no text was discarded by the guard and left no trace anywhere. Observed 2026-08-24: a user asked the brain to create a scheduled task, the message was banked to working memory at 18:49:30, and then nothing — no reply, no tool call, no error SSE, and zero `ERROR`-level log lines all day. The user was left believing the task was being created; it never existed. `forward_chat_events` (`clients/chat.rs`) now owns the detection, because `tx` is moved into that task and dropped when it ends — code after the provider loops in `run()` can no longer reach the client. Three properties make it work: it **holds back every `Done`** and re-emits exactly one at the very end (the client closes its reader on `done`, so an error sent after it is never rendered — a turn must be marked failed *before* it is marked finished); it suppresses the report when an `Error` was already emitted, so a turn that failed loudly keeps its own explanation rather than gaining a vaguer second one; and it always terminates the stream, including for a provider loop that returned without sending `Done`. Unit-tested via `drive_forwarder` over scripted loop output — dropped turn, already-errored turn, and normal turn.
- **An empty completion is retried, not mistaken for a finished turn.** Detecting a dropped turn (above) says *that* a turn produced nothing; it does not say why, and for the most common cause the answer is that the provider returned nothing at all. All four chat loops ended their turn on `if tool_calls.is_empty() { if !content.is_empty() { send } break }` — so a response carrying neither text nor a tool call fell straight out of the loop having emitted nothing. Observed 2026-08-25: a user's message was banked at 14:55:11, the turn dropped, they re-sent the identical message 31 seconds later, and it dropped again. The telemetry ledger recorded both calls as `success=true` with `tokens_out` of **246** and **218** — the model generated a response and none of it arrived.

  Reproduced against `ollama.com` directly, bypassing the brain: `/v1/chat/completions` streamed exactly three chunks — an empty `content` delta, `finish_reason: "stop"`, and a usage block reporting `completion_tokens: 249`. The native `/api/chat` endpoint behaves identically (`content`, `thinking`, and `tool_calls` all empty, `eval_count: 213`), so this is not a parsing bug on our side — there is genuinely nothing in the stream. Rate on that prompt: **3 of 12**. Every run spends 230–290 tokens emitting a tool call; ~75% of the time Ollama's server-side template parser extracts it into `tool_calls`, and ~25% of the time it fails to parse it but strips it from `content` anyway, leaving both fields empty. Intermittent because the model samples at `temperature: 0.7`; the two consecutive drops were not bad luck so much as a 25% coin flipped twice.

  The fix is the same distinction `classify_unavailable` draws in `shared_llm.rs` — *the provider could not answer* is retryable, *the provider answered and declined* is not — applied one layer up. `MAX_EMPTY_COMPLETION_RETRIES` (2) re-sends the **identical** message list; at a 25% rate that takes the user-visible failure to ~1.5%. Two properties matter. Retries consume a slot from `MAX_TOOL_ITERATIONS` (they `continue` the same loop), which is why the cap is small. And when every attempt comes back empty the turn fails **loudly**, with a message that says nothing was carried out — an empty completion is indistinguishable, from the user's seat, from a turn where the assistant quietly did the work, and the turn that exposed this was a request to go and change something. Applied to all four loops (`run_anthropic_loop`, `run_ollama_tool_loop`, `run_ollama_cloud_loop`, `run_text_loop`): fixing one and leaving three is the "labelled in one path, not the other" failure this codebase keeps re-learning. Unit-tested in `empty_completion_tests` against a `wiremock` server replaying the captured SSE — recovery on retry, and a bounded call count with exactly one error when it never recovers.

- **The `/chat` fallback ladder — `chat_fallback_ladder` in `models.yaml`.** Retrying the same model recovers ~75% of empty completions; the rest need a different model. The catalog's ollama-cloud block was also wrong about what the key could reach — `qwen3.5:cloud` and `deepseek-v4-flash:cloud` answered `403 "requires a subscription"`, `ministral-3:cloud` and `nemotron-3-nano:cloud` answered `404`, and a catalog entry that can only fail is worse than a missing one because the router still picks it. All seven entries now in the block were verified with a trial `/api/chat` call on 2026-08-25, and their `context_window`/capabilities come from `/api/show` rather than from guesses (the old `gemma4:31b-cloud` row claimed a 1 000 000-token window; it is 262 144).

  **Chat now runs `gpt-oss:120b-cloud`** (`CHAT_LLM_MODEL` in `docker-compose.yml`), because the ladder is a safety net and the cheaper fix was to stop standing on the one broken rung. The brain's internals stay on `gemma4:31b-cloud` for its vision capability. Note the coupling this introduces: setting any `CHAT_LLM_*` var gives chat its own config handle, so `use_model` from a chat session now retargets the **brain only** — unsetting it puts both back on one model.

- **Running out of tool calls is an outcome, so it gets a wrap-up round (`FINAL_ROUND_NUDGE`).** Swapping chat to `gpt-oss:120b-cloud` made things *worse* before it made them better: 4 of 8 turns failed, none of which the fallback ladder caught. The model is fast and never returns an empty completion — but it **loops**. A trace of one failing turn shows exactly ten tool calls, ending with `search_notes{query:"Tech Dependency Synthesis"}` at four different `limit` values, then nothing. `for _iteration in 0..MAX_TOOL_ITERATIONS` simply fell through, sent `Done`, and the dropped-turn detector reported *"produced no response, and reported no error"* — true, and useless to someone who just watched ten searches go by. The ladder could not help, and correctly did not try: tool calls **had** streamed, so by the `AttemptOutcome` rule the turn belonged to that model.

  The loop now runs one pass beyond the budget with **no tools in the request at all**. That is the load-bearing detail — a model that keeps choosing to search cannot choose to search again, and everything it gathered is already in its context; nudging with the tools still attached just buys an eleventh search. Two related rules: `tool_rounds` counts only rounds that actually spent a tool call, so an empty-completion retry never eats the budget (a provider failure is not the model using its allowance); and if even the wrap-up round produces nothing, the turn reports `NO_ANSWER_AFTER_TOOLS_MESSAGE`, which says the tool calls *did* run and work may have happened — the generic dropped-turn line claims no error was reported, which is wrong once this fires.

  Measured after the change, same prompt, 8 runs: **8/8 answered**, with the wrap-up round firing on **5** of them and recovering every one. Before it, the same model on the same prompt was 4/8. Wired into `run_ollama_cloud_loop` and `run_ollama_tool_loop` (the local rung).

  Measured over the same 12k-token tool-calling prompt, four runs each: `gpt-oss:120b-cloud` 0/4 empty at **1.8s**, `nemotron-3-super:cloud` 0/4 at 12.4s, `gpt-oss:20b-cloud` 0/4 at 8.0s, `minimax-m3:cloud` 0/4 at 11.7s, `nemotron-3-ultra:cloud` 0/4 at 19.4s, `nemotron-3-nano:30b-cloud` 0/4 at 23.4s, and `gemma4:31b-cloud` **1/4 empty**. The bug is one model's, not the platform's — which is also why `gemma4:31b-cloud` is absent from the ladder despite staying in the catalog as the only free-tier vision model. `nemotron-3-super` sits at rung 2 rather than the faster `gpt-oss:20b` because 20b shares 120b's chat template and the failure being escaped is a template-parsing bug; the first fallback should be the rung least likely to fail the same way. Local `gemma4:latest` is last — slower and weaker than every cloud rung, and the only one that still answers when ollama.com is down.

  **Two rungs were dropped on 2026-08-31, leaving five.** `minimax-m3:cloud` left the free tier (402 — see the catalog note above); a rung that can only fail turns one failure into two at the moment something is already broken. `nemotron-3-ultra:cloud` was dropped on latency: the 19.4s above is a small prompt, and on a real 7.8k-char `reason` payload it took **299s**, against the catalog's own `timeout_secs: 120`. A fallback that stalls past the timeout is worse than no fallback, because the user is already waiting on a turn that failed once. It stays in the catalog — on that same run it was the only model to apply a watch report's date gate correctly — it is just never a destination for a failed chat turn.

  **The mechanism is `run_one_attempt`, and the condition is "did anything reach the client" — not "did it error".** A model that streamed a token, a tool call, or a message owns the turn even if it fails afterwards: handing that turn onward would re-execute its tool calls and stream a second answer underneath the first. So the relay forwards events through as they arrive (a working model's tokens are never buffered waiting for a verdict), swallows `Done` (`forward_chat_events` re-emits exactly one for the whole turn — an attempt's `Done` would close the client's reader mid-ladder), and **holds back `Error`**: forwarded at the end if the model delivered, returned unsent if it did not, so a recovered turn is not narrated with the failure that preceded it. Because the test is positional rather than provider-specific, the ladder covers 403s, 429s, 5xx, and unreachable hosts for free, and needed **no changes inside the four provider loops**.

  Each fallback announces itself as a `Thinking` event (`⚙ no response from the previous model — retrying on …`) — a turn that quietly changes model is a turn whose answer cannot be attributed later. `config_for_catalog_entry` (extracted from `model_router.rs`, now shared with capability routing) turns a catalog row into a callable config: endpoint and credentials come from the environment, never from the checked-in YAML. An unknown ladder name is dropped with a warning rather than guessed at — the ladder is walked when something is already broken, and a rung naming a model the provider has never heard of turns one failure into two at the worst moment.
- **A `reason` result that fell back to raw prose now says so.** On a JSON parse failure `reason_structured` re-prompts for plain text and wraps it, which is the right degradation — but it filled the rest of the envelope with **fabricated** values: `confidence: 0.5`, `caveats: []`, `gaps: []`. Empty caveats assert "the model declared no limits" when in fact the model never answered the question, and `0.5` sat *exactly* on the wrong side of `LOW_CONFIDENCE` in `reason_limits_marker`, so the fallback drew no marker at all — every field the marker keys on was empty, and the early return suppressed it. This is the same defect as the evaluator fabricating a 3.0 (see `parse_evaluator_score`): a placeholder that is indistinguishable from a measurement. Measured 2026-08-24: a `reason(structured, run_critic)` call failed all 3 attempts, returned off-topic prose, logged once at **INFO**, and a chat session built a three-stage "intelligence report" on top of it. `ReasonOutput.structured_output_failed` now flags it, the log line is **WARN**, `confidence` is `0.0`, the tool result carries an explicit caveat, and the chat marker fires on the flag *before* the empty-field early return: `[REASON — THE TOOL DID NOT PRODUCE A STRUCTURED ANSWER …]`. The flag is emitted only when true, so a normal result is byte-identical.

The behavioural half lives in the `PROJECT-STATUS RULE` in `contexts/general.yaml`: check schedules *and* tasks *and* notes, treat an alias found in a note as a lead to re-query rather than an answer, and report running work in the present tense.

### Delivering `notify_user` to the UI (`hbi-frontend`)

A schedule that ends in `notify_user` writes an `(:AgentNotification)` and broadcasts `notifications/agent_chat` over SSE; the UI surfaces it twice — as a count badge on the Chat nav button, and as the in-chat `NotificationBanner` with a **Continue conversation** button that opens the `related_session_id` thread. Until 2026-08-20 those two views ran **independent** fetches of `GET /api/notifications?unread=true`, and only one of them refreshed:

- `App.tsx` polled every 30 s and kept a count.
- `ChatPanel` fetched once per `visible` false→true transition and never again.

`ChatPanel` is mounted permanently (hidden with `display: none`) so conversation state survives tab switches, and `chat` is the default tab — so for anyone sitting on the Chat tab, `visible` was `true` from mount and never flipped. The badge counted the new notification; the banner never re-rendered; clicking the already-active Chat tab changed nothing. Same root cause for the History sidebar: `loadSessions()` ran on mount and after a send, so the `todos-<date>` / `news-<date>` session the schedule had just created was not listed either.

The fix is structural, not another poller: **`App.tsx` owns the notification list** (`src/api/notifications.ts` holds the type and both calls) and passes `notifications` + `onDismissNotification` down. The badge is `notifications.length` of that same array, so the two views cannot disagree by construction. `ChatPanel` reloads History whenever the notification-id set changes, and dismissal is optimistic with a `dismissedRef` guard so the 30 s poll cannot resurrect a notification before its `POST /read` lands.

**The SSE push had never worked, for a reason nothing could surface.** `notify_user` broadcasts `notifications/agent_chat` to every session (`notify_all`), but a notification-only POST to `/mcp` returned `200` with a `null` body instead of **`202 Accepted`**. The MCP TypeScript SDK opens its standalone GET SSE stream only on `isInitializedNotification(message) && response.status === 202` (`streamableHttp.js`), so no browser client ever opened the stream and every server-initiated push — `agent_chat` *and* `agent_job` — went to zero listeners. 202 is what the Streamable HTTP spec requires anyway; the notification arm of `handle_post_mcp` now returns it (empty body, session-id header), and `handle_post_mcp` returns `Response` so both arms share a type. Verified with raw curl: the GET stream now receives `event: agent_chat` within a second of the tool call, through the nginx proxy as well.

`App.tsx` subscribes via `onNotification` and calls `getMcpClient()` — registering a handler does not connect anything, and nothing else in the app would have. The 30 s poll stays as the fallback: the SDK's GET stream does not reconnect on its own, so push alone would go quiet after the first drop. Related: `resetMcpClient()` used to `_notifHandlers.clear()`, so the first transport hiccup inside `callTool` silently unsubscribed every panel — it no longer does, since subscribers hold unsubscribe closures and expect delivery to survive a reconnect.

**History labels sessions by kind, not by their first line.** Every agent-created session opens with the same banner — eleven claim sweeps all began `## CLAIM VERIFICATION SWEEP`, every news sweep `## METRO DETROIT — raw search results` — so a sidebar labelled from turn-0 content rendered them as one repeated string. `sessionLabel()` in `ChatPanel.tsx` derives the label from the session id instead:

- The id's prefix names the run (`claims-` → "Claim verification"). `SESSION_KINDS` must stay in sync with `grep -rhoE 'session_id: *"[^"]+"' schedules/ chains/`, longest prefix first so `news-raw-` is tested before `news-`. **Never add a speculative prefix** — a human session that happens to match one loses its real identity. Guessing at `verify-` and `eval-` relabelled four hand-made test chats as "Verification · migratio" before the list was checked against the source.
- Chains name scratch sessions `<kind>-{{task_id}}`, so `list_sessions` resolves that id against `(:Task)` and returns `task_goal` — "Knowledge gap: AI Governance and Biosecurity Protocols" rather than a uuid. The `LIMIT` lands before the `OPTIONAL MATCH`es so the lookup runs for the returned page only. Date-scoped ids (`news-`, `todos-`, `claims-`) match no Task and fall back to the formatted date, which the meta row then omits rather than printing twice.
- Plain chat sessions are left alone: turn 0 is the user's own opening message and is already the best label available.

**Sessions render every role, not just `user`/`assistant`.** `switchSession` filtered the other roles out of `/api/sessions/:id/entries`. Agent-written sessions bank their work under other roles — the daily news chain pushes eight `role: observation` entries into `news-raw-<date>` — so History advertised "8 msgs" next to a session that opened completely blank. Non-`assistant` roles now render with the role name in the message meta, and a `sessionNote` distinguishes "loaded, and it was empty" from "the load failed" instead of both falling through to the generic new-chat empty state.

The meta-lesson, twice over: a UI element that announces state it cannot itself display is the frontend version of the silent-failure class above. Prefer one owner and one fetch over two views that agree only by luck, and never drop data on the floor in a `.filter()` without rendering something that says so.

### ScheduledTask Ownership (`managed_by`)

Built-in ScheduledTask definitions live in `schedules/*.yaml` (seeded by `seed_built_ins` via `schedule_seeder`). There is no hardcoded fallback — a missing `schedules/` directory aborts startup (`std::process::exit(1)`). The graph is always the runtime authority (the scheduler only reads `(:ScheduledTask)` nodes); YAML is the definition source for the tasks it owns. Every node carries a `managed_by` property:

- **`yaml`** — owned by a `schedules/*.yaml` file (matched by exact `name`). Steps, description, and interval are force-synced on every startup, so file edits propagate and runtime edits are overwritten. Legacy nodes without `managed_by` that match a YAML name are claimed as `yaml` at seed time.
- **`runtime`** — created at runtime via `manage_scheduled_task` or `POST /api/scheduled-tasks`. The seeder never touches these. Nodes left unclaimed after seeding are backfilled to `runtime`.

Ownership can be transferred explicitly: `manage_scheduled_task` upsert accepts `managed_by` (`runtime` detaches a task from its YAML; `yaml` hands it back). Updating a yaml-owned task without transferring ownership returns a warning that the change will be overwritten on restart. To make a runtime task durable and reviewable, write a `schedules/*.yaml` with the exact same `name` — the seeder claims and syncs it on the next startup.

**A runtime node shadows its YAML file permanently, and nothing louder than a WARN says so.** Matching is by exact `name`, and `sync_yaml_scheduled_task`'s `WHERE s.managed_by IS NULL OR s.managed_by = 'yaml'` means a runtime-owned node with the same name returns `RuntimeOwned` and the file is skipped — on every startup, forever. The graph is the runtime authority, so the YAML being "the definition source" is only true for the tasks it actually owns: **check `managed_by` before assuming a file is what executes.**

Worked example, resolved 2026-08-27. A chat session created **"Weekly hardware tripwire: …"** as a runtime node on 08-08; `schedules/hardware-tripwire.yaml` was committed on 08-10 under the identical name and was therefore skipped by every startup since — it never ran once. The node's own definition was fine at first (its notes carry the file's `# Hardware tripwire — {{date}}` heading), and it produced three clean weekly reports. Then on **08-25 15:26** another chat session overwrote it with a 6-step version that dropped the baseline retrieval, dropped the WorkingMemory banking (so its synthesis step saw only the last sweep), and routed all six steps to cloud. Between 15:28 and 19:07 the same session created **six** near-duplicate "Secondary Supply Chain …" schedules, three of whose first runs failed. Nothing rejected any of it, and nothing reported it: the schedule kept a plausible name, kept running weekly, and kept writing notes.

The repair was: hand the node back with `SET s.managed_by = 'yaml'` (keeping its `id`, `last_run_at`, and `next_run_at`, so the weekly cadence and the delta chain survive), fold the duplicates' one distinct contribution — substitution and alternative architectures — into the file as a fourth sweep plus a `## SUBSTITUTION OUTLOOK` section, and delete the six. `ScheduledTask` nodes carry no relationships, so deleting one cannot cascade to the Tasks or Notes it produced.

**Two lessons generalise.** A name collision between a file and a runtime node is silent by construction, so `manage_scheduled_task(action=list, verbose=true)` and its `managed_by` column are the only routine way to notice; a schedule that looks right and runs on time can still be running something no file describes. And a chat session that cannot see existing schedules will rebuild them — the six duplicates are the same self-knowledge failure that motivated making `list` a tool at all.

**`enabled` is graph-owned, and that is what makes pausing safe.** `sync_yaml_scheduled_task` writes `steps`, `description`, `interval_seconds`, `managed_by`, and `updated_at` — deliberately **not** `enabled` — so disabling a yaml-owned schedule survives restarts and rebuilds. Do it with a targeted Cypher `SET`, **not** `manage_scheduled_task(action=upsert)`: upsert *requires* `steps` and force-writes description and interval too, so using it to flip one flag means reproducing the whole definition exactly and silently rewrites it if you get it wrong.

`paused_reason` (+ `paused_at`) records *why*, and `manage_scheduled_task(action=list)` emits it for disabled tasks only, so an ordinary listing is byte-identical. A disabled schedule and a paused one are indistinguishable from the outside, and the difference is the entire question a human asks: broken, superseded, or deliberately off and due back? Without a reason the honest answer months later is a guess — and the failure mode this codebase keeps re-learning is that a guess gets reported as a fact, then "fixed" by re-enabling exactly the thing someone turned off.

**`scripts/pause_cloud_schedules.sh {pause|resume|status}`** applies this to a cloud outage. When Ollama Cloud is unreachable, a cloud-routed step does **not** fail — `classify_unavailable` retries it on local `gemma4:latest`, which is right for a chain that would otherwise die and wrong for a report: the weaker model answers, the chain succeeds, and a thinner analysis is stored as `source_record`/`semantic` and read back later as though nothing happened. The script disables only the schedules whose stored steps carry a cloud step, plus the three that spawn Tasks routing to a cloud step in `chains/` (`Daily news analysis` → `fill knowledge gap:`, `Media watch` → `watch video:`, `Brain exercise` → research goals). Everything else already ran entirely local, so pausing it would cost availability and buy nothing. `resume` matches on a `cloud-paused:` marker in `paused_reason`, so it re-enables only what it paused and never sweeps up a schedule disabled for an unrelated reason. Audit the target list against the graph, not the files — `MATCH (s:ScheduledTask) WHERE s.enabled RETURN s.name, s.steps` and look for `provider_hint` other than `ollama` or any `required_capabilities`.

**When the quota goes, check the ledger before reaching for this script — the schedules are usually not what spent it.** The free tier was exhausted on 2026-08-26 07:51 and all eight cloud schedules were paused for five days. They were not the cause. Per-day cloud calls, split by `tool_name = 'chat'` vs everything else:

| day | background | tokens in | chat | tokens in |
|---|---|---|---|---|
| 08-19 → 08-22 | 78 → 125 | 291k → 619k | 0 | 0 |
| 08-23 | 61 | 282k | 7 | 81k |
| 08-24 | 67 | 324k | 36 | 434k |
| **08-25** | 47 | 188k | **260** | **3 170k** |

Background ran at 29–125 calls and 139–619k tokens **per day for nine days without exhausting anything**. Then one day of chat benchmarking — the empty-completion measurements, the fallback ladder trials, and the `FINAL_ROUND_NUDGE` work, all on 08-25 — spent **3.17M input tokens across 260 calls**, 5× the heaviest background day, and the tier was gone by the next morning.

The asymmetry is structural, not incidental: a chat turn carries the full system prompt plus every tool definition and re-sends the whole transcript each iteration (~12k tokens/call measured), while a background `reason` step sends one distilled payload (~4.6k). **One interactive debugging session costs more than a week of autonomous work.** The five standing reports together are 1.4 cloud calls/day.

So the script pauses the cheap thing. Keep it for a genuine *outage* — where the concern is silent degradation, not spend — and for quota, look at `model_usage` grouped by `tool_name` first. Pausing schedules to protect a quota that chat is spending buys ~1% and costs every report.

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

The two watch schedules each bank every `search_web` result into a `WorkingMemory` scratch session and reassemble with `neo4j_query` before the `reason` step — `{{_prev}}` carries only the *previous* step's output, so consecutive searches would otherwise discard all but the last (the `chains/video-learning.yaml` idiom). Step 1 of each pulls the prior run's note so `reason` performs a delta rather than re-describing the world; the retrieval also matches the original hand-written `BASELINE: …` notes by content prefix. The monthly synthesis performs **no** web search by design — it correlates what the other two accumulated, issues a SUPPORTED/NOT SUPPORTED/INSUFFICIENT EVIDENCE verdict, and spawns a `fill knowledge gap:` Task from its weakest link.

The tripwire runs **four** sweeps (memory/HBM, advanced packaging, raw materials/geopolitics, and substitution/alternatives); the SLM watch runs three. The fourth asks a different question from the other three: they measure how tight a bottleneck is, it measures whether the bottleneck can be routed around, and a constrained input with a maturing substitute is a price event where the same constraint with no alternative is a capacity ceiling. It feeds a `## SUBSTITUTION OUTLOOK` section that must answer "no signal" per bottleneck rather than reasoning from general knowledge about the technology — a named alternative with no stated timeline is not a mitigation. This scope arrived from the six duplicate schedules described under ScheduledTask ownership above, and was folded in rather than widened into the raw-materials query: each sweep is deliberately narrow, and one query spanning export controls, memory architectures, and battery chemistries returns worse results for all three.

`schedules/off-grid-networking-monitor.yaml` (weekly, `source_context: off_grid_networking_watch`) follows the same idiom for mesh networking — three sweeps (protocol/firmware, hardware, alternatives) banked into `offgrid-{{task_id}}`, delta against the prior run, stored as `source_record`, then claim-extracted.

**A chat-authored schedule gets no validation whatsoever, and this one is the worked example.** The monitor was created live in a `/chat` session on 2026-08-12 via `manage_scheduled_task`, and `dispatch_one_scheduled_task` enqueues `st.steps` verbatim — no `goal_to_steps()`, no adversarial pre-flight, no evaluator. Nothing anywhere checks that a step's arguments make sense. Its three steps were each independently broken, and its first run is the proof:

- **`reason` with a `question` but no `context`.** The step ran RAG over the graph instead of over the sweep it had just fetched, and answered *"there is no information regarding new hardware releases or specific protocol updates"* — written seconds after `search_web` returned 5 results. This is the same contamination guard the other watch schedules carry a comment about; a chat-authored step carries no such institutional memory.
- **`store_note` content with no `{{_prev}}`.** The argument was the literal string `"Updates on off-grid networking research for {{date}}."`, so the stored note *was* that one sentence — 55 characters. The findings were discarded on every run. A `store_note` step that does not interpolate its upstream is indistinguishable, at creation time, from one that does.
- **`semantic` with no `source_context`.** The placeholder accumulated as knowledge the brain established, and step 1 of the next run had no key to retrieve its predecessor by, so "delta vs prior run" could never work.

Net effect: a weekly search plus a cloud `reason` call, burned to store a placeholder, indefinitely and silently. The rewritten version produces a 2 455-char cited report and 7 claims with `ASSERTED_IN` provenance from the same inputs. The general rule this argues for: a schedule created from chat should be written to `schedules/*.yaml` and handed to `managed_by='yaml'` promptly, because the file is where the review happens — the graph will run anything.

**Chain linting (`lint_chain_steps` in `skills/scheduler.rs`).** Deserializing steps as `Vec<ChainStep>` proves the JSON is a legal chain and nothing more — the broken monitor passed that check on the way in. The lint covers the difference: a `reason` step past position 1 with no `context`, a persisting step (`store_note`/`push_context`/`write_workspace_file`/`create_task`/`notify_user`) past position 1 whose arguments never mention `{{_prev}}`, a `store_note` with no `source_context`, a `store_note` typed `semantic` in a chain that searches the web, an unregistered tool name, a chain that never persists anything, and **a step with no `provider_hint`**. `references_prev` accepts `{{_prev`, `{{_prev.<path>}}`, and the `{{result` alias — flagging a dotted path would push authors to delete the very thing that builds the `ASSERTED_IN` edge.

The `provider_hint` check earns its place because the field's default is the opposite of what it looks like. `use_local` in `queue.rs` is `provider_hint == Some("ollama")`, so **omitting** the hint is not "default to local" — it falls through to the active config, which is a cloud model. Chat-authored schedules essentially never set the field, so every step of one lands on cloud: the six duplicate supply-chain schedules and the clobbered hardware tripwire put `search_web` and `store_note` calls on a cloud model that had no reason to be involved. Only *omission* is flagged — an explicit `ollama-cloud` is a decision someone made, and faulting it would train authors to delete the field the warning is asking for. The same trap catches anyone *auditing* routing: a predicate like "cloud if `provider_hint` is present and not `ollama`" reads every chat-authored schedule as local. Query it as `provider_hint <> 'ollama' OR provider_hint IS NULL`.

It runs in two places. `manage_scheduled_task(action=upsert)` returns `step_warnings` + `needs_fix` + an `action_required` line telling the caller to fix and re-upsert *before* reporting success — the schedule is still saved, because these are **warnings, not rejections**: a first step may legitimately store a literal and a lone `reason` step is legitimate, so a false positive that blocks a valid schedule costs more than a warning an author dismisses. `action=audit` runs the same function over every stored task and reports a `degraded` bucket — tasks whose tool names all resolve but whose steps discard their inputs, which is precisely the class that produces no error and no output. Audit passes an empty `live_tools` to the lint so the dead-tool finding is not double-reported alongside `dead_tools`.

The behavioural half is the `BUILD RULE` in `contexts/general.yaml`: a note named after a mechanism is not that mechanism (use `manage_media_source` for the watch list); writing down an intention changes no behaviour (behaviour lives in chains, schedules, profiles, and code — name the one you edited or do not claim it); and a schedule's steps run verbatim, so `step_warnings` must be fixed before the schedule is reported as working. A companion `CITATION RULE` covers the fourth failure from the same session — `search_web` returned real YouTube URLs and the reply rendered them as search terms to go and look up, discarding the provenance and leaving nothing any later step could act on.

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
- **Claim kind separates "was asserted" from "is true".** Extraction classifies each claim as `event` (a plain occurrence with nothing contested behind it), `attribution` (a named party said/demonstrated something — confirming it establishes only that they did), or `mechanism` (the causal/efficacy assertion itself). A corroborated `attribution` renders as `corroborates that it was asserted, not that it is true`. Without this, "corroborated" next to a claim about psionic summoning reads as endorsement of efficacy, when all that was corroborated is that a group said they did it. The prompt gives a **decision rule**, not definitions — *"if this claim were fully confirmed, would that settle whether the underlying phenomenon is real?"* — because definitions alone had the model classifying "Group G demonstrated technique T" as `event` (their doing so *is* an event) and losing exactly the distinction the category exists for.
- **Corroboration is described, not ranked.** `classify_domains` splits corroborating domains into `primary sources` (institutional — `.gov`/`.mil`/`.edu`/`.gov.uk`, decided mechanically from the domain, no editorial judgement), `established sources` (on a curated `:SourceList`), and `unclassified`. The tier rides in the label: `[CLAIM · corroborated · primary sources · …]` vs `[CLAIM · corroborated · unclassified sources only · …]`. Verification is **never gated** on tier — doing so would encode "mainstream equals true" and make niche-but-accurate sources permanently unverifiable, which is its own censorship. `unclassified` means "not on our list", not "unreliable". Note the first version tiered on `:SourceList` alone and labelled a Congressional-hearing claim corroborated by `congress.gov`/`house.gov` as "unclassified sources only" — the lists were curated for *search restriction*, not classification, and a mislabel like that is worse than no label.
- **Independence is checked, not assumed.** `check_independence` requires `MIN_INDEPENDENT_DOMAINS` (2) distinct non-self-referential domains before support counts. It rejects the circular case — a claim about Skywatcher "corroborated" by `skywatcher.ai`. **It does not solve source independence**: five topic-aligned outlets republishing one origin pass any count-based test, observed live. Corroborating domains are recorded on the edge so this stays inspectable rather than hidden behind a status word. Contradiction is deliberately *not* gated — gating it would bias the system toward belief.

- **The verification sweep needs a cursor, because "no evidence" is a stable state.** `claim(action=verify)` batches through the backlog via `unverified_claims`, which ordered by `created_at ASC LIMIT 8`. Finding nothing correctly leaves a claim `unverified` (absence of evidence is not refutation — see above), so the oldest 8 re-qualified on every run and were re-selected **forever**, permanently head-of-line blocking everything behind them. Measured 2026-08-18: twelve consecutive 6-hourly sweeps each processed the *same eight* claim ids, attached zero edges, and reported success, while 465 other claims were never attempted once — corroboration frozen at exactly 17/482 for weeks. Nothing errored, so it read as a slow backlog rather than a deadlock. `last_verify_attempt_at` (native `DATETIME`) is now the cursor: `mark_verify_attempt` stamps it **before** each attempt — `verify_one` has several early returns that don't change `claim_status`, so stamping only on success would re-create the deadlock — and selection orders on `COALESCE(last_verify_attempt_at, epoch) ASC`. The COALESCE is load-bearing: Neo4j sorts NULL *last* in ascending order, which would put never-attempted claims behind attempted ones. Rotation doubles as the cooldown (8 per 6h ⇒ a full cycle takes weeks), so there is no separate retry gate. The general lesson: any sweep whose "nothing found" outcome leaves the selection predicate unchanged will re-select the same rows until something records the attempt.
- **A sweep result must name the claim, not just its id.** `verify_one` returned `claim: <text>` on its two success paths but a bare `claim_id` on all five early returns — no evidence, search unavailable, search failed, evidence unstorable, assessment failed. Those are precisely the outcomes a human reads, and `schedules/claim-verification.yaml` banks the raw result into `claims-<date>` as the *only* surface where verification is visible. During the SearXNG outage above, every sweep therefore rendered as a page of eight UUIDs against `"verdict": "no_evidence_found"` — nothing that can be read, discussed, or acted on, and no way to tell which claims had gone unchecked. Every return path now carries `claim`. The rule generalises to any batch tool whose output a human is expected to read: **a result that says nothing happened must still say what it was that did not happen.**

- **`unsourced_synthesis`: fixing a chain does not fix what it already wrote.** Before 2026-08-10 `chains/fill-knowledge-gap.yaml` searched only internal notes — it could not fill a gap by construction — and stored the model's own prior as `semantic`, i.e. knowledge the brain established. The chain was fixed; the 380 notes it had already written stayed exactly as they were, and because `label_claims` labels **by type**, they kept reaching reasoning unlabelled and indistinguishable from cited material. Measured at migration: 380 parents, **0** containing a URL, 0 with a `source_context`, none carrying the fixed chain's `## ANSWER`/`## STILL UNKNOWN` headings. The 648 written after the fix are 95% cited and were left alone — the split is clean at the fix date, which is what makes the predicate safe.

  The cost was a full laundering cycle. `schedules/tech-dependency-synthesis.yaml` ran on 2026-08-08, honestly returned `VERDICT: INSUFFICIENT EVIDENCE … it cannot establish a causal link or a correlation`, and — working as designed — spawned a gap task from its `WEAKEST LINK: Correlation between HBM/CoWoS shortages and the deployment rate of models under 10B parameters`. The pre-fix chain answered that from general knowledge in a 3297-char essay opening *"a **clear correlation**"*, with no source of any kind. On 2026-08-24 a `/chat` session searching that exact phrasing got the essay as its **top hit** (its title contains the query terms verbatim) and reported it as a confirmed NIA finding: *"Status: Established Trend — Confidence: High"* — inverting the verdict that had spawned it, and carrying across a "Confidence: High" that in the source was attached to the insufficiency. **Uncertainty became a gap task, the gap task produced unsourced prose, and the prose came back as the answer to the question it had failed to settle.**

  `unsourced_synthesis` is a retrieval-labelled type (`[UNSOURCED SYNTHESIS — the brain's own reasoning, cites no source; not established knowledge · date · age]`) and is excluded from consolidation source selection. Both halves are required: consolidation drops the label along with the type, and 71 `consolidated` notes had already absorbed these before the filter existed — that contamination is **not** undone by retyping, since it lives in rewritten prose. Chunks are retyped alongside their parents, because `(:Note)-[:PART_OF]->(:Note)` children are independently retrievable and 794 unlabelled fragments would reproduce the exact "labelled in one path, not the other" failure that made the first claim-labelling pass ineffective. Migration: `scripts/migrate_unsourced_gap_notes.cypher` (idempotent, type-only — no content, embedding, or edge is touched, since provenance is the thing being repaired).

  The general rule: **a data fix and a code fix are separate deliverables.** Correcting the writer leaves every prior write in place, and where retrieval keys on a property the writer set, the old rows keep their old behaviour indefinitely.

Known gaps: extraction does not distinguish "X was asserted" from "X is true"; `learn_chain` notes (248) are still typed `semantic`. The 25 claims extracted before `{{_prev.id}}` existed are not backfilled — a claim's source note can only be guessed from `source_context` plus timestamp proximity, and a wrong provenance edge is worse than a missing one.

**`ASSERTED_IN` from a chain — fixed.** Chain-extracted claims used to carry `source_context`/`asserted_by` but no edge to the note they came from, because `extract_result_text` unwraps a result envelope's `answer` before it is stamped onto the next job, discarding `store_note`'s `id` upstream of any substitution. Two changes close it, and both were needed:

- `AgentJob.prev_result_raw` (Neo4j `prev_result_raw_json`) keeps the *structured* envelope alongside the unwrapped text, populated only when extraction was actually lossy (`structured_prev_to_preserve`) so the payload is not duplicated on every chained job.
- `{{_prev.<path>}}` resolves against that envelope. `{{_prev}}` still yields the unwrapped body, so `text: "{{_prev}}"` and `source_note_id: "{{_prev.id}}"` sit side by side in the same step — wired into `chains/learn.yaml`, `chains/video-learning.yaml`, `schedules/daily-news.yaml`, and `schedules/slm-benchmark-watch.yaml`.

**The `claim` tool now echoes its input as `answer`.** Tool results that a chain passes through must be transparent: `store_note` and `notify_user` already do this, and `claim` did not, so inserting a claim step mid-chain silently replaced the payload with `{"stored":N,"claims":[…]}`. This was live but not yet triggered — the claim step had been seeded into `schedules/daily-news.yaml` between `store_note` and `notify_user`, and the next run would have delivered claim metadata to the user as the daily brief. Any new tool that is safe to insert mid-chain needs the same echo.

**The schedules predate this layer, and that is where it leaked.** The epistemics work covered the *ingest* paths (video summaries, news) but not the standing investigation schedules, which kept writing `note_type: semantic` — i.e. "knowledge the brain established". Observed end-to-end on 2026-08-10: `schedules/slm-benchmark-watch.yaml` ran unrestricted, took a **Facebook group post** as its sole source, promoted **ToRA-7B** to `## MOVERS` as a new addition, and stored the report as `semantic`. A chat session then retrieved it unlabelled and relayed it as an NIA finding — *"ToRA-7B has emerged as a powerhouse"* — with strategic recommendations attached. The model is real; it is also from September 2023, and its headline "+22% over the top open-source model" was a Sept-2023 comparison quoted as a current standing. Nothing in the pipeline was equipped to notice, because `## DELTA vs PRIOR LIST` never asked for a release date.

Four changes, all of which generalise to the other standing schedules:

1. **`note_type: source_record`, not `semantic`.** Nothing in a watch report is knowledge the brain established — every figure is a benchmark number some page asserted. As `source_record`, `label_claims` prefixes it on retrieval with the explicit not-verified marker, which is what stops the relay-as-fact.
2. **Claim extraction step.** A benchmark score is precisely the shape the claims layer exists for: attributable, dated, corroborable against an independent domain. Without it the report is one undifferentiated blob retrieval can only accept or reject wholesale.
3. **A `source_list`.** New `sources/ml-research.yaml` (22 domains: preprints, weights registries, leaderboards, inference runtimes, first-party lab announcements). Not a quality judgement about other sites — a scope judgement: a model release has a paper, a weights repo, or a registry entry, and reposts on social feeds carry no date and rank by engagement.
4. **A recency gate in the prompt.** MOVERS now requires a release date, demotes anything over 12 months old or undated to WATCH, and must render aged comparisons as *"+N% vs. the SOTA of \<date\>"*. `{{date}}` is substituted so the model has today's date to compare against.

Chain-ordering note: `store_note` returns `{"id":…,"answer":…}`, so a step chained directly off it receives that JSON wrapper, not the report. The watch banks the report to its `slmwatch-` WorkingMemory session and re-queries it (`WHERE w.content STARTS WITH '## REPORT'`) before both `store_note` and `claim`, keeping each on clean text — the same banking idiom the sweeps already use.

### Retrieval evaluation harness (`services/retrieval_eval.rs`, `eval-retrieval` CLI)

Every knob in `search_notes_inner` — the `0.7/0.3` freshness weights, RRF `k=60`, chunk→parent resolution, the candidate pool size, and the choice of embedding model — was set by intuition and there was **nothing that measured whether changing one helped**. So every retrieval "improvement" was an argument, not a number, and the freshness weights in particular have never been validated against anything. This harness is the missing instrument: it runs a **golden set** of `(query → note-that-should-come-back)` judgements (`eval/retrieval_golden.yaml`, human-owned and version-controlled) through the *real* pipeline and reports **recall@k** and **MRR**, per case and in aggregate.

- **Run it:** `cargo run -- eval-retrieval [--fixture …] [--k 10]`. Standalone like `repair-notes` — connects to the live graph, no MCP server. **`--bootstrap N`** instead samples N distinctive notes and prints *proposed* cases to curate (the query is a phrase lifted from the note, which tests only the retrieval floor — rewrite each into a real question before trusting the numbers). This exists because writing the first 30 cases by hand is the friction that kills eval harnesses.
- **Measurement must not perturb what it measures.** `search_notes` *writes* on every call — it bumps `access_count`/`last_accessed_at` and advances the spaced-repetition schedule, which is exactly the freshness signal `apply_freshness_boost` reads back. An eval loop calling it would move its own ranking input, so a new non-perturbing path — `search_notes_readonly` → `search_notes_inner(track_access=false)` — was added and is the only thing the harness calls. Every other caller passes `track_access=true`, so normal retrieval is byte-identical.
- **A case matches if any `expect_ids` OR `expect_substrings` (case-insensitive) lands in top-k.** Prefer substrings: a note id churns on re-ingest/re-embed, but a distinctive body phrase survives exactly the change (e.g. swapping the embedding model) you most want to keep measuring across. A case with neither expectation is reported malformed and excluded from the score, never silently counted as a miss.
- **Retrieval tuning via the harness, 2026-08-31: recall@10 went 0.840 → 0.920 and MRR 0.47 → 0.76, and none of it was the embedding model.** The path there is the point. (a) A freshness-weight sweep on the original pipeline was a **null result** — `RETRIEVAL_RRF_WEIGHT` ∈ {0.5,0.7,0.85,1.0} × `RETRIEVAL_RECENCY_TAU_DAYS` ∈ {30,90,180} left recall@10 flat at 0.840 and MRR within one-rank noise, and freshness fully off (`w=1.0`) was identical to the default. Structural reason: `apply_freshness_boost` ran *after* `rrf_merge` truncated to top-k, so it could only reorder within the window, never promote from below it. A widening-k probe confirmed the misses were retrievable but mis-ranked: recall@25=0.92, @50=0.96, **@100=1.00**. So the bottleneck was ranking, not recall and not the embedder. (b) **Fix 1 — widen the re-rank pool.** `pool_limit` is now always `fetch_limit` (≈3×k), so RRF+freshness+chunk→parent see the whole candidate pool and the single `merged.truncate(limit)` after the note_type filter is the only cut to k. On its own this made things *worse* (recall 0.64), which exposed a latent bug rather than causing one. (c) **Fix 2 — the real bug: chunk→parent resolution was discarding the ranking.** Step 3.5 rebuilt the list from the parent-lookup query's rows, which come back in Neo4j's arbitrary storage order — so the RRF+freshness rank was thrown away and the list rebuilt in storage order. Invisible while the pool in equalled the output `limit` (same members survived a reorder), fatal once the pool was widened (it truncated to an arbitrary top-k), and all along it was silently depressing MRR by handing the caller storage order instead of rank order. It now builds a `hit_id → parent` map and walks `merged` in ranked order. Together (b)+(c): **recall@10 = 0.920, MRR = 0.741** at the default weights. (d) With the pipeline correct, the freshness sweep is finally live and shows a genuine optimum: too much freshness hurts (`w=0.5` → recall 0.88 / MRR 0.66), a little helps (`w=0.85` → recall 0.92 / MRR **0.762**, marginally beating pure RRF's 0.722), and `tau` is second-order (0.759–0.762 across 15–180 days). The `w=0.85` edge over 0.7 is ~one rank on one case across 25 cases — inside the noise — so the **default stays 0.7 / 30** pending a larger golden set; grow it before fine-tuning `w`. Two misses remain (a very short Reticulum note; a five-month-old self-assessment), both plausibly rank-quality cases where a stronger embedder or a reranker — lever (3) — is now the *next* thing to justify with the harness, not the first. `RETRIEVAL_RRF_WEIGHT` / `RETRIEVAL_RECENCY_TAU_DAYS` are read in `apply_freshness_boost`; unset leaves 0.7 / 30, so production is byte-identical to these defaults.
- **Two caveats for interpreting a run.** Query embeddings need Ollama reachable; with it down, vector search degrades to BM25-only (by the NaN-fallback design in `search_notes_inner`) and the run measures the *BM25 path*, not the hybrid one — so check the embed endpoint before trusting a number. And the **running container carries whatever binary it was built with**: a code change to retrieval is not measured until the image is rebuilt, the same `selection_rank` trap the catalog note describes.
- **Bootstrap incidentally surfaces graph hygiene:** a random sample is dominated by `Scheduler dispatched task (id: …)` episodic log notes, i.e. operational noise that dilutes retrieval. Worth a separate look at whether those belong in the `:Note` vector space at all.

### The brain's sense of time (`services/clock.rs`)

Two rules that pull in opposite directions, and the brain needs both:

1. **Everything stored stays UTC.** `created_at`, `next_review_at`, `asserted_at`, job timestamps. A graph with mixed local and UTC timestamps cannot be ordered, and the breakage shows up twice a year at a DST boundary.
2. **Everything *said* is local.** A date in a prompt, a search query, or a report is a statement about the user's day, not UTC's.

Until 2026-08-12 the brain did only (1) and used it for (2) as well: every `{{date}}` and every "Today's date is …" came from `Utc::now()`, in a container with **no `TZ` set at all**. In `America/Detroit` that means the brain rolls over to tomorrow at 20:00 local. Measured that evening: host `Wed Aug 12 21:22 EDT`, container `Thu Aug 13 01:22 UTC`. So for the last four hours of every day the brain dated its notes to tomorrow, had `schedules/daily-news.yaml` search for tomorrow's news, and told anyone who asked in `/chat` that it was tomorrow — confidently, because nothing in the pipeline had a second opinion about the date.

Local time needs **both halves**: `TZ` in the container environment *and* code that asks for local rather than UTC. Set only the first and every call site still returns UTC; do only the second and `chrono::Local` silently resolves to UTC. Because that failure is silent in both directions, `log_resolved_timezone()` runs from `main.rs` at startup and **warns** when no zone is configured — an accidental UTC is indistinguishable from a deliberate one at every later call site, so it gets named once, where it can be checked.

- **Resolution order:** `BRAIN_TIMEZONE` → `TZ` → the `/etc/localtime` symlink target → `UTC` (flagged unconfigured). Resolved once in a `OnceLock`; the environment does not change under a running container.
- **`TZ` is set on `agent-brain` only, deliberately not on `neo4j`.** Neo4j's `datetime()`/`localdatetime()` follow its own `db.temporal.timezone` setting rather than `TZ`, and the only thing a container `TZ` would change there is log timestamps — not worth touching the store's temporal behaviour for.
- **The four date-producing call sites are now local:** the chat base prompt (`clients/chat.rs`), both scheduler substitution sites (`services/scheduler.rs`, for one-shot chains and for ScheduledTask dispatch), and the constructor's step substitution (`skills/constructor.rs`). `services/sleep.rs` keeps UTC for dataset **filenames**, which want to be stable and sortable, not local.

**What "an accurate sense of time" means beyond a correct date string.** The old prompt said `Today's date is 2026-08-13.` — a date with no clock, no weekday, no zone, and no way to tell how stale anything retrieved was. Three additions:

- **A fully qualified instant, rebuilt every turn.** `now_stamp()` renders `Wednesday 2026-08-12 21:22 America/Detroit (UTC-04:00)`. Zone name *and* numeric offset are both present on purpose: the name alone requires the reader to know the current DST state, the offset alone loses which zone it is. `build_base_system_prompt()` is called per turn from `run()` and is not cached, so a long session stays correct across a local midnight. The prompt also carries the current UTC instant and states that stored timestamps are UTC, so the model converts instead of quoting a stored value as a local one.
- **`{{now}}` and `{{weekday}}` template vars** alongside `{{date}}`, on both scheduler substitution paths. A schedule that needs to reason about time of day or day of week previously had no way to know either.
- **Relative age on retrieval.** `label_claims` already selected `created_at` and rendered it as a bare date; it now appends a humanised age (`age_from_iso` → `11 months ago`). This is the missing half of the ToRA-7B failure documented above: a Sept-2023 benchmark result was relayed as a current standing, and an absolute date only helps a model that knows today's date and does the subtraction. `humanize_age` names a future timestamp rather than rendering it as an enormous positive duration, and `age_from_iso` returns `None` on anything unparseable — a guessed age is worse than no age.

The behavioural half is the `TIME RULE` in `contexts/general.yaml`: never state a date from recollection, never carry one across a possible midnight, and treat a note's label age as being about the *note* — a stale figure stated confidently is still stale.

### Tool-integrated reasoning (`execute_code` + the `sandbox` sidecar)

A language model asked for a shortage date, a capacity delta, or a percentage produces a fluent, confident, unchecked number. Nothing downstream can catch it: the evaluator greps for `Score: N/5`, consolidation rewrites prose, and retrieval labels provenance — none of them re-derive arithmetic. `execute_code` closes that hole by letting a step *compute* its figures and read the output, so a wrong number becomes a traceback instead of a sentence.

This is the transferable result from the ToRA paper (arXiv 2309.17452), which the 2026-08-08 SLM benchmark watch surfaced as a model recommendation. The recommendation did not survive checking — ToRA-7B is a Sept-2023 LLaMA-2 derivative with a 4096 context, absent from the Ollama registry, and its 44.6% MATH score is a **tool-integrated** number produced by interleaving reasoning with executed Python. Adopting the weights without an executor buys nothing; building the executor helps every model in the catalog. See "Epistemics" below for how a Facebook-group post became a MOVERS entry in the first place.

- **Execution never happens in the brain process.** `skills/exec.rs` is only an HTTP client for the `sandbox` compose service. The isolation contract lives in `docker-compose.yml`:
  - `sandbox-net` is **`internal: true`** — no default gateway, therefore no egress. This is the only real boundary; the per-run limits inside the container are runaway guards, not security.
  - **No credentials, no bind mounts.** The service gets no `env_file`, so `NEO4J_PASSWORD` / `OLLAMA_API_KEY` / `GITHUB_TOKEN` are unreachable from submitted code, and neither the repo nor the workspace is mounted.
  - `read_only: true` root, `cap_drop: ALL`, `no-new-privileges`, `mem_limit: 2g`, `pids_limit: 128`, tmpfs `/tmp` mounted `noexec`.
  - **Do not put `sandbox` on `brain-internal` and do not add a `ports:` mapping** — either restores the egress this design removes. Verified at build time: DNS resolution fails for both `example.com` and `neo4j`.
- **Per-run limits** (`sandbox/server.py`): `os.setsid()` + process-group SIGKILL on wall-clock timeout (default 30 s, max 120 s), `RLIMIT_AS` 2 GB, `RLIMIT_CPU` 60 s, `RLIMIT_FSIZE` 32 MB, `RLIMIT_NPROC` 64, a scrubbed 6-key environment, `python3 -I` isolated mode, and a fresh scratch cwd deleted after each run. Output is capped at 64 KB **keeping the tail**, because the traceback and the final printed result both land at the end.
- **A failed run is a successful tool call.** Non-zero exit and timeout return `isError: false` with the traceback in `stderr` and `success: false` in the payload. This is load-bearing: a tool *error* burns a retry and can dead-letter the job and — via chain-death attribution — fail the owning Task, whereas a traceback handed back to the model is exactly what it needs to fix the code and call again.
- **No state between calls.** Each run gets an empty directory and keeps nothing, and the sandbox cannot fetch anything, so every input must be passed inline in the code. The tool description says so explicitly.
- `numpy`, `sympy`, and `pandas` are installed for submitted code. `sympy` matters most — symbolic algebra is where prose arithmetic fails worst and an exact solver wins outright. pip runs at **build** time only; at run time there is no network to install anything.

**Routing:** `models.yaml` defines a `computation` capability held by **`qwen2.5-coder:7b` alone**, which is what makes its `selection_rank` of 150 harmless — with one holder there is nothing to tie against. Widen the capability and rank starts deciding, and at 150 this model loses to every cloud entry, so a step meant to run Python would route to one that narrates arithmetic instead. Treat the capability as a routing selector, not a description of aptitude; `computation_still_routes_to_the_code_model` in `model_config.rs` fails the build if this stops holding. VRAM: 4.68 GB on an 8 GB card already holding gemma4 + Whisper, so it swaps rather than co-resides; keep computation steps rare and batched.

**Behavioural half:** the `COMPUTATION RULE` in `contexts/general.yaml` — any reported number that came from a calculation must come from an `execute_code` run whose output was read; assumptions go in named variables at the top of the program; quoted figures are attributed, not recomputed. `execute_code` is in the `general` and `self-analyst` allowlists (note `MCP_TOOL_PROFILE=general` also bounds `tools/list`).

### Web Search: engine failover ladder + usage ledger

`search_web` does not have "a search engine" — it has an ordered **ladder** of them (`SEARCH_ENGINE_ORDER`, default `searxng,google,serpapi,brave`). It walks the ladder until one engine answers **with results**, so a single dead provider degrades quality instead of failing the call.

This exists because of a concrete outage: on 2026-08-08 the SerpApi free tier hit `429 "Your account has run out of searches."` and, because `search_web` hard-failed on its one default engine, it took **39 jobs and 38 tasks** with it and stopped the daily news brief for two days. The arithmetic was never survivable — `schedules/daily-news.yaml` alone issues 8 searches/day (~240/month) against SerpApi's 100/month cap, and measured total volume is 11–20/day.

- **`searxng` leads by design.** A [SearXNG](https://docs.searxng.org/dev/search_api.html) sidecar (compose service `searxng`, config in `searxng/settings.yml`) aggregates ~70 upstream engines behind one JSON API with **no key and no quota** — the only rung that cannot exhaust. It is deliberately **not** published to a host port; it lives on `brain-internal` only, which is what makes the disabled bot `limiter` in its settings safe. Do not add a `ports:` mapping without re-enabling the limiter. The settings file must keep `json` in `search.formats` — the upstream default enables `html` only, and its absence makes `?format=json` answer 403.
- **Requested engine ≠ only engine.** Passing `engine: "serpapi"` promotes it to the head of the ladder but does not truncate the rest: a caller wants an answer more than it wants a specific provider.
- **Quota cooldown.** An engine that reported `quota_exhausted` within `QUOTA_COOLDOWN_HOURS` (6) is moved to the *back* of the ladder rather than dropped — a daily quota recovers overnight on its own, but must not cost a wasted round-trip on all eight searches of the news chain until it does. Ordering rules are pure (`order_engines` in `skills/search.rs`) and unit-tested.
- **Result normalisation.** SearXNG's `url`/`content` fields are mapped to the `link`/`snippet` shape SerpApi and Google CSE already emit, so downstream `reason` steps see one schema regardless of which rung answered. (Brave still emits `url`/`description` — the `source_list` post-filter accepts both.)
- **Usage ledger.** Every engine *attempt* writes a row to the DuckDB `search_usage` table (engine, query, success, result count, duration, `error_kind`) — a failover that tries two engines writes two rows, because quota accounting needs to know what each engine was actually asked to do. `get_search_usage` reports per-engine totals plus a per-day breakdown; default window is 720h (~30 days) because monthly caps are the ones that bite.
- **Zero results is not an answer.** A rung that returns `200` with an empty result set falls through to the next one, and `result_count` treats unparseable output as zero for the same reason. If *every* rung comes back empty the call returns `[]` rather than an error — "nobody has anything on this query" is a legitimate outcome, and erroring would burn the job's retries and fail the owning Task through chain-death attribution. Falling through is logged at WARN (`Search engine returned zero results`, then `Search ladder fell through at least one engine` listing what each rung said), because the whole failure mode here is one that produces no error.

**The second outage, 2026-08-18 → 08-20, is why that rule exists.** The SearXNG container lost DNS entirely — `getent hosts google.com` failed inside it, every upstream answered `HTTP connection error`, and SearXNG dutifully returned a well-formed `{"results": []}`. Because that is not an *error*, the ladder stopped at rung one and never tried the keyed engines. `search_web` returned `[]` for **every query for three days**: the daily news brief produced "No news available for this date" on 08-20 and never completed at all on 08-18 or 08-19, and the 6-hourly claim verification sweep recorded `no_evidence_found` against its entire backlog — an outage that looks exactly like a quiet news day and an uncorroborated backlog. The usage ledger recorded all 803 of those as **successes**, because they were, at the HTTP level.

Two lessons, both general. An engine that cannot fail loudly will fail quietly, so "did this rung actually produce anything" has to be part of the success test, not just "did it return". And recreating the container fixed the DNS (its `resolv.conf` points at the host's Tailscale resolver, `100.100.100.100`; the brain container on the same bridge was unaffected) — root cause unresolved, so **it can recur**; the WARN lines above are the detection.

**Right now SearXNG is the only working rung.** Measured 2026-08-20: `google` → `GOOGLE_API_KEY not configured`, `brave` → `BRAVE_API_KEY not configured`, `serpapi` → `429 account has run out of searches`. The ladder is four names deep and one engine wide, so the fall-through has nowhere to fall. Configuring Google CSE (100 free queries/day) is what would make the ladder actually redundant.
- **Quota exhaustion now alerts.** `is_quota_exhausted_error` in `services/queue.rs` is a deliberate subset of `is_transient_infra_error`: meta-learning is still skipped (the brain cannot reason its way out of a billing cap) but the coordinator now raises a deduped `:AgentNotification` (once per tool per 24h) instead of logging "skipping meta-learning" and going quiet. Silence is what turned a spent quota into a two-day outage.

### SourceLists (approved-domain lists for `search_web`)

`(:SourceList {name, domains, description})` nodes restrict `search_web` results to approved domains (the tool adds `site:` operators and post-filters results). Definitions live in `sources/*.yaml` (`name`, `description`, `domains`) and are seeded by `source_seeder` **ON CREATE only** — unlike schedules, the graph owns each list after first creation, so runtime edits (`neo4j_query` with `readonly=false`: `MATCH (s:SourceList {name:'news'}) SET s.domains = [...]`) persist across restarts. Delete a node to re-seed it from its YAML. A `source_list` name that doesn't resolve degrades gracefully: the search runs unrestricted. Built-ins: `news` (national/world outlets), `michigan-news` (metro Detroit and Michigan outlets), `ml-research` (preprints, weights registries, leaderboards, inference runtimes, first-party lab announcements — used by the SLM benchmark watch so a "MOVER" is always traceable to a paper, repo, or registry entry).

### Media Learning (watch & summarize videos)

The brain watches and summarizes videos to learn new concepts and stay current on topics it already knows. `MediaSkill` (`skills/media.rs`) + `MediaService` (`services/media.rs`) own the pipeline; `project-docs/VIDEO_LEARNING_PLAN.md` is the full spec.

- **Transcript acquisition (captions-first):** `MediaService` shells out to `yt-dlp -J` for metadata, then lets **yt-dlp download the caption file itself** (`--write-subs --write-auto-subs --sub-langs … --sub-format json3` into a scratch dir) and parses the `json3` to plain text. (Fetching the timedtext URL directly via reqwest was tried and abandoned — it works for manual subs but fails for auto-captions, which need yt-dlp's session.) **Whisper fallback (Phase 4, implemented):** when a video has no captions and `WHISPER_PROVIDER` is set, `MediaService` downloads best audio (`yt-dlp -f bestaudio`, no ffmpeg) and POSTs it to a self-hosted **OpenAI-compatible** Whisper endpoint (`WHISPER_BASE_URL`) via `services/transcribe.rs` (`Transcriber` trait → `HttpTranscriber`). The `whisper` compose service runs `faster-whisper-server` on GPU (`:latest-cuda`, `float16`) — host driver 535.261.03 (CUDA 12.2) satisfies the image's 12.2 runtime on the RTX 3060 Ti; the base model uses ~380 MiB VRAM and unloads on idle TTL. Fall back to `:latest-cpu` + `WHISPER__INFERENCE_DEVICE=cpu` if the driver ever lags the image's CUDA version. `WHISPER_PROVIDER=none` (the code default) keeps caption-less videos erroring cleanly. Subprocess safety: `yt-dlp` is always invoked with an **arg array**, and URLs are scheme-validated (`http`/`https` only).
- **Map-reduce summarization happens *inside* `ingest_media`,** not as chain steps — chains are fixed-length but transcripts aren't. Short transcripts are single-pass; long ones are chunked on sentence boundaries ("map"), then synthesized via `generate_json` into `{summary, key_concepts}` ("reduce").
- **On-demand:** `ingest_media(url)` (accepts a bare URL or a goal like `"watch video: <url>"`) and `fetch_transcript(url)`. A Task whose goal starts with `watch video:` routes to `chains/video-learning.yaml` (ingest → bank/reassemble in a `video-{{task_id}}` WorkingMemory session → new-vs-known `reason` → `store_note` → cleanup).
- **Autonomous watch:** `poll_media_sources` iterates active `:MediaSource` nodes, lists new videos from YouTube's **free per-channel RSS feed** (`youtube.com/feeds/videos.xml?channel_id=…`), and fans each new upload out into a `"watch video: <url>"` Task (chains can't loop a dynamic list, so we create Tasks). Gated by `MEDIA_WATCH_ENABLED` — a no-op when unset, so `schedules/media-watch.yaml` (6h) is safe to seed everywhere. Dedup: a video is skipped if a `:Media` node exists **or** an open Task already targets it. **Duration pre-filter (2026-08-17):** RSS feeds carry no duration, so before creating a Task the poll probes each new candidate with `MediaService::fetch_meta` (`yt-dlp -J`, metadata only) and, if it exceeds `MEDIA_MAX_DURATION_SECS`, records a `:Media` node with `transcript_source:"skipped_too_long"` and creates **no** Task. Without this, an over-length video (long-form podcasts from e.g. `julian-dorey`) fanned out into a chain whose `ingest_media` step could only fail 3×, dead-letter, and fail the owning Task — the single largest media failure bucket in the two-week review. The skip node makes `media_exists` dedup it on the next poll, so the same video is never re-probed. A probe failure (transient yt-dlp/network) is non-fatal: it falls through and creates the Task as before, so a good video is never silently dropped. The ingest-time cap check remains (for direct `ingest_media` calls) and still returns an error, which correctly lets a `watch video:` chain die cleanly rather than storing a junk summary.
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

See `project-docs/BRAIN_TO_BRAIN_PLAN.md` for the federated brain-to-brain transport spec — **design only, nothing implemented.** Multiple brain instances exchanging knowledge with provenance that survives storage. The load-bearing decision is a signed, transport-agnostic envelope (Ed25519 identity per brain), *not* a choice of mesh network: every transport authenticates the channel, and that guarantee dies the moment a payload lands in Neo4j, while `asserted_by` has to stay verifiable for months. Two transports with different failure modes — HTTP+protobuf over Tailscale for snapshots/bulk, LXMF over Reticulum (Python sidecar) for claims/task offers. Two constraints worth knowing before touching it: the upstream Reticulum license forbids use in creating a training dataset, which collides with `digest_experiences` (use RetiNet or Reticulum_CE — the protocol is public domain); and a peer brain must **never** count toward `MIN_INDEPENDENT_DOMAINS` in `check_independence`, since keypairs are free to generate and the brains share ingest paths, so a fleet corroborating itself is the same circular case the code already rejects for `skywatcher.ai`.

- [ ] Phase 0: Measure — snapshot size, claims/day, `rncp` timing between two RetiNet nodes (no code; verifies the traffic split the plan asserts)
- [ ] Phase 1: Identity + envelope (`proto/brain_envelope.proto`, `services/peer_identity.rs`, key via `SECRET_PROVIDER`)
- [ ] Phase 2: Transport A over Tailscale (`peers/*.yaml` seed dir + `PEERS_DIR` compose env, `(:PeerBrain)`, `skills/peer.rs`)
- [ ] Phase 3: Ingest + epistemics (`peer_brain` tier in `classify_domains`, `label_claims` variants, independence exclusion, task-offer default-deny)
- [ ] Phase 4: Transport B — Reticulum sidecar (start on `TCPClientInterface` over a tailnet address; no radio hardware needed)
- [ ] Phase 5: Evaluate iroh (Ed25519 `EndpointId` matches Reticulum's identity-as-address model; `tonic-iroh-transport` is pre-1.0)

## Critical Dev Notes

**LlmConfig:** `base_url` is `Option<String>`. Default model: `"qwen3.5:4b"`. Tests: `config.base_url.as_deref()`.

**Structured LLM output:** For tool outputs that must be strict JSON, call `LlmProvider::generate_json(prompt, system, required_keys, max_retries)` (default method on the trait in `services/traits.rs`) instead of hand-rolling `generate` + `extract_json` + `serde_json::from_str().unwrap_or_else(fallback)`. It runs the "targeted self-correction" loop: on a parse error or a missing required key it re-prompts the model with the specific error, up to `max_retries` extra attempts. Wired in `reason` `clarify` and `reason_structured`. `extract_json` (in `services/llm.rs`) now picks the **earliest-opening** delimiter, so a top-level `[{...}]` array is no longer mis-extracted to its first object — pass `&[]` for `required_keys` to accept any valid JSON (arrays/scalars).

**A cloud LLM that cannot answer falls back to local; one that answers and refuses does not.** `SharedLlm::generate` (`services/shared_llm.rs`) retries on the local model when a cloud call fails, and `classify_unavailable` decides which failures qualify. Four kinds do: `rate_limited` (429/quota), `subscription_required` (Ollama Cloud's undocumented free tier answers 403 "requires a subscription"), `transport` (DNS failure, refused connection, TLS error, client-side timeout — the call never reached the provider), and `server_error` (a 5xx — the provider is up but not serving). Anything else propagates unchanged, because the distinction that matters is **"the provider could not answer"** vs **"the provider answered and rejected this request"**: falling back on a 400 or a 401 re-sends a bad request to a weaker model and gets a worse rejection out of it.

Only the first two were covered until 2026-08-23, and the gap cost a weekly report. `schedules/off-grid-networking-monitor.yaml` runs one cloud-routed `reason` step; on 2026-08-19 it failed 3/3 with `Provider error: HTTP request failed: error sending request for url (https://ollama.com/v1/chat/completions)`, dead-lettered, and — via chain-death attribution — failed the owning Task. Retrying an unreachable host three times cannot succeed, and `gemma4:latest` was sitting idle the whole time. Nothing alerted (the notification path in `is_quota_exhausted_error` covers quota, not transport), so the only symptom was a missing report, and a `/chat` session eleven days later answered a mesh-networking question off the stale 08-12 baseline while describing the research as "in progress".

Two rules for anything added here. `is_server_error` matches the canonical reason phrases (`502 Bad Gateway`, `503 Service Unavailable`, …) rather than bare numbers — `contains("500")` fires on a token count in an unrelated error body. And the fallback is guarded by `!is_local_route`, so a failing *local* model never retries against itself. Note that a capability-routed step (`required_capabilities`) can land on a local model lacking the capability it asked for; a degraded report beats a dead chain, and `record_model_usage` logs the failed cloud call with its `error_kind` first so the router still learns the outage.

**Skill registration:** Register to BOTH `tool_registry` (for `tools/list`) AND `skills` vec (for `tools/call`). Forgetting either causes invisible tools or dispatch failures.

**Ollama serves a 4096-token context by default, whatever the model claims.** `gemma4:latest` advertises `gemma4.context_length = 131072` via `/api/show`, but Ollama serves it at `num_ctx = 4096` unless the request sets `options.num_ctx` — and input past the window is **truncated silently**, with no error and no warning. Measured: a 24 000-char prompt came back with `prompt_eval_count: 4096` and the model answered the surviving fragment (a chatty "I have processed the updates, how can I help?") instead of following the instruction, which had been cut off the end. Two consequences: (1) any prompt built from a large `{{_prev}}`, RAG context, or map-reduce chunk may be silently losing its tail; (2) put the operative instruction **before and after** a large payload, never only after. `LlmConfig::num_ctx` (`Option<u32>`, default `None` = leave Ollama's default) sets it per config; only the distiller raises it today (`DISTILL_NUM_CTX = 16384`), since a bigger window costs VRAM on a shared GPU.

**Embedding can fail per-input — never let it be fatal.** Ollama returns `500 … {"error":"failed to encode response: json: unsupported value: NaN"}` when the embedding model emits NaN into the vector (Go's `encoding/json` refuses to marshal it). It is **deterministic for that exact string**, not flaky: reproduced 5/5 on `bge-m3:latest` under Ollama 0.20.7, with a hard boundary at 97 characters for one goal string while unrelated 100+ character strings embed fine. Retrying cannot clear it, so a hard failure here burns all `max_attempts`, dead-letters the job, and takes the owning chain and its Task down via chain-death attribution. Both vector-search entry points in `services/knowledge.rs` therefore degrade to the BM25 `note_content_fulltext` index instead of propagating the error: `search_notes_inner` (`if let Ok(embedding) = …`) and `fetch_similar_notes` (used by `synthesize_knowledge`). Any new embed call site must do the same. The BM25 fallback must run its query through `sanitize_lucene_query` — an unescaped `:` or `/` makes `queryNodes` throw, turning the fallback into a second failure, and goal strings routinely contain colons.

**`neo4j_query` used to corrupt multi-line Cypher, and the graph's timestamps are of two different types.** Both surfaced in one chat turn on 2026-08-13: asked *"What did we learn today?"*, the brain answered *"My memory is currently clear of any new learnings from the last 48 hours"* — with 504 notes created that day. The model's Cypher was correct throughout; two mechanisms below it produced the wrong answer, and neither raised anything the model could see.

- **LIMIT injection matched a substring.** `handle_neo4j_query` appended `LIMIT <limit>` when the query contained `RETURN` but not `" LIMIT "` — with a **leading space**. A model writing clause-per-line Cypher ends with `\nLIMIT 20`, which fails that substring test, so the tool appended a second clause and sent `LIMIT 20 LIMIT 100`. Neo4j rejects it as `RETURN can only be used at the end of the query`, pointing at the RETURN four lines earlier — so the model saw a syntax error in a query it wrote correctly, reformatted onto one line (which passes the substring test and runs), and never learned what actually happened. `needs_limit_injection()` now matches `RETURN`/`LIMIT` as **whitespace-separated words**, and the injected clause goes on its own line so a trailing `//` comment can't swallow it.
- **Timestamps were mixed types, and a mismatch is silent.** Cypher compares a temporal value to a string as **null** — not `false` — so the predicate is neither true nor false, `WHERE` drops the row, and the query succeeds with zero results. `WHERE n.created_at >= '2026-08-13T00:00:00Z'` matched **0** rows where `datetime('2026-08-13T00:00:00Z')` matched 504. Zero rows is indistinguishable from absent data, which is why it was reported as fact.

  The same defect had been live elsewhere for months without anyone noticing, which is the argument for fixing the representation rather than the caller: `find_similar_tasks` (the dedup check in `create_task`) compared string `Task.created_at` against `datetime() - duration({days:7})` and returned **0 instead of 815** on every call, so task deduplication never once ran.

**This is now fixed: every temporal property in the graph is a native `DATETIME` in UTC.** The rules, the enforcement, and the reasoning live in `project-docs/schema.md` under "Timestamps: one representation, always" — read that before writing any Cypher that touches a date. In short: `datetime($param)` on writes, compare against `datetime(...)` never a string literal, `toString(prop) AS prop` when reading into a `String` field, and `repository::node_ts` for whole-node reads. Model structs stay `String`, so REST/MCP payloads are unchanged.

Three things keep it from silently returning, since Neo4j does not enforce property types:

- `Neo4jClient::string_timestamp_violations()` — warns at startup from `main.rs`, naming every offending `label.property` and its count.
- `no_string_timestamps` — integration test asserting the violation list is empty.
- `neo4j_query` attaches a `hint` when a query filtering on a quoted `YYYY-MM-DD` literal returns zero rows, pointing at `valueType(n.prop)`. This survives the migration on purpose: it catches a *caller* comparing against a string literal, which the schema rule cannot prevent.

Migration: `scripts/migrate_timestamps_to_datetime.cypher` (idempotent, guarded on `valueType(...) STARTS WITH 'STRING'`, converts 34 properties across 17 labels plus `CORROBORATED_BY.found_at`). Run it with the brain **stopped** — the old binary writes strings, so a concurrent write reintroduces exactly what it removes.

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
