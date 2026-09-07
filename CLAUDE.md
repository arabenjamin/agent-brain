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
| `OLLAMA_LOCAL_MODEL` | `gemma4:latest` (**compose sets `granite4:latest`**) | Model used exclusively for all background/scheduled jobs. Always routes to `OLLAMA_LOCAL_URL` — never touches cloud quota. The code default is stale: it names a model that does not fit this GPU. See "Pick the local model by residency, not by leaderboard" below |
| `OLLAMA_LOCAL_TIMEOUT_SECS` | `600` | Request timeout for the **local** background model only; every cloud call keeps the shared 120s default. A cloud model that has not answered in 120s is not going to; a local one on a shared consumer GPU routinely is. Non-positive or unparseable values warn and fall back to the default rather than disabling the timeout. See "The local model has its own timeout" below |
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
| `TELEMETRY_DB_PATH` | - | Path to DuckDB file for interaction logging (enables `SleepSkill`). **Reading it from the host: copy the `.wal` alongside the `.duckdb`.** The brain holds the write lock, so analysis runs against a copy — and DuckDB keeps recent commits in `telemetry.duckdb.wal` until a checkpoint, so copying the `.duckdb` alone silently returns a database that is correct but stale by minutes to hours. It does not error; it reports zero rows for the window you are asking about, which is indistinguishable from "nothing ran" and is exactly the reading you take when investigating whether something is still running. Copy both files (same basename) and open the copy `-readonly` |
| `SEARCH_ENGINE_ORDER` | `searxng,google,serpapi,brave` | Comma-separated `search_web` failover ladder, most-preferred first. Each engine is tried until one answers, so a single exhausted quota no longer fails the call. Reorder to change preference without a rebuild |
| `SEARXNG_URL` | `http://searxng:8080` | Base URL of the self-hosted SearXNG metasearch sidecar — the default first rung of the ladder. No API key, no quota. Also readable from a `searxng` `ApiContext` node (`base_url` field) |
| `SERPAPI_KEY` | - | SerpApi key for `search_web` tool. Free tier is **250 searches/month** (re-verified 2026-09-04; the earlier "100" was wrong). Backs both the `serpapi` (Google organic) and `google_news` engines, which **share one account quota**. The daily news brief now requests `engine: google_news` on its 7 news searches (~210/month), so this is a primary for news; general/background mining stays on unmetered SearXNG |
| `BRAVE_API_KEY` | - | Brave Search API key for `search_web` tool. Brave **killed its free tier in Feb 2026** (card required, metered, no spending cap) — last rung by default |
| `GOOGLE_API_KEY` | - | Google Custom Search API key for `search_web` tool. Free tier is 100 queries/day. Requires the *Custom Search API* to be **enabled** in the GCP project or every call 403s |
| `GOOGLE_CX` | - | Google Custom Search Engine ID for `search_web` tool |
| `CLOUD_TIER` | `1` | Cloud autonomy tier for per-step model routing. `0` = local Ollama only; `1` = local + $0-cost Ollama Cloud models (needs `OLLAMA_API_KEY`); `2` = any provider with a configured key ("income mode"). **Compose sets `2`** so `gemini-2.5-flash` is reachable for `required_capabilities: ["reasoning"]` steps — it wins them because on the free tier its catalog cost is 0.0 and its `selection_rank` (5) sits below the ollama-cloud band. A 429 there falls back to `gpt-oss:120b-cloud`, then local (`build_fallback_chain`) |
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
| `CHAT_SYNTHESIS_PROVIDER` | - | Enables the **worker/voice split** for `/chat`: the model in `CHAT_LLM_MODEL` runs the tool loop, and this provider composes the final reply from what it gathered. Accepts `ollama`, `ollama-cloud`, `gemini`, `anthropic`. Unset ⇒ the worker also speaks (previous behaviour). See "The worker/voice split" below |
| `CHAT_SYNTHESIS_MODEL` | `gemma4:31b-cloud` | Model used for the synthesis pass. Only consulted when `CHAT_SYNTHESIS_PROVIDER` is set |
| `SUBAGENT_ENABLED` | `false` | Phase 0 spike for in-chat sub-agent delegation. When `true`/`1`/`yes`, the chat orchestrator gains a `delegate(agent, task)` pseudo-tool (intercepted in `run_ollama_cloud_loop`, not dispatched to ToolHandler) that runs a bounded, read-only worker sub-agent from a curated roster (`researcher`/`code-analyst`/`knowledge-worker`) and folds its findings back into the turn. Depth cap 1 (sub-agents get no `delegate`). Default OFF. See `project-docs/SUBAGENT_ORCHESTRATION_PLAN.md` |

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
- `(:ApiContext)-[:EXPOSES]->(:ApiEndpoint)` — endpoints ingested by `ingest_api` (see below). `:ApiEndpoint {api_name, method, path, summary, operation_id, parameters, security, tags}` MERGE'd on `(api_name, method, path)`, so re-ingest updates in place.

### Native REST API ingestion (`ingest_api`)

`ingest_api(spec_url [, name, description])` in `HttpSkill` (`skills/http.rs`) fetches an OpenAPI 3.x / Swagger 2.0 doc (JSON **or** YAML), parses **every** operation deterministically (a `serde_json::Value` walk — no LLM, so no truncation/hallucination), and stores each as an `(:ApiEndpoint)` under an `(:ApiContext)` via `[:EXPOSES]`. This is the properly-resurrected agent-api "spec → endpoint graph" idea (the old `ingest_openapi` was a doc-comment ghost; the code was never ported). base_url resolves 3.x `servers[0].url` (absolute or relative-to-origin) → 2.0 `schemes[0]://host+basePath` → the spec URL's origin. Persistence uses the atomic `UNWIND $endpoints` + `RETURN count` read-back pattern (the same lesson as the dynamic-tool fix: one query, no cross-connection MATCH, verify the write landed). Fetch runs **in the brain process** (has egress), never the sandbox. Query results with `neo4j_query`. Parser tests: `ingest_api_tests`. Scope today is **OpenAPI/Swagger only** (HTML docs / codebase introspection are deferred).

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

## Architecture: subsystem rules

> Each subsystem below is stated as its **operative rules only**. The full narrative — root causes, measurements, and the failures that motivated each rule — lives verbatim in **`project-docs/engineering-history.md`** (§ *Architecture & subsystem deep-dives*). Read the matching section there before changing one of these; nearly every rule marks a failure mode the codebase re-learns whenever the reasoning is dropped.

### Self-Improvement Loop

`SchedulerService` background task: lists `status=created` Tasks → `goal_to_steps()` → `enqueue_chain()` → marks `in_progress`. After `idle_sleep_after_ticks` (3) idle ticks → `enter_sleep()` (consolidate → prune → snapshot). `notify_activity()` resets the idle count. `QueueService` coordinator runs jobs serially per provider semaphore, retries transient failures, unparks dependents on success.

- **Dead-job meta-learning:** on retry exhaustion, store a reflection note and — for non-infra tools (`should_meta_learn`) — enqueue an Analyze→Hypothesize→Test→Integrate chain. Guards: `is_transient_infra_error` (skip for quota/rate/timeout/5xx) and `recently_meta_learned` (≤ once/tool/24h).
- **Chain-death task attribution:** `enqueue_chain_owned(steps, session_id, Some(&task_id))` stamps `__owner_task_id` on every step; on step death the coordinator calls `fail_task_with_reason(task_id, reason)`, guarded to `status IN ['created','in_progress']`. `enqueue_chain` is the no-owner wrapper (chat/bedtime/meta-learning).

### Evaluator Loop (Generator-Evaluator)

A Task with `success_criteria` gets a `reflect_on_work` evaluator step appended by `goal_to_steps()`. It parses a `Score: N/5` line; `< min_score` (default 3.5) → mark task `failed` + create a new Task with the critique in `context`. **Retry cap 3** (`handle_evaluator_requeue` counts `"RETRY —"`). **Unparseable output = pass** (`parse_evaluator_score` → `None`; do *not* fabricate a 3.0 and do not grade the AgentSpec). `ChainStep`: `is_evaluator`/`min_score`/`evaluator_task_id`; args `__evaluator_min_score`/`__evaluator_task_id`. A `:SchedulerChain` may carry `evaluation_rubric` to override the goal text. Full story: history § Evaluator Loop.

### Adversarial Critic (pre-flight plan review)

A Task with `success_criteria` also gets an `adversarial_plan_review` step **prepended**. `overall_robustness < min_robustness` (default 2.5) → cancel downstream + `handle_adversarial_requeue()` (mark failed, new Task with critique). **Abort cap 3** (`"ADVERSARIAL ABORT"`). Skipped when: no `success_criteria`, chain `no_adversarial: true`, or context already has `"ADVERSARIAL ABORT"`. `ChainStep`: `is_adversarial`/`n_hypotheses`/`min_robustness`/`adversarial_task_id`. `parse_adversarial_robustness()` defaults 3.0. Also callable directly (KnowledgeSkill); each run stores a `semantic` note `source_context: "adversarial_review"`. Full story: history § Adversarial Critic.

### Externalized Agent Chains

Scheduler routing chains are `chains/*.yaml` seeded as `(:SchedulerChain)` by `init-db` and force-refreshed on the first tick after every startup. `goal_to_steps()` queries Neo4j, falls back to `build_diagnosis_chain()`.

**Chain YAML schema:** `name` (MERGE key), `pattern` (primary CONTAINS), `patterns: [...]` (OR), `priority` (lower = matched first; default chain 9999), `no_evaluator`, `no_adversarial`, `evaluation_rubric`, `steps: [...]`.

- **Matching is plain CONTAINS; priority is the only tiebreak.** A broad substring pattern silently hijacks a specific one if it sorts first — put specific prefixes at low priority numbers, and check no broader pattern outranks a new chain.
- **Template vars** in step `arguments`: `{{goal}}`, `{{goal_topic}}`, `{{task_id}}`, `{{date}}` (local date), `{{now}}` (local instant), `{{weekday}}` (local), `{{file_slug}}`. **Use `{{goal_topic}}` for anything fed to a search engine** (strips the routing prefix). Substitution is **value-level** (`substitute_template_vars` walks parsed JSON, replaces inside string values only — quote/backslash/newline-safe).
- **`{{_prev}}` vs `{{_prev.<path>}}`:** `{{_prev}}` = the previous step's output *after* `extract_result_text` unwraps `answer`/single-column `rows` (lossy by design). `{{_prev.<path>}}` (e.g. `{{_prev.id}}`, alias `{{result.id}}`) resolves a dotted path against the **pre-unwrap** envelope (`prev_result_raw`), never distilled. Unresolvable path → **empty string**, never the literal placeholder (consumers read empty as absent). Both forms may co-occur; paths substituted first; strings inserted raw.
- **Per-step model routing:** a step may declare `required_capabilities: [...]`; `services/model_router.rs` picks the best catalog model within `CLOUD_TIER`, routed via the `SELECTED_LLM` task-local (precedence: capability-selected > `USE_LOCAL_LLM` background pin > active config). Metadata: `__required_capabilities`. **"Best" = `cost ASC, selection_rank ASC, context_window DESC`** — and since every local/cloud model costs 0.0, **`selection_rank` is the decision** (per model in `models.yaml`, lower wins). Bands: `0–9` free-tier Gemini treated as $0 (only `gemini-2.5-flash`, rank 5 — below ollama-cloud so it wins reasoning at tier 2), `10–99` ollama-cloud, `110–199` local, `210+` paid. Rank is **global, not per-capability**; a single-holder capability is only safe while it stays single. Guards: `selection_order_tests` + `model_config.rs` assert the checked-in `models.yaml` (incl. `reasoning_at_tier2_routes_to_gemini_flash` and `vision_at_tier2_stays_on_the_ollama_cloud_vision_model`). `minimax-m3:cloud` removed 2026-08-31 (left free tier); the seven ollama-cloud entries are the whole free tier. **Gemini free-tier caveat:** `gemini-2.5-flash`/`gemini-3-flash` are catalogued at cost 0.0, which is accurate only on the free tier (429 at the ceiling, no billing) — move them off 0.0 if the deployment ever exceeds it.
- **`CURRENT_TOOL`** task-local attributes every in-job LLM call to `job.tool_name` in the usage ledger.
- **Distilled handoffs (`distill_prev: true`, `distill_max_chars`, `distill_focus`):** `maybe_distill_prev` compresses upstream output on the **local** model before substitution. Soft budget (overshoots ~50%); only hard guarantee is "never longer than raw". Own `LlmClient` at `num_ctx=16384`. **Never** on a step that persists `{{_prev}}` verbatim, nor on evaluator/adversarial steps. Every failure path falls back to raw. Wired in `learn.yaml`, `git-history.yaml`, `video-learning.yaml`.
- **UI chain** (`chains/ui-frontend.yaml`): frontend keywords → `workspace/ui/{{file_slug}}.md`, `no_evaluator: true`.

Full story (routing-capture incident, selection_rank reasoning, distill measurements): history § Externalized Agent Chains.

### Self-knowledge: making running state visible

A model that can't see running work reports it as absent, confidently. Rules that fix the class:

- `manage_scheduled_task(action=list)` is a **tool** (lean payload; `verbose: true` for the rest); **empty-filter guardrail** returns every name + a warning rather than an empty list. `list_tasks` returns **totals** and leads with open work.
- **Truncated tool results are marked** (`truncate_tool_result` → `[TRUNCATED … INCOMPLETE]`). A `reason` result's declared limits are **hoisted to the front** (`annotate_tool_limits`/`reason_limits_marker`). `search_web` results are **prefixed with a numbered source list** (`search_sources_marker`, accepts `link` or `url`). `prepare_tool_result` is the single annotate-then-truncate entry point; the marker leads so truncation can't drop it.
- Failed protocol steps log at **WARN** (`tool_call FAILED`). A **dropped chat turn is reported** (`forward_chat_events`) instead of vanishing. An **empty completion is retried** (`MAX_EMPTY_COMPLETION_RETRIES`), not mistaken for a finished turn; `FINAL_ROUND_NUDGE` gives a tools-off wrap-up round when tool calls are exhausted. `chat_fallback_ladder` in `models.yaml`; a `reason` fallback to raw prose sets `structured_output_failed` (`confidence 0.0`, WARN).
- **General lesson:** a signal the model must act on belongs at **position zero in a loud delimiter**, not a field — prompt rules do not reliably beat payload position. Behavioural half: `PROJECT-STATUS RULE` in `contexts/general.yaml`.

Full story (the "conceptual blueprint" incident, all markers, the Nebula/Meshtastic case): history § Self-knowledge.

### Worker/voice split + write guard

`CHAT_SYNTHESIS_PROVIDER`/`CHAT_SYNTHESIS_MODEL` enable the split: the worker (`CHAT_LLM_MODEL`) runs the tool loop (its prose emitted as `Thinking`), the voice model composes the reply from the gathered results (passed as an argument, not scraped from history). Runs on the cloud path via `config_for_catalog_entry`; replaces `NO_ANSWER_AFTER_TOOLS_MESSAGE`. **Does not fix fabrication.**

**`TurnWriteGuard`** (all four loops via `execute_guarded`), non-blocking: **grounding** — a `store_note` typed `semantic`/absent is rewritten to `unsourced_synthesis` (+ loud notice) when no retrieval tool has succeeded this turn; **duplicate suppression** — a write tool called twice with byte-identical args runs once (repeat returns success + notice). `observe()` runs after dispatch and takes the success flag; the duplicate check runs before the downgrade. `write_guard_tests`. Full story: history § worker/voice split.

### Delivering `notify_user` to the UI (`hbi-frontend`)

`App.tsx` **owns the notification list** (`src/api/notifications.ts`); badge and banner read the same array. A notification-only POST to `/mcp` must return **`202 Accepted`** (empty body, session-id header) or the SDK never opens its GET SSE stream — every server push went to zero listeners before this. `resetMcpClient()` must not clear `_notifHandlers`. History labels sessions by kind (`sessionLabel()` in `ChatPanel.tsx`, `SESSION_KINDS` synced to `grep -rhoE 'session_id: *"[^"]+"' schedules/ chains/`, longest prefix first — **never add a speculative prefix**); renders every role. Full story: history § notify_user UI.

### ScheduledTask ownership (`managed_by`)

Built-in ScheduledTasks live in `schedules/*.yaml` (seeded by `schedule_seeder`); a missing `schedules/` dir aborts startup. The **graph is the runtime authority**; YAML is the definition source only for tasks it owns.

- **`managed_by`:** `yaml` (steps/description/interval force-synced every startup — file edits win, runtime edits overwritten) vs `runtime` (seeder never touches). Transfer via `manage_scheduled_task` upsert `managed_by`.
- **A runtime node shadows its YAML file permanently and silently** (matching is by exact `name`; `sync_yaml_scheduled_task` skips a runtime-owned name). Check `managed_by` before assuming a file is what executes; `manage_scheduled_task(action=list, verbose=true)` is the only routine way to notice.
- **`enabled` is graph-owned** (deliberately not synced) — so pausing survives restarts. Disable with a targeted Cypher `SET`, **not** `upsert` (upsert requires `steps` and force-rewrites description/interval). `paused_reason`/`paused_at` record why. **`scripts/pause_cloud_schedules.sh {pause|resume|status}`** disables only cloud-routed schedules; audit targets against the graph (`provider_hint <> 'ollama' OR provider_hint IS NULL`). When a quota goes, check the ledger by `tool_name` first — chat, not schedules, usually spent it. **A chain's cloud cost is a multiplication (once per item), not a rate** — bound the item count before adding `ollama-cloud` to a fan-out step; cap burst and daily total separately.
- **A ScheduledTask's steps are enqueued verbatim** — no `goal_to_steps()`, no adversarial/evaluator, no auto-retrieval. Step prompts must be correct alone; a schedule needing fresh info must include its own `search_web`. **Chat-authored schedules get no validation** → `lint_chain_steps` (`skills/scheduler.rs`) covers: a `reason` past position 1 with no `context`; a persisting step past position 1 not mentioning `{{_prev}}`; a `store_note` with no `source_context`; `semantic` in a web-searching chain; unregistered tool; a chain that never persists; **a step with no `provider_hint`** (omission ≠ local — `use_local` is `provider_hint == Some("ollama")`, so omission falls through to cloud). Warnings, not rejections. Runs in `upsert` (`step_warnings`/`needs_fix`) and `audit`. Behavioural half: `BUILD RULE` in `contexts/general.yaml`.
- **One-shot Task vs recurring ScheduledTask:** `create_task` fires once on the next tick — cadence words in the goal are inert. Recurrence needs `manage_scheduled_task(upsert, interval_seconds)` or a `schedules/*.yaml`.
- **Reading prior runs:** chunks inherit `source_context` from their parent, so any Cypher retrieving a prior report by `source_context` must add `AND NOT (n)-[:PART_OF]->()`.
- **Critical Tech Dependency schedules** (three-tier standing investigation, each banks searches into a WorkingMemory scratch session and delta's against the prior run): `hardware-tripwire.yaml` (weekly, `hardware_tripwire`, 4 sweeps), `slm-benchmark-watch.yaml` (bi-weekly, `slm_benchmark_watch`, 3 sweeps), `tech-dependency-synthesis.yaml` (monthly, `tech_dependency_synthesis`, no web search — correlates the other two). Also `off-grid-networking-monitor.yaml` (weekly, `off_grid_networking_watch`, 3 sweeps).

Full story (the tripwire shadow/clobber incident, six duplicate schedules, the broken off-grid monitor, cost tables): history § ScheduledTask ownership.

### Curiosity engine (`chains/fill-knowledge-gap.yaml`)

Gap tasks (`"fill knowledge gap: …"`) come from `spawn_gap_tasks` (a video's `## FOLLOW UP`) and the daily news schedule. The chain searches **both** graph (`search_notes`) and web (`search_web`) — a gap is by definition what the graph lacks — banks into `gap-{{task_id}}`, reassembles, answers under `## ANSWER` (cited) / `## WHAT THIS ADDS` / `## STILL UNKNOWN`. Priority **15** so it stops losing its own tasks to `learn.yaml`. Search steps use `{{goal_topic}}`. Full story: history § Curiosity engine.

### Epistemics: claims, source records, retrieval labelling

Three note types the brain used to conflate: `semantic` (knowledge the brain established), `source_record` (what an external source *said* — video summaries, `news`), `claim` (one checkable proposition + evidentiary status).

- **`(:Note {note_type:'claim', claim_status, asserted_by, asserted_at})`** with `ASSERTED_IN`/`CORROBORATED_BY`/`CONTRADICTED_BY` edges. Status is **derived** from edges (`recompute_status`), never asserted; verification attaches evidence and recomputes, never edits the claim. Support+contradiction → `disputed`; absence → `unverified` (never refuted).
- **`label_claims` (`services/knowledge.rs`) runs in BOTH retrieval paths** — miss one and the unlabelled copy still reaches context. Prefixes claims `[CLAIM · status · asserted by X · date]` and source records `[SOURCE RECORD — …]`, with a humanised age. `claim`/`source_record` are **excluded from consolidation source selection** (consolidation would launder the status away).
- **Claim kind** (`event`/`attribution`/`mechanism`) separates "was asserted" from "is true"; corroborated `attribution` renders "corroborates that it was asserted, not that it is true". **`classify_domains`** tiers corroborating domains (primary/established/unclassified) mechanically and rides it in the label; verification is **never gated** on tier. **`check_independence`** requires `MIN_INDEPENDENT_DOMAINS` (2) distinct non-self-referential domains; recorded on the edge (does not solve true independence).
- **Verification needs a cursor** — "no evidence" is a stable state, so `last_verify_attempt_at` is stamped **before** each attempt and selection orders on `COALESCE(last_verify_attempt_at, epoch) ASC`. Every `verify_one` return path names the `claim`, not just its id.
- **`unsourced_synthesis`** (retrieval-labelled, excluded from consolidation) is the type for the brain's own uncited reasoning; migrating a writer does not fix prior writes (`scripts/migrate_unsourced_gap_notes.cypher`). **A data fix and a code fix are separate deliverables.**
- Chain-extracted claims get their `ASSERTED_IN` edge via `{{_prev.id}}` + `prev_result_raw`. The `claim` tool **echoes its input as `answer`** (safe to insert mid-chain). Standing schedules write `source_record` (not `semantic`) + a claim-extraction step + a `source_list` + a recency gate.

Full story (the UAP/ToRA-7B/psionics laundering incidents, the full tier reasoning): history § Epistemics.

### Retrieval evaluation harness (`services/retrieval_eval.rs`, `eval-retrieval` CLI)

`cargo run -- eval-retrieval [--fixture …] [--k 10]` runs a golden set (`eval/retrieval_golden.yaml`, human-owned) through the real pipeline and reports **recall@k** + **MRR**. `--bootstrap N` proposes cases to curate. Measurement must not perturb: `search_notes_readonly` → `search_notes_inner(track_access=false)` is the only non-perturbing path. Prefer `expect_substrings` over `expect_ids` (ids churn on re-embed). Tuning result (2026-08-31): recall@10 0.840→0.920, MRR 0.47→0.76 — from **widening the re-rank pool** + fixing a **chunk→parent order-scrambling bug**, *not* the embedder. Defaults stay `RETRIEVAL_RRF_WEIGHT=0.7`/`RETRIEVAL_RECENCY_TAU_DAYS=30`. Query embeddings need Ollama up (else BM25-only); the container carries whatever binary it was built with. Full story: history § Retrieval eval harness.

### The brain's sense of time (`services/clock.rs`)

**Everything stored stays UTC; everything said is local.** Local time needs both `TZ`/`BRAIN_TIMEZONE` in the env *and* code asking for local (both failures silent) — `log_resolved_timezone()` warns at startup when unconfigured. Resolution: `BRAIN_TIMEZONE` → `TZ` → `/etc/localtime` → UTC. `TZ` on `agent-brain` only, not `neo4j`. The four date-producing call sites (chat prompt, both scheduler substitution sites, constructor) are local; `services/sleep.rs` keeps UTC for filenames. `now_stamp()` renders a fully-qualified instant per turn; `{{now}}`/`{{weekday}}` template vars; `label_claims` appends humanised age. Behavioural half: `TIME RULE` in `contexts/general.yaml`. Full story: history § sense of time.

### Tool-integrated reasoning (`execute_code` + `sandbox` sidecar)

Lets a step *compute* its figures instead of hallucinating them; a wrong number becomes a traceback. **Execution never happens in the brain process** — `skills/exec.rs` is an HTTP client for the `sandbox` compose service. Isolation contract (in `docker-compose.yml`): **`sandbox-net` is `internal: true`** (no egress — the only real boundary), no `env_file` (no creds), no bind mounts, `read_only` root, `cap_drop: ALL`, `no-new-privileges`, `mem_limit: 2g`, `pids_limit: 128`, tmpfs `/tmp` `noexec`. **Do not put `sandbox` on `brain-internal` and do not add a `ports:` mapping.** Per-run limits in `sandbox/server.py`. **A failed run is a successful tool call** (`isError: false`, traceback in `stderr`) — a tool *error* would burn a retry and fail the owning Task. No state between calls; pass all input inline. `numpy`/`sympy`/`pandas` installed at build time (no run-time network). Routing: `computation` capability held by **`qwen2.5-coder:7b` alone** (`selection_rank 150`); `execute_code` in `general`/`self-analyst` allowlists. Behavioural half: `COMPUTATION RULE`. Full story: history § Tool-integrated reasoning.

### Web Search: engine failover ladder + usage ledger

`search_web` walks an ordered **ladder** (`SEARCH_ENGINE_ORDER`, default `searxng,google,serpapi,brave`) until one answers **with results**.

- **`searxng` leads** (self-hosted, no key/quota; `brain-internal` only, disabled `limiter` — do not add a `ports:` mapping without re-enabling it; keep `json` in `search.formats`). A **requested `engine:` is promoted, not made exclusive.** **Quota cooldown** (`QUOTA_COOLDOWN_HOURS` 6) moves an exhausted engine to the back, not out. Results normalised to `link`/`snippet`. Every attempt writes a `search_usage` row; `get_search_usage` reports per-engine + per-day.
- **Zero results is not an answer** — a rung returning `200` + empty falls through to the next; if *all* rungs come back empty, return `[]` (erroring would burn retries). Fall-through logged at WARN. **`unresponsive_engines` in the JSON is the diagnostic** (distinguishes "cannot connect" from "connected and refused"); `scripts/searxng_engine_health.py` probes both evergreen and fresh query shapes. **Never probe a single engine with `&engines=<name>`** (silent fallback to the default set); attribute by each result's own `engine` field. A relevance failure cannot be caught by a count.
- **News: `engine: "google_news"`** (`search_serpapi_news`, SerpApi `engine=google_news`). SerpApi free tier is **250/month** (not 100); `serpapi`+`google_news` share one account (ladder demotes both when either is exhausted). No `source_list` on Google News steps. `google_news` is **not** in the default ladder. **Query rewriting was investigated and rejected** (null result — the queries were never the problem; the blocked engines were). **Quota exhaustion alerts** (`is_quota_exhausted_error`, deduped `:AgentNotification` once/tool/24h).

Full story (the four outages — DNS/empty results, bot-blocks, junk results — and the query-rewriting A/B): history § Web Search.

### SourceLists

`(:SourceList {name, domains, description})` restrict `search_web` to approved domains (`site:` + post-filter). Defined in `sources/*.yaml`, seeded **ON CREATE only** (graph-owned after; delete a node to re-seed; runtime edits persist). An unresolved name degrades to unrestricted search. Built-ins: `news`, `michigan-news`, `ml-research`. Full detail: history § SourceLists.

### Media Learning (watch & summarize videos)

`MediaSkill` (`skills/media.rs`) + `MediaService` (`services/media.rs`); spec in `project-docs/VIDEO_LEARNING_PLAN.md`. Captions-first (`yt-dlp` downloads the caption file, parse `json3`); Whisper fallback (`WHISPER_PROVIDER` set → POST audio to OpenAI-compatible endpoint via `services/transcribe.rs`); `WHISPER_PROVIDER=none` errors caption-less videos cleanly. **Map-reduce summarization happens inside `ingest_media`** (transcripts aren't fixed-length, chains are). On-demand: `ingest_media(url)`, `fetch_transcript(url)`; a `watch video:` Task routes to `chains/video-learning.yaml`. Autonomous: `poll_media_sources` (RSS → `watch video:` Task fan-out, `MEDIA_WATCH_ENABLED`-gated, duration pre-filter via `fetch_meta`), `discover_media_sources` (weekly, stages **inactive** sources for review). Watchlist `(:MediaSource {…, active, managed_by})` seeded from `sources-media/*.yaml` ON CREATE. Nodes: `:MediaSource`, `:Media`; edges `FROM_SOURCE`, `SUMMARIZES`, reused `MENTIONS`. Phase 5 (podcast RSS + local files) implemented. Full detail: history § Media Learning.

### Context Profiles

YAML in `contexts/` define tool allowlists + system prompts per persona; `boot.yaml` runs every startup, `init.yaml` on empty graph; `auto_assign(goal)` keyword-matches. **A profile applies four layers to a chat turn**: `tools` allowlist filters the registry, `system_prompt` appended after the base, `pre_load_query` output appended under `LIVE SELF-STATE` (all three via `build_system_prompt`), and **`model_preference` (+ optional `provider_hint`) overrides the chat model for that turn** (`ChatService::resolve_profile_model` in `clients/chat.rs`). The model is resolved against the DuckDB catalog (exact key/model match → substring for legacy aliases like `sonnet`/`haiku` → `provider_hint` for a non-catalog model), and credentials/endpoint come from the env via `config_for_catalog_entry`, so the checked-in YAML never carries a key; the chat fallback ladder still applies on top (an unreachable pin — e.g. an Anthropic model with no key — falls down it). An unresolvable/blank preference keeps the chat default. **This is honored ONLY on the human `/chat` path, not background jobs** (those use capability routing / `provider_hint`). **A profile run as a sub-agent (`AgentRunner::run`, i.e. `run_agent`/`delegate`) honors `model_preference` too, but CLAMPS it to the unmetered band**: a pin that resolves to a metered (Anthropic) or rate-limited (Gemini free tier — the 5-RPM trap) provider is discarded and the run falls back to `base` (the active unmetered Ollama-Cloud config), with a WARN. Only `ollama`/`ollama-cloud` pins survive. This is where the "sub-agents never land on a rate-limited model" invariant is actually enforced — the roster's pins exist for the `/chat` persona (e.g. `code-analyst` on Gemini's free tier), not for delegated loops. A **bare `provider_hint` with no `model_preference` is inert** (the resolver is only reached via `model_preference`), so a sub-agent profile that wants local must set `model_preference`, not just the hint. The `:ContextProfile` `model_preference`/`provider_hint` are also mirrored to the meta-graph and echoed by `/api/models` + the frontend "🤖 Agents" modal, which writes them via `/api/contexts`. **`pre_load_query` accepts Cypher or a keyword** — read-only Cypher runs verbatim (`is_read_cypher()` rejects any write token); a failing query warns and continues. `contexts/self-analyst.yaml` is the self-inspection persona (injects `:ModelDef`/`:SchedulerChain`/`:ScheduledTask`/`:ToolDef` live state; enforces MEASURE→LOCATE→COMPARE→PROPOSE). `contexts/general.yaml` carries a smaller version + read-only introspection tools; `MCP_TOOL_PROFILE=general` also bounds `tools/list`. Full detail: history § Context Profiles.

## TODO / Planned Features

See `project-docs/REFACTOR_PLAN.md` for the ongoing structural refactoring roadmap.

See `project-docs/SUBAGENT_ORCHESTRATION_PLAN.md` for the in-chat sub-agent orchestration spec — **design only, nothing implemented.** A conversational orchestrator (gemma) delegates bounded, tool-heavy lookups to worker sub-agents (gpt-oss) whose structured results return *into the same turn*, via a `delegate` primitive intercepted in the chat loop (not a normal MCP tool — it runs a nested LLM loop). Formalises **agent = context profile** (adds `spawnable`/`specialty`), and is the general form of the existing worker/voice split (`run_synthesis`), which Phase 4 re-expresses on top of it. Load-bearing constraints: sub-agents run on unmetered gpt-oss, never a rate-limited model (the Gemini 5-RPM trap); depth cap 1 and bounded per-loop iterations; the nested loop reuses `execute_guarded`/the write guard/the fallback ladder rather than re-implementing them; a `delegate` result counts as retrieval for the caller's grounding guard.

See `project-docs/UNIFIED_AGENT_EXECUTION_PLAN.md` for the unified agent-execution spec — **design only, nothing implemented.** Generalises the chat `delegate` primitive so all three goal entrypoints (chat, tasks, schedules) reach one shared agent loop. The load-bearing fact: on the background path a chain step is a single fixed tool call — `context_profile` is annotation-only, never read at execution (`scheduler.rs:344`), so `provider_hint`/`required_capabilities` pick a *model*, never run an *agent*. Fix: extract the sub-agent loop out of `ChatService` into a shared `AgentRunner` service (the correct form of the deferred SUBAGENT Phase 4 — share the core, don't swap the lead), then expose it as a `run_agent(agent, task)` tool a chain/schedule step can call. Decisions settled: task-level grain default; providers local-first, Ollama Cloud allowed, Gemini never (Phase 1 measures whether local can drive an agent loop). Open: `:Project` node for iterative build↔review loops (leaning defer). **Phase 0 done (2026-09-07):** the sub-agent loop is extracted into `services/agent_runner.rs` (`AgentRunner::run` → `AgentFindings`); `ChatService::run_subagent` is a thin streaming wrapper over it, and `ChatService` drives the loop through the `SubagentLoopRunner` trait (provider loops stay on `ChatService`; cycle broken with a `Weak`, wired via `Arc::new_cyclic`). Behaviour-neutral (all lib tests green). **Phase 1 done (2026-09-07):** `skills/agent_runner.rs` (`AgentRunnerSkill`) exposes the `run_agent(agent, task)` background tool — calls `AgentRunner::run` with no sink + `allow_writes: true`, bounded (8 rounds), depth 1; findings returned under `answer` (flows via `{{_prev}}`). Gated on `SUBAGENT_ENABLED` (default OFF). The loop engine is `ChatService`, so `run_agent` is wired via a lazy `BrainCore::agent_runner` slot that `McpServerCore::chat_service()` populates — **HTTP-path only for now** (stdio has no chat service → the tool reports unavailable). `lint_chain_steps` rejects a `run_agent` step missing `agent`/`task`; spawnable-membership is enforced live by the tool (returns the roster). `AgentFindings.rejected` distinguishes a config error (hard tool error) from a ran-but-empty result (non-fatal). **Phase 2 done (2026-09-07):** deterministic LLM-free "choose" — `AgentRunner::select_agent(goal)` token-overlap-matches the spawnable roster (best above threshold, else none = build cue); a `select_agent` tool exposes it; `run_agent` accepts `agent: "auto"` (auto-select, no match → rejection naming the roster). Prefer-roster-then-construct is prompt policy in the tool descriptions; the "build" half stays `construct_agent`. **Phase 3 done (2026-09-07):** three front doors reach the shared loop. Task path — `chains/run-agent.yaml` (`run agent:`/`delegate to agent:` prefix → single `run_agent(auto)` step, `no_evaluator`/`no_adversarial`). Schedule path — any step can be `run_agent` (tool + lint support it; no live schedule ships one while the feature's off). Chat path — `delegate` (Phase 0). Schedule-authoring flow — `contexts/schedule-author.yaml` (spawnable drafting specialist encoding the lint rules; read-only, so it DRAFTS and the orchestrator UPSERTS) + `contexts/orchestrator.yaml` gained `manage_scheduled_task` and a RECURRING WORK rule. Fixed a latent bug: `chain_seeder.rs` never seeded `no_adversarial` (documented YAML field was a silent no-op; `goal_to_steps` reads it, `manage_chain` wrote it, the seeder didn't). Phase 4 (iterative build↔review Project loop) remains tentative (§8 decision 3).

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

> Long root-cause narratives for the starred rules live verbatim in **`project-docs/engineering-history.md`** (§ *Critical Dev Notes*).

**LlmConfig:** `base_url` is `Option<String>`. Default model `"qwen3.5:4b"`. Tests: `config.base_url.as_deref()`.

**Structured LLM output:** for strict JSON, call `LlmProvider::generate_json(prompt, system, required_keys, max_retries)` (self-correcting re-prompt loop) rather than hand-rolling `generate` + `extract_json` + `from_str().unwrap_or_else`. `extract_json` picks the **earliest-opening** delimiter; pass `&[]` for `required_keys` to accept any valid JSON.

**★ Pick the local model by VRAM residency, not leaderboard.** Residency is a cliff, not a curve: every model that fits generates at 67–125 tok/s, one that doesn't (`gemma4:latest`, 9.86 GB on an 8 GB card) at ~26. Models don't co-reside here — a swap is a replacement; unload the outgoing model (`keep_alive: 0`). **`granite4:latest`** is the local model (`OLLAMA_LOCAL_MODEL` in compose; already `selection_rank 120`), 18× faster than `gemma4` in production. Benchmark the **structured** path too (a fast model can emit unparseable JSON), and beware thinking models — `qwen3.5:4b` (the stale `DEFAULT_MODEL` in `llm.rs`) emits into `thinking` and returns an **empty** `response`. Output length is a cost, not a quality signal. Full tables: history.

**★ The local model has its own timeout.** `OLLAMA_LOCAL_TIMEOUT_SECS` (default **600**) applies to the local background config alone (built in `brain_core.rs`); cloud calls keep the 120s default. It's burstiness, not average load — `poll_media_sources` fans ~30 videos out at once; at 30–75 calls/hour latency climbs to 120s and every call clips. Killing a call at 120s throws away spent compute *and* the chain built on it. Not the cause: concurrency (Ollama batches) or Ollama being down. Full incident (the `0426218` fan-out move): history.

**A `reqwest` failure says what went wrong.** `describe_reqwest` (`llm_providers/mod.rs`) prefixes the kind (`operation timed out:` / `connection refused:`) — the "why" lives in `source()`, which `Display` drops — and appends the full cause chain. The prefix survives job-`error` truncation and matches the phrases `classify_unavailable` uses.

**Every failed LLM call records an `error_kind`.** `SharedLlm::generate`'s per-call `record_model_usage` used to hardcode `None`; now every attempt (primary and each fallback rung) goes through `run_and_record`, whose `failure_kind` reuses `classify_unavailable` and falls back to `"other"`.

**★ A cloud LLM that cannot answer walks a fallback chain; one that answers and refuses does not.** `classify_unavailable` (`services/shared_llm.rs`) qualifies four kinds: `rate_limited`, `subscription_required` (403 "requires a subscription"), `transport` (DNS/refused/TLS/client timeout — never reached the provider), `server_error` (5xx). Anything else propagates (falling back on a 400/401 just gets a worse rejection). `is_server_error` matches canonical reason phrases, not bare numbers (`contains("500")` fires on token counts). Fallback is guarded by `!is_local_route` (a local model never retries against itself) and the chain order comes from `build_fallback_chain`: a **capability-selected** cloud route (e.g. a `reasoning` step routed to `gemini-2.5-flash`) falls back to the **active config first** — the free Ollama-Cloud model, which does not share the selected model's quota, i.e. "what we were doing before" — and only then to local; a route that WAS the active config skips straight to local. This is why routing reasoning at `gemini-2.5-flash` and hitting its 1500 RPD ceiling degrades to `gpt-oss:120b-cloud`, not to weak local granite4. Each rung (primary included) records its own `model_usage` row via `run_and_record`. Full incident (the lost off-grid report): history.

The **chat path has its own ladder** (`chat_fallback_ladder` in `models.yaml`, walked by `run_one_attempt` in `clients/chat.rs`) — it turns on "did the turn produce output", not the error class, and is unmetered-cloud→local (no metered/rate-limited rung). **`run_bounded_loop` (the sub-agent loop behind `run_agent`/`delegate`) walks that same ladder**, so a transient 500 on a sub-agent's primary model degrades to the next rung instead of surfacing the raw error as the whole delegated answer — consistent with the sub-agent unmetered clamp (the ladder introduces no metered rung). Before this it dispatched a single provider loop with no fallback and failed a delegated task on a provider blip.

**Skill registration:** register to BOTH `tool_registry` (for `tools/list`) AND the `skills` vec (for `tools/call`). Forgetting either = invisible tool or dispatch failure.

**★ Ollama serves a 4096-token context by default, whatever the model claims** — input past the window is **truncated silently**. Put the operative instruction **before and after** a large payload, never only after. `LlmConfig::num_ctx` (`Option<u32>`, default `None`) sets it per config; only the distiller raises it (`DISTILL_NUM_CTX=16384`). Detail: history.

**★ Embedding can fail per-input — never let it be fatal.** Ollama returns `500 … NaN` deterministically for certain strings; retrying can't clear it, so a hard failure dead-letters the job and its chain. Both vector-search entry points in `services/knowledge.rs` degrade to the BM25 `note_content_fulltext` index (`search_notes_inner`, `fetch_similar_notes`); any new embed call site must too. The BM25 fallback must run its query through `sanitize_lucene_query`. Detail: history.

**★ `neo4j_query`, and the graph's timestamps.** LIMIT injection now matches `RETURN`/`LIMIT` as whitespace-separated words (a substring test sent `LIMIT 20 LIMIT 100`). **Every temporal property is a native `DATETIME` in UTC** — read `project-docs/schema.md` § "Timestamps: one representation, always" before writing date Cypher (Cypher compares a temporal to a string as **null**, not false, so a mismatch silently drops the row and the query succeeds with zero results). Use `datetime($param)` on writes, compare against `datetime(...)`, `toString(prop)` when reading into a `String`, `repository::node_ts` for whole-node reads. Guards: `string_timestamp_violations()` (startup WARN), `no_string_timestamps` (integration test), and a `neo4j_query` hint on a zero-row date-literal filter. Migration: `scripts/migrate_timestamps_to_datetime.cypher` (run with the brain **stopped**). Full incident (the "we learned nothing" / 504-notes day): history.

**`McpServer`** is a thin backward-compatible wrapper around `McpServerCore` (stdio path only).

**HTTP session init:** always send `notifications/initialized` AFTER `initialize`, or the server stays `Initializing` and rejects all tool calls.

**Initialization order:** `SchedulerService::new()` after `QueueService::new()`; `QueueService::spawn_coordinator()` after the tool handler is set (end of `build_skills`).

**Consolidation loop:** uses `[Memory N]` labels (not `Note N:`). Source selection excludes LLM-generated types and only considers notes whose `next_review_at` is due (without the due filter the fixed topic embedding re-selects the same top-K every cycle). Empty selection is a no-op success. Source notes get `next_review_at = now + 30 days`.

**`services/mod.rs`:** must re-export `pub use llm::{LlmClient, LlmConfig, LlmProviderType};`

**Context Profiles:** YAML in `contexts/` (`CONTEXTS_DIR`). Loaded by `ContextBuilderService::load_profiles()` in `build_skills()`. `boot.yaml` runs after each `build_skills()`, `init.yaml` on an empty graph. `ContextSkill` registered when `context_builder_arc` is `Some`.

## Branch Strategy
DO NOT REMOVE THIS LINE:Never write in credidation to LLMs or coding agents or assistants.

- `feature/*` - Feature branches (no CI)
- `dev` - Development (format + unit tests)
- `test` - Testing (full pipeline with integration tests)
- `prod` - Production (full pipeline + Docker build)
- Update the documentation first, the README, claude, plan, markdowns should reflect our changes.
