# Brain Status

**Build:** passing — 214 unit tests, 0 failures
**Tool count:** 64 static registered + N runtime (DynamicSkill)
**Last updated:** 2026-02-27

---

## Architecture Overview

| Layer | Technology | Status |
|-------|-----------|--------|
| Protocol | MCP (JSON-RPC 2.0) via stdio + HTTP/SSE | Live |
| Graph DB | Neo4j via `neo4rs` | Live |
| Vector search | Ollama embeddings (bge-m3, 1024-dim) + BM25 hybrid RRF | Live |
| LLM | Ollama (local) | Live |
| Cloud LLM | Anthropic / Gemini | Live |
| Job queue | Priority BinaryHeap + Neo4j persistence + Tokio coordinator | Live |
| Secret store | Local AES-256-GCM / HashiCorp Vault / AWS Secrets Manager | Live |
| Telemetry | DuckDB (`brain_logs.db`) | Live |

---

## Skill Registry (65 tools static + N runtime)

| Skill | File | Tools | Notes |
|-------|------|-------|-------|
| ApiSkill | `src/skills/api.rs` | 14 | Core OpenAPI ingestion, query, execute, heal, export |
| KnowledgeSkill | `src/skills/knowledge.rs` | 10 | RAG, reasoning, audit, explain, spaced-repetition |
| TaskSkill | `src/skills/task.rs` | 6 | Goal tracking, decomposition, outcomes, reflection |
| AgentSkill | `src/skills/agent.rs` | 8 | Background job queue + sequential chaining |
| AdminSkill | `src/skills/admin.rs` | 5 | Graph cleanup: delete API, purge duplicates/orphans, reset, backfill embeddings |
| ModelSkill | `src/skills/model.rs` | 5 | Model registry + intelligent selection |
| SchedulerSkill | `src/skills/scheduler.rs` | 5 | Autonomous background scheduler with configurable tick interval |
| DynamicSkill | `src/skills/dynamic.rs` | 4 + N | Runtime tool definition, hot-registration |
| WorkingMemorySkill | `src/skills/working_memory.rs` | 3 | Session scratchpad, LLM summarisation |
| ProcedureSkill | `src/skills/procedure.rs` | 2 | Stored multi-step workflows |
| SleepSkill | `src/skills/sleep.rs` | 2 | Training data export (`digest_experiences`), knowledge gap analysis |
| SearchSkill | `src/skills/search.rs` | 1 | Web search (SerpApi / Brave / Google) |

---

## What Was Built (Recent Sessions)

### Phase 1 — Brain Self-Improvement Roadmap (29 → 40 tools)

**Phase 1 — Task Lifecycle (+4)**
- `decompose_goal` — LLM breaks a task into ordered sub-tasks, creates `SUBTASK_OF` edges
- `update_task` — sets task status + optional progress note
- `list_tasks` — filtered task list with parent_id
- `record_outcome` — episodic outcome note linked to a task

**Phase 2 — Cognitive Layer (+3)**
- `reason` — RAG + LLM inference; stores `(:Note {note_type:'inference'})-[:DERIVED_FROM]->(:Note)`
- `audit_action` — checks a proposed action against stored principles via LLM
- `explain_reasoning` — narrates why a decision was made, citing source notes

**Phase 3 — Dynamic Tool Builder (+4)**
- `define_tool` — define a new MCP tool backed by a procedure pipeline; hot-registered
- `execute_procedure` — run a stored procedure with `{{input.field}}` template substitution
- `list_dynamic_tools` — list all runtime-defined tools
- `remove_dynamic_tool` — delete a dynamic tool and unregister it live

New infrastructure:
- `src/services/procedure_executor.rs` — template substitution engine for procedure steps
- `src/skills/dynamic.rs` — `DynamicSkill` with shared `Arc<RwLock<HashMap>>` between registry and handler instances
- `src/repository/task.rs` — `link_subtask`, `list_tasks`, `store_outcome_note`
- `src/repository/client.rs` — DynamicTool constraints + index

### Queue + Worker Infrastructure (40 → 47 tools)

Background job execution system — submit any MCP tool call as a durable, prioritised background job.

New files:
- `src/models/agent_job.rs` — `AgentJob`, `AgentJobStatus` (queued/running/completed/failed/dead/parked/cancelled), `PrioritizedJob` (BinaryHeap ordering)
- `src/repository/agent_job.rs` — Neo4j CRUD: create, get, list, started/completed/failed/dead, retry, stats
- `src/services/queue.rs` — `QueueService` + `WorkerConfig`
  - `BinaryHeap<PrioritizedJob>` — max-heap, priority 0-3, FIFO within same priority
  - `Arc<Semaphore>` — concurrency limit (default: 5 concurrent jobs)
  - `Arc<Notify>` — immediate wakeup on enqueue
  - 30-second periodic Neo4j poll for missed jobs
  - Startup recovery: resets crashed `running` → `queued`
  - Lazy cancellation via tombstone `HashSet`
  - Retry: resets `attempt_count`, re-enqueues; after `max_attempts` → Dead
- `src/skills/agent.rs` — `AgentSkill` (7 tools)

New tools:
- `enqueue_agent` — submit a tool call as a background job
- `queue_status` — stats: pending, running, per-status counts
- `get_job_result` — poll a job for status/result
- `cancel_job` — cancel a queued or running job
- `retry_job` — requeue a failed/dead/cancelled job
- `set_worker_config` — change concurrency, enable/pause, poll interval
- `drain_queue` — cancel all pending jobs

### Graph Cleanup + Job Chaining (54 → 59 tools)

**AdminSkill (+4 tools, `src/skills/admin.rs`)**
- `delete_api` — cascade-delete all graph nodes for one API (dry_run supported); evicts context cache
- `purge_duplicate_endpoints` — remove duplicate Endpoint nodes (same resource + path + method)
- `purge_orphaned_schemas` — delete Schema nodes with no Endpoint relationships
- `reset_graph` — wipe all API data (requires `confirm: true`); knowledge data preserved

New files:
- `src/repository/admin.rs` — `CleanupStats` struct + 6 methods on `Neo4jClient` (count/delete/purge/reset)
- `src/skills/admin.rs` — `AdminSkill` (Neo4j + ContextStore)

**Job Chaining — `enqueue_chain` (+1 tool in `AgentSkill`, now 8 tools)**
- Sequential chain: step 1 is queued immediately; steps 2..N stored as `parked` (waiting for predecessor)
- On job completion: coordinator auto-promotes parked children to `queued` and pushes onto heap
- On job death (exhausted retries): parked children are cancelled
- On retryable failure: children stay parked — they run if the job is retried and succeeds

New repository methods in `agent_job.rs`:
- `create_agent_job_parked` — creates job with `status: 'parked'`
- `unpark_children(parent_id)` — promotes parked children to queued, returns `Vec<AgentJob>`
- `cancel_parked_children(parent_id)` — cancels parked children, returns count

New service:
- `ChainStep` struct in `queue.rs` (tool_name, arguments, priority, max_attempts, provider_hint)
- `QueueService::enqueue_chain(steps, session_id)` — creates chain, pushes head onto heap
- `QueueService::unpark_and_enqueue_children(parent_id)` — helper called inside `execute_job`

---

## Where We Left Off

The queue is the **first phase of a larger subagent orchestration system**. The next two phases are:

### Phase 2 — Multi-Provider LLM Client ✓

Anthropic, Gemini, and Ollama providers implemented via `LlmProvider` trait.

- `src/services/llm_providers/mod.rs` — `LlmProvider` trait + `ProviderConfig`
- `src/services/llm_providers/ollama.rs` — Ollama HTTP client
- `src/services/llm_providers/anthropic.rs` — Anthropic Messages API client
- `src/services/llm_providers/gemini.rs` — Gemini generativeLanguage client
- `LlmClient` refactored as thin wrapper over `Arc<dyn LlmProvider>`
- `ModelSkill` (`list_models`, `use_model`) added for runtime provider switching

### Phase 3 — Model Registry + Intelligent Selection ✓

The brain now stores knowledge about each model's capabilities and cost, and selects the cheapest capable model automatically.

New files:
- `src/models/model_spec.rs` — `ModelSpec { id, name, provider, cost_per_1k_tokens_input, cost_per_1k_tokens_output, context_window, capabilities }`
- `src/repository/model_spec.rs` — Neo4j CRUD (upsert by name, usage stats from AgentJob)
- `src/services/model_selector.rs` — capability-match → cheapest-first selection algorithm
- `src/skills/model.rs` — 5 tools: `list_models`, `use_model`, `register_model`, `select_model`, `get_model_stats`

QueueService updated:
- Replaced single `semaphore` with three per-provider semaphores: `ollama` (3), `anthropic` (2), `gemini` (5)
- Coordinator picks semaphore from `job.provider_hint` field
- `queue_status` response includes `per_provider` breakdown

---

## Known Issues / Backlog

- **`graph_query_endpoint` natural language matching** — `CONTAINS` queries fail on paraphrased queries; should use `endpoint_embeddings` vector index for semantic matching
- **DynamicSkill load on legacy McpServer** — sync `build_skills()` can't call `load_from_neo4j().await`; dynamic tools unavailable on stdio path after restart
- **Per-provider semaphores not resizable** — sizes fixed at startup; `set_worker_config` updates `WorkerConfig` fields only
