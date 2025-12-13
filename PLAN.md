# Implementation Plan: Autonomous API Knowledge Graph

## Current State Summary

**Completed (~35%):**
- ✅ Data models (Resource, Endpoint, Schema, Parameter, HealingEvent)
- ✅ Neo4j repository layer with full CRUD operations
- ✅ Relationship management for all graph edges
- ✅ Error handling infrastructure

**Missing (~65%):**
- ❌ MCP server implementation
- ❌ OpenAPI parser and ingestion
- ❌ HTTP request executor
- ❌ LLM integration (Ollama)
- ❌ Self-healing orchestration
- ❌ Tests
- ❌ Docker setup
- ❌ CI/CD pipeline

---

## Phase 1: Project Infrastructure

### 1.1 Configuration Management
- [ ] Create `src/config.rs` with environment-based configuration
- [ ] Support: Neo4j URI/credentials, Ollama endpoint, log level
- [ ] Add `dotenvy` crate for `.env` file support

### 1.2 Logging & Observability
- [ ] Add `tracing` + `tracing-subscriber` crates
- [ ] Structured JSON logging for production
- [ ] Request/response tracing for debugging

### 1.3 Application Bootstrap
- [ ] Implement async `main()` with Tokio runtime
- [ ] Initialize Neo4j connection and schema on startup
- [ ] Graceful shutdown handling

---

## Phase 2: Core Services

### 2.1 OpenAPI Parser (`src/services/openapi.rs`)
- [ ] Add `openapiv3` crate for spec parsing
- [ ] Parse from URL (fetch) or file path
- [ ] Map OpenAPI paths → Endpoint nodes
- [ ] Map OpenAPI schemas → Schema nodes
- [ ] Map OpenAPI parameters → Parameter nodes
- [ ] Extract tags → Resource groupings
- [ ] Bulk insert with transaction support

### 2.2 HTTP Executor (`src/services/http.rs`)
- [ ] Wrap `reqwest` client with timeout/retry config
- [ ] Build requests from Endpoint + Parameter data
- [ ] Capture response status, body, timing
- [ ] Classify responses (success/client-error/server-error)

### 2.3 LLM Client (`src/services/llm.rs`)
- [ ] Ollama REST API client
- [ ] Prompt templates for error analysis
- [ ] Structured output parsing for healing suggestions
- [ ] Configurable model selection (llama3, mistral)

### 2.4 Healing Orchestrator (`src/services/healing.rs`)
- [ ] Implement retry loop from architecture doc
- [ ] On 4xx/5xx: call LLM for analysis
- [ ] Apply suggested fix, retry request
- [ ] On success: create HealingEvent, update graph
- [ ] On continued failure: mark endpoint broken

---

## Phase 3: MCP Server

### 3.1 MCP Protocol Layer (`src/mcp/`)
- [ ] Initialize MCP server with stdio transport
- [ ] Register tool schemas using `schemars`

### 3.2 Tool Implementations
- [ ] `ingest_openapi` - Parse spec, load graph, return counts
- [ ] `graph_query_endpoint` - Search endpoints by natural language
- [ ] `execute_http_request` - Run request with healing loop

### 3.3 Query Enhancement
- [ ] Fuzzy matching on path, summary, operation_id
- [ ] Full-text search index in Neo4j
- [ ] (Optional) Vector embeddings for semantic search

---

## Phase 4: Testing Strategy

### 4.1 Unit Tests
Location: Inline in each module with `#[cfg(test)]`

| Module | Test Coverage |
|--------|---------------|
| `models/*` | Serialization/deserialization roundtrips |
| `config` | Environment parsing, defaults |
| `services/openapi` | Spec parsing, node extraction (mock specs) |
| `services/http` | Request building, response classification |
| `services/llm` | Prompt generation, response parsing |
| `services/healing` | State machine transitions |

### 4.2 Integration Tests
Location: `tests/` directory

```
tests/
├── common/mod.rs          # Test utilities, fixtures
├── repository_test.rs     # Neo4j CRUD (requires test container)
├── openapi_test.rs        # End-to-end ingestion
├── healing_test.rs        # Healing loop with mock HTTP/LLM
└── mcp_test.rs            # MCP protocol compliance
```

**Test Infrastructure:**
- [ ] Add `testcontainers` crate for Neo4j
- [ ] Add `wiremock` for HTTP mocking
- [ ] Add `tokio-test` for async test utilities
- [ ] Fixture OpenAPI specs in `tests/fixtures/`

### 4.3 Test Commands
```bash
cargo test                     # All tests
cargo test --lib               # Unit tests only
cargo test --test '*'          # Integration tests only
cargo test <test_name>         # Single test
cargo test -- --nocapture      # Show println output
```

---

## Phase 5: Docker Setup

### 5.1 Application Dockerfile
```dockerfile
# Multi-stage build for minimal image
FROM rust:1.75 AS builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates && rm -rf /var/lib/apt/lists/*
COPY --from=builder /app/target/release/agent-api /usr/local/bin/
CMD ["agent-api"]
```

### 5.2 Docker Compose Stack
```yaml
services:
  agent-api:
    build: .
    environment:
      - NEO4J_URI=bolt://neo4j:7687
      - NEO4J_USER=neo4j
      - NEO4J_PASSWORD=password
      - OLLAMA_URL=http://ollama:11434
    depends_on:
      - neo4j
      - ollama
    stdin_open: true   # For MCP stdio

  neo4j:
    image: neo4j:5
    environment:
      - NEO4J_AUTH=neo4j/password
    ports:
      - "7474:7474"    # Browser
      - "7687:7687"    # Bolt
    volumes:
      - neo4j_data:/data

  ollama:
    image: ollama/ollama
    ports:
      - "11434:11434"
    volumes:
      - ollama_data:/root/.ollama

volumes:
  neo4j_data:
  ollama_data:
```

### 5.3 Docker Commands
```bash
docker compose up -d           # Start stack
docker compose logs -f         # View logs
docker compose down            # Stop stack
docker compose down -v         # Stop + remove volumes
```

---

## Phase 6: CI/CD Pipeline

### 6.0 Branch Strategy

```
feature/* ──→ dev ──→ test ──→ prod
    │          │        │        │
    │          │        │        └── Full pipeline + deploy
    │          │        └── Full pipeline (integration tests)
    │          └── Format check + unit tests
    └── No CI triggers
```

| Branch | Triggers | Jobs |
|--------|----------|------|
| `feature/*` | None | - |
| `dev` | Push | Format, Unit Tests |
| `test` | Push, PR | Format, Clippy, Unit Tests, Integration Tests |
| `prod` | Push, PR | Full pipeline + Docker build |

### 6.1 GitHub Actions Workflow

`.github/workflows/ci.yml`:
```yaml
name: CI

on:
  push:
    branches: [dev, test, prod]
  pull_request:
    branches: [test, prod]

env:
  CARGO_TERM_COLOR: always

jobs:
  check:
    name: Check
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo check

  fmt:
    name: Format
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: rustfmt
      - run: cargo fmt --all -- --check

  clippy:
    name: Clippy
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
        with:
          components: clippy
      - uses: Swatinem/rust-cache@v2
      - run: cargo clippy -- -D warnings

  test-unit:
    name: Unit Tests
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test --lib

  test-integration:
    name: Integration Tests
    runs-on: ubuntu-latest
    services:
      neo4j:
        image: neo4j:5
        env:
          NEO4J_AUTH: neo4j/testpassword
        ports:
          - 7687:7687
        options: >-
          --health-cmd "cypher-shell -u neo4j -p testpassword 'RETURN 1'"
          --health-interval 10s
          --health-timeout 5s
          --health-retries 5
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test --test '*'
        env:
          NEO4J_URI: bolt://localhost:7687
          NEO4J_USER: neo4j
          NEO4J_PASSWORD: testpassword

  build:
    name: Build Release
    runs-on: ubuntu-latest
    needs: [check, fmt, clippy, test-unit]
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo build --release
      - uses: actions/upload-artifact@v4
        with:
          name: agent-api
          path: target/release/agent-api

  docker:
    name: Build Docker Image
    runs-on: ubuntu-latest
    needs: [test-integration]
    if: github.ref == 'refs/heads/main'
    steps:
      - uses: actions/checkout@v4
      - uses: docker/setup-buildx-action@v3
      - uses: docker/build-push-action@v5
        with:
          context: .
          push: false
          tags: agent-api:latest
          cache-from: type=gha
          cache-to: type=gha,mode=max
```

### 6.2 Pipeline Stages

```
┌─────────┐   ┌─────────┐   ┌─────────┐
│  Check  │   │   Fmt   │   │ Clippy  │
└────┬────┘   └────┬────┘   └────┬────┘
     │             │             │
     └──────────┬──┴─────────────┘
                │
         ┌──────▼──────┐
         │ Unit Tests  │
         └──────┬──────┘
                │
      ┌─────────▼─────────┐
      │ Integration Tests │ (with Neo4j service)
      └─────────┬─────────┘
                │
         ┌──────▼──────┐
         │ Build Docker│ (main branch only)
         └─────────────┘
```

---

## Phase 7: File Structure (Final)

```
agent-api/
├── .github/
│   └── workflows/
│       └── ci.yml
├── src/
│   ├── main.rs
│   ├── config.rs
│   ├── models/
│   │   ├── mod.rs
│   │   ├── resource.rs
│   │   ├── endpoint.rs
│   │   ├── schema.rs
│   │   ├── parameter.rs
│   │   └── healing.rs
│   ├── repository/
│   │   ├── mod.rs
│   │   ├── client.rs
│   │   ├── error.rs
│   │   ├── resource.rs
│   │   ├── endpoint.rs
│   │   ├── schema.rs
│   │   ├── parameter.rs
│   │   └── healing.rs
│   ├── services/
│   │   ├── mod.rs
│   │   ├── openapi.rs
│   │   ├── http.rs
│   │   ├── llm.rs
│   │   └── healing.rs
│   └── mcp/
│       ├── mod.rs
│       ├── server.rs
│       └── tools/
│           ├── mod.rs
│           ├── ingest.rs
│           ├── query.rs
│           └── execute.rs
├── tests/
│   ├── common/
│   │   └── mod.rs
│   ├── fixtures/
│   │   └── petstore.json
│   ├── repository_test.rs
│   ├── openapi_test.rs
│   ├── healing_test.rs
│   └── mcp_test.rs
├── Cargo.toml
├── Cargo.lock
├── Dockerfile
├── docker-compose.yml
├── .env.example
├── .gitignore
├── CLAUDE.md
├── PLAN.md
└── architecture_context.md
```

---

## Implementation Order

| Order | Task | Depends On | Estimated Effort |
|-------|------|------------|------------------|
| 1 | Config + Logging | - | Small |
| 2 | Docker Compose (Neo4j + Ollama) | - | Small |
| 3 | Unit test infrastructure | 1 | Small |
| 4 | Integration test infrastructure | 2, 3 | Medium |
| 5 | CI/CD pipeline | 3, 4 | Small |
| 6 | OpenAPI parser service | 1 | Medium |
| 7 | HTTP executor service | 1 | Small |
| 8 | LLM client service | 1 | Medium |
| 9 | Healing orchestrator | 6, 7, 8 | Large |
| 10 | MCP server + tools | 9 | Large |
| 11 | Dockerfile | 10 | Small |

---

## Additional Dependencies to Add

```toml
# Cargo.toml additions
[dependencies]
dotenvy = "0.15"              # .env file loading
tracing = "0.1"               # Structured logging
tracing-subscriber = "0.3"    # Log output formatting
openapiv3 = "2"               # OpenAPI spec parsing

[dev-dependencies]
tokio-test = "0.4"            # Async test utilities
wiremock = "0.6"              # HTTP mocking
testcontainers = "0.15"       # Docker containers for tests
testcontainers-modules = { version = "0.3", features = ["neo4j"] }
```

---

## Questions for Clarification

1. **Container Registry**: Should Docker images be pushed to GitHub Container Registry, Docker Hub, or another registry?

2. **Deployment Target**: Is this intended to run as a standalone CLI tool, a long-running service, or both?

3. **LLM Fallback**: Should there be a fallback if Ollama is unavailable (e.g., skip healing, use a different provider)?

4. **Test Coverage Target**: What's the minimum acceptable test coverage percentage?

5. **Branch Strategy**: Using GitFlow, trunk-based development, or another branching model?
