# TODO — Agent Brain Backlog

<!-- Restored 2026-06-11 from commit 9f7e449 after the automated doc-update
     schedule drifted this file into hallucinated content. Do not regenerate
     this file wholesale; make targeted, commit-grounded edits only. -->

## Completed Phases

- **Phase 1** — Core MCP server: Neo4j graph, OpenAPI ingestion, HTTP executor, self-healing ✓
- **Phase 2** — Multi-Provider LLM Client (Anthropic + Gemini + Ollama via `LlmProvider` trait) ✓
- **Phase 3** — Model Registry + Intelligent Selection (`ModelSpec`, `ModelSelector`, 5-tool `ModelSkill`, per-provider semaphores) ✓
- **Phase 4** — Autonomous Scheduler (`SchedulerSkill`, 5 tools; background Tokio task with configurable tick interval, error budget, keyword-based goal-to-chain mapping, and runtime control via MCP tools) ✓
- **Phase 5** — SnapshotService + AdminSkill expansion (10 tools: snapshot/restore/list/verify/analyze) ✓
- **Phase 6** — CLAUDE.md condensation (~776→~130 lines) + project-docs/ reference split ✓
- **Tier 1** — All 6 brain self-improvement capabilities: memory consolidation, semantic chunking, entity extraction, multi-hop graph RAG, get_note, procedural control flow ✓
- **Tier 2** — All 8 HBI frontend items: graph sizing, MCP reconnect, knowledge panel load, graph node click, subtask tree, live graph data, auth settings, logs panel ✓
- **Idle Sleep Mode** — Scheduler backs off after N idle ticks; runs bedtime consolidation chain; wakes on any tool call ✓
- **Additional tools** — `list_notes`, `search_by_entity`, `delete_note`, `update_note` ✓ (KnowledgeSkill now 16 tools, total 81 static)
- **Consolidation loop fix** — `[Memory N]` labels, `next_review_at` reset, `"recent experiences"` default topic ✓
- **Chat features** — Expandable event bubbles (thinking/tool_call/tool_result), session history sidebar, research mode, context profile selector, export ✓

---

## P0 — Critical (fix before next deployment)

- [ ] **Harden the doc-update schedule against hallucination drift** — the 12h `schedules/doc-update.yaml` chain regenerates TODO.md/STATUS.md whole-file via a local model and ratcheted both files into generic fiction (restored 2026-06-11). Add a validation step before `write_codebase_doc` (required headings present, diff ratio cap) or switch the chain to targeted edits / proposal notes instead of full-file rewrites.

---

## P1 — Open Bugs

- [ ] **Docker image needs rebuild** — The local Rust build and frontend are ahead of the running Docker container. The 4000-char SSE preview increase and expandable events won't be visible in the container until a rebuild:
  ```bash
  docker compose build agent-brain && docker compose up -d agent-brain
  ```

- [ ] **Stale dynamic tools in Neo4j** — `home_arm` and `safe_move_arm` dynamic tools were registered for testing and persist in the graph across restarts. Remove with `remove_dynamic_tool` via MCP or directly via Neo4j.

---

## P2 — Enhancements

- [ ] **SSE push for job results on stdio transport** — stdio path has no session manager; callers must poll `get_job_result`. Consider a lightweight event bus or callback hook.

- [ ] **Rhai scripting in procedure steps** — current conditionals (`on_failure`, `{{context.steps.N}}`) cover basic flow. Full Rhai embed enables arbitrary conditional logic in step args. Still deferred.

- [x] **`configure_scheduler` tool exposes `idle_sleep_after_ticks` / `sleep_interval_secs`** — confirmed in `skills/scheduler.rs` lines 85-90; both fields in schema and handler.

---

## P2 — Frontend (hbi-frontend)

- [x] **Knowledge panel note search + filter by type** — `note_type` dropdown filter + `GET /api/notes?q=` text search (REST, no MCP). Filter applied client-side on results.

- [x] **Graph panel — entity node styling** — Entity nodes rendered as diamonds (◇) in orange; task nodes as squares; note nodes as circles by note_type color.

- [ ] **Graph panel — performance for large graphs** — `max_nodes=200` cap is in place; virtualization/LOD deferred until graph regularly exceeds that limit.

- [x] **Chat panel — code block syntax highlighting** — `rehype-highlight` + `highlight.js/styles/github-dark.css` in place.

- [x] **Chat panel — streaming token display** — Token events accumulated and rendered incrementally; `displayText = finalText || tokenText`.

- [x] **Knowledge panel — REST-only mutations** — All note create/update/delete calls now use `POST/PUT/DELETE /api/notes` (no MCP dependency). Related notes section is collapsible to prevent content overflow.

---

## P3 — Infrastructure

- [x] **Create dev/test/prod branches** — all three branches exist on `origin` (dev, test, prod). CI pipeline wired.

- [x] **Docker Compose — add `hbi-frontend` service** — service defined in `docker-compose.yml` (line 83), multi-stage `Dockerfile` in `hbi-frontend/`.

- [ ] **GHCR package visibility** — configure after first prod push.

---

## P3 — Documentation Debt

- [x] **Update ROADMAP.md** — Tier 1/2/3 complete; Tier 4 evolution items added (federated memory, multimodal, proactive interrupts, self-modifying chains, fine-tuning pipeline).

- [x] **Add schema_version policy** to `project-docs/schema.md` — policy section added; documents how `init_schema()` serves as the migration script.

- [x] **`project-docs/architecture.md`** — skill registry table updated (16 skills, 61+ tools); job chain lifecycle ASCII diagram added; stale sections (SnapshotService, Self-Healing, vLLM) removed.

- [x] **`project-docs/tools.md`** — full rewrite: removed deleted ApiSkill (14 tools); documents all 16 current skills with accurate tool lists and args.
