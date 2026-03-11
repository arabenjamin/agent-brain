# CLI Reference

## Run as MCP Server

```bash
# stdio transport (default — for Claude Desktop / MCP clients)
cargo run -- serve
cargo run

# HTTP transport
cargo run -- serve --transport http                            # localhost:3000
cargo run -- serve --transport http --bind 0.0.0.0:8080       # custom bind
cargo run -- serve --transport http --api-key my-secret-key   # with auth
```

## Database

```bash
# Initialize database schema (create indexes, constraints)
cargo run -- init-db
```

## OpenAPI Operations

```bash
# Ingest a spec (URL or file path)
cargo run -- ingest path/to/spec.json
cargo run -- ingest https://example.com/openapi.json

# Query endpoints
cargo run -- query "users"
cargo run -- query "/api/v1"

# Execute HTTP request against a loaded API
cargo run -- execute -m GET https://api.example.com/users
cargo run -- execute -m POST https://api.example.com/users \
  -b '{"name":"test"}' \
  -H "Content-Type: application/json"

# Show database statistics
cargo run -- stats
```

## Export & Diff

```bash
# Export healed graph to OpenAPI spec
cargo run -- export                           # YAML to stdout
cargo run -- export -o spec.yaml              # to file
cargo run -- export -f json -o spec.json      # as JSON
cargo run -- export --annotations=false       # without x-healed-by-ai
cargo run -- export --include-broken          # include broken endpoints

# Generate diff report (original vs healed)
cargo run -- diff                             # Markdown report
cargo run -- diff -f changelog                # Git-style changelog
cargo run -- diff -f json                     # JSON format
cargo run -- diff --breaking-only             # only breaking changes
```
