# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Autonomous API Knowledge Graph - An MCP server in Rust that ingests OpenAPI/Swagger specifications into a Neo4j graph database, enabling natural language queries and live API testing with "self-healing" documentation capabilities.

## Tech Stack

- **Language:** Rust (Tokio async runtime, Edition 2024)
- **Protocol:** Model Context Protocol (MCP) via stdio transport
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
# Run as MCP server (default)
cargo run -- serve
cargo run                      # Same as above

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
```

## Environment Variables

Copy `.env.example` to `.env` and configure:

| Variable | Default | Description |
|----------|---------|-------------|
| `NEO4J_URI` | `bolt://localhost:7687` | Neo4j connection URI |
| `NEO4J_USER` | `neo4j` | Neo4j username |
| `NEO4J_PASSWORD` | *required* | Neo4j password |
| `OLLAMA_URL` | `http://localhost:11434` | Ollama API endpoint |
| `OLLAMA_MODEL` | `llama3` | LLM model to use |
| `LOG_LEVEL` | `info` | Log level (trace/debug/info/warn/error) |
| `LOG_FORMAT` | `pretty` | Log format (pretty/json) |

## Local Development

```bash
# Start Neo4j and Ollama
docker compose up -d

# Initialize database schema
cargo run -- init-db

# Run the application
cargo run
```

## Project Structure

```
src/
├── lib.rs              # Library exports
├── main.rs             # CLI entry point
├── cli.rs              # Clap CLI definitions
├── config.rs           # Environment configuration
├── logging.rs          # Tracing setup
├── models/             # Data models (Resource, Endpoint, Schema, Parameter, HealingEvent, HttpMethod)
├── repository/         # Neo4j database layer
├── services/           # Core business logic
│   ├── openapi.rs      # OpenAPI spec parser and ingester
│   ├── http.rs         # HTTP request executor with response classification
│   ├── llm.rs          # Ollama LLM client for error analysis
│   ├── healing.rs      # Self-healing orchestrator
│   └── context.rs      # In-memory API context store with DB fallback
└── mcp/                # MCP server implementation
    ├── protocol.rs     # JSON-RPC 2.0 message types
    ├── transport.rs    # Async stdio transport
    ├── tools.rs        # Tool definitions and handlers
    └── server.rs       # MCP server state machine

tests/
├── common/mod.rs       # Test utilities
├── repository_test.rs  # Integration tests for Neo4j
└── fixtures/           # Test data (petstore.json)
```

## Architecture

### Graph Schema (Neo4j)

**Nodes:**
- `Resource` - High-level API groupings (e.g., "Users", "Payments")
- `Endpoint` - Specific API path + method with `path`, `method`, `summary`, `operationId`
- `Schema` - Data object definitions with `name` and `json_structure`
- `Parameter` - Endpoint inputs with `name`, `in` (query/path/body/header), `required`
- `HealingEvent` - Immutable records of AI-driven documentation fixes

**Relationships:**
- `(:Resource)-[:HAS_ENDPOINT]->(:Endpoint)`
- `(:Endpoint)-[:REQUIRES_PARAM]->(:Parameter)`
- `(:Endpoint)-[:RETURNS_SCHEMA {status: 200}]->(:Schema)`
- `(:Endpoint)-[:ACCEPTS_SCHEMA]->(:Schema)`
- `(:Schema)-[:LINKS_TO]->(:Schema)`
- `(:Endpoint)-[:HAS_HISTORY]->(:HealingEvent)`

### MCP Tools

The server exposes six tools via JSON-RPC 2.0:

**Core Tools:**

1. **`ingest_openapi`** - Parses OpenAPI specs (URL or file path) and loads into Neo4j
   - Input: `{ "source": "https://example.com/openapi.json" }`
   - Returns: Count of resources, endpoints, schemas, and parameters created
   - Auto-populates the in-memory context store for fast access

2. **`graph_query_endpoint`** - Search endpoints by path pattern or keywords
   - Input: `{ "query": "users" }` or `{ "query": "/api/v1" }`
   - Returns: Matching endpoints with parameters and schemas

3. **`execute_http_request`** - Execute HTTP requests with optional self-healing
   - Input: `{ "method": "GET", "url": "https://api.example.com/users", "headers": {}, "body": {} }`
   - Returns: Status code, response body, duration, headers
   - Supports automatic error analysis and retry with LLM assistance

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

### Self-Healing Flow

When `execute_http_request` encounters errors (4xx/5xx):
1. Pass request, error body, and graph schema to LLM for analysis
2. LLM suggests corrections based on error message
3. Retry with corrected payload
4. On success: update Neo4j with `HealingEvent` node and corrected schema
5. On failure: mark endpoint as `status='broken'`

## Branch Strategy

- `feature/*` - Feature branches (no CI)
- `dev` - Development (format + unit tests)
- `test` - Testing (full pipeline with integration tests)
- `prod` - Production (full pipeline + Docker build)
