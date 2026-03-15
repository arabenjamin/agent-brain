# Agent Brain — Usage Guide

An autonomous AI agent backed by a persistent Neo4j knowledge graph, exposing 90 tools via MCP and an interactive REPL.

---

## Interactive REPL

The quickest way to use the brain directly — no MCP client required.

```bash
cargo run -- repl
# With a context profile and persistent session
cargo run -- repl --profile knowledge-worker --session my-project
```

**Welcome banner:**

```
Agent Brain v0.1.0
━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━━
LLM:       Ollama (qwen3.5:4b)
Connected: bolt://localhost:7687

Type anything to interact. /help for commands, /quit to exit.
>
```

### REPL meta-commands

| Command | Action |
|---------|--------|
| `/quit`, `/exit` | Exit the REPL |
| `/clear` | Clear conversation history (keeps session) |
| `/new` | Clear history and start a new session ID |
| `/profile <name>` | Activate a context profile (e.g. `knowledge-worker`) |
| `/status` | Show session ID, turn count, and active profile |
| `/help` | List meta-commands |

### Example interactions

```
> Store a note: the brain uses qwen3.5:4b for Ollama generation

> Search my notes for anything about authentication

> Create a task to review all loaded API endpoints for broken schemas

> What do I know about Neo4j indexing?

> Run the scheduler tick and show me what jobs were dispatched
```

All 90 brain tools are available through natural language. The LLM decides which tools to call.

---

## Serving as an MCP Server

### Stdio transport (Claude Desktop, local MCP clients)

```bash
# Build once
cargo build --release

# Run (stdio is the default transport)
cargo run -- serve
```

Configure your MCP client:

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

### HTTP transport (remote / Docker)

```bash
cargo run -- serve --transport http
cargo run -- serve --transport http --bind 0.0.0.0:8080
cargo run -- serve --transport http --api-key my-secret-key

# Or via Docker Compose
docker compose up -d --build
MCP_API_KEY=your-key docker compose up -d --build
```

**Endpoints:**
- `POST http://localhost:3000/mcp` — JSON-RPC requests
- `GET  http://localhost:3000/mcp` — SSE stream
- `POST http://localhost:3000/chat` — Agentic chat with SSE event stream
- `GET  http://localhost:3000/health` — Health check

### HTTP session lifecycle

```bash
# 1. Initialize — capture session ID from response header
SESSION_ID=$(curl -si -X POST http://localhost:3000/mcp \
  -H "Content-Type: application/json" \
  -H "mcp-protocol-version: 2024-11-05" \
  -d '{
    "jsonrpc": "2.0", "id": 1, "method": "initialize",
    "params": {
      "protocolVersion": "2024-11-05",
      "capabilities": {},
      "clientInfo": {"name": "curl", "version": "1.0"}
    }
  }' | grep -i mcp-session-id | awk '{print $2}' | tr -d '\r')

# 2. Confirm initialization (REQUIRED — server rejects tool calls without this)
curl -s -X POST http://localhost:3000/mcp \
  -H "Content-Type: application/json" \
  -H "mcp-protocol-version: 2024-11-05" \
  -H "mcp-session-id: $SESSION_ID" \
  -d '{"jsonrpc": "2.0", "method": "notifications/initialized"}'

# 3. Call any tool
curl -s -X POST http://localhost:3000/mcp \
  -H "Content-Type: application/json" \
  -H "mcp-protocol-version: 2024-11-05" \
  -H "mcp-session-id: $SESSION_ID" \
  -d '{
    "jsonrpc": "2.0", "id": 2, "method": "tools/call",
    "params": {"name": "get_scheduler_status", "arguments": {}}
  }'
```

---

## Status Command

```bash
cargo run -- status
```

Shows API resource/endpoint/schema counts and healing event statistics.

---

## API Integration (subcommand)

OpenAPI spec management is grouped under `api`:

```bash
# Ingest a spec
cargo run -- api ingest https://petstore3.swagger.io/api/v3/openapi.json
cargo run -- api ingest ./openapi.yaml

# Query endpoints
cargo run -- api query "pets"
cargo run -- api query "/api/v1/payments"

# Execute an HTTP request
cargo run -- api execute -m GET https://petstore3.swagger.io/api/v3/pet/1
cargo run -- api execute -m POST https://api.example.com/users \
  -b '{"name": "Alice"}' -H "Authorization: Bearer token"

# Export the healed graph back to OpenAPI
cargo run -- api export -o healed.yaml
cargo run -- api export -f json -o healed.json
cargo run -- api export --annotations=false -o clean.yaml

# Diff original spec vs healed graph
cargo run -- api diff
cargo run -- api diff -f changelog
cargo run -- api diff --breaking-only

# Generate missing endpoint embeddings
cargo run -- api embed
```

---

## Environment Variables

Copy `.env.example` to `.env` and configure:

```bash
# Neo4j (required)
NEO4J_URI=bolt://localhost:7687
NEO4J_USER=neo4j
NEO4J_PASSWORD=your-password

# LLM (choose one provider)
LLM_PROVIDER=ollama            # or: anthropic, gemini, vllm
OLLAMA_URL=http://localhost:11434
OLLAMA_MODEL=qwen3.5:4b
OLLAMA_EMBED_MODEL=bge-m3:latest
# ANTHROPIC_API_KEY=sk-ant-...
# GEMINI_API_KEY=...
# VLLM_URL=http://localhost:8000
# VLLM_MODEL=meta-llama/Llama-3.1-8B-Instruct

# MCP transport
MCP_TRANSPORT=stdio            # or: http
MCP_HTTP_BIND=0.0.0.0:3000
MCP_API_KEY=your-secret-key    # optional

# Autonomous scheduler
SCHEDULER_ENABLED=true
SCHEDULER_INTERVAL_SECS=300    # poll every 5 minutes

# Web search (configure at least one)
SERPAPI_KEY=...
# BRAVE_API_KEY=...
# GOOGLE_API_KEY=... + GOOGLE_CX=...
```

See `project-docs/env.md` for the complete variable reference.

---

## Common Tool Patterns

### Store knowledge and reason over it

```json
{"name": "store_note", "arguments": {"content": "The Stripe API uses idempotency keys via the Idempotency-Key header."}}
{"name": "reason", "arguments": {"question": "How should I handle Stripe duplicate requests?"}}
```

### Decompose a goal and let the scheduler run it

```json
{"name": "create_task", "arguments": {"goal": "Audit all loaded APIs for broken endpoints"}}
{"name": "decompose_goal", "arguments": {"goal_task_id": "<id>", "max_steps": 5}}
{"name": "run_scheduler_tick", "arguments": {}}
{"name": "queue_status", "arguments": {}}
```

### Submit a background job chain

```json
{
  "name": "enqueue_chain",
  "arguments": {
    "steps": [
      {"tool_name": "search_notes", "arguments": {"query": "API authentication patterns"}},
      {"tool_name": "reason", "arguments": {"question": "Best auth pattern for public APIs?"}}
    ]
  }
}
```

### Ingest and query an API

```json
{"name": "ingest_openapi", "arguments": {"source": "https://petstore3.swagger.io/api/v3/openapi.json"}}
{"name": "graph_query_endpoint", "arguments": {"query": "pets"}}
{"name": "execute_http_request", "arguments": {"method": "GET", "url": "https://petstore3.swagger.io/api/v3/pet/1"}}
```

### Switch LLM provider at runtime

```json
{"name": "use_model", "arguments": {"provider": "Anthropic", "model": "claude-haiku-4-5-20251001"}}
```

---

## Skill Capabilities (90 tools across 15 skills)

See `project-docs/tools.md` for full input/output schemas.

| Skill | Tools | Description |
|-------|-------|-------------|
| `KnowledgeSkill` | 16 | Notes, RAG, reasoning, entity extraction |
| `TaskSkill` | 6 | Goals, decomposition, reflection, outcomes |
| `AgentSkill` | 8 | Background job queue management |
| `SchedulerSkill` | 5 | Autonomous tick loop |
| `ApiSkill` | 14 | OpenAPI ingest, query, execute, heal, export |
| `ModelSkill` | 5 | LLM provider switching and model registry |
| `ContextSkill` | 4 | YAML context profiles |
| `WorkingMemorySkill` | 4 | Session scratchpad |
| `DynamicSkill` | 4+N | Runtime tool definition |
| `ProcedureSkill` | 2 | Named multi-step workflows |
| `ResourceSkill` | 4 | Shared connection/token registry |
| `WsSkill` | 5 | WebSocket connections |
| `AdminSkill` | 10 | Graph cleanup, snapshots, integrity |
| `SearchSkill` | 1 | Web search |
| `SleepSkill` | 2 | Experience digestion, gap analysis |
