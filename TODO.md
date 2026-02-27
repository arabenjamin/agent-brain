# TODO — Agent Brain Backlog

## Completed Phases

- **Phase 2** — Multi-Provider LLM Client (Anthropic + Gemini + Ollama via `LlmProvider` trait) ✓
- **Phase 3** — Model Registry + Intelligent Selection (`ModelSpec`, `ModelSelector`, 5-tool `ModelSkill`, per-provider semaphores) ✓

---

## Bugs

- [x] **Context not reloaded on restart** — Fixed: `ContextStore::load_all()` is called in `build_skills()` at startup.

- [ ] **`graph_query_endpoint` natural language matching** — `CONTAINS` queries fail on paraphrased queries. Fix: use embedding similarity search via the `note_embeddings` vector index pattern.

- [ ] **DynamicSkill skips Neo4j load on legacy McpServer** — `McpServer::build_skills()` is sync so it can't call `load_from_neo4j().await`. Dynamic tools not available on stdio path after restart.

- [ ] **Per-provider semaphores not resizable at runtime** — `set_worker_config` updates `WorkerConfig` but the underlying semaphores were fixed at startup.

---

## Enhancements

- [x] **Wire up SleepSkill** — registered in `build_skills()` when telemetry is available. `DATASET_DIR` env var added (default `./datasets`).

- [x] **Graph cleanup tools** — New `AdminSkill` (4 tools): `delete_api` (cascade), `purge_duplicate_endpoints`, `purge_orphaned_schemas`, `reset_graph` (confirm guard). All support `dry_run`.

- [x] **Job chaining** — `enqueue_chain` tool added to `AgentSkill` (now 8 tools). Takes an ordered list of steps; step 1 is queued immediately, steps 2..N are `parked`. On parent completion the coordinator auto-promotes children; on parent death/exhaustion children are cancelled.

- [ ] **SSE push for job results** — Callers must poll `get_job_result`. Push `notifications/jobs/completed` over the SSE stream when a job finishes instead.

- [ ] **Rhai scripting in procedure steps** — Current template substitution is string-only (`{{input.field}}`). Embed Rhai for conditional logic in step args. Deferred v2.

- [ ] **`graph_query_endpoint` semantic search** — Replace `CONTAINS` with embedding similarity via the `endpoint_embeddings` vector index for better natural-language query matching.
