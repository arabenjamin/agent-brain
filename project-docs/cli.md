# CLI Reference

```
agent-brain [OPTIONS] [COMMAND]

Commands:
  repl     Interactive chat REPL (default when no command given)
  serve    Run as MCP server
  status   Show brain status
  init-db  Initialize Neo4j schema
  api      OpenAPI spec management
```

## Interactive REPL (default)

```bash
# Start REPL (same as running with no args)
cargo run -- repl
cargo run

# With context profile and session continuity
cargo run -- repl --profile knowledge-worker --session my-session
```

## Serve as MCP Server

```bash
# stdio transport (default — for Claude Desktop / local MCP clients)
cargo run -- serve

# HTTP transport
cargo run -- serve --transport http                           # localhost:3000
cargo run -- serve --transport http --bind 0.0.0.0:8080      # custom bind
cargo run -- serve --transport http --api-key my-secret-key  # with auth
```

## Status

```bash
cargo run -- status
```

Shows API resource/endpoint/schema counts and healing event statistics.

## Database

```bash
# Initialize schema (indexes, constraints)
cargo run -- init-db
```

## OpenAPI Operations (`api` subcommand)

```bash
# Ingest a spec
cargo run -- api ingest path/to/spec.json
cargo run -- api ingest https://example.com/openapi.json

# Query endpoints
cargo run -- api query "users"
cargo run -- api query "/api/v1"

# Execute HTTP request
cargo run -- api execute -m GET https://api.example.com/users
cargo run -- api execute -m POST https://api.example.com/users \
  -b '{"name":"test"}' \
  -H "Content-Type: application/json"

# Export healed graph to OpenAPI spec
cargo run -- api export                          # YAML to stdout
cargo run -- api export -o spec.yaml             # to file
cargo run -- api export -f json -o spec.json     # as JSON
cargo run -- api export --annotations=false      # without x-healed-by-ai
cargo run -- api export --include-broken         # include broken endpoints

# Generate diff report (original vs healed)
cargo run -- api diff                            # Markdown report
cargo run -- api diff -f changelog               # Git-style changelog
cargo run -- api diff -f json                    # JSON format
cargo run -- api diff --breaking-only            # only breaking changes

# Generate missing endpoint embeddings
cargo run -- api embed
```

## Global Options

```
--neo4j-uri <URI>       Neo4j connection URI [env: NEO4J_URI] [default: bolt://localhost:7687]
--neo4j-user <USER>     Neo4j username [env: NEO4J_USER] [default: neo4j]
--neo4j-password <PASS> Neo4j password [env: NEO4J_PASSWORD]
--log-level <LEVEL>     Log level [env: LOG_LEVEL] [default: info]
--log-format <FORMAT>   Log format: pretty | json [env: LOG_FORMAT] [default: pretty]
```
