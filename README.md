# Agent Brain

A persistent, self-improving autonomous agent backed by a Neo4j knowledge graph. Exposes ~50 tools via MCP with built-in interactive chat.

## What It Does

- **Remembers** — notes with hybrid BM25+vector search, spaced repetition, entity extraction
- **Reasons** — LLM inference over stored knowledge with derivation tracking
- **Plans** — goal decomposition into ordered sub-tasks, autonomous scheduling
- **Executes** — durable priority job queue with per-provider concurrency
- **Extends** — runtime tool definition via stored procedure pipelines
- **Integrates** — OpenAPI spec ingestion, credential injection, HTTP execution

## Prerequisites

- Rust 1.75+
- Docker & Docker Compose
- Ollama (for LLM features)

## Getting Started

```bash
# Clone and build
git clone <repo-url>
cd agent-brain
cargo build --release

# Start dependencies
docker compose up -d

# Configure
cp .env.example .env

# Initialize database
cargo run --release -- init-db

# Run interactive chat
cargo run --release -- repl

# Or run as MCP server
cargo run --release -- serve
```

## MCP Tools (~50 tools)

### Knowledge
`store_note`, `search_notes`, `prune_old_notes`, `consolidate_memories`, `reason`, `synthesize_knowledge`

### Tasks
`create_task`, `reflect_on_work`, `decompose_goal`, `update_task`, `record_outcome`

### Job Queue
`manage_job`, `set_worker_config`, `enqueue_jobs`, `dead_letter`, `update_job_progress`

### Scheduler
`scheduler_control`, `run_scheduler_tick`, `manage_chain`, `manage_scheduled_task`

### Working Memory
`push_context`, `notify_user`, `summarise_session`

### Codebase
`read_codebase_file`, `list_codebase_files`, `search_codebase`, `get_file_tree`, `get_git_log`, `get_git_diff`, `list_proposals`, `read_proposal`, `dismiss_proposal`, `write_proposal`, `analyze_own_structure`

### Query
`neo4j_query`, `duckdb_query`

### WebSocket
`ws_connect`, `ws_send`, `ws_receive`, `ws_close`

### Dynamic Tools
`manage_dynamic_tool`, `execute_procedure`, `store_procedure`

### HTTP / API
`http_request`, `define_api_context`

### Model
`use_model`, `reload_models`

### Code Execution
`execute_code`

Runs Python in an isolated `sandbox` sidecar and returns what it printed, so
quantitative steps compute their numbers instead of narrating them — prose
arithmetic fails silently and confidently, and nothing downstream re-derives it.
The sandbox sits on an `internal: true` Docker network with **no egress**, a
read-only root filesystem, dropped capabilities, none of the brain's
credentials, and no bind mounts; every input must be passed inline in the code.
`numpy`, `sympy`, and `pandas` are available. A failed run returns its traceback
as a *successful* tool call, so the model can fix the code rather than burn a
retry. Registered only when `SANDBOX_URL` is set.

### Other
`search_web`, `get_search_usage`, `resource`, `context`, `digest_experiences`, `analyze_gaps`

`search_web` walks an engine failover ladder (`SEARCH_ENGINE_ORDER`, default
`searxng,google,serpapi,brave`) so one exhausted free tier cannot fail the call.
The self-hosted SearXNG sidecar leads because it has no key and no quota;
`get_search_usage` reports per-engine burn rate against the keyed fallbacks.

## Connect via HTTP

```bash
# Start server
cargo run --release -- serve --transport http

# With custom port and API key
cargo run --release -- serve --transport http --bind 0.0.0.0:8080 --api-key your-secret-key
```

**Endpoints:**
- `POST /mcp` — JSON-RPC requests
- `GET /mcp` — SSE stream
- `POST /chat` — Interactive chat with SSE
- `GET /health` — Health check

### Claude Desktop Config

```json
{
  "mcpServers": {
    "agent-brain": {
      "command": "/path/to/agent-brain",
      "args": ["serve"],
      "env": {
        "NEO4J_URI": "bolt://localhost:7687",
        "NEO4J_PASSWORD": "password"
      }
    }
  }
}
```

### Docker

```bash
docker compose up -d --build
# With API key
MCP_API_KEY=secret docker compose up -d --build
```

## Using the API

```bash
# Ingest an OpenAPI spec
cargo run --release -- api ingest ./openapi.yaml

# Query endpoints
cargo run --release -- api query "pets"

# Execute HTTP request
cargo run --release -- api execute -m GET https://api.example.com/users

# Export healed spec
cargo run --release -- api export -o healed.yaml
```

## CLI Commands

```
repl     Interactive chat (default)
serve    Run as MCP server
init-db  Initialize Neo4j schema
api      OpenAPI management
status   Show brain status
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NEO4J_URI` | `bolt://localhost:7687` | Neo4j connection |
| `NEO4J_PASSWORD` | — | Neo4j password (required) |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama endpoint |
| `OLLAMA_MODEL` | `qwen3.5:4b` | LLM model |
| `MCP_TRANSPORT` | `stdio` | Transport type |
| `MCP_HTTP_BIND` | `127.0.0.1:3000` | HTTP bind |
| `MCP_API_KEY` | — | HTTP auth |
| `SANDBOX_URL` | — | Code-execution sandbox endpoint. Unset ⇒ `execute_code` is not registered |

## Development

```bash
cargo build
cargo fmt
cargo clippy
cargo test --lib
```

## License

MIT