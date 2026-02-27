# Agent Brain

An autonomous MCP server — a persistent, self-improving AI agent backed by a Neo4j knowledge graph. It ingests OpenAPI specs, manages long-term memory via hybrid vector+BM25 RAG, executes background jobs in a priority queue, reasons over stored knowledge, learns from its own outcomes, and runs an autonomous background scheduler that continuously improves itself.

## What It Does

- **Ingests** OpenAPI/Swagger specs into a queryable knowledge graph
- **Self-heals** documentation when API requests fail (AI-powered corrections)
- **Exports** healed specs back to OpenAPI 3.0 for version control
- **Remembers** notes and knowledge with hybrid vector+BM25 search and spaced-repetition
- **Reasons** over stored knowledge to answer questions and derive new inferences
- **Plans** by decomposing high-level goals into ordered sub-tasks
- **Executes** background jobs asynchronously in a durable priority queue
- **Extends itself** by defining new MCP tools backed by stored procedure pipelines
- **Searches** the web via SerpApi, Brave, or Google Custom Search
- **Schedules** autonomous background ticks to dispatch pending tasks to the job queue
- **Connects** to any MCP-compatible client via stdio or HTTP/SSE

## Quick Start

```bash
# 1. Clone and build
git clone <repo-url>
cd agent-brain
cargo build --release

# 2. Start Neo4j
docker compose up -d

# 3. Initialize database
cp .env.example .env
cargo run -- init-db

# 4. Ingest an API spec
cargo run -- ingest https://petstore3.swagger.io/api/v3/openapi.json

# 5. Query endpoints
cargo run -- query "pets"
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

# Start Neo4j database
docker compose up -d

# Copy environment config
cp .env.example .env

# Build the project
cargo build --release

# Initialize database schema
cargo run --release -- init-db
```

### Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NEO4J_URI` | `bolt://localhost:7688` | Neo4j connection URI |
| `NEO4J_USER` | `neo4j` | Neo4j username |
| `NEO4J_PASSWORD` | `password` | Neo4j password |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama API endpoint |
| `OLLAMA_MODEL` | `granite3.3:8b` | LLM model for text generation / self-healing |
| `OLLAMA_EMBED_MODEL` | - | Ollama model for embeddings (e.g. `bge-m3:latest`). Falls back to `OLLAMA_MODEL` if unset |
| `LOG_LEVEL` | `info` | Log level (trace/debug/info/warn/error) |
| `MCP_TRANSPORT` | `stdio` | MCP transport (stdio/http) |
| `MCP_HTTP_BIND` | `127.0.0.1:3000` | HTTP bind address |
| `MCP_API_KEY` | - | API key for HTTP authentication |
| `LLM_PROVIDER` | `ollama` | LLM provider (ollama/anthropic/gemini) |
| `ANTHROPIC_API_KEY` | - | Anthropic API key |
| `GEMINI_API_KEY` | - | Gemini API key |
| `SECRET_PROVIDER` | `local` | Secret provider (local/vault/aws/none) |
| `SECRETS_FILE` | `.secrets.enc` | Encrypted secrets file path |
| `SECRETS_ENCRYPTION_KEY` | - | Encryption key for local secrets |
| `SERPAPI_KEY` | - | SerpApi key for `search_web` tool |
| `BRAVE_API_KEY` | - | Brave Search API key for `search_web` tool |
| `GOOGLE_API_KEY` | - | Google Custom Search API key |
| `GOOGLE_CX` | - | Google Custom Search Engine ID |
| `SCHEDULER_INTERVAL_SECS` | `300` | How often the autonomous scheduler polls for pending tasks |
| `SCHEDULER_ENABLED` | `true` | Enable autonomous background scheduling at startup |
| `DATASET_DIR` | `./datasets` | Directory for training data export |

## CLI Usage

### Ingest OpenAPI Specs

```bash
# From URL
cargo run -- ingest https://api.example.com/openapi.json

# From local file
cargo run -- ingest ./openapi.yaml
```

### Query Endpoints

```bash
# Search by path or keyword
cargo run -- query "users"
cargo run -- query "/api/v1/payments"
```

### Execute HTTP Requests

```bash
# GET request
cargo run -- execute -m GET https://api.example.com/users

# POST with body and headers
cargo run -- execute -m POST https://api.example.com/users \
  -b '{"name": "John", "email": "john@example.com"}' \
  -H "Content-Type: application/json" \
  -H "Authorization: Bearer token123"
```

### Export Healed Specs

```bash
# Export to YAML (default)
cargo run -- export -o healed-spec.yaml

# Export to JSON
cargo run -- export -f json -o healed-spec.json

# Without AI annotations
cargo run -- export --annotations=false -o clean-spec.yaml
```

### Generate Diff Reports

```bash
# Markdown report
cargo run -- diff

# Git-style changelog
cargo run -- diff -f changelog

# JSON format
cargo run -- diff -f json

# Only breaking changes
cargo run -- diff --breaking-only
```

### Other Commands

```bash
# Show database statistics
cargo run -- stats

# Run as MCP server (stdio transport — default)
cargo run -- serve

# Run as MCP server (HTTP transport — for remote access)
cargo run -- serve --transport http
cargo run -- serve --transport http --bind 0.0.0.0:8080
cargo run -- serve --transport http --api-key my-secret-key

# Initialize/reset database schema
cargo run -- init-db
```

## MCP Client Integration

Connect this tool to any MCP-compatible client for AI-assisted API exploration, autonomous task execution, and knowledge management.

### Stdio Transport (Local)

Build the binary and configure your MCP client:

```bash
cargo build --release
```

Example `mcpServers` configuration entry:

```json
{
  "mcpServers": {
    "agent-brain": {
      "command": "/path/to/agent-brain/target/release/agent-brain",
      "args": ["serve"],
      "env": {
        "NEO4J_URI": "bolt://localhost:7688",
        "NEO4J_USER": "neo4j",
        "NEO4J_PASSWORD": "password",
        "OLLAMA_URL": "http://localhost:11434",
        "OLLAMA_MODEL": "granite3.3:8b",
        "SERPAPI_KEY": "your-serpapi-key"
      }
    }
  }
}
```

### HTTP Transport (Remote / Docker)

```bash
# Build and start all services (Neo4j + MCP Server)
docker compose up -d --build

# With API key authentication
MCP_API_KEY=your-secret-key docker compose up -d --build
```

**Endpoints:**
- `POST http://localhost:3000/mcp` — JSON-RPC requests
- `GET  http://localhost:3000/mcp` — SSE stream for server-initiated messages
- `GET  http://localhost:3000/health` — Health check

### Available MCP Tools (64)

**API Tools (14)**

| Tool | Description |
|------|-------------|
| `ingest_openapi` | Load OpenAPI specs into the knowledge graph |
| `graph_query_endpoint` | Search endpoints by path or keyword |
| `execute_http_request` | Execute API calls with auto-credential injection |
| `get_api_context` | Retrieve API summaries for context |
| `list_loaded_apis` | List all ingested APIs |
| `clear_api_context` | Clear cached API context |
| `discover_openapi` | Auto-discover OpenAPI specs from a base URL |
| `build_openapi_from_docs` | Generate specs from documentation pages |
| `build_openapi_from_repo` | Generate specs from repository source code |
| `export_openapi` | Export healed specs to YAML/JSON |
| `diff_api_spec` | Generate documentation drift reports |
| `configure_api_credential` | Store API credentials for automatic injection |
| `list_api_credentials` | List all configured credentials |
| `delete_api_credential` | Remove an API credential |

**Search Tools (1)**

| Tool | Description |
|------|-------------|
| `search_web` | Search the web via SerpApi, Brave, or Google Custom Search |

**Task Management Tools (6)**

| Tool | Description |
|------|-------------|
| `create_task` | Create and persist a high-level goal or task |
| `reflect_on_work` | LLM critique of current progress; persists a reflection Note |
| `decompose_goal` | LLM-breaks a task into ordered sub-tasks with `SUBTASK_OF` graph edges |
| `update_task` | Set task status (in_progress/completed/failed/blocked) with optional note |
| `list_tasks` | List tasks with optional status filter and parent_id |
| `record_outcome` | Store an episodic outcome note linked to a task |

**Knowledge Tools (10)**

| Tool | Description |
|------|-------------|
| `store_note` | Persist a note; auto-chunks, embeds, links similar notes, extracts entities |
| `search_notes` | Hybrid BM25+vector RRF search with multi-hop graph expansion |
| `find_related_notes` | Find notes linked via RELATES_TO graph edges |
| `prune_old_notes` | Delete stale notes via adaptive decay or time-based thresholds |
| `consolidate_memories` | LLM synthesis of multiple notes into a summary note |
| `review_due_notes` | Return notes whose spaced-repetition review interval has elapsed |
| `search_by_entity` | Find notes mentioning a named entity |
| `reason` | RAG + LLM inference; stores inference notes with DERIVED_FROM edges |
| `audit_action` | Check a proposed action against stored principles via LLM |
| `explain_reasoning` | Narrate why a decision was made, citing source notes |

**Procedural Memory Tools (2)**

| Tool | Description |
|------|-------------|
| `store_procedure` | Store a named multi-step workflow |
| `search_procedures` | Search stored procedures by name or description |

**Working Memory Tools (3)**

| Tool | Description |
|------|-------------|
| `push_context` | Append an entry to a session scratchpad |
| `get_context` | Retrieve all session scratchpad entries in turn order |
| `summarise_session` | LLM-summarise the session scratchpad into a long-term Note |

**Dynamic Tool Builder (4 + runtime)**

| Tool | Description |
|------|-------------|
| `define_tool` | Define a new MCP tool backed by a procedure pipeline; hot-registered immediately |
| `execute_procedure` | Run a stored procedure with template substitution (`{{input.field}}`) |
| `list_dynamic_tools` | List all runtime-defined tools |
| `remove_dynamic_tool` | Delete a dynamic tool and unregister it live |

**Agent Job Queue (8)**

| Tool | Description |
|------|-------------|
| `enqueue_agent` | Submit any MCP tool as a background job (priority 0-3, persistent, retryable) |
| `enqueue_chain` | Submit an ordered chain of jobs; each step auto-promotes when its predecessor completes |
| `queue_status` | Stats: pending, running, per-status counts, worker config |
| `get_job_result` | Poll a job for its current status and result |
| `cancel_job` | Cancel a queued or running job |
| `retry_job` | Requeue a failed, dead, or cancelled job |
| `set_worker_config` | Change concurrency limit, enable/pause processing, poll interval |
| `drain_queue` | Cancel all currently pending jobs |

**Graph Admin Tools (4)**

| Tool | Description |
|------|-------------|
| `delete_api` | Cascade-delete all graph nodes for one ingested API (dry_run supported) |
| `purge_duplicate_endpoints` | Remove duplicate Endpoint nodes (same resource + path + method) |
| `purge_orphaned_schemas` | Delete Schema nodes with no Endpoint relationships |
| `reset_graph` | Wipe all API data; knowledge data preserved (requires `confirm: true`) |

**Model Registry Tools (5)**

| Tool | Description |
|------|-------------|
| `list_models` | List available LLM providers and all registered model specs |
| `use_model` | Switch the active LLM provider and model at runtime |
| `register_model` | Register a new model spec (capabilities, cost, context window) |
| `select_model` | Auto-select the cheapest capable model for a set of requirements |
| `get_model_stats` | Get usage statistics for a model from job history |

**Sleep / Telemetry Tools (2)**

| Tool | Description |
|------|-------------|
| `digest_experiences` | Export successful interactions to JSONL datasets for fine-tuning |
| `analyze_gaps` | Identify knowledge gaps and missing capabilities from telemetry |

**Autonomous Scheduler Tools (5)**

| Tool | Description |
|------|-------------|
| `start_scheduler` | Enable autonomous scheduling; optionally set interval and session_id |
| `stop_scheduler` | Pause the autonomous scheduler |
| `get_scheduler_status` | Get current scheduler config, stats, and running state |
| `configure_scheduler` | Update interval, enabled state, max tasks per tick, error budget |
| `run_scheduler_tick` | Manually trigger a scheduler tick immediately |

## How Self-Healing Works

When an API request fails (4xx/5xx error):

1. **Capture** the error response and current endpoint schema
2. **Analyze** with LLM to identify the issue (wrong parameter name, type mismatch, etc.)
3. **Suggest** a correction based on the error message
4. **Retry** the request with the fix applied
5. **Update** the knowledge graph if successful
6. **Record** a `HealingEvent` with the change details

The healed documentation can then be exported and committed to version control.

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                  Any MCP-Compatible Client                  │
└─────────────────────────┬───────────────────────────────────┘
                          │ JSON-RPC 2.0
               ┌──────────┴──────────┐
               ▼                     ▼
┌─────────────────────┐  ┌─────────────────────┐
│   Stdio Transport   │  │   HTTP Transport    │
│  (local CLI usage)  │  │  (remote/cloud)     │
│                     │  │  POST /mcp          │
│                     │  │  GET  /mcp (SSE)    │
│                     │  │  API key auth       │
└──────────┬──────────┘  └──────────┬──────────┘
           └──────────┬─────────────┘
┌─────────────────────▼───────────────────────────────────────┐
│                     McpServerCore                           │
│   ┌─────────────────────────────────────────────────────┐   │
│   │  65 Tools (12 Skills + runtime-defined tools)       │   │
│   │  ApiSkill(14)  SearchSkill(1)  TaskSkill(6)         │   │
│   │  KnowledgeSkill(10)  ProcedureSkill(2)              │   │
│   │  WorkingMemorySkill(3)  DynamicSkill(4+runtime)     │   │
│   │  AgentSkill(8)  AdminSkill(5)  ModelSkill(5)        │   │
│   │  SleepSkill(2)  SchedulerSkill(5)                   │   │
│   └─────────────────────────────────────────────────────┘   │
│   ┌─────────────┐  ┌─────────────┐  ┌────────────────────┐  │
│   │  Sessions   │  │  ToolReg.   │  │  Protocol Handler  │  │
│   │  (HTTP)     │  │  (Skill     │  │  (JSON-RPC 2.0)    │  │
│   │             │  │   dispatch) │  │                    │  │
│   └─────────────┘  └─────────────┘  └────────────────────┘  │
└────────────────────────┬────────────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────────────┐
│                      Services Layer                         │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────────┐   │
│  │ OpenAPI  │ │   HTTP   │ │   LLM    │ │  Knowledge   │   │
│  │ Parser   │ │ Executor │ │  Client  │ │  Service     │   │
│  └──────────┘ └──────────┘ └──────────┘ └──────────────┘   │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────────┐   │
│  │ Queue    │ │Scheduler │ │Procedure │ │    Export    │   │
│  │ Service  │ │ Service  │ │ Executor │ │   Module     │   │
│  │(BinaryH) │ │(background│ │(template)│ │              │   │
│  └──────────┘ └──────────┘ └──────────┘ └──────────────┘   │
│  ┌──────────────────────────────────────────────────────┐   │
│  │  Secrets  │  Local(AES-GCM) │ Vault(KV v2) │ AWS    │   │
│  └──────────────────────────────────────────────────────┘   │
└────────────────────────┬────────────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────────────┐
│                   Neo4j Knowledge Graph                     │
│                                                             │
│  (Resource)──►(Endpoint)──►(Parameter/Schema/HealingEvent)  │
│  (Note)──►RELATES_TO/DERIVED_FROM/SUMMARIZED_BY/PART_OF     │
│  (Note)──►REFLECTS_ON──►(Task)──►SUBTASK_OF──►(Task)        │
│  (Note)──►MENTIONS──►(Entity)                               │
│  (DynamicTool)──►USES──►(Procedure)                         │
│  (AgentJob) — background job lifecycle                      │
│  (ModelSpec) — model registry                               │
└─────────────────────────────────────────────────────────────┘
                         │
┌────────────────────────▼────────────────────────────────────┐
│              DuckDB  (brain_logs.db — Telemetry)            │
│   interactions table │ knowledge_gaps table                 │
└─────────────────────────────────────────────────────────────┘
```

## Project Structure

```
agent-brain/
├── src/
│   ├── main.rs              # CLI entry point
│   ├── cli.rs               # Command definitions
│   ├── config.rs            # Environment configuration
│   ├── models/              # Data models
│   │   ├── agent_job.rs     # AgentJob + AgentJobStatus + PrioritizedJob
│   │   ├── credential.rs    # API credential model
│   │   ├── model_spec.rs    # ModelSpec — model registry entry
│   │   ├── procedure.rs     # Procedure (stored workflow) model
│   │   ├── task.rs          # Task / goal model
│   │   └── ...              # Endpoint, Schema, Parameter, etc.
│   ├── repository/          # Neo4j database layer
│   │   ├── agent_job.rs     # AgentJob CRUD + chain unpark/cancel
│   │   ├── client.rs        # Neo4jClient + schema init
│   │   ├── credential.rs    # Credential CRUD
│   │   ├── model_spec.rs    # ModelSpec CRUD (upsert by name)
│   │   ├── task.rs          # Task CRUD (including link_subtask, list_tasks, store_outcome_note)
│   │   └── telemetry.rs     # DuckDB telemetry client
│   ├── services/            # Business logic
│   │   ├── queue.rs         # QueueService — priority BinaryHeap + Tokio coordinator
│   │   ├── scheduler.rs     # SchedulerService — autonomous background tick loop
│   │   ├── knowledge.rs     # Notes/RAG — reason, audit_action, explain_reasoning
│   │   ├── model_selector.rs # ModelSelector — capability-filter + cheapest-first selection
│   │   ├── procedure_executor.rs # Template-substitution step runner ({{input.x}})
│   │   ├── openapi.rs       # Spec parser + ingester
│   │   ├── http.rs          # HTTP executor with self-healing
│   │   ├── llm.rs           # Multi-provider LLM client (Ollama/Anthropic/Gemini)
│   │   ├── healing.rs       # Self-healing orchestrator
│   │   ├── context.rs       # In-memory API context store
│   │   ├── discovery.rs     # Spec auto-discovery
│   │   ├── docgen.rs        # Doc-to-spec generator
│   │   ├── repo.rs          # Repo-to-spec generator
│   │   ├── sleep.rs         # Sleep cycle / experience digestion
│   │   ├── export/          # Graph-to-spec export module
│   │   └── secrets/         # Secret provider abstraction (local/Vault/AWS)
│   ├── skills/              # Pluggable skill implementations
│   │   ├── mod.rs           # Skill trait
│   │   ├── admin.rs         # AdminSkill — 5 graph cleanup tools
│   │   ├── agent.rs         # AgentSkill — 8 queue management tools
│   │   ├── api.rs           # ApiSkill — 14 tools
│   │   ├── dynamic.rs       # DynamicSkill — 4 tools + runtime-defined tools
│   │   ├── knowledge.rs     # KnowledgeSkill — 10 tools
│   │   ├── model.rs         # ModelSkill — 5 model registry tools
│   │   ├── procedure.rs     # ProcedureSkill — 2 tools
│   │   ├── scheduler.rs     # SchedulerSkill — 5 autonomous scheduler tools
│   │   ├── search.rs        # SearchSkill — 1 tool
│   │   ├── sleep.rs         # SleepSkill — 2 telemetry / experience digestion tools
│   │   ├── task.rs          # TaskSkill — 6 tools
│   │   └── working_memory.rs # WorkingMemorySkill — 3 tools
│   └── mcp/                 # MCP server implementation
│       ├── protocol.rs      # JSON-RPC 2.0 message types
│       ├── transport.rs     # Stdio transport
│       ├── transport_trait.rs  # McpTransport abstraction
│       ├── http_transport.rs   # Axum-based HTTP+SSE transport
│       ├── session.rs       # HTTP session management
│       ├── auth.rs          # API key authentication
│       ├── tools.rs         # ToolRegistry + ToolHandler
│       └── server.rs        # McpServerCore + McpServer (legacy stdio)
├── tests/
│   ├── common/              # Test utilities
│   ├── fixtures/            # Sample OpenAPI specs (petstore.json)
│   ├── repository_test.rs
│   ├── context_tools_test.rs
│   ├── discovery_test.rs
│   ├── docgen_test.rs
│   ├── repo_analyzer_test.rs
│   ├── http_transport_test.rs
│   └── task_test.rs
├── STATUS.md                # Current state and skill registry
├── TODO.md                  # Backlog and next phases
├── USAGE.md                 # Deployment and session guide
├── docker-compose.yml       # Neo4j + MCP server stack
└── .github/workflows/       # CI/CD pipelines
```

## Development

### Run Tests

```bash
# Unit tests
cargo test --lib

# Integration tests (requires Neo4j)
cargo test --test '*'

# All tests
cargo test
```

### Code Quality

```bash
# Format code
cargo fmt

# Run linter
cargo clippy
```

### Build Release

```bash
cargo build --release
# Binary at: target/release/agent-brain
```

## CI/CD

The repository includes GitHub Actions workflows:

- **ci.yml**: Format, lint, and test on push
- **api-contract.yml**: Validate OpenAPI specs, detect breaking changes

See `.github/workflows/` for details.

## License

MIT
