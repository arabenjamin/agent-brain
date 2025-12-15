# API Knowledge Graph

An MCP server that ingests OpenAPI specifications into a Neo4j graph database, enabling natural language queries, live API testing, and self-healing documentation.

## What It Does

- **Ingests** OpenAPI/Swagger specs into a queryable knowledge graph
- **Self-heals** documentation when API requests fail (AI-powered corrections)
- **Exports** healed specs back to OpenAPI 3.0 for version control
- **Generates** drift reports showing what changed vs. original docs
- **Connects** to Claude CLI as an MCP server for AI-assisted API work

## Quick Start

```bash
# 1. Clone and build
git clone <repo-url>
cd agent-api
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
- (Optional) Ollama for self-healing features

### Setup

```bash
# Clone repository
git clone <repo-url>
cd agent-api

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
| `OLLAMA_MODEL` | `llama3` | LLM model for self-healing |
| `LOG_LEVEL` | `info` | Log level (trace/debug/info/warn/error) |
| `MCP_TRANSPORT` | `stdio` | MCP transport (stdio/http) |
| `MCP_HTTP_BIND` | `127.0.0.1:3000` | HTTP bind address |
| `MCP_API_KEY` | - | API key for HTTP authentication |
| `SECRET_PROVIDER` | `local` | Secret provider (local/vault/aws/none) |
| `SECRETS_FILE` | `.secrets.enc` | Encrypted secrets file path |
| `SECRETS_ENCRYPTION_KEY` | - | Encryption key for local secrets |

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

# Run as MCP server (stdio transport - for Claude CLI)
cargo run -- serve

# Run as MCP server (HTTP transport - for remote access)
cargo run -- serve --transport http
cargo run -- serve --transport http --bind 0.0.0.0:8080
cargo run -- serve --transport http --api-key my-secret-key

# Initialize/reset database schema
cargo run -- init-db
```

## Claude CLI Integration

Connect this tool to Claude CLI for AI-assisted API exploration and testing.

### Setup

1. **Build the release binary:**
   ```bash
   cargo build --release
   ```

2. **Ensure Neo4j is running:**
   ```bash
   docker compose up -d
   ```

3. **Add to Claude CLI settings:**

   Edit your settings file:
   - macOS: `~/Library/Application Support/claude-code/settings.json`
   - Linux: `~/.config/claude-code/settings.json`

   ```json
   {
     "mcpServers": {
       "api-knowledge-graph": {
         "command": "/path/to/agent-api/target/release/agent-api",
         "args": ["serve"],
         "env": {
           "NEO4J_URI": "bolt://localhost:7688",
           "NEO4J_USER": "neo4j",
           "NEO4J_PASSWORD": "password",
           "OLLAMA_URL": "http://localhost:11434",
           "OLLAMA_MODEL": "llama3"
         }
       }
     }
   }
   ```

4. **Restart Claude CLI** to load the MCP server.

### Available MCP Tools

Once connected, Claude can use these tools:

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
| `export_openapi` | Export healed specs to YAML/JSON |
| `diff_api_spec` | Generate documentation drift reports |
| `configure_api_credential` | Store API credentials for automatic injection |
| `list_api_credentials` | List all configured credentials |
| `delete_api_credential` | Remove an API credential |

### Example Prompts

```
"Ingest the Stripe API spec"

"What endpoints handle payments?"

"Execute a GET request to /users and show me the response"

"Generate a diff report of API changes"

"Export the healed spec to YAML"
```

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
│                      Claude CLI / MCP Client                │
└─────────────────────────┬───────────────────────────────────┘
                          │ JSON-RPC 2.0
               ┌──────────┴──────────┐
               ▼                     ▼
┌─────────────────────┐  ┌─────────────────────┐
│   Stdio Transport   │  │   HTTP Transport    │
│  (local CLI usage)  │  │  (remote/cloud)     │
│                     │  │  - POST /mcp        │
│                     │  │  - GET /mcp (SSE)   │
│                     │  │  - API key auth     │
└──────────┬──────────┘  └──────────┬──────────┘
           └──────────┬─────────────┘
┌─────────────────────▼───────────────────────────────────────┐
│                       MCP Server Core                       │
│  ┌─────────────┐ ┌─────────────┐ ┌─────────────────────┐   │
│  │   Tools     │ │  Sessions   │ │   Protocol Handler  │   │
│  │ (13 tools)  │ │  (HTTP)     │ │    (JSON-RPC 2.0)   │   │
│  └─────────────┘ └─────────────┘ └─────────────────────┘   │
└─────────────────────────┬───────────────────────────────────┘
                          │
┌─────────────────────────▼───────────────────────────────────┐
│                      Services Layer                         │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────────┐   │
│  │ OpenAPI  │ │   HTTP   │ │   LLM    │ │   Healing    │   │
│  │ Parser   │ │ Executor │ │  Client  │ │ Orchestrator │   │
│  └──────────┘ └──────────┘ └──────────┘ └──────────────┘   │
│  ┌──────────┐ ┌──────────┐ ┌──────────┐ ┌──────────────┐   │
│  │ Context  │ │Discovery │ │  DocGen  │ │    Export    │   │
│  │  Store   │ │ Service  │ │ Service  │ │   Module     │   │
│  └──────────┘ └──────────┘ └──────────┘ └──────────────┘   │
│  ┌──────────────────────────────────────────────────────┐   │
│  │                  Secrets Module                       │   │
│  │  ┌──────────┐ ┌──────────┐ ┌──────────┐             │   │
│  │  │  Local   │ │  Vault   │ │   AWS    │ (Providers) │   │
│  │  │(AES-GCM) │ │ (KV v2)  │ │(Secrets) │             │   │
│  │  └──────────┘ └──────────┘ └──────────┘             │   │
│  └──────────────────────────────────────────────────────┘   │
└─────────────────────────┬───────────────────────────────────┘
                          │
┌─────────────────────────▼───────────────────────────────────┐
│                    Neo4j Knowledge Graph                    │
│                                                             │
│   (Resource)──[:HAS_ENDPOINT]──▶(Endpoint)                 │
│                                      │                      │
│                          ┌───────────┼───────────┐          │
│                          ▼           ▼           ▼          │
│                    (Parameter)  (Schema)  (HealingEvent)    │
│                                                             │
│   (ApiCredential) ← Stores credential metadata              │
└─────────────────────────────────────────────────────────────┘
```

## Project Structure

```
agent-api/
├── src/
│   ├── main.rs              # CLI entry point
│   ├── cli.rs               # Command definitions
│   ├── config.rs            # Environment configuration
│   ├── models/              # Data models
│   │   └── credential.rs    # API credential model
│   ├── repository/          # Neo4j database layer
│   │   └── credential.rs    # Credential CRUD operations
│   ├── services/            # Business logic
│   │   ├── openapi.rs       # Spec parser
│   │   ├── http.rs          # HTTP executor
│   │   ├── llm.rs           # Ollama client
│   │   ├── healing.rs       # Self-healing orchestrator
│   │   ├── context.rs       # In-memory context store
│   │   ├── discovery.rs     # Spec auto-discovery
│   │   ├── docgen.rs        # Doc-to-spec generator
│   │   ├── export/          # Export module
│   │   │   ├── builder.rs   # OpenAPI builder
│   │   │   ├── exporter.rs  # Graph-to-spec export
│   │   │   ├── differ.rs    # Diff generator
│   │   │   └── report.rs    # Report formatter
│   │   └── secrets/         # Secret provider abstraction
│   │       ├── provider.rs  # SecretProvider trait
│   │       ├── local.rs     # AES-256-GCM encrypted storage
│   │       ├── vault.rs     # HashiCorp Vault KV v2
│   │       ├── aws.rs       # AWS Secrets Manager
│   │       └── manager.rs   # CredentialManager
│   └── mcp/                 # MCP server implementation
│       ├── protocol.rs      # JSON-RPC types
│       ├── transport.rs     # Stdio transport
│       ├── transport_trait.rs  # Transport abstraction
│       ├── http_transport.rs   # HTTP+SSE transport (Axum)
│       ├── session.rs       # HTTP session management
│       ├── auth.rs          # API key authentication
│       ├── tools.rs         # Tool handlers (13 tools)
│       └── server.rs        # Server state machine (thread-safe)
├── tests/                   # Integration tests
├── docs/                    # Documentation
├── docker-compose.yml       # Neo4j + Ollama stack
├── openapi.yaml             # Sample spec for testing
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
# Binary at: target/release/agent-api
```

## CI/CD

The repository includes GitHub Actions workflows:

- **ci.yml**: Format, lint, and test on push
- **api-contract.yml**: Validate OpenAPI specs, detect breaking changes

See `.github/workflows/` for details.

## License

MIT
