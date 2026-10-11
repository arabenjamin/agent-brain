# CLI Reference

Generated from `crates/app/src/cli.rs`; keep the two in step.

```
agent-brain [OPTIONS] [COMMAND]

Commands:
  serve            Run as MCP server (default when no command is given: stdio)
  init-db          Initialize the Neo4j database schema
  repair-notes     One-off repair of notes that stored a query envelope instead of content
  resolve-entities Fold entities that are one real thing (curated aliases + spelling), reversibly
  eval-retrieval   Score retrieval quality against a golden set (read-only)
  todo             Manage todo items over HTTP (requires a running server)
```

There is no interactive REPL: talk to the brain through `/chat` (the HBI
frontend), the OpenAI-compatible `/v1` front door, or an MCP client.

## Serve as MCP Server

```bash
# stdio transport (default — for Claude Desktop / local MCP clients)
cargo run -- serve
cargo run                       # same thing

# HTTP transport
cargo run -- serve --transport http                           # localhost:3000
cargo run -- serve --transport http --bind 0.0.0.0:8080      # custom bind
cargo run -- serve --transport http --api-key my-secret-key  # with auth
```

## Database

```bash
# Initialize schema (indexes, constraints) and seed workflows, topics, entities
cargo run -- init-db
```

## Restore a snapshot

```bash
agent-brain restore-snapshot <file.json.gz> [--dry-run]
```

Restores a knowledge-graph snapshot from `KNOWLEDGE_SNAPSHOT_DIR` (notes, tasks, entities, aliases, procedures, relationships). MERGE semantics: safe on a non-empty graph, nothing is deleted. `--dry-run` prints what the file holds. In compose: `docker compose exec agent-brain agent-brain restore-snapshot /home/agent/snapshots/<file>`.

## Repair notes

```bash
# Rewrite notes written before the 2026-08-10 extract_result_text fix.
# Idempotent; run with the scheduler paused.
cargo run -- repair-notes --dry-run
cargo run -- repair-notes
```

## Entity resolution

```bash
# What init-db would merge on the next start (read-only): curated aliases from
# entities/*.yaml, plus names differing only by case, punctuation or spacing.
cargo run -- resolve-entities --dry-run
cargo run -- resolve-entities                 # apply now (pre_entity_merge snapshot first)
cargo run -- resolve-entities --unmerge trump # undo one merge: the alias becomes its own entity again
```

## Retrieval evaluation

```bash
cargo run -- eval-retrieval                                  # eval/retrieval_golden.yaml, k=10
cargo run -- eval-retrieval --fixture my_cases.yaml --k 5
cargo run -- eval-retrieval --bootstrap 20 > proposed.yaml   # sample notes to curate into cases
```

## Todos

```bash
cargo run -- todo add "Review PR" --priority 1 --due 2026-10-10 --tags review,code
cargo run -- todo list --status pending
cargo run -- todo done <id>
cargo run -- todo status <id> in_progress
cargo run -- todo delete <id>
# --url (env BRAIN_URL, default http://localhost:3000) points at the server
```

## Global Options

```
--neo4j-uri <URI>       Neo4j connection URI [env: NEO4J_URI] [default: bolt://localhost:7687]
--neo4j-user <USER>     Neo4j username [env: NEO4J_USER] [default: neo4j]
--neo4j-password <PASS> Neo4j password [env: NEO4J_PASSWORD]
--log-level <LEVEL>     Log level [env: LOG_LEVEL] [default: info]
--log-format <FORMAT>   Log format: pretty | json [env: LOG_FORMAT] [default: pretty]
```
