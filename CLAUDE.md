# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Autonomous Agent Brain — A persistent, self-improving MCP server in Rust backed by a Neo4j knowledge graph. Manages long-term memory with hybrid vector+BM25 RAG, executes background jobs in a durable priority queue, reasons over stored knowledge, ingests and self-heals OpenAPI specs, and runs an autonomous background scheduler that continuously improves itself by dispatching pending tasks as job chains.

## Tech Stack

- **Language:** Rust (Tokio async runtime, Edition 2024)
- **Protocol:** Model Context Protocol (MCP) via stdio or HTTP transport
- **Web Framework:** Axum (for HTTP transport with SSE streaming)
- **Database:** Neo4j via `neo4rs` driver
- **AI Model:** Pluggable LLM providers — Ollama (local), Anthropic, or Gemini

## Build Commands

```bash
cargo build                    # Build the project
cargo build --release          # Build optimized release
cargo fmt                      # Format code
cargo clippy                   # Run linter
```

## Test Commands

```bash
cargo test --lib               # Unit tests only
cargo test --test '*'          # Integration tests only (requires Neo4j)
cargo test                     # All tests
cargo test <test_name>         # Single test
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

# Ingest OpenAPI spec
cargo run -- ingest path/to/spec.json
cargo run -- ingest https://example.com/openapi.json

# Query endpoints
cargo run -- query "users"
cargo run -- query "/api/v1"

# Execute HTTP request
cargo run -- execute -m GET https://api.example.com/users
cargo run -- execute -m POST https://api.example.com/users -b '{"name":"test"}' -H "Content-Type: application/json"

# Show database statistics
cargo run -- stats

# Export healed graph to OpenAPI spec
cargo run -- export                           # Output YAML to stdout
cargo run -- export -o spec.yaml              # Output to file
cargo run -- export -f json -o spec.json      # Output as JSON
cargo run -- export --annotations=false       # Without x-healed-by-ai annotations
cargo run -- export --include-broken          # Include broken endpoints

# Generate diff report (original vs healed)
cargo run -- diff                             # Markdown report
cargo run -- diff -f changelog                # Git-style changelog
cargo run -- diff -f json                     # JSON format
cargo run -- diff --breaking-only             # Only breaking changes
```

## Environment Variables

Copy `.env.example` to `.env` and configure:

| Variable | Default | Description |
|----------|---------|-------------|
| `NEO4J_URI` | `bolt://localhost:7688` | Neo4j connection URI |
| `NEO4J_USER` | `neo4j` | Neo4j username |
| `NEO4J_PASSWORD` | *required* | Neo4j password |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama API endpoint |
| `OLLAMA_MODEL` | `granite4:latest` | LLM model to use for text generation |
| `OLLAMA_EMBED_MODEL` | - | Ollama model for embeddings (e.g. `bge-m3:latest`). Falls back to `OLLAMA_MODEL` if unset |
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

## Local Development

```bash
# Start Neo4j and Ollama
docker compose up -d

# Initialize database schema
cargo run -- init-db

# Run the application
cargo run
```

## Docker Deployment (HTTP Transport)

Deploy the MCP server with HTTP transport for integration with OpenWebUI or other HTTP-based MCP clients:

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

**OpenWebUI Integration:**
- MCP URL: `http://host.docker.internal:3000/mcp` (from Docker) or `http://localhost:3000/mcp` (from host)
- Authentication: Bearer token (if `MCP_API_KEY` is set)

## Project Structure

```
src/
├── lib.rs              # Library exports
├── main.rs             # CLI entry point
├── cli.rs              # Clap CLI definitions
├── config.rs           # Environment configuration
├── logging.rs          # Tracing setup
├── models/             # Data models (Resource, Endpoint, Schema, Parameter, HealingEvent, HttpMethod, ApiCredential, Task, AgentJob, ModelSpec)
├── repository/         # Neo4j database layer
│   ├── admin.rs        # Graph cleanup operations (stats, purge, reset)
│   ├── agent_job.rs    # AgentJob CRUD + chain unpark/cancel operations
│   ├── credential.rs   # Credential CRUD operations
│   ├── model_spec.rs   # ModelSpec CRUD (upsert by name, usage stats)
│   └── task.rs         # Task CRUD operations
├── services/           # Core business logic
│   ├── openapi.rs      # OpenAPI spec parser and ingester
│   ├── http.rs         # HTTP request executor with response classification
│   ├── llm.rs          # Multi-provider LLM client (Ollama/Anthropic/Gemini)
│   ├── healing.rs      # Self-healing orchestrator
│   ├── context.rs      # In-memory API context store with DB fallback
│   ├── discovery.rs    # OpenAPI spec auto-discovery with LLM assistance
│   ├── docgen.rs       # Documentation-to-OpenAPI generator with LLM
│   ├── repo.rs         # Repository-to-OpenAPI generator with LLM
│   ├── knowledge.rs    # Notes/RAG service with vector and keyword search
│   ├── model_selector.rs # Capability-filter + cheapest-first model selection
│   ├── procedure_executor.rs # Template-substitution procedure step runner
│   ├── queue.rs        # Priority job queue + coordinator (AgentJob execution)
│   ├── scheduler.rs    # Autonomous scheduler (self-improvement loop)
│   ├── sleep.rs        # Experience digestion and training data export
│   ├── export/         # Graph-to-Spec export module
│   │   ├── builder.rs  # OpenAPI spec builder
│   │   ├── exporter.rs # Graph traversal and spec reconstruction
│   │   ├── differ.rs   # Spec diff generator
│   │   └── report.rs   # Markdown/JSON report generator
│   └── secrets/        # Secret provider abstraction
│       ├── provider.rs # SecretProvider trait
│       ├── local.rs    # AES-256-GCM encrypted file storage
│       ├── vault.rs    # HashiCorp Vault KV v2 provider
│       ├── aws.rs      # AWS Secrets Manager provider
│       ├── manager.rs  # CredentialManager with URL matching
│       └── error.rs    # Secret error types
├── skills/             # Pluggable skill implementations
│   ├── mod.rs          # Skill trait definition
│   ├── admin.rs        # Graph Admin skill (5 tools)
│   ├── agent.rs        # Agent Job Queue skill (8 tools)
│   ├── api.rs          # API Expert skill (14 tools)
│   ├── dynamic.rs      # Dynamic Tool Builder skill (4 tools + runtime tools)
│   ├── knowledge.rs    # Knowledge Manager skill (10 tools)
│   ├── model.rs        # Model Registry skill (5 tools)
│   ├── procedure.rs    # Procedural Memory skill (2 tools)
│   ├── scheduler.rs    # Autonomous Scheduler skill (5 tools)
│   ├── search.rs       # Web Search skill (1 tool)
│   ├── sleep.rs        # Sleep / Telemetry skill (2 tools)
│   ├── task.rs         # Task Manager skill (6 tools)
│   └── working_memory.rs # Working Memory skill (3 tools)
└── mcp/                # MCP server implementation
    ├── protocol.rs     # JSON-RPC 2.0 message types
    ├── transport.rs    # Async stdio transport
    ├── transport_trait.rs  # McpTransport trait abstraction
    ├── http_transport.rs   # Axum-based HTTP+SSE transport
    ├── session.rs      # HTTP session management
    ├── auth.rs         # API key authentication
    ├── tools.rs        # Tool registry (skill-based dispatch)
    └── server.rs       # MCP server state machine (thread-safe)

tests/
├── common/mod.rs          # Test utilities
├── repository_test.rs     # Integration tests for Neo4j
├── context_tools_test.rs  # Context management tool tests
├── discovery_test.rs      # Discovery service tests
├── docgen_test.rs         # Doc-to-OpenAPI generation tests
├── repo_analyzer_test.rs  # Repo-to-OpenAPI generation tests
├── http_transport_test.rs # HTTP transport infrastructure tests
├── task_test.rs           # Task model and repository tests
└── fixtures/              # Test data (petstore.json)
```

## Architecture

### Graph Schema (Neo4j)

**Nodes:**
- `Resource` - High-level API groupings (e.g., "Users", "Payments")
- `Endpoint` - Specific API path + method with `path`, `method`, `summary`, `operationId`
- `Schema` - Data object definitions with `name` and `json_structure`
- `Parameter` - Endpoint inputs with `name`, `in` (query/path/body/header), `required`
- `HealingEvent` - Immutable records of AI-driven documentation fixes
- `ApiCredential` - Credential configuration for API authentication
- `Task` - High-level goals with `id`, `goal`, `context`, `status` (created/in_progress/completed/failed/blocked)
- `Note` - Stored text memories with optional vector `embedding`, `access_count`, `last_accessed_at`, `note_type` (`semantic`/`episodic`/`reflection`/`consolidated`/`outcome`/`inference`), `next_review_at`, `review_interval_days`, `source_context`, `event_at`
- `Procedure` - Named multi-step workflows with `id`, `name`, `description`, `steps` (JSON array), `created_at`
- `WorkingMemory` - Session-scoped scratchpad entries with `id`, `session_id`, `content`, `role`, `turn_index`, `created_at`
- `Entity` - Named entities extracted from notes with `id`, `name` (unique, lowercased), `entity_type`, `created_at`
- `DynamicTool` - Runtime-defined MCP tools with `id`, `name` (unique), `description`, `input_schema` (JSON), `created_at`
- `AgentJob` - Background job record with `id`, `tool_name`, `args_json`, `priority` (0-3), `status` (queued/running/completed/failed/dead/parked/cancelled), `attempt_count`, `max_attempts`, `result_json`, `error`, timestamps, `session_id`, `parent_job_id`

**Relationships:**
- `(:Resource)-[:HAS_ENDPOINT]->(:Endpoint)`
- `(:Endpoint)-[:REQUIRES_PARAM]->(:Parameter)`
- `(:Endpoint)-[:RETURNS_SCHEMA {status: 200}]->(:Schema)`
- `(:Endpoint)-[:ACCEPTS_SCHEMA]->(:Schema)`
- `(:Schema)-[:LINKS_TO]->(:Schema)`
- `(:Endpoint)-[:HAS_HISTORY]->(:HealingEvent)`
- `(:Note)-[:RELATES_TO {similarity: float}]->(:Note)` — auto-created when similarity ≥ 0.75
- `(:Note)-[:SUMMARIZED_BY]->(:Note)` — source notes pointing to their consolidated summary
- `(:Note)-[:REFLECTS_ON]->(:Task)` — reflection/outcome notes linked to the task they critique
- `(:Note)-[:PART_OF]->(:Note)` — semantic chunk linked to its parent note
- `(:Note)-[:MENTIONS {count}]->(:Entity)` — entity mentions extracted from note content
- `(:Note {note_type:'inference'})-[:DERIVED_FROM]->(:Note)` — inference notes citing their sources
- `(:Task)-[:SUBTASK_OF]->(:Task)` — sub-tasks created by `decompose_goal`
- `(:DynamicTool)-[:USES]->(:Procedure)` — links a dynamic tool to its step definition

### Transport Architecture

The MCP server supports two transport mechanisms:

**Stdio Transport (Default)**
- Standard input/output for local CLI usage
- Best for MCP clients like Claude Desktop that spawn the server as subprocess
- No authentication required (trusted local process)

**HTTP Transport**
- Streamable HTTP with Server-Sent Events (SSE) per MCP specification
- POST `/mcp` - JSON-RPC requests, returns JSON or SSE stream
- GET `/mcp` - SSE stream for server-initiated messages
- DELETE `/mcp` - Terminate session
- GET `/health` - Health check endpoint
- Headers: `Mcp-Protocol-Version`, `Mcp-Session-Id` for session management
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
               │                   ┌─────────▼─────────┐
               │                   │  SessionManager   │
               │                   │ (Mcp-Session-Id)  │
               │                   └─────────┬─────────┘
               │                             │
     ┌─────────▼─────────────────────────────▼─────────┐
     │            McpTransport Trait                   │
     └─────────────────────┬───────────────────────────┘
                           │
     ┌─────────────────────▼───────────────────────────┐
     │              McpServerCore                      │
     │    (Arc<RwLock<ServerState>> for thread-safe)  │
     └─────────────────────┬───────────────────────────┘
                           │
     ┌─────────────────────▼───────────────────────────┐
     │    Skill Registry (68 static + N runtime)               │
     │  ApiSkill(14)  SearchSkill(1)  TaskSkill(6)             │
     │  KnowledgeSkill(15)  ProcedureSkill(2)  AgentSkill(8)  │
     │  WorkingMemorySkill(3)  DynamicSkill(4+runtime)         │
     │  AdminSkill(5)  ModelSkill(5)  SleepSkill(2)            │
     │  SchedulerSkill(5)                                      │
     └─────────────────────────────────────────────────────────┘
```

### MCP Tools

The server exposes seventy tools via JSON-RPC 2.0, organised across eleven skills (plus runtime-defined tools from DynamicSkill):

**Core Tools:**

1. **`ingest_openapi`** - Parses OpenAPI specs (URL or file path) and loads into Neo4j
   - Input: `{ "source": "https://example.com/openapi.json" }`
   - Returns: Count of resources, endpoints, schemas, and parameters created
   - Auto-populates the in-memory context store for fast access

2. **`graph_query_endpoint`** - Search endpoints by path pattern or keywords
   - Input: `{ "query": "users" }` or `{ "query": "/api/v1" }`
   - Returns: Matching endpoints with parameters and schemas

3. **`execute_http_request`** - Execute HTTP requests with auto-credential injection
   - Input: `{ "method": "GET", "url": "https://api.example.com/users", "headers": {}, "body": {} }`
   - Returns: Status code, response body, duration, headers
   - Automatically injects credentials for matching API URLs

**Context Management Tools:**

4. **`get_api_context`** - Retrieve API summaries from in-memory context
   - Input: `{ "api_name": "Petstore", "format": "summary" }` (both optional)
   - Formats: `summary` (default JSON), `detailed` (includes schemas), `compact` (text)
   - Returns all loaded APIs if `api_name` omitted
   - Falls back to Neo4j on cache miss

5. **`list_loaded_apis`** - List all APIs currently in the context store
   - Input: `{}` (no parameters)
   - Returns: API names, versions, endpoint counts, load timestamps

6. **`clear_api_context`** - Remove APIs from in-memory context
   - Input: `{ "api_name": "Petstore" }` (optional - clears all if omitted)
   - Data remains in Neo4j and can be reloaded

**Discovery & Generation Tools:**

7. **`discover_openapi`** - Auto-discover OpenAPI specifications for an API
   - Input: `{ "base_url": "https://api.example.com", "use_llm": true, "auto_ingest": false }`
   - Probes common paths (`/openapi.json`, `/swagger.json`, `/api-docs`, etc.)
   - Parses HTML documentation pages for spec links
   - Uses LLM to intelligently suggest additional locations
   - Optional `auto_ingest` to automatically load discovered specs

8. **`build_openapi_from_docs`** - Generate OpenAPI specs from documentation pages
   - Input: `{ "doc_urls": ["https://docs.example.com/api"], "api_title": "My API", "api_version": "1.0.0", "base_url": "https://api.example.com", "output_format": "json", "auto_ingest": false }`
   - Fetches and parses HTML/markdown documentation
   - Uses LLM to extract API endpoints, parameters, and schemas
   - Generates valid OpenAPI 3.0 specification
   - Output formats: `json` (default) or `yaml`
   - Optional `auto_ingest` to load generated spec into the knowledge graph

9. **`build_openapi_from_repo`** - Generate OpenAPI specs from repository source code
   - Input: `{ "repo_url": "https://github.com/owner/repo", "api_title": "My API", "api_version": "1.0.0", "base_url": "https://api.example.com", "ref_name": "main", "subdirectory": "src/api", "merge_strategy": "enhance", "output_format": "json", "auto_ingest": false }`
   - Supports GitHub and GitLab repositories (public and private)
   - Auto-detects access method (API for small repos, clone for larger)
   - Uses LLM for framework-agnostic code analysis
   - Detects and merges with existing OpenAPI specs found in the repository
   - Merge strategies: `enhance` (merge), `replace` (code only), `ignore` (skip existing)
   - Configure credentials via `configure_api_credential` with `api_name: "GitHub"` or `"GitLab"`
   - Output formats: `json` (default) or `yaml`
   - Optional `auto_ingest` to load generated spec into the knowledge graph

**Export & Diff Tools:**

10. **`export_openapi`** - Export healed knowledge graph back to OpenAPI 3.0 spec
   - Input: `{ "format": "yaml", "include_annotations": true, "include_broken": false }`
   - Traverses Neo4j graph and reconstructs valid OpenAPI spec
   - Adds `x-healed-by-ai` annotations on AI-corrected fields
   - Includes `x-original-value` for healed fields when annotations enabled
   - Output formats: `yaml` (default) or `json`

11. **`diff_api_spec`** - Compare original spec vs current healed graph state
    - Input: `{ "api_name": "Petstore", "format": "markdown", "breaking_only": false }`
    - Analyzes HealingEvents to generate change report
    - Categorizes changes: Parameter, Endpoint, Schema, Response
    - Identifies breaking vs non-breaking changes
    - Output formats: `markdown` (default), `changelog`, `json`

**Credential Management Tools:**

12. **`configure_api_credential`** - Store API credentials for automatic injection
    - Input: `{ "api_name": "OpenWeatherMap", "credential_type": "api_key", "inject_location": "query", "inject_key": "appid", "secret_value": "your-api-key" }`
    - Credential types: `api_key`, `bearer`, `basic`, `oauth2_client_credentials`
    - Inject locations: `header`, `query`
    - Secrets stored securely via configured provider (local/vault/aws)

13. **`list_api_credentials`** - List all configured API credentials
    - Input: `{}` (no parameters)
    - Returns: Credential metadata (secrets are masked)

14. **`delete_api_credential`** - Remove an API credential
    - Input: `{ "api_name": "OpenWeatherMap" }`
    - Deletes both the credential metadata and the stored secret

**Web Search Tools (SearchSkill):**

15. **`search_web`** - Search the web for information
    - Input: `{ "query": "rust async patterns", "engine": "serpapi", "count": 5 }`
    - Engines: `serpapi` (default), `brave`, `google`
    - Count: 1–20 results (default 5)
    - Requires corresponding API key env var (`SERPAPI_KEY`, `BRAVE_API_KEY`, or `GOOGLE_API_KEY`+`GOOGLE_CX`)

**Task Management Tools (TaskSkill):**

16. **`create_task`** - Create and persist a high-level goal
    - Input: `{ "goal": "Integrate Stripe API", "context": "Use the v3 endpoints" }`
    - Stores task in Neo4j with UUID, timestamps, and initial status `created`
    - Returns: `{ "task_id": "...", "status": "created" }`

17. **`reflect_on_work`** - Critique current progress against a goal using LLM
    - Input: `{ "goal": "...", "current_state": "...", "plan": "...", "task_id": "..." }` (`plan`, `task_id` optional)
    - Uses Ollama to analyse the gap between goal and current state
    - When `task_id` is provided, persists a `reflection` Note with a `REFLECTS_ON` edge to the task
    - Returns a critique, suggested next steps, and optional `reflection_note_id`
    - Requires Ollama to be configured

**Knowledge Tools (KnowledgeSkill):**

18. **`store_note`** - Persist a text note in the knowledge graph
    - Input: `{ "content": "...", "note_type": "semantic", "source_context": "...", "event_at": "..." }` (all except content optional)
    - Note types: `semantic` (default), `episodic`, `reflection`, `consolidated`
    - Long notes (>1500 chars) are automatically chunked into sub-notes with `PART_OF` edges
    - Generates vector embedding via Ollama if available (falls back to text-only)
    - Tracks `access_count`, `last_accessed_at`, `next_review_at`, `review_interval_days`
    - Auto-creates `RELATES_TO` edges to similar notes (cosine similarity ≥ 0.75)
    - Extracts named entities when LLM is available (creates `Entity` nodes + `MENTIONS` edges)
    - Returns: `{ "note_id": "...", "links_created": N, "success": true }`

19. **`search_notes`** - Retrieve notes via hybrid BM25 + vector search with graph expansion
    - Input: `{ "query": "...", "limit": 5, "graph_hops": 2, "entity_expansion": false }` (all except query optional)
    - Merges vector and full-text BM25 results via Reciprocal Rank Fusion (RRF) with freshness boost
    - Expands results via up to `graph_hops` RELATES_TO traversals
    - `entity_expansion: true` also bridges through Entity nodes (MENTIONS→Entity←MENTIONS) for related notes
    - Falls back to case-insensitive keyword `CONTAINS` query if indexes unavailable
    - Updates `access_count`, `last_accessed_at`, and doubles `review_interval_days` on hits
    - Returns: `{ "count": N, "notes": [...] }`

20. **`find_related_notes`** - Find notes linked via RELATES_TO graph edges
    - Input: `{ "note_id": "..." }`
    - Returns notes connected by similarity edges, ordered by score
    - Returns: `{ "count": N, "related_notes": [{ "content", "similarity" }] }`

21. **`prune_old_notes`** - Delete stale notes using adaptive decay or simple time-based pruning
    - Input: `{ "score_threshold": 0.1, "lambda": 0.1, "dry_run": false, "days_stale": 30, "min_accesses": 2 }` (all optional)
    - Adaptive decay: computes `score = access_rate * exp(-lambda * days_idle)` weighted by graph in-degree
    - Legacy mode: deletes by `days_stale` + `min_accesses` (used when score_threshold/lambda absent)
    - Protected types (`consolidated`, `reflection`) are never deleted
    - `dry_run: true` returns count without deleting
    - Returns: `{ "deleted": N, "message": "..." }` or `{ "would_delete": N, "dry_run": true, ... }`

22. **`consolidate_memories`** - LLM-powered memory consolidation
    - Input: `{ "topic": "...", "limit": 10 }` (`limit` optional, default 10)
    - Vector-searches top-N notes by topic, feeds them to LLM for synthesis
    - Stores the consolidated summary as a new Note with `note_type: "consolidated"`
    - Creates `SUMMARIZED_BY` edges from source notes to the consolidated note
    - Returns: `{ "consolidated_note_id": "...", "source_count": N, "preview": "..." }`
    - Requires LLM to be configured

23. **`review_due_notes`** - Fetch notes whose spaced-repetition review interval has elapsed
    - Input: `{ "limit": 10 }` (optional, default 10)
    - Returns notes where `next_review_at <= now()` (excludes consolidated notes)
    - Searching a note via `search_notes` doubles its `review_interval_days`
    - Returns: `{ "count": N, "notes": [{ "id", "content", "note_type", "next_review_at", "access_count" }] }`

24. **`search_by_entity`** - Find notes that mention a named entity
    - Input: `{ "entity_name": "neo4j", "entity_type": "technology", "limit": 5 }` (`entity_type`, `limit` optional)
    - Entities are extracted automatically from notes when LLM is available
    - Case-insensitive partial match on entity name
    - Returns: `{ "entity_name": "...", "count": N, "notes": [{ "note_id", "content", "entity", "entity_type", "mention_count" }] }`

25. **`reason`** - Retrieve relevant notes and derive new inferences via LLM
    - Input: `{ "question": "...", "limit": 8, "store_inference": true }` (`limit`, `store_inference` optional)
    - Vector + BM25 search with 1-hop graph expansion, then LLM inference
    - Stores inference as a Note with `DERIVED_FROM` edges to source notes
    - Returns: `{ "answer", "inferences", "confidence", "gaps", "inference_note_id"? }`

26. **`audit_action`** - Check a proposed action against stored values and principles
    - Input: `{ "action": "...", "context": "..." }` (`context` optional)
    - Retrieves ethical guidelines from knowledge graph, evaluates alignment via LLM
    - Returns: `{ "aligned": bool, "confidence", "concerns", "suggestions", "reasoning" }`

27. **`explain_reasoning`** - Narrate why a decision was taken, citing knowledge sources
    - Input: `{ "decision": "...", "task_id": "...", "limit": 10 }` (`task_id`, `limit` optional)
    - Fetches relevant notes + task reflection notes, generates plain-language explanation
    - Returns: `{ "explanation", "knowledge_sources": [{ "note_id", "preview" }] }`

28. **`ask_clarification`** - Analyze a request for ambiguity before acting
    - Input: `{ "request": "...", "context": "...", "available_tools": [...] }` (`context`, `available_tools` optional)
    - Uses LLM to identify underspecified or multi-interpretation requests
    - Returns: `{ "needs_clarification": bool, "ambiguities": [...], "clarifying_questions": [...], "assumptions": [...], "recommended_approach": "..." }`

29. **`get_note`** - Fetch a single note by its UUID
    - Input: `{ "id": "..." }`
    - Updates `access_count` and `last_accessed_at` on retrieval
    - Returns: `{ "id", "content", "note_type", "created_at", "access_count", "review_interval_days" }` or `{ "error": "not found" }`

30. **`delete_note`** - Permanently delete a note and all its relationships
    - Input: `{ "id": "..." }`
    - DETACH DELETE removes the node and all edges; use to clean up bad, duplicate, or unwanted notes
    - Returns: `{ "deleted": true, "id": "..." }` or error if not found

31. **`update_note`** - Update note content in-place, preserving all graph edges and metadata
    - Input: `{ "id": "...", "content": "..." }`
    - Overwrites only the `content` field; access_count, note_type, embeddings, edges all preserved
    - Returns: `{ "updated": true, "id": "..." }` or error if not found

31. **`export_graph_visualization`** - Export the full knowledge graph as a JSON graph for visualization
    - Input: `{ "max_nodes": 200 }` (`max_nodes` optional, default 200)
    - Returns Note, Entity, and Task nodes plus all relationship edges (RELATES_TO, MENTIONS, PART_OF, SUMMARIZED_BY, REFLECTS_ON, SUBTASK_OF, DERIVED_FROM)
    - Returns: `{ "nodes": [{ "id", "label", "type" }], "edges": [{ "source", "target", "type" }] }`

**Task Management Tools (TaskSkill):**

28. **`create_task`** - Create a new high-level task or goal
    - Input: `{ "goal": "...", "context": "..." }` (`context` optional)
    - Returns: `{ "task_id": "...", "status": "created" }`

29. **`reflect_on_work`** - Critique current progress against a goal using LLM
    - Input: `{ "goal": "...", "current_state": "...", "plan": "...", "task_id": "..." }` (`plan`, `task_id` optional)
    - When `task_id` provided, persists a reflection Note with `REFLECTS_ON` edge
    - Returns: `{ "critique", "status", "reflection_note_id"? }`

30. **`decompose_goal`** - Break a task into ordered sub-tasks using LLM
    - Input: `{ "goal_task_id": "...", "context": "...", "max_steps": 5 }` (`context`, `max_steps` optional)
    - Creates subtask nodes in Neo4j with `SUBTASK_OF` edges to the parent
    - Returns: `{ "parent_task_id", "subtasks": [{ "id", "title", "purpose", "tool_hint" }] }`

31. **`update_task`** - Update a task's status and optionally attach a progress note
    - Input: `{ "task_id": "...", "status": "completed", "note": "..." }` (`note` optional)
    - Status values: `in_progress`, `completed`, `failed`, `blocked`
    - Returns: `{ "task_id", "status", "note_id"? }`

32. **`list_tasks`** - List tasks with optional status filter
    - Input: `{ "status": "...", "limit": 20 }` (both optional)
    - Returns parent_id for sub-tasks created via `decompose_goal`
    - Returns: `{ "count", "tasks": [{ "id", "goal", "status", "parent_id"?, "created_at" }] }`

33. **`record_outcome`** - Store an episodic outcome note for a tool call or task attempt
    - Input: `{ "tool_name": "...", "summary": "...", "success": bool, "task_id": "..." }` (`task_id` optional)
    - Stores as `note_type: 'outcome'`, retrievable via `search_notes`
    - Returns: `{ "outcome_id", "tool_name", "success" }`

**Procedural Memory Tools (ProcedureSkill):**

34. **`store_procedure`** - Store a named multi-step workflow
    - Input: `{ "name": "...", "description": "...", "steps": [{ "tool", "args"?, "purpose" }] }`
    - Persists a `Procedure` node in Neo4j with steps as JSON
    - Returns: `{ "procedure_id": "...", "name": "...", "steps_count": N }`

35. **`search_procedures`** - Search stored procedures by keyword
    - Input: `{ "query": "...", "limit": 5 }` (`limit` optional, default 5)
    - Case-insensitive CONTAINS search on name and description
    - Returns: `{ "count": N, "procedures": [{ "id", "name", "description", "steps" }] }`

**Working Memory Tools (WorkingMemorySkill):**

36. **`push_context`** - Append an entry to the session working-memory scratchpad
    - Input: `{ "session_id": "...", "content": "...", "role": "observation" }` (`role` optional)
    - Roles: `observation` (default), `plan`, `result`, `error`
    - Entries are auto-numbered by `turn_index` within each session
    - Returns: `{ "entry_id": "...", "turn_index": N, "session_id": "..." }`

37. **`get_context`** - Retrieve working-memory entries for a session
    - Input: `{ "session_id": "...", "limit": 20 }` (`limit` optional, default 20)
    - Returns entries in turn order
    - Returns: `{ "session_id": "...", "count": N, "entries": [{ "turn", "role", "content" }] }`

38. **`summarise_session`** - LLM-summarise a session and persist to long-term memory
    - Input: `{ "session_id": "...", "delete_after_summarise": false }` (`delete_after_summarise` optional)
    - Fetches all session entries, feeds to LLM, stores consolidated Note
    - Optionally deletes raw WorkingMemory entries after summarising
    - Returns: `{ "note_id": "...", "session_id": "...", "entries_summarised": N, "deleted": bool }`
    - Requires LLM to be configured

**Dynamic Tool Builder (DynamicSkill):**

39. **`define_tool`** - Define a new MCP tool at runtime backed by a procedure pipeline
    - Input: `{ "name": "...", "description": "...", "input_schema": {...}, "steps": [...], "test_input"?: {...} }`
    - Steps support `{{input.field}}` and `{{context.var}}` template substitution; `output_var` and `condition` fields
    - Persists `DynamicTool` and `Procedure` nodes in Neo4j; available immediately in `tools/list`
    - Survives restarts (loaded via `load_from_neo4j` at startup)
    - Returns: `{ "tool_id", "name", "steps_count", "registered": true }`

40. **`execute_procedure`** - Execute a stored procedure by ID with optional input
    - Input: `{ "procedure_id": "...", "input"?: {...}, "dry_run"?: bool }`
    - `dry_run: true` validates steps and substitutions without calling tools
    - Returns: `{ "procedure_id", "steps_executed", "results": [{step_index, tool, success, output_preview}], "total_success" }`

41. **`list_dynamic_tools`** - List all runtime-defined tools
    - Input: `{}` (no parameters)
    - Returns: `{ "count", "tools": [{ "id", "name", "description", "created_at" }] }`

42. **`remove_dynamic_tool`** - Remove a runtime-defined tool by name
    - Input: `{ "name": "..." }`
    - Deletes `DynamicTool` + linked `Procedure` nodes from Neo4j and unregisters immediately
    - Returns: `{ "removed": true, "name" }`

**Agent Job Queue (AgentSkill):**

43. **`enqueue_agent`** - Submit an MCP tool call as a background job
    - Input: `{ "tool_name": "...", "arguments"?: {}, "priority"?: 0-3, "max_attempts"?: N, "session_id"?: "...", "parent_job_id"?: "..." }`
    - Priority: 0=low, 1=normal (default), 2=high, 3=critical
    - Jobs are persisted to Neo4j and survive server restarts
    - Returns: `{ "job_id": "...", "status": "queued", "tool_name": "...", "priority": N }`

44. **`queue_status`** - Get current queue statistics
    - Input: `{}` (no parameters)
    - Returns: `{ "in_memory_pending", "running_now", "max_concurrent", "enabled", "by_status": {...} }`

45. **`get_job_result`** - Get the status and result of a specific job
    - Input: `{ "job_id": "..." }`
    - Returns: Full `AgentJob` JSON with status, result, error, attempt_count, timestamps

46. **`cancel_job`** - Cancel a queued or running job
    - Input: `{ "job_id": "..." }`
    - Returns: `{ "cancelled": true, "job_id": "..." }`

47. **`retry_job`** - Requeue a failed, dead, or cancelled job
    - Input: `{ "job_id": "..." }`
    - Resets attempt_count to 0 and re-enqueues
    - Returns: `{ "requeued": true, "job_id": "...", "status": "queued" }`

48. **`set_worker_config`** - Update queue worker settings at runtime
    - Input: `{ "max_concurrent"?: N, "enabled"?: bool, "poll_interval_secs"?: N }`
    - `enabled: false` pauses job processing without losing queued jobs
    - Returns: updated config object

49. **`drain_queue`** - Cancel all currently pending (queued) jobs
    - Input: `{}` (no parameters)
    - Returns: `{ "cancelled": N, "message": "..." }`

50. **`enqueue_chain`** - Submit a sequential chain of background jobs
    - Input: `{ "steps": [{ "tool_name": "...", "arguments"?: {}, "priority"?: 0-3, "max_attempts"?: N, "provider_hint"?: "..." }], "session_id"?: "..." }`
    - Step 1 queued immediately; steps 2..N stored as `parked` (each with `parent_job_id` → predecessor)
    - On step completion: parked children auto-promoted to `queued`
    - On step death (exhausted retries): parked children cancelled
    - Returns: `{ "chain_length": N, "job_ids": [...], "message": "..." }`

**Graph Admin Tools (AdminSkill):**

51. **`delete_api`** - Cascade-delete all graph nodes for a specific ingested API
    - Input: `{ "api_name": "Petstore", "dry_run"?: false }`
    - Deletes Resource → Endpoints → Parameters → HealingEvents + exclusively-owned Schemas
    - Also evicts the API from the in-memory context cache
    - Returns: `{ "deleted": true, "api_name": "...", "removed": { ... } }`

52. **`purge_duplicate_endpoints`** - Remove duplicate Endpoint nodes
    - Input: `{ "dry_run"?: false }`
    - Finds endpoints with same Resource + path + method; keeps oldest, removes extras
    - Returns: `{ "deleted": N, "message": "..." }`

53. **`purge_orphaned_schemas`** - Delete Schema nodes with no Endpoint relationships
    - Input: `{ "dry_run"?: false }`
    - Removes schemas not referenced by any RETURNS_SCHEMA, ACCEPTS_SCHEMA, or LINKS_TO relation
    - Returns: `{ "deleted": N, "message": "..." }`

54. **`reset_graph`** - Wipe all API data from the graph
    - Input: `{ "confirm": true, "dry_run"?: false }`
    - Deletes Resource, Endpoint, Schema, Parameter, HealingEvent nodes
    - Knowledge data (Notes, Tasks, Procedures, WorkingMemory, AgentJobs) is preserved
    - Requires `confirm: true` — cannot be undone
    - Returns: `{ "reset": true, "removed": { ... }, "message": "..." }`

55. **`backfill_endpoint_embeddings`** - Generate embeddings for Endpoint nodes that are missing them
    - Input: `{ "dry_run"?: false }`
    - Required to unlock semantic search in `graph_query_endpoint` for endpoints ingested without an LLM
    - `dry_run: true` counts endpoints needing embeddings without generating any
    - Returns: `{ "total_endpoints": N, "already_had_embeddings": N, "updated": N, "failed": N }`

**Model Registry Tools (ModelSkill):**

56. **`list_models`** - List available LLM providers and all registered model specs
    - Input: `{}` (no parameters)
    - Returns: active provider config + list of `ModelSpec` records from Neo4j

57. **`use_model`** - Switch the active LLM provider and model at runtime
    - Input: `{ "provider": "Ollama"|"Anthropic"|"Gemini", "model"?: "...", "api_key"?: "..." }`
    - Updates the shared `Arc<RwLock<Option<LlmConfig>>>` used by all skills
    - Returns: updated provider config

58. **`register_model`** - Register a model spec in the knowledge graph
    - Input: `{ "name": "...", "provider": "...", "cost_per_1k_input"?: N, "cost_per_1k_output"?: N, "context_window"?: N, "capabilities"?: [...] }`
    - Upserts a `ModelSpec` node in Neo4j
    - Returns: `{ "model_id": "...", "name": "...", "registered": true }`

59. **`select_model`** - Auto-select the cheapest capable model for given requirements
    - Input: `{ "required_capabilities": [...], "max_cost_per_1k"?: N }`
    - Filters registered models by capabilities, sorts cheapest-first
    - Returns: `{ "selected": { name, provider, cost, capabilities } }`

60. **`get_model_stats`** - Get usage statistics for a model from AgentJob history
    - Input: `{ "model_name": "..." }`
    - Returns: `{ "total_jobs": N, "success_rate": 0.0-1.0, "avg_duration_ms": N }`

**Sleep / Telemetry Tools (SleepSkill):**

61. **`digest_experiences`** - Export successful interactions to JSONL training datasets
    - Input: `{ "min_score"?: N }` (min_score optional, filters by feedback score 1-5)
    - Reads from DuckDB `interactions` table; writes JSONL to `DATASET_DIR`
    - Returns: `{ "exported": N, "file": "..." }`

62. **`analyze_gaps`** - Identify knowledge gaps and missing capabilities from telemetry
    - Input: `{ "limit"?: N }` (default 20)
    - Reads from DuckDB `knowledge_gaps` table
    - Returns: `{ "count": N, "gaps": [{ "topic", "frequency", "last_seen" }] }`

**Scheduler Tools (SchedulerSkill):**

63. **`start_scheduler`** - Enable the autonomous scheduler loop
    - Input: `{ "interval_secs"?: N, "session_id"?: "..." }` (both optional)
    - Sets `enabled = true`; optionally updates poll interval and session ID
    - Returns: `{ "started": true, "interval_secs": N, "session_id": "..." }`

64. **`stop_scheduler`** - Pause the scheduler loop
    - Input: `{}` (no parameters)
    - Sets `enabled = false`; in-flight jobs continue running
    - Returns: `{ "stopped": true, "message": "..." }`

65. **`get_scheduler_status`** - Return current scheduler config and runtime state
    - Input: `{}` (no parameters)
    - Returns: `{ "config": { interval_secs, enabled, max_tasks_per_run, error_budget, session_id }, "state": { tasks_dispatched, consecutive_errors, last_run_at, last_error, is_running } }`

66. **`configure_scheduler`** - Update scheduler settings at runtime
    - Input: `{ "interval_secs"?: N, "enabled"?: bool, "max_tasks_per_run"?: N, "error_budget"?: N, "session_id"?: "..." }` (all optional)
    - Supports setting `session_id` to `null` to clear it
    - Returns: `{ "updated": true, "config": { ... } }`

67. **`run_scheduler_tick`** - Execute a scheduler tick immediately (bypasses timer)
    - Input: `{}` (no parameters)
    - Lists `created` tasks, builds job chains, enqueues them
    - Returns: `{ "success": true, "tasks_found": N, "tasks_dispatched": K, "skipped": M }`

### Self-Healing Flow

When `execute_http_request` encounters errors (4xx/5xx):
1. Pass request, error body, and graph schema to LLM for analysis
2. LLM suggests corrections based on error message
3. Retry with corrected payload
4. On success: update Neo4j with `HealingEvent` node and corrected schema
5. On failure: mark endpoint as `status='broken'`

## TODO / Planned Features

- [x] Add capability to clean up/purge the Neo4j graph — implemented as `AdminSkill` (delete_api, purge_duplicate_endpoints, purge_orphaned_schemas, reset_graph)
- [x] Semantic search for `graph_query_endpoint` via vector embeddings — use `backfill_endpoint_embeddings` to populate missing embeddings
- [x] SSE push notifications for completed agent jobs — coordinator sends `notifications/agent_job` on all terminal states (completed/failed/dead)

## Branch Strategy
Never write in credidation to LLMs or coding agents or assistants.

- `feature/*` - Feature branches (no CI)
- `dev` - Development (format + unit tests)
- `test` - Testing (full pipeline with integration tests)
- `prod` - Production (full pipeline + Docker build)
- Update the documentation first, the README, claude, plan, markdowns should reflect our changes.