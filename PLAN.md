# Implementation Plan: Autonomous API Knowledge Graph

## Current State Summary

**Completed (100% core implementation):**
- ✅ Data models (Resource, Endpoint, Schema, Parameter, HealingEvent, ApiCredential)
- ✅ Neo4j repository layer with full CRUD operations
- ✅ Relationship management for all graph edges
- ✅ Error handling infrastructure
- ✅ Configuration management (config.rs)
- ✅ Logging & observability (tracing)
- ✅ CLI with clap (serve, init-db, ingest, query, execute, stats, export, diff)
- ✅ OpenAPI parser and ingestion service
- ✅ HTTP request executor service
- ✅ LLM client service (Ollama integration)
- ✅ Self-healing orchestrator service
- ✅ In-memory API context store with DB fallback
- ✅ OpenAPI spec auto-discovery with LLM assistance
- ✅ Documentation-to-OpenAPI generator with LLM
- ✅ OpenAPI export module (Graph-to-Spec with diff reporting)
- ✅ Custom MCP server implementation (JSON-RPC 2.0 over stdio)
- ✅ 13 MCP tools (ingest, query, execute, context, discovery, docgen, export, diff, credentials)
- ✅ Secret provider abstraction (Local AES-GCM, Vault, AWS Secrets Manager)
- ✅ Credential management with URL-based auto-injection
- ✅ Unit tests (154 tests passing)
- ✅ Integration tests (21+ tests)
- ✅ Docker Compose setup (Neo4j + Ollama)
- ✅ GitHub Actions CI/CD pipeline
- ✅ Production Dockerfile with multi-stage build

**Optional Enhancements:**
- ⏳ Testcontainers for isolated Neo4j in CI
- ⏳ Vector/semantic search for endpoints

---

## Phase 1: Project Infrastructure ✅

### 1.1 Configuration Management
- [x] Create `src/config.rs` with environment-based configuration
- [x] Support: Neo4j URI/credentials, Ollama endpoint, log level
- [x] Add `dotenvy` crate for `.env` file support

### 1.2 Logging & Observability
- [x] Add `tracing` + `tracing-subscriber` crates
- [x] Structured JSON logging for production
- [x] Request/response tracing for debugging

### 1.3 Application Bootstrap
- [x] Implement async `main()` with Tokio runtime
- [x] Initialize Neo4j connection and schema on startup
- [x] Graceful shutdown handling

---

## Phase 2: Core Services ✅

### 2.1 OpenAPI Parser (`src/services/openapi.rs`)
- [x] Add `openapiv3` crate for spec parsing
- [x] Parse from URL (fetch) or file path
- [x] Map OpenAPI paths → Endpoint nodes
- [x] Map OpenAPI schemas → Schema nodes
- [x] Map OpenAPI parameters → Parameter nodes
- [x] Extract tags → Resource groupings
- [x] Bulk insert with transaction support

### 2.2 HTTP Executor (`src/services/http.rs`)
- [x] Wrap `reqwest` client with timeout/retry config
- [x] Build requests from Endpoint + Parameter data
- [x] Capture response status, body, timing
- [x] Classify responses (success/client-error/server-error)

### 2.3 LLM Client (`src/services/llm.rs`)
- [x] Ollama REST API client
- [x] Prompt templates for error analysis
- [x] Structured output parsing for healing suggestions
- [x] Configurable model selection (llama3, mistral)

### 2.4 Healing Orchestrator (`src/services/healing.rs`)
- [x] Implement retry loop from architecture doc
- [x] On 4xx/5xx: call LLM for analysis
- [x] Apply suggested fix, retry request
- [x] On success: create HealingEvent, update graph
- [x] On continued failure: mark endpoint broken

---

## Phase 3: MCP Server ✅

### 3.1 MCP Protocol Layer (`src/mcp/`)
- [x] Custom JSON-RPC 2.0 protocol implementation (protocol.rs)
- [x] Async stdio transport with tokio channels (transport.rs)
- [x] MCP server state machine (Created → Initializing → Running → ShuttingDown)
- [x] Tool registration with JSON Schema definitions

### 3.2 Tool Implementations
- [x] `ingest_openapi` - Parse spec, load graph, return counts
- [x] `graph_query_endpoint` - Search endpoints by path pattern
- [x] `execute_http_request` - Run request with auto-credential injection
- [x] `get_api_context` - Retrieve API summaries from in-memory context
- [x] `list_loaded_apis` - List all APIs in the context store
- [x] `clear_api_context` - Remove APIs from in-memory context
- [x] `discover_openapi` - Auto-discover OpenAPI specs with LLM assistance
- [x] `build_openapi_from_docs` - Generate OpenAPI specs from documentation pages
- [x] `export_openapi` - Export healed graph back to OpenAPI 3.0 spec
- [x] `diff_api_spec` - Compare original vs healed graph, generate diff reports
- [x] `configure_api_credential` - Store API credentials for auto-injection
- [x] `list_api_credentials` - List all configured credentials
- [x] `delete_api_credential` - Remove an API credential

### 3.3 Query Enhancement
- [x] Fuzzy matching on path, summary, operation_id
- [ ] Full-text search index in Neo4j (optional)
- [ ] Vector embeddings for semantic search (optional)

---

## Phase 4: Testing Strategy ✅

### 4.1 Unit Tests ✅
Location: Inline in each module with `#[cfg(test)]`
**Status: 154 tests passing**

| Module | Test Coverage |
|--------|---------------|
| `models/*` | ✅ Serialization/deserialization roundtrips |
| `models/credential` | ✅ Credential types, inject locations |
| `config` | ✅ Environment parsing, defaults |
| `services/openapi` | ✅ Spec parsing, node extraction (mock specs) |
| `services/http` | ✅ Request building, response classification |
| `services/llm` | ✅ Prompt generation, response parsing |
| `services/healing` | ✅ State machine transitions |
| `services/context` | ✅ Context store operations |
| `services/discovery` | ✅ OpenAPI validation, link extraction |
| `services/docgen` | ✅ Spec generation, YAML/JSON output |
| `services/export` | ✅ OpenAPI builder, exporter, differ, report generator |
| `services/secrets/local` | ✅ AES-GCM encryption, persistence |
| `services/secrets/vault` | ✅ URL building, config validation |
| `services/secrets/aws` | ✅ Secret name building, prefix handling |
| `services/secrets/manager` | ✅ Credential formatting, URL matching |
| `mcp/protocol` | ✅ JSON-RPC message parsing |
| `mcp/tools` | ✅ Tool registry, input parsing (13 tools) |
| `mcp/server` | ✅ Server state transitions |

### 4.2 Integration Tests ✅
Location: `tests/` directory

```
tests/
├── common/mod.rs             # Test utilities, fixtures
├── repository_test.rs        # Neo4j CRUD (requires running Neo4j)
├── context_tools_test.rs     # Context management tool tests
├── discovery_test.rs         # Discovery service tests
├── docgen_test.rs            # Doc-to-OpenAPI generation tests
└── fixtures/petstore.json    # Sample OpenAPI spec
```

**Test Infrastructure:**
- [x] Fixture OpenAPI specs in `tests/fixtures/`
- [x] Integration tests for context management (5 tests)
- [x] Integration tests for discovery service (6 tests)
- [x] Integration tests for docgen service (10 tests)
- [ ] Add `testcontainers` crate for Neo4j (optional)

### 4.3 Test Commands
```bash
cargo test                     # All tests
cargo test --lib               # Unit tests only
cargo test --test '*'          # Integration tests only
cargo test <test_name>         # Single test
cargo test -- --nocapture      # Show println output
```

---

## Phase 5: Docker Setup ✅

### 5.1 Application Dockerfile ✅
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

### 5.2 Docker Compose Stack ✅
**Implemented in `docker-compose.yml`** - Includes Neo4j and Ollama services with proper health checks and volume mounts.

### 5.3 Docker Commands
```bash
docker compose up -d           # Start stack
docker compose logs -f         # View logs
docker compose down            # Stop stack
docker compose down -v         # Stop + remove volumes
```

---

## Phase 6: CI/CD Pipeline ✅

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

### 6.1 GitHub Actions Workflow ✅

**Implemented in `.github/workflows/ci.yml`**
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

## Phase 7: File Structure (Current)

```
agent-api/
├── .github/
│   └── workflows/
│       └── ci.yml              # GitHub Actions CI pipeline
├── src/
│   ├── lib.rs                  # Library exports
│   ├── main.rs                 # CLI entry point
│   ├── cli.rs                  # Clap CLI definitions
│   ├── config.rs               # Environment configuration
│   ├── logging.rs              # Tracing setup
│   ├── models/
│   │   ├── mod.rs
│   │   ├── resource.rs
│   │   ├── endpoint.rs
│   │   ├── schema.rs
│   │   ├── parameter.rs
│   │   ├── healing.rs
│   │   ├── http.rs             # HTTP method, response types
│   │   └── credential.rs       # API credential model
│   ├── repository/
│   │   ├── mod.rs
│   │   ├── client.rs           # Neo4j connection
│   │   ├── error.rs
│   │   ├── resource.rs
│   │   ├── endpoint.rs
│   │   ├── schema.rs
│   │   ├── parameter.rs
│   │   ├── healing.rs
│   │   └── credential.rs       # Credential CRUD operations
│   ├── services/
│   │   ├── mod.rs
│   │   ├── openapi.rs          # OpenAPI parser (~500 lines)
│   │   ├── http.rs             # HTTP executor (~750 lines)
│   │   ├── llm.rs              # Ollama LLM client (~890 lines)
│   │   ├── healing.rs          # Self-healing orchestrator (~740 lines)
│   │   ├── context.rs          # In-memory API context store (~350 lines)
│   │   ├── discovery.rs        # OpenAPI auto-discovery (~650 lines)
│   │   ├── docgen.rs           # Doc-to-OpenAPI generator (~880 lines)
│   │   ├── export/
│   │   │   ├── mod.rs          # Export module exports
│   │   │   ├── builder.rs      # OpenAPI spec builder (~435 lines)
│   │   │   ├── exporter.rs     # Graph-to-Spec exporter (~515 lines)
│   │   │   ├── differ.rs       # Spec diff generator (~340 lines)
│   │   │   └── report.rs       # Markdown report generator (~200 lines)
│   │   └── secrets/            # Secret provider abstraction
│   │       ├── mod.rs          # Module exports
│   │       ├── error.rs        # Secret error types
│   │       ├── provider.rs     # SecretProvider trait
│   │       ├── local.rs        # AES-256-GCM encrypted storage (~440 lines)
│   │       ├── vault.rs        # HashiCorp Vault KV v2 (~250 lines)
│   │       ├── aws.rs          # AWS Secrets Manager (~220 lines)
│   │       └── manager.rs      # CredentialManager (~400 lines)
│   └── mcp/
│       ├── mod.rs
│       ├── protocol.rs         # JSON-RPC 2.0 types (~300 lines)
│       ├── transport.rs        # Stdio transport (~100 lines)
│       ├── tools.rs            # Tool registry & handlers (~1600 lines)
│       └── server.rs           # MCP server (~300 lines)
├── tests/
│   ├── common/
│   │   └── mod.rs              # Test utilities
│   ├── fixtures/
│   │   └── petstore.json       # Sample OpenAPI spec
│   ├── repository_test.rs      # Neo4j integration tests
│   ├── context_tools_test.rs   # Context management tool tests
│   ├── discovery_test.rs       # Discovery service tests
│   └── docgen_test.rs          # Doc-to-OpenAPI generation tests
├── Cargo.toml
├── Cargo.lock
├── docker-compose.yml
├── .env.example
├── .gitignore
├── CLAUDE.md
├── PLAN.md
└── architecture_context.md
```

---

## Implementation Order

| Order | Task | Status |
|-------|------|--------|
| 1 | Config + Logging | ✅ Complete |
| 2 | Docker Compose (Neo4j + Ollama) | ✅ Complete |
| 3 | Unit test infrastructure | ✅ Complete |
| 4 | Integration test infrastructure | ✅ Basic |
| 5 | CI/CD pipeline | ✅ Complete |
| 6 | OpenAPI parser service | ✅ Complete |
| 7 | HTTP executor service | ✅ Complete |
| 8 | LLM client service | ✅ Complete |
| 9 | Healing orchestrator | ✅ Complete |
| 10 | MCP server + tools | ✅ Complete |
| 11 | Application Dockerfile | ✅ Complete |
| 12 | OpenAPI export module (Graph-to-Spec) | ✅ Complete |
| 13 | Diff reporting (original vs healed) | ✅ Complete |
| 14 | Secret provider abstraction | ✅ Complete |
| 15 | Credential management tools | ✅ Complete |

---

## Dependencies (Current)

All core dependencies are in place in `Cargo.toml`:

```toml
[dependencies]
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
serde_yaml = "0.9"
neo4rs = "0.8"
reqwest = { version = "0.12", features = ["json"] }
openapiv3 = "2"
chrono = { version = "0.4", features = ["serde"] }
uuid = { version = "1", features = ["v4", "serde"] }
thiserror = "2"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["json", "env-filter"] }
clap = { version = "4", features = ["derive", "env"] }
dotenvy = "0.15"
url = "2"
scraper = "0.21"               # HTML parsing for discovery/docgen
schemars = "0.8"               # JSON Schema generation for MCP tools

# Secret provider dependencies
aes-gcm = "0.10"               # AES-256-GCM encryption for local secrets
rand = "0.8"                   # Random nonce generation
base64 = "0.22"                # Base64 encoding for Basic auth
aws-config = { version = "1.5", features = ["behavior-version-latest"] }
aws-sdk-secretsmanager = "1.54"  # AWS Secrets Manager
```

---

## Future Enhancements (Optional)

1. **Vector/Semantic Search**: Add embeddings for natural language endpoint queries
2. **Testcontainers**: Use `testcontainers` crate for isolated Neo4j in CI
3. **HTTP Mocking**: Add `wiremock` for deterministic HTTP tests
4. **Container Registry**: Push Docker images to GHCR or Docker Hub
