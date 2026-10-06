# CLAUDE.md

Guidance for working with the Agent Brain codebase. Forked from [Agent Api](https://github.com/arabenjamin/agent-api) and since rewritten into a persistent, self-improving autonomous agent brain.

This file is loaded into every session, so it holds only what applies to most work. **Detail lives in `project-docs/`** — see "Where things are documented" below, and read the matching section before changing a subsystem.

> **Do not add content to this file.** New rules, lessons, env vars and plan status go in the matching `project-docs/` file. If a new doc is needed, add one row to the "Where things are documented" table pointing at it, and nothing more.

## Project Overview

A persistent, self-improving MCP server in Rust backed by a Neo4j knowledge graph. Long-term memory with hybrid vector+BM25 RAG, a durable priority job queue, reasoning over stored knowledge, and an autonomous scheduler that dispatches pending tasks as job chains.

**Stack:** Rust (Tokio, Edition 2024) · MCP over stdio or HTTP (Axum + SSE) · Neo4j via `neo4rs` · DuckDB telemetry · pluggable LLMs (host Ollama, llama.cpp sidecar, Ollama Cloud, Anthropic, Gemini, any OpenAI-compatible API via `providers:` in `models.yaml`), chosen per call by the routing policy.

## Commands

```bash
cargo build | cargo build --release | cargo fmt | cargo clippy
cargo test --lib               # Unit tests (all crates)
cargo test --test '*'          # Integration tests (require Neo4j)
cargo run -- serve [--transport http --bind 0.0.0.0:8080 --api-key KEY]   # stdio is default
cargo run -- init-db           # Initialize schema
cargo run -- eval-retrieval    # Retrieval recall@k / MRR against eval/retrieval_golden.yaml
docker compose up -d --build   # Full stack; brain on host port 3001
docker compose logs -f agent-brain
```

More: `project-docs/cli.md`. Endpoints: `POST/GET/DELETE /mcp`, `GET /health`, `/v1/models` + `/v1/chat/completions` (OpenAI-compatible front door), `/api/*` REST.

## Deployment facts

- **The brain runs in Docker.** Test Rust changes by rebuilding the image; never install deps on the host; external CLI tools (yt-dlp, ffmpeg) go in the Dockerfile.
- **The brain has no `env_file`** — every key must be passed through in `docker-compose.yml`, or it never reaches the process. A new seed dir needs its compose env var too.
- Env reference: **`project-docs/env.md`**. Compose sets the values that matter (`LOCAL_LLM_BACKEND=llamacpp`, `CHAT_DEFAULT_PROFILE`/`MCP_TOOL_PROFILE=GeneralDefault-Orchrestator`, `CLOUD_TIER=2`, `TZ`); code defaults are often stale.
- The GPU is an 8 GB RTX 3060 Ti on driver 535 / CUDA 12.2. The llama.cpp sidecar is built from source for it (Gemma-4-E4B, `-c 32768 -np 2` ⇒ 16384 tokens/request). Embeddings (bge-m3) always stay on host Ollama.
- `.githooks/post-commit` runs `docker compose up -d --build` (`git config core.hooksPath .githooks`).

## Project Structure

Cargo workspace; source is in `crates/*/src/`, not `src/`.

```
crates/protocol/    agent-brain-protocol: MCP types, Skill trait, ToolHandler
crates/models/      pure data types (agent_job, task, scheduled_task, procedure, provenance)
crates/repository/  Neo4j + DuckDB layer (client.rs init_schema, agent_job.rs, telemetry.rs, temporal.rs)
crates/app/src/
  main.rs, cli.rs, config.rs, brain_core.rs (storage + LLM + skill registry + scheduler)
  clients/chat/     ChatService — one round loop for every provider (round_loop.rs)
  services/         knowledge/, llm*, routing/budget/provider_gate, model_router, queue, scheduler,
                    agent_runner, context_builder, *_seeder, validation, cypher_guard, graph_schema
  skills/           one file per skill (`list_capabilities` for live counts)
  mcp/              server.rs, tools.rs, transport.rs, http_transport.rs (+ openai_compat.rs)
YAML seed dirs:     chains/ schedules/ contexts/ sources/ sources-media/ mcp-servers/ schemas/ models.yaml
```

## Rules that apply to most changes

- **Skill registration:** push the skill once onto `skills` in `BrainCore::build_skills`; registry and handler share its `Arc`. Dispatch is first-match, so `DynamicSkill` stays last and a tool name belongs to the first skill that claims it.
- **No hardcoded data in Rust.** Declarative data (domains, seeds, chains, schedules, profiles, models) lives in YAML seed dirs and the graph.
- **Timestamps:** every temporal graph property is a native UTC `DATETIME`. Cypher compares a datetime to a string as null and silently drops the row. Read `project-docs/schema.md` § Timestamps before writing date Cypher. Stored is UTC; anything said to a person is local (`services/clock.rs`).
- **Structured LLM output:** use `LlmProvider::generate_json`, and parse with `first_json_object`, never first-`{`-to-last-`}`.
- **Host Ollama defaults to a 4096-token context** and truncates silently; every host-Ollama config sets `num_ctx`.
- **Chain/schedule steps:** `provider_hint` is a step top-level field (omitting it means cloud, not local); a `reason` step past position 1 takes `context: "{{_prev}}"`; search steps use `{{goal_topic}}`, not `{{goal}}`; a ScheduledTask's steps run verbatim with no evaluator, critic or retrieval.
- **A `runtime`-owned ScheduledTask silently shadows its YAML file.** Check `managed_by` before assuming the file is what runs. Pause with a targeted Cypher `SET enabled=false`, not `upsert`.
- **Privacy before budget:** work is `private` unless marked `data_class: public`, and private work only reaches `no_log` providers. Paid providers need a spend cap and `ROUTING_PAID_FOR`.
- **Failures must be visible.** A truncated result is marked, a count must be drillable to its rows, and an empty answer names what does exist. A tool error and a quality miss (`isError: false`) are different things; check the tool's actual output before blaming the model.
- **A local small-model prompt is a size budget** (keep it near 2-3k chars, terse imperatives). Local E4B handles one-shot synthesis; investigative loops need cloud. Measure the answer against a cloud twin before pinning an agent local.
- **Sandboxes** (`sandbox`, `rust-sandbox`) stay on an internal network with no egress and no `ports:` mapping. A failed run is a successful tool call.
- **Docs first:** update README and the `project-docs/` files to reflect changes. A new subsystem rule goes in `project-docs/subsystem-rules.md`, its incident story in `project-docs/engineering-history.md`, never here.

## Where things are documented

| Topic | File |
|---|---|
| Every subsystem's operative rules (scheduler, evaluator, critic, chains, self-knowledge tools, chat loop, front door, schedules, curiosity engine, epistemics, search ladder, media, MCP client, context profiles, going local) + data model + critical dev notes | `project-docs/subsystem-rules.md` |
| Why each rule exists (incidents, measurements) | `project-docs/engineering-history.md` |
| Environment variables | `project-docs/env.md` |
| Plan docs and their phase status | `project-docs/PLANS_INDEX.md` |
| Graph schema, timestamps | `project-docs/schema.md` |
| Skill registry, init order | `project-docs/architecture_context.md` |
| Tool counts, feature status | `project-docs/STATUS.md` |
| CLI | `project-docs/cli.md` |
| Fine-tuning | `project-docs/FINE_TUNING_PLAYBOOK.md`, `RUST_FINETUNE_EXPERIMENT.md` |

## Branch Strategy
DO NOT REMOVE THIS LINE:Never write in credidation to LLMs or coding agents or assistants.

- `feature/*` - Feature branches (no CI)
- `dev` - Development (format + unit tests)
- `test` - Testing (full pipeline with integration tests)
- `prod` - Production (full pipeline + Docker build)
- Update the documentation first, the README, claude, plan, markdowns should reflect our changes.
