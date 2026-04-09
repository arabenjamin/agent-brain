# Agent Brain

An autonomous AI agent backed by a persistent Neo4j knowledge graph. Exposes 90 tools via the Model Context Protocol (MCP) and includes a built-in interactive REPL for direct use without an MCP client.

## What It Does

- **Remembers** — notes with hybrid BM25+vector search, spaced-repetition review, entity extraction, and multi-hop graph expansion
- **Reasons** — LLM inference over stored knowledge, derivation tracking with `DERIVED_FROM` edges
- **Plans** — goal decomposition into ordered sub-tasks, autonomous scheduling via background tick loop
- **Executes** — durable priority job queue with per-provider concurrency (Ollama/Anthropic/Gemini)
- **Extends** — runtime tool definition backed by stored procedure pipelines; hot-registered immediately
- **Integrates** — ingest any OpenAPI spec, self-heal broken documentation, call APIs with credential injection

## Quick Start

```bash
# 1. Build
cargo build --release

# 2. Start Neo4j and Ollama
docker compose up -d

# 3. Initialize database schema
cp .env.example .env
cargo run -- init-db

# 4. Start the interactive REPL
cargo run -- repl

# OR run as MCP server (stdio for Claude Desktop, http for remote)
cargo run -- serve
cargo run -- serve --transport http
```

## Installation

### Prerequisites

- Rust 1.75+ (`curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`)
- Docker & Docker Compose
- Ollama (for LLM-powered features: reasoning, memory consolidation, self-healing)

### Setup

```bash
# Clone repository
git clone <repo-url>
cd agent-brain

# Start Neo4j database and Ollama
docker compose up -d

# Copy environment config and build
cp .env.example .env
cargo build --release

# Initialize database schema
cargo run --release -- init-db
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NEO4J_URI` | `bolt://localhost:7687` | Neo4j connection URI |
| `NEO4J_USER` | `neo4j` | Neo4j username |
| `NEO4J_PASSWORD` | — | Neo4j password (required) |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama API endpoint |
| `OLLAMA_MODEL` | `qwen3.5:4b` | LLM model for text generation |
| `OLLAMA_EMBED_MODEL` | — | Ollama model for embeddings (e.g. `bge-m3:latest`) |
| `LOG_LEVEL` | `info` | Log level (trace/debug/info/warn/error) |
| `MCP_TRANSPORT` | `stdio` | MCP transport (stdio/http) |
| `MCP_HTTP_BIND` | `127.0.0.1:3000` | HTTP bind address |
| `MCP_API_KEY` | — | API key for HTTP authentication |
| `LLM_PROVIDER` | `ollama` | LLM provider (ollama/anthropic/gemini/vllm) |
| `ANTHROPIC_API_KEY` | — | Anthropic API key |
| `GEMINI_API_KEY` | — | Gemini API key |
| `SECRET_PROVIDER` | `local` | Secret provider (local/vault/aws/none) |
| `SECRETS_FILE` | `.secrets.enc` | Encrypted secrets file path |
| `SECRETS_ENCRYPTION_KEY` | — | Encryption key for local secrets |
| `SERPAPI_KEY` | — | SerpApi key for `search_web` tool |
| `BRAVE_API_KEY` | — | Brave Search API key |
| `SCHEDULER_INTERVAL_SECS` | `300` | How often the autonomous scheduler polls for pending tasks |
| `SCHEDULER_ENABLED` | `true` | Enable autonomous background scheduling at startup |
| `DATASET_DIR` | `./datasets` | Directory for training data export |

## CLI Usage

```
agent-brain [OPTIONS] [COMMAND]

Commands:
  repl     Interactive chat REPL (default when no command given)
  serve    Run as MCP server (stdio or http)
  status   Show brain status: counts and scheduler state
  init-db  Initialize Neo4j schema
  api      OpenAPI spec management (see below)
```

### Interactive REPL

```bash
# Start with default settings
cargo run -- repl

# With a context profile and persistent session
cargo run -- repl --profile knowledge-worker --session my-session
```

Inside the REPL, type naturally. Available meta-commands:

| Command | Action |
|---------|--------|
| `/help` | Show all meta-commands |
| `/quit` | Exit |
| `/clear` | Clear conversation history |
| `/new` | Start a fresh session |
| `/profile <name>` | Set context profile |
| `/status` | Show session info |

### MCP Server

```bash
# Stdio (for Claude Desktop, local MCP clients)
cargo run -- serve

# HTTP with SSE (for remote access)
cargo run -- serve --transport http
cargo run -- serve --transport http --bind 0.0.0.0:8080 --api-key my-secret-key
```

### OpenAPI Integration

```bash
# Ingest a spec
cargo run -- api ingest https://petstore3.swagger.io/api/v3/openapi.json
cargo run -- api ingest ./openapi.yaml

# Query endpoints
cargo run -- api query "pets"
cargo run -- api query "/api/v1/payments"

# Execute HTTP requests
cargo run -- api execute -m GET https://api.example.com/users
cargo run -- api execute -m POST https://api.example.com/users \
  -b '{"name": "Alice"}' -H "Authorization: Bearer token123"

# Export healed spec
cargo run -- api export -o healed.yaml
cargo run -- api export -f json -o healed.json

# Diff report
cargo run -- api diff
cargo run -- api diff -f changelog --breaking-only

# Generate embeddings for existing endpoints
cargo run -- api embed
```

## MCP Client Integration

### Stdio Transport (Claude Desktop, local)

```bash
cargo build --release
```

Add to your MCP client config:

```json
{
  "mcpServers": {
    "agent-brain": {
      "command": "/path/to/target/release/agent-brain",
      "args": ["serve"],
      "env": {
        "NEO4J_URI": "bolt://localhost:7687",
        "NEO4J_USER": "neo4j",
        "NEO4J_PASSWORD": "password",
        "OLLAMA_URL": "http://localhost:11434",
        "OLLAMA_MODEL": "qwen3.5:4b"
      }
    }
  }
}
```

### HTTP Transport (Remote / Docker)

```bash
docker compose up -d --build
# MCP_API_KEY=your-key docker compose up -d --build
```

**Endpoints:**
- `POST http://localhost:3000/mcp` — JSON-RPC requests
- `GET  http://localhost:3000/mcp` — SSE stream
- `POST http://localhost:3000/chat` — Agentic chat with SSE event stream
- `GET  http://localhost:3000/health` — Health check

## Available MCP Tools (90)

### Knowledge (16)

| Tool | Description |
|------|-------------|
| `store_note` | Persist a note; auto-chunks, embeds, links similar notes, extracts entities |
| `search_notes` | Hybrid BM25+vector RRF search with multi-hop graph expansion |
| `get_note` | Fetch a note by UUID, updates access stats |
| `update_note` | Update note content in-place, preserving graph edges |
| `delete_note` | Permanently remove a note |
| `find_related_notes` | Find notes linked via RELATES_TO edges |
| `list_notes` | List recent notes, optionally by type |
| `search_by_entity` | Find notes mentioning a named entity |
| `prune_old_notes` | Delete stale notes via adaptive decay or time-based thresholds |
| `consolidate_memories` | LLM synthesis of multiple notes into a summary |
| `review_due_notes` | Return notes whose spaced-repetition review interval has elapsed |
| `export_graph_visualization` | Export Note/Entity/Task graph as JSON for visualization |
| `reason` | RAG + LLM inference; stores inference with DERIVED_FROM edges |
| `audit_action` | Check a proposed action against stored principles |
| `explain_reasoning` | Narrate why a decision was made, citing source notes |
| `ask_clarification` | Analyze a request for ambiguity before acting |

### Task Management (6)

| Tool | Description |
|------|-------------|
| `create_task` | Create and persist a high-level goal |
| `update_task` | Set task status with optional note |
| `list_tasks` | List tasks with optional status filter |
| `reflect_on_work` | LLM critique of current progress |
| `decompose_goal` | Break a task into ordered sub-tasks |
| `record_outcome` | Store an episodic outcome note linked to a task |

### Agent Job Queue (6 + 1 dynamic)

| Tool | Description |
|------|-------------|
| `enqueue_jobs` | Submit one tool call or an ordered chain; steps 2..N park until predecessors complete |
| `queue_status` | Pending/running counts, worker config |
| `cancel_job` | Cancel a queued or running job |
| `retry_job` | Requeue a failed or dead job |
| `set_worker_config` | Change concurrency limits, enable/pause processing |
| `drain_queue` | Cancel all pending jobs |
| `get_job_result` | (dynamic tool, seeded from Neo4j) Poll a job for status and result |

### Autonomous Scheduler (5)

| Tool | Description |
|------|-------------|
| `start_scheduler` | Enable autonomous scheduling |
| `stop_scheduler` | Pause the scheduler |
| `get_scheduler_status` | Scheduler config, stats, sleep state |
| `configure_scheduler` | Update interval, error budget, idle sleep threshold |
| `run_scheduler_tick` | Trigger a tick immediately |

### API Integration (14)

| Tool | Description |
|------|-------------|
| `ingest_openapi` | Load OpenAPI specs into the knowledge graph |
| `graph_query_endpoint` | Search endpoints by path or keyword |
| `execute_http_request` | Execute API calls with self-healing and credential injection |
| `get_api_context` | Retrieve API summaries |
| `list_loaded_apis` | List all ingested APIs |
| `clear_api_context` | Clear cached API context |
| `discover_openapi` | Auto-discover OpenAPI specs from a base URL |
| `build_openapi_from_docs` | Generate specs from documentation pages |
| `build_openapi_from_repo` | Generate specs from repository source code |
| `export_openapi` | Export healed specs to YAML/JSON |
| `diff_api_spec` | Generate documentation drift reports |
| `configure_api_credential` | Store API credentials for automatic injection |
| `list_api_credentials` | List all configured credentials |
| `delete_api_credential` | Remove a credential |

### Model Registry (4)

| Tool | Description |
|------|-------------|
| `list_models` | List providers and registered model specs |
| `use_model` | Switch provider and model at runtime |
| `select_model` | Auto-select cheapest capable model |
| `reload_models` | Re-read `models.yaml` and sync into DuckDB |

> For model usage analytics (previously `get_model_stats` / `get_cloud_usage`), use the generic `duckdb_query` tool against the `model_usage` table.

### Context Profiles (4)

| Tool | Description |
|------|-------------|
| `list_context_profiles` | List all loaded context profiles |
| `get_context_profile` | Fetch a profile's tool allowlist and system prompt |
| `auto_assign_context` | Match a goal to the best profile |
| `build_agent_context` | Build a context bundle for a profile |

### Working Memory (4)

| Tool | Description |
|------|-------------|
| `push_context` | Append an entry to a session scratchpad |
| `get_context` | Retrieve session scratchpad entries |
| `summarise_session` | LLM-summarise the scratchpad into long-term memory |
| `list_sessions` | List all working-memory sessions |

### Dynamic Tool Builder (4 + runtime)

| Tool | Description |
|------|-------------|
| `define_tool` | Define a new MCP tool backed by a procedure; hot-registered immediately |
| `execute_procedure` | Run a stored procedure with template substitution |
| `list_dynamic_tools` | List all runtime-defined tools |
| `remove_dynamic_tool` | Delete a dynamic tool and unregister it live |

### Procedural Memory (2)

| Tool | Description |
|------|-------------|
| `store_procedure` | Store a named multi-step workflow |
| `search_procedures` | Search procedures by name or description |

### Shared Resource Registry (4)

| Tool | Description |
|------|-------------|
| `resource_register` | Register a named resource (connection, token, etc.) |
| `resource_get` | Retrieve a resource by name |
| `resource_list` | List registered resources |
| `resource_release` | Release a resource |

### WebSocket (5)

| Tool | Description |
|------|-------------|
| `ws_connect` | Open a WebSocket connection |
| `ws_send` | Send a message |
| `ws_receive` | Receive a message (with timeout) |
| `ws_close` | Close a connection |
| `ws_list` | List open connections |

### Graph Admin (10)

| Tool | Description |
|------|-------------|
| `delete_api` | Cascade-delete all graph nodes for one API |
| `purge_duplicate_endpoints` | Remove duplicate endpoint nodes |
| `purge_orphaned_schemas` | Delete unreferenced schema nodes |
| `reset_graph` | Wipe all API data (knowledge preserved) |
| `backfill_endpoint_embeddings` | Generate missing embeddings |
| `snapshot_knowledge` | Take a compressed graph snapshot |
| `restore_knowledge` | Restore from a snapshot |
| `list_snapshots` | List available snapshots |
| `verify_knowledge_integrity` | Scan for orphaned/duplicate/bad notes |
| `analyze_own_structure` | Introspect source tree and tool registry |

### Search (1)

| Tool | Description |
|------|-------------|
| `search_web` | Web search via SerpApi, Brave, or Google Custom Search |

### Sleep / Telemetry (2)

| Tool | Description |
|------|-------------|
| `digest_experiences` | Export successful interactions to JSONL for fine-tuning |
| `analyze_gaps` | Identify knowledge and capability gaps from telemetry |

## Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                         Entry Points                             │
│  cargo run -- repl   │   cargo run -- serve   │  MCP client      │
└────────────┬─────────┴──────────┬─────────────┴─────────────────┘
             │                    │ JSON-RPC 2.0
             ▼              ┌─────┴──────┐
     ┌───────────────┐      │  Stdio /   │
     │  REPL (chat)  │      │  HTTP+SSE  │
     └───────┬───────┘      └─────┬──────┘
             └──────────┬─────────┘
┌────────────────────────▼────────────────────────────────────────┐
│                       McpServerCore                             │
│  ┌──────────────────────────────────────────────────────────┐   │
│  │              90 Tools across 15 Skills                   │   │
│  │  Knowledge(16)  Task(6)  Agent(8)  Scheduler(5)          │   │
│  │  Api(14)  Model(5)  Context(4)  WorkingMemory(4)         │   │
│  │  Dynamic(4+N)  Procedure(2)  Resource(4)  Ws(5)          │   │
│  │  Admin(10)  Search(1)  Sleep(2)                          │   │
│  └──────────────────────────────────────────────────────────┘   │
└────────────────────────┬────────────────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────────────────┐
│                       Services Layer                            │
│  KnowledgeService  SchedulerService  QueueService  ChatService  │
│  LlmClient(Ollama/Anthropic/Gemini/vLLM)  SnapshotService       │
│  ContextBuilderService  HttpExecutor  OpenApiParser  Healing     │
│  Secrets(Local AES-GCM / Vault / AWS)                           │
└────────────────────────┬────────────────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────────────────┐
│                  Neo4j Knowledge Graph                          │
│  (Note)─RELATES_TO/DERIVED_FROM/SUMMARIZED_BY/PART_OF──(Note)   │
│  (Note)─MENTIONS──(Entity)                                      │
│  (Note)─REFLECTS_ON──(Task)─SUBTASK_OF/DEPENDS_ON──(Task)       │
│  (AgentJob)─RETURNS_SCHEMA──(Endpoint)─(Parameter/Schema)       │
│  (DynamicTool)─USES──(Procedure)  (ModelSpec)                   │
└─────────────────────────────────────────────────────────────────┘
```

## Project Structure

```
src/
├── main.rs              # CLI entry point + command dispatch
├── cli.rs               # Command definitions (agent-brain)
├── repl.rs              # Interactive terminal REPL
├── config.rs            # Environment configuration
├── models/              # Data models
├── repository/          # Neo4j database layer
├── services/            # Business logic
│   ├── knowledge.rs     # Notes/RAG + hybrid search
│   ├── chat.rs          # ChatService (agentic loop + SSE)
│   ├── queue.rs         # Priority job queue
│   ├── scheduler.rs     # Autonomous background scheduler
│   ├── llm.rs           # Multi-provider LLM client
│   ├── snapshot.rs      # Graph snapshot/restore
│   ├── context_builder.rs # Context profiles + boot protocol
│   └── ...
├── skills/              # Pluggable skill implementations
└── mcp/                 # MCP server (protocol, transport, tools)

contexts/                # Context profile YAML files
project-docs/            # Detailed reference docs
snapshots/               # Knowledge graph snapshots (gitignored)
```

## Development

```bash
cargo build                # Build
cargo fmt                  # Format
cargo clippy               # Lint
cargo test --lib           # Unit tests
cargo test --test '*'      # Integration tests (requires Neo4j)
cargo build --release      # Optimized build
# Binary at: target/release/agent-brain
```

## License

MIT
