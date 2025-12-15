# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Autonomous API Knowledge Graph - An MCP server in Rust that ingests OpenAPI/Swagger specifications into a Neo4j graph database, enabling natural language queries and live API testing with "self-healing" documentation capabilities.

## Tech Stack

- **Language:** Rust (Tokio async runtime, Edition 2024)
- **Protocol:** Model Context Protocol (MCP) via stdio or HTTP transport
- **Web Framework:** Axum (for HTTP transport with SSE streaming)
- **Database:** Neo4j via `neo4rs` driver
- **AI Model:** Local LLM (Ollama - Llama 3 or Mistral) via REST

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
| `OLLAMA_MODEL` | `llama3` | LLM model to use |
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
docker compose logs -f agent-api

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
├── models/             # Data models (Resource, Endpoint, Schema, Parameter, HealingEvent, HttpMethod, ApiCredential)
├── repository/         # Neo4j database layer
│   └── credential.rs   # Credential CRUD operations
├── services/           # Core business logic
│   ├── openapi.rs      # OpenAPI spec parser and ingester
│   ├── http.rs         # HTTP request executor with response classification
│   ├── llm.rs          # Ollama LLM client for error analysis
│   ├── healing.rs      # Self-healing orchestrator
│   ├── context.rs      # In-memory API context store with DB fallback
│   ├── discovery.rs    # OpenAPI spec auto-discovery with LLM assistance
│   ├── docgen.rs       # Documentation-to-OpenAPI generator with LLM
│   ├── repo.rs         # Repository-to-OpenAPI generator with LLM
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
└── mcp/                # MCP server implementation
    ├── protocol.rs     # JSON-RPC 2.0 message types
    ├── transport.rs    # Async stdio transport
    ├── transport_trait.rs  # McpTransport trait abstraction
    ├── http_transport.rs   # Axum-based HTTP+SSE transport
    ├── session.rs      # HTTP session management
    ├── auth.rs         # API key authentication
    ├── tools.rs        # Tool definitions and handlers (14 tools)
    └── server.rs       # MCP server state machine (thread-safe)

tests/
├── common/mod.rs          # Test utilities
├── repository_test.rs     # Integration tests for Neo4j
├── context_tools_test.rs  # Context management tool tests
├── discovery_test.rs      # Discovery service tests
├── docgen_test.rs         # Doc-to-OpenAPI generation tests
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

**Relationships:**
- `(:Resource)-[:HAS_ENDPOINT]->(:Endpoint)`
- `(:Endpoint)-[:REQUIRES_PARAM]->(:Parameter)`
- `(:Endpoint)-[:RETURNS_SCHEMA {status: 200}]->(:Schema)`
- `(:Endpoint)-[:ACCEPTS_SCHEMA]->(:Schema)`
- `(:Schema)-[:LINKS_TO]->(:Schema)`
- `(:Endpoint)-[:HAS_HISTORY]->(:HealingEvent)`

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
     │              ToolHandler (14 tools)             │
     └─────────────────────────────────────────────────┘
```

### MCP Tools

The server exposes fourteen tools via JSON-RPC 2.0:

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

### Self-Healing Flow

When `execute_http_request` encounters errors (4xx/5xx):
1. Pass request, error body, and graph schema to LLM for analysis
2. LLM suggests corrections based on error message
3. Retry with corrected payload
4. On success: update Neo4j with `HealingEvent` node and corrected schema
5. On failure: mark endpoint as `status='broken'`

## TODO / Planned Features

- [ ] Add capability to clean up/purge the Neo4j graph (remove duplicate endpoints, reset database, or delete specific APIs)

## Branch Strategy
Never write in credidation to LLMs or coding agents or assistants.

- `feature/*` - Feature branches (no CI)
- `dev` - Development (format + unit tests)
- `test` - Testing (full pipeline with integration tests)
- `prod` - Production (full pipeline + Docker build)
- Update the documentation first, the README, claude, plan, markdowns should reflect our changes.