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

# Query endpoints
cargo run -- query "users"

# Show database statistics
cargo run -- stats

# Ingest OpenAPI spec (not yet implemented)
cargo run -- ingest path/to/spec.json

# Execute HTTP request (not yet implemented)
cargo run -- execute -m GET http://api.example.com/users
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
├── models/             # Data models (Resource, Endpoint, Schema, Parameter, HealingEvent)
└── repository/         # Neo4j database layer

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

### MCP Tools (planned)

1. **`ingest_openapi`** - Parses OpenAPI specs (URL or file) and loads into Neo4j
2. **`graph_query_endpoint`** - Natural language search over endpoints
3. **`execute_http_request`** - Live HTTP requests with self-healing retry logic

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
