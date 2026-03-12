# CLAUDE.md

Guidance for working with the Agent Brain codebase.

## Project Overview

Autonomous Agent Brain — A persistent, self-improving MCP server in Rust backed by a Neo4j knowledge graph. Manages long-term memory with hybrid vector+BM25 RAG, executes background jobs in a durable priority queue, reasons over stored knowledge, and runs an autonomous background scheduler.

## Tech Stack

- **Language:** Rust (Tokio async runtime, Edition 2024)
- **Protocol:** Model Context Protocol (MCP) via stdio or HTTP transport
- **Web Framework:** Axum (HTTP transport with SSE streaming)
- **Database:** Neo4j via `neo4rs` driver
- **AI Model:** Pluggable — Ollama (local), Anthropic, or Gemini

## Build & Test Commands

```bash
cargo build                    # Build
cargo build --release          # Optimized build
cargo fmt                      # Format
cargo clippy                   # Lint
cargo test --lib               # Unit tests only
cargo test --test '*'          # Integration tests (requires Neo4j)
cargo test                     # All tests
cargo test -- --nocapture      # Show println output
```

See `project-docs/cli.md` for full CLI command reference.

## Environment Variables

See `project-docs/env.md` for the full table. Key required vars:
- `NEO4J_PASSWORD` — required; URI defaults to `bolt://localhost:7687`
- `LLM_PROVIDER` — `ollama` (default), `anthropic`, `gemini`

## Local Development

```bash
docker compose up -d       # Start Neo4j and Ollama
cargo run -- init-db       # Initialize schema
cargo run                  # Run MCP server (stdio)
```

## Project Structure

```
src/
├── config.rs               # Config (KNOWLEDGE_SNAPSHOT_DIR, AUTO_SNAPSHOT_*)
├── models/                 # Data models
├── repository/             # Neo4j layer (admin.rs, agent_job.rs, task.rs, ...)
├── services/               # Business logic
│   ├── knowledge.rs        # Notes/RAG + auto-snapshot hook in consolidate_memories
│   ├── snapshot.rs         # SnapshotService (gzip JSON backup/restore)
│   ├── queue.rs            # Job queue + coordinator
│   ├── scheduler.rs        # Autonomous scheduler
│   └── ...                 # llm.rs, healing.rs, context.rs, chat.rs, ...
├── skills/                 # Pluggable skills (admin.rs, knowledge.rs, task.rs, ...)
└── mcp/                    # MCP server (server.rs, protocol.rs, tools.rs, ...)
tests/                      # Integration tests + fixtures
project-docs/               # Detailed reference docs (tools, schema, env, cli, architecture)
snapshots/                  # Knowledge graph snapshots (auto-created, gitignored)
```

## Architecture Summary

78 MCP tools across 13 skills. See `project-docs/architecture.md` for skill registry table, initialization order, and mechanics. See `project-docs/schema.md` for Neo4j node types, relationships, and transport architecture ASCII diagram.

## MCP Tools

78 static tools + N runtime-defined dynamic tools. See `project-docs/tools.md` for full input/output schemas.

**AdminSkill (10 tools):** `delete_api`, `purge_duplicate_endpoints`, `purge_orphaned_schemas`, `reset_graph`, `backfill_endpoint_embeddings`, `snapshot_knowledge`, `restore_knowledge`, `list_snapshots`, `verify_knowledge_integrity`, `analyze_own_structure`

**ContextSkill (4 tools):** `list_context_profiles`, `get_context_profile`, `auto_assign_context`, `build_agent_context`

## Self-Healing Flow

`execute_http_request` on 4xx/5xx → LLM analyzes error → retry with correction → persist `HealingEvent` on success, mark `broken` on failure.

## TODO / Planned Features

See `TODO.md` for the full tiered backlog (P0 critical → P3 infrastructure).

## Branch Strategy

Never write attribution to LLMs or coding agents or assistants.

- `feature/*` — Feature branches (no CI)
- `dev` — Development (format + unit tests)
- `test` — Testing (full pipeline + integration tests)
- `prod` — Production (full pipeline + Docker build)
- Update documentation first: README, CLAUDE.md, project-docs/ should reflect changes.

## Critical Dev Notes

**LlmConfig:** `base_url` is `Option<String>`. Default model: `"granite4:latest"`. Tests: `config.base_url.as_deref()`.

**Skill registration:** Register to BOTH `tool_registry` (for `tools/list`) AND `skills` vec (for `tools/call`). Forgetting either causes invisible tools or dispatch failures.

**AdminSkill constructor:** `AdminSkill::new(neo4j, context_store, llm_config, snapshot_svc, tool_registry)` — 5 args; `tool_registry: Arc<RwLock<ToolRegistry>>` added for `analyze_own_structure`.

**`McpServer`** is a thin backward-compatible wrapper around `McpServerCore` (stdio path only).

**HTTP session init:** Always send `notifications/initialized` AFTER `initialize`, or the server stays in `Initializing` state and rejects all tool calls.

**Initialization order:** `SchedulerService::new()` must be called AFTER `QueueService::new()`. `QueueService::spawn_coordinator()` must be called AFTER the tool handler is set (end of `build_skills`).

**Consolidation loop:** Uses `[Memory N]` labels (not `Note N:`), instructs LLM not to echo them. Auto-generated consolidation topics use `"recent experiences and knowledge"`. Source notes get `next_review_at = now + 30 days` after consolidation. `KnowledgeService::consolidate_memories()` takes a `pre_consolidate` snapshot before the LLM call.

**`services/mod.rs`:** Must re-export `LlmProviderType`: `pub use llm::{LlmClient, LlmConfig, LlmProviderType};`

**`RepoSource::parse`:** Non-URL strings treated as local paths (Ok), not errors. Only unsupported platforms (bitbucket) return Err.

**Context Profiles:** YAML files in `contexts/` (CONTEXTS_DIR env var). Loaded by `ContextBuilderService::load_profiles()` in `build_skills()`. Boot protocol (`contexts/boot.yaml`) runs after each `build_skills()`. Init protocol (`contexts/init.yaml`) runs on empty graph. `ContextSkill` registered when `context_builder_arc` is Some. `ChatService` holds `Arc<RwLock<Option<Arc<ContextBuilderService>>>>` (shared with McpServerCore, reads lazily per request). `ChainStep` and `AgentJob` both have `context_profile: Option<String>` fields. `SchedulerService::new_with_context()` auto-assigns profiles in `goal_to_steps()`.
