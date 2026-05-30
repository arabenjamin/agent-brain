# Architecture Notes

## Skill Registry

| Skill | Tools | Conditional on | Notes |
|-------|-------|----------------|-------|
| KnowledgeSkill | 7 | Neo4j | store_note, search_notes, reason, consolidate_memories, prune_old_notes, synthesize_knowledge, adversarial_plan_review |
| TaskSkill | 6 | Neo4j | create_task, update_task, decompose_goal, record_outcome, reflect_on_work, store_note |
| AgentSkill | 5 | QueueService | enqueue_jobs, manage_job, dead_letter, set_worker_config, update_job_progress |
| CodebaseSkill | 12+2 | CODEBASE_DIR | read/list/search/tree/git/proposals/write_codebase_doc (+workspace tools if WORKSPACE_DIR set) |
| GitSkill | 6 | CODEBASE_DIR | git_status, git_create_branch, git_commit, git_push, git_create_pr, write_codebase_file |
| DynamicSkill | 3 | Neo4j | manage_dynamic_tool, store_procedure, execute_procedure |
| HttpSkill | 2 | — | http_request, define_api_context |
| QuerySkill | 2 | Neo4j + DuckDB | neo4j_query, duckdb_query |
| ModelSkill | 2 | — | use_model, reload_models |
| SchedulerSkill | 4 | Neo4j + QueueService | scheduler_control, run_scheduler_tick, manage_chain, manage_scheduled_task |
| WorkingMemorySkill | 3 | Neo4j | summarise_session, notify_user, push_context |
| WsSkill | 4 | — | ws_connect, ws_send, ws_receive, ws_close |
| SleepSkill | 2 | DuckDB telemetry | digest_experiences, analyze_gaps |
| SearchSkill | 1 | — | search_web |
| ContextSkill | 1 | ContextBuilderService | context |
| ResourceSkill | 1 | — | resource |
| **Total static** | **61+** | | + N runtime tools from DynamicSkill |

## Skill Registration Pattern

In `build_skills()` (`brain_core.rs`): register to BOTH `tool_registry` (listing) AND `skills` vec (execution).

```rust
// Registry (for tools/list response)
registry.register_skill(Box::new(KnowledgeSkill::new(...)));

// Handler skills (for tools/call execution)
skills.push(Box::new(KnowledgeSkill::new(...)));
```

DynamicSkill is special: `clone_shared()` for registry, original for handler (shared `tools_map`).

## Critical Constructor Signatures

```rust
KnowledgeSkill::new(neo4j, llm_config)
QueueService::new(neo4j, tool_handler, session_manager: Option<Arc<SessionManager>>)
SchedulerService::new_with_context(neo4j, queue, context_builder)  // spawns background Tokio task
CodebaseSkill::new(codebase_dir, workspace_dir, proposals_dir, knowledge, neo4j)
```

## Initialization Order (build_skills)

1. `DynamicSkill::new()` + `load_from_neo4j()` — must be first (async await)
2. `QueueService::new()` + `recover()` — recovers queued/running jobs from Neo4j
3. `SchedulerService::new()` — must be created AFTER queue is ready
4. `ContextStore::with_neo4j()` + `load_all()` — pre-loads API contexts
5. Register all skills to registry + skills vec
6. `QueueService::spawn_coordinator()` — spawns job processing loop AFTER handler is set

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
| Ollama (local) | `ollama` | Default; `OLLAMA_URL` + `OLLAMA_MODEL` |
| Ollama Cloud | `ollama-cloud` | `OLLAMA_URL=https://ollama.com`; requires `OLLAMA_API_KEY` |
| Anthropic | `anthropic` | Requires `ANTHROPIC_API_KEY` |
| Gemini | `gemini` | Requires `GEMINI_API_KEY` |

Background/scheduled jobs always route to `OLLAMA_LOCAL_URL` + `OLLAMA_LOCAL_MODEL` regardless of the active provider (enforced via `USE_LOCAL_LLM` task-local in `queue.rs`).
Switch at runtime with `use_model` tool (`provider`: `"Ollama"` | `"OllamaCloud"` | `"Anthropic"` | `"Gemini"`).

## LlmConfig Key Details

- `base_url` field is `Option<String>`, not `String`
- Default Ollama model: `"qwen3.5:4b"` (not `"llama3"`)
- Tests: `assert_eq!(config.base_url.as_deref(), Some("http://..."))`

## Job Chain Lifecycle

```
  enqueue_chain(steps)
        │
        ▼
  ┌─────────────┐    ┌──────────────┐    ┌──────────────┐
  │  Job[0]     │    │  Job[1]      │    │  Job[2]      │
  │  queued     │    │  parked      │    │  parked      │
  │  (runs now) │    │  parent=J[0] │    │  parent=J[1] │
  └──────┬──────┘    └──────────────┘    └──────────────┘
         │
    coordinator picks up Job[0]
         │
         ├── success ──► unpark_children(J[0]) ──► Job[1] → queued
         │                                               │
         │                                          success ──► Job[2] → queued
         │                                          failure (retryable) ──► Job[2] stays parked
         │                                          dead (exhausted) ──► cancel_parked_children(J[1])
         │
         ├── retryable failure ──► Job[0] re-queued; Job[1]/Job[2] stay parked
         │
         └── dead (exhausted retries) ──► cancel_parked_children(J[0]) ──► Job[1],Job[2] cancelled
```

Result of each step is passed as `{{_prev}}` template variable into the next step's arguments.

**Evaluator step** (appended when `Task.success_criteria` is set):
- Calls `reflect_on_work` with prior output; parses `Score: N/5`
- Score < `min_score` (default 3.5) → marks task `failed`, creates retry task with critique in context
- Retry cap: 3 `"RETRY —"` occurrences in context → terminal failure, no re-queue

**Adversarial pre-flight step** (prepended when `Task.success_criteria` is set):
- Calls `adversarial_plan_review`; parses `overall_robustness`
- Robustness < `min_robustness` (default 2.5) → cancels downstream steps, re-queues with critique
- Abort cap: 3 `"ADVERSARIAL ABORT"` occurrences → terminal failure

## Per-Provider Job Semaphores

QueueService has 3 semaphores: `semaphore_ollama(3)`, `semaphore_anthropic(2)`, `semaphore_gemini(5)`.
Job's `provider_hint` field selects which semaphore to acquire.
Semaphores are resizable at runtime via `set_worker_config` — uses `Arc<RwLock<Arc<Semaphore>>>` to swap inner capacity atomically; in-flight jobs keep their old permit, new jobs pick up the replacement.
