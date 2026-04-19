# Architecture Notes

## Skill Registry

| Skill | Tools | Notes |
|-------|-------|-------|
| HttpSkill | 2 | Generic HTTP requests, ApiContext management |
| KnowledgeSkill | 6 | Notes, RAG, reasoning, consolidation |
| TaskSkill | 5 | Goal tracking, decomposition, outcomes, reflection |
| AgentSkill | 5 | Background job queue, chaining |
| QuerySkill | 2 | Generic Neo4j (Cypher) and DuckDB (SQL) primitives |
| ModelSkill | 2 | LLM registry, runtime provider switching |
| SchedulerSkill | 4 | Autonomous self-improvement loop |
| ContextSkill | 1 | Context profile management |
| WorkingMemorySkill | 2 | Session scratchpad, summarization |
| DynamicSkill | 3 | Runtime tool definition and procedures |
| CodebaseSkill | 7 | Self-analysis, file reading, git integration |
| WsSkill | 4 | WebSocket connections |
| ResourceSkill | 1 | Shared token/resource registry |
| SearchSkill | 1 | Web search (SerpApi/Brave/Google) |
| SleepSkill | 2 | Experience export, gap analysis |
| **Total** | **47** | |

## Skill Registration Pattern

In `build_skills()`: register to BOTH `tool_registry` (listing) AND `skills` vec (execution).

```rust
// Registry (for tools/list response)
registry.register_skill(Box::new(AdminSkill::new(...)));

// Handler skills (for tools/call execution)
skills.push(Box::new(AdminSkill::new(...)));
```

DynamicSkill is special: `clone_shared()` for registry, original for handler (shared tools_map).

## Critical Constructor Signatures

```rust
AdminSkill::new(neo4j, context_store, llm_config, snapshot_svc: Option<Arc<SnapshotService>>)
KnowledgeSkill::new(neo4j, llm_config)   // builds KnowledgeService with optional snapshot
QueueService::new(neo4j, tool_handler, session_manager: Option<Arc<SessionManager>>)
SchedulerService::new(neo4j, queue)      // spawns background Tokio task
```

## Initialization Order (build_skills)

1. `DynamicSkill::new()` + `load_from_neo4j()` — must be first (async await)
2. `QueueService::new()` + `recover()` — recovers queued/running jobs from Neo4j
3. `SchedulerService::new()` — must be created AFTER queue is ready
4. `ContextStore::with_neo4j()` + `load_all()` — pre-loads API contexts
5. Register all skills to registry + skills vec
6. `QueueService::spawn_coordinator()` — spawns job processing loop AFTER handler is set

## Self-Healing Flow

When `execute_http_request` encounters 4xx/5xx:
1. Pass request + error body + graph schema to LLM
2. LLM suggests corrections
3. Retry with corrected payload
4. On success: persist `HealingEvent` node with corrected schema
5. On failure: mark endpoint as `status='broken'`

## Scheduler Self-Improvement Loop

`SchedulerService::do_tick()` runs every `SCHEDULER_INTERVAL_SECS`:
1. List tasks with `status='created'`
2. Map each goal to a `ChainStep[]` via `goal_to_steps()`
3. `queue.enqueue_chain()` each chain
4. Mark tasks `in_progress`
5. `perception_scan()`: count failure outcomes per tool (7-day window); create "Analyze repeated failures" tasks when ≥3 failures. Trigger consolidation when ≥10 overdue spaced-rep notes or ≥50 episodic notes.

### `goal_to_steps()` Heuristic Map

| Keyword match | Chain produced |
|--------------|----------------|
| `document`, `current state` | search_notes → consolidate_memories |
| `prioriti`, `roadmap`, `plan` | search_notes → reason → store_note |
| `improve`, `execute` | search_notes → reason → reflect_on_work |
| `identify`, `opportunit` | reason → store_note |
| `consolidat` | consolidate_memories → prune_old_notes → update_task |
| `failure`, `root cause`, `debug` | search_notes → reason → store_note → reflect_on_work |
| `search web`, `look up`, `find … recent` | search_web → store_note |
| `learn`, `research`, `study`, `understand` | search_notes → reason → store_note |
| `review`, `analyz`, `source` | search_notes → reason |
| *(default)* | search_notes → reason → reflect_on_work |

All chains append `update_task(completed)` as the final step.

Auto-pauses after `error_budget` consecutive errors (default 5).

## Memory Consolidation (Corruption Prevention)

Fixed bugs (2026-03-01):
- **Prompt**: use `[Memory N]` labels, explicitly instruct "do NOT repeat labels in output"
- **Topic extraction**: auto-generated overdue/episodic goals use "recent experiences and knowledge" (not parsed keywords)
- **Spaced-rep reset**: after consolidation, set `next_review_at = now + 30 days` on all source notes
- **Auto-snapshot**: `KnowledgeService::consolidate_memories()` takes a `pre_consolidate` snapshot before LLM call (guarded by `AUTO_SNAPSHOT_BEFORE_CONSOLIDATION` env var)

## LLM Providers

Four providers, selected via `LLM_PROVIDER` env var:

| Provider | Env value | Notes |
|----------|-----------|-------|
| Ollama | `ollama` | Local; default `http://localhost:11434` |
| Anthropic | `anthropic` | Requires `ANTHROPIC_API_KEY` |
| Gemini | `gemini` | Requires `GEMINI_API_KEY` |
| vLLM / OpenAI-compat | `vllm` | Any OpenAI-compat server; default `http://localhost:8000` |

Implementation: `src/services/llm_providers/openai_compat.rs` — reusable for vLLM, LM Studio, Groq, Together AI, etc.
Switch at runtime with `use_model` tool (`provider`: `"VLlm"`).

## LlmConfig Key Details

- `base_url` field is `Option<String>`, not `String`
- Default Ollama model: `"qwen3.5:4b"` (not `"llama3"`)
- Tests: `assert_eq!(config.base_url.as_deref(), Some("http://..."))`

## SnapshotService

Snapshots are compressed JSON (`.json.gz` via `flate2`).
Location: `KNOWLEDGE_SNAPSHOT_DIR` (default `./snapshots`).
Embeddings are **excluded** from snapshots (use `backfill_endpoint_embeddings` after restore).
MERGE-based restore is idempotent — safe to run on non-empty graph.

## Job Chain Mechanics

- Steps 2..N stored as `parked` with `parent_job_id` → predecessor
- On parent `completed`: coordinator calls `unpark_children()` → promotes `parked` → `queued`
- On parent `dead` (exhausted retries): `cancel_parked_children()`
- On parent retryable failure: children stay `parked` (resume if parent retried + succeeds)

## Per-Provider Job Semaphores

QueueService has 3 semaphores: `semaphore_ollama(3)`, `semaphore_anthropic(2)`, `semaphore_gemini(5)`.
Job's `provider_hint` field selects which semaphore to acquire.
Semaphores are resizable at runtime via `set_worker_config` — uses `Arc<RwLock<Arc<Semaphore>>>` to swap inner capacity atomically; in-flight jobs keep their old permit, new jobs pick up the replacement.
