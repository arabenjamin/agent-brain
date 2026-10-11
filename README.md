# Agent Brain

A persistent, self-improving ( lol hardly ) autonomous agent ( kinda ) backed by a Neo4j knowledge graph (thats true).

## tl;dr
Human here, this project got out of hand quickly. It started as an mcp server, that grew into something very different. 
Let me just say I dont care for the name, but thats what I picked when I started it. 
Despite my best attempts, this is a managed dumpster fire, though, it may yet have it's uses and some potential. 
Did I vibe code this ? If you want to call it that. I'm certainly a sofware engineer and know my way around software and programming. 
I have been using claude to put this together. Nothing about this is one-shot or from the hip, there has been a lot of planning and replanning, iteration after iteration, and careful evaluation. I've dog-fooded it, and make others do the same.
I have a NVidia3060, and the goal was to get this to work locally on a small GPU, and to some degree of success. Though it looks like it works a lot better with a 5090. One could certainly use this with frontier cloud models and the quality and the richness of the results is definately felt. But the idea was to get this to run on smaller software.



## What It Does

- **Remembers** — notes with hybrid BM25+vector search, spaced repetition, entity extraction
- **Reasons** — LLM inference over stored knowledge with derivation tracking
- **Plans** — goal decomposition into ordered sub-tasks, autonomous scheduling
- **Executes** — durable priority job queue with per-provider concurrency
- **Extends** — runtime tool definition via stored procedure pipelines
- **Integrates** — OpenAPI spec ingestion, credential injection, HTTP execution
- **Specialises** — plugin bundles under `plugins/` (`core` always; others via `PLUGINS_ENABLED`) carry workflows, agent profiles, source lists and MCP servers; `GET /api/plugins` shows what loaded and why not

## Prerequisites

- Rust 1.75+
- Docker & Docker Compose
- Ollama (for LLM features, or llama.cpp)

## Getting Started

### Quickest path to victory
```bash
docker compose up -d --build
```

### Longer path

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

# Run as MCP server (stdio; add --transport http for the HTTP + /chat front door)
cargo run --release -- serve
```

## MCP Tools

The live list is `list_capabilities` (`kind: tools`) or `GET /api/tools`; it
also names tools that were removed and what replaced them. A guide by group:

### Knowledge
`store_note`, `search_notes`, `recall_recent`, `prune_old_notes`, `consolidate_memories`, `reason`, `synthesize_knowledge`

### Tasks
`create_task`, `update_task`, `list_tasks`, `reflect_on_work`

### Job Queue
`enqueue_jobs`, `manage_job`, `dead_letter`

### Scheduler
`scheduler_control` (`action: tick` runs one now), `manage_workflow` (list, get, run by name with inputs, upsert, delete, audit)

### Working Memory
`push_context`, `notify_user`

### Query
`neo4j_query`, `duckdb_query`, `describe_graph`, `list_capabilities`

### Plugins
`manage_plugin` (drafts a bundle into `PLUGINS_RUNTIME_DIR`; it loads only once you add it to `PLUGINS_ENABLED`). Declared tools (a bundle's `tools/*.yaml`, read-only Cypher) such as `list_tasks`

Plugin tools (each from its bundle in `plugins/`, on by default in compose):
- `media`: `ingest_media`, `ingest_article`, `poll_media_sources`, `manage_media_source`, `discover_media_sources`, `spawn_gap_tasks`
- `reference`: `link_entities`, `wiki_background` (needs `WIKIMEDIA_CONTACT`); routes under `/api/plugins/reference/`
- `alias-judge`: no tools; ranks entity merge candidates on idle time (`ALIAS_JUDGE_MODEL`)
- `developer`: `read_codebase_file`, `search_codebase`, `get_git_log`, `write_codebase_doc`, `write_proposal`, `analyze_own_structure`, `write_workspace_file`, `list_workspace_files`, `snapshot_architecture`; the architecture view at `/api/plugins/developer/architecture`
- `osint-data`: `acled_events` (needs `ACLED_EMAIL`, `ACLED_PASSWORD`)

### HTTP / API / MCP
`http_request`, `define_api_context`, `ingest_api`, `mcp_list_tools` (no `server` lists the servers), `mcp_call`

### Model
`get_llm_budget`, `gpu_lease`

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
`search_web`, `get_search_usage`

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


## CLI Commands

```
serve            Run as MCP server (default)
init-db          Initialize Neo4j schema
repair-notes     One-off note repair migration
rewrite-claims   Restate stored claims so each stands alone (--dry-run first)
eval-retrieval   Score retrieval against a golden set
todo             Manage todos over HTTP
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
| `PLUGINS_DIR` / `PLUGINS_ENABLED` | `plugins` / *(core only)* | Where the seed bundles live, and which load besides `core` (see `project-docs/env.md`) |
| `TZ` | — (⇒ UTC) | Wall-clock timezone (e.g. `America/Detroit`). What the brain **says** about time is local; what it **stores** stays UTC. Unset means its "today" is UTC's, which is a different day for part of every day outside UTC |

## Development


## License

MIT
