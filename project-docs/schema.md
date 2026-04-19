# Database Schema

Complete schema reference for both storage backends.

---

## DuckDB — Telemetry & Operational Data

File path configured via `TELEMETRY_DB_PATH` env var. Schema is auto-initialised by `TelemetryClient::new()`.

### `todos`

Personal todo items managed by `TodoSkill`.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| `id` | TEXT | PRIMARY KEY | UUID string |
| `title` | TEXT | NOT NULL | Short summary |
| `description` | TEXT | — | Extended detail (nullable) |
| `status` | TEXT | NOT NULL, DEFAULT `'pending'` | `pending` \| `in_progress` \| `done` |
| `priority` | INTEGER | NOT NULL, DEFAULT `2` | 0=urgent 1=high 2=normal 3=low |
| `tags` | TEXT | NOT NULL, DEFAULT `'[]'` | JSON array of strings |
| `due_at` | TEXT | — | RFC3339 deadline (nullable) |
| `created_at` | TEXT | NOT NULL | RFC3339 |
| `updated_at` | TEXT | NOT NULL | RFC3339 |

### `interactions`

Every agent turn — the "hippocampus" store for experience replay and fine-tuning export.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| `id` | UUID | PRIMARY KEY | — |
| `timestamp` | TIMESTAMPTZ | NOT NULL | Wall-clock time of the interaction |
| `prompt` | TEXT | NOT NULL | User/system prompt |
| `response` | TEXT | — | Agent response text |
| `tools_used` | JSON | — | Array of tool names invoked |
| `success` | BOOLEAN | — | Whether the interaction succeeded |
| `feedback_score` | INTEGER | — | Optional human rating |
| `feedback_text` | TEXT | — | Optional human annotation |
| `latency_ms` | INTEGER | — | End-to-end latency |
| `model_used` | TEXT | — | Model name used for this turn |

### `knowledge_gaps`

Queries the agent couldn't answer — used by `SleepSkill` to surface topics for self-improvement.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| `id` | UUID | PRIMARY KEY | — |
| `timestamp` | TIMESTAMPTZ | NOT NULL | When the gap was detected |
| `query` | TEXT | NOT NULL | The unanswerable query |
| `context` | TEXT | — | Surrounding context |
| `gap_type` | TEXT | — | E.g. `tool_failure`, `missing_knowledge` |

### `model_registry`

Canonical LLM catalog synced from `models.yaml` at startup. Source of truth for `ModelSkill`.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| `name` | TEXT | PRIMARY KEY | Logical name, e.g. `"sonnet-3-7"` |
| `provider` | TEXT | NOT NULL | `Ollama` \| `Anthropic` \| `Gemini` \| `VLlm` |
| `model` | TEXT | NOT NULL | Provider model ID string |
| `context_window` | INTEGER | NOT NULL | Max context tokens |
| `cost_input` | DOUBLE | NOT NULL | Cost per 1k input tokens (USD) |
| `cost_output` | DOUBLE | NOT NULL | Cost per 1k output tokens (USD) |
| `capabilities` | TEXT | NOT NULL | JSON array of capability strings |
| `system_prompt` | TEXT | — | Optional default system prompt |
| `temperature` | DOUBLE | — | Optional default temperature |
| `max_tokens` | INTEGER | — | Optional default max output tokens |
| `timeout_secs` | INTEGER | — | Optional request timeout |
| `loaded_at` | TIMESTAMPTZ | DEFAULT `current_timestamp` | Last sync time |

### `model_usage`

Per-invocation telemetry for every LLM call. Aggregated by `get_model_stats`.

| Column | Type | Constraints | Description |
|--------|------|-------------|-------------|
| `id` | TEXT | PRIMARY KEY | UUID string |
| `model_name` | TEXT | NOT NULL | FK → `model_registry.name` (logical) |
| `tool_name` | TEXT | — | MCP tool that triggered the call |
| `success` | BOOLEAN | — | Whether the call returned without error |
| `duration_ms` | INTEGER | — | Wall-clock latency |
| `tokens_in` | INTEGER | — | Prompt tokens consumed |
| `tokens_out` | INTEGER | — | Completion tokens generated |
| `cost` | DOUBLE | — | Computed cost in USD (may be NULL) |
| `created_at` | TIMESTAMPTZ | DEFAULT `current_timestamp` | — |

---

## Neo4j — Knowledge Graph

Schema initialised by `Neo4jClient::init_schema()` (run via `cargo run -- init-db`).

### Node Types

#### `Task`

High-level goals tracked by the autonomous scheduler.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID |
| `goal` | String | Human-readable goal statement |
| `context` | String? | Optional extra context |
| `status` | String | `created` \| `in_progress` \| `completed` \| `failed` \| `blocked` |
| `created_at` | String | RFC3339 |
| `updated_at` | String | RFC3339 |

#### `Note`

Long-term memory entries with hybrid vector+BM25 retrieval and spaced-repetition scheduling.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID |
| `content` | String | The note body |
| `note_type` | String | `semantic` \| `episodic` \| `reflection` \| `consolidated` \| `outcome` \| `inference` |
| `embedding` | Float[] | 1024-dim bge-m3 vector (nullable — absent if no embed model configured) |
| `access_count` | Integer | Number of times retrieved |
| `last_accessed_at` | String? | RFC3339 |
| `next_review_at` | String? | RFC3339 — spaced repetition due date |
| `review_interval_days` | Float? | Current review interval |
| `source_context` | String? | Where/how the note was created |
| `event_at` | String? | RFC3339 — when the described event occurred (episodic notes) |
| `created_at` | String | RFC3339 |

#### `Entity`

Named entities extracted from note content (7 types: `person`, `tool`, `technology`, `concept`, `organisation`, `url`, `date`).

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID |
| `name` | String | Lowercased, unique |
| `entity_type` | String | One of the 7 types above |
| `created_at` | String | RFC3339 |

#### `Procedure`

Named multi-step workflows stored as JSON step arrays and executed by `ProcedureSkill`.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID |
| `name` | String | Unique name |
| `description` | String? | Human-readable summary |
| `steps` | String | JSON array of procedure steps |
| `created_at` | String | RFC3339 |

#### `WorkingMemory`

Session-scoped scratchpad entries. Each HTTP session or stdio invocation has its own session_id.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID, unique |
| `session_id` | String | Groups entries by session |
| `content` | String | Entry body |
| `role` | String | `user` \| `assistant` \| `system` |
| `turn_index` | Integer | Ordering within the session |
| `created_at` | String | RFC3339 |

#### `DynamicTool`

Runtime-defined MCP tools created and dispatched by `DynamicSkill`.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID |
| `name` | String | Unique tool name (registered at runtime) |
| `description` | String | Shown in `tools/list` |
| `input_schema` | String | JSON schema for tool arguments |
| `created_at` | String | RFC3339 |

#### `AgentJob`

Background job records executed by `QueueService`. Supports chaining, retry, and priority queuing.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID, unique |
| `tool_name` | String | MCP tool to invoke |
| `args_json` | String | JSON-encoded tool arguments |
| `priority` | Integer | 0=critical 1=high 2=normal 3=low |
| `status` | String | `queued` \| `running` \| `completed` \| `failed` \| `dead` \| `parked` \| `cancelled` |
| `attempt_count` | Integer | Number of execution attempts so far |
| `max_attempts` | Integer | Max retries before marking `dead` |
| `result_json` | String? | JSON-encoded tool result on success |
| `error` | String? | Last error message on failure |
| `session_id` | String? | Associated HTTP session (for SSE push notifications) |
| `parent_job_id` | String? | Predecessor job ID (used for chaining — parked until parent succeeds) |
| `provider_hint` | String? | `"ollama"` \| `"anthropic"` \| `"gemini"` — selects concurrency semaphore |
| `created_at` | String | RFC3339 |
| `updated_at` | String | RFC3339 |
| `started_at` | String? | RFC3339 |
| `completed_at` | String? | RFC3339 |

#### `ScheduledTask`

Recurring job definitions managed by `SchedulerSkill`. The scheduler dispatches due tasks on each tick.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID, unique |
| `name` | String | Unique human-readable name; used as the spawned `Task.goal` |
| `description` | String? | Optional display description |
| `enabled` | Boolean | Whether the scheduler will dispatch this task |
| `interval_seconds` | Integer | Recurrence period in seconds (e.g. 86400 = daily) |
| `steps` | String | JSON-encoded `Vec<ChainStep>` — the job chain to enqueue |
| `last_run_at` | String? | RFC3339 — timestamp of last successful dispatch |
| `next_run_at` | String | RFC3339 — next due time |
| `created_at` | String | RFC3339 |
| `updated_at` | String | RFC3339 |

#### `ApiContext`

HTTP API connection profiles used by `HttpSkill`. Seeded at startup from code; auth stored as an env var name only (never the value).

| Property | Type | Description |
|----------|------|-------------|
| `name` | String | Unique context name, e.g. `"github"` |
| `base_url` | String | Root URL for the API |
| `auth_scheme` | String? | `"Bearer"` \| `"Basic"` \| `"ApiKey"` |
| `auth_param` | String? | Header/query param name for the credential |
| `auth_env_var` | String? | Env var name that holds the secret value |
| `default_headers` | String? | JSON object of default request headers |
| `description` | String? | Human-readable summary |

#### `SchedulerChain` *(planned)*

Stored job-chain templates matched by the scheduler's `goal_to_steps()` query. Supports `{{task_id}}`, `{{goal}}`, `{{date}}` substitutions.

| Property | Type | Description |
|----------|------|-------------|
| `pattern` | String | Keyword or regex matched against task goal |
| `priority` | Integer | Default priority for the enqueued chain |
| `steps` | String | JSON-encoded `Vec<ChainStep>` template |
| `description` | String? | What this chain does |

---

### Relationships

| Relationship | Pattern | Properties | Description |
|-------------|---------|------------|-------------|
| `RELATES_TO` | `(Note)→(Note)` | `similarity: float` | Auto-created when cosine similarity ≥ 0.75 |
| `SUMMARIZED_BY` | `(Note)→(Note)` | — | Source note pointing to its consolidated summary |
| `REFLECTS_ON` | `(Note)→(Task)` | — | Reflection or outcome note linked to its task |
| `PART_OF` | `(Note)→(Note)` | — | Semantic chunk pointing to its parent note |
| `MENTIONS` | `(Note)→(Entity)` | `count: int` | Entity mention extracted from note content |
| `DERIVED_FROM` | `(Note{inference})→(Note)` | — | Inference note citing its source notes |
| `SUBTASK_OF` | `(Task)→(Task)` | — | Sub-task created by `decompose_goal` |
| `DEPENDS_ON` | `(Task)→(Task)` | — | Task ordering dependency |
| `USES` | `(DynamicTool)→(Procedure)` | — | Dynamic tool backed by a named procedure |

---

### Constraints

| Name | Node | Property | Type |
|------|------|----------|------|
| `scheduled_task_id` | `ScheduledTask` | `id` | UNIQUE |
| `scheduled_task_name` | `ScheduledTask` | `name` | UNIQUE |
| `procedure_id` | `Procedure` | `id` | UNIQUE |
| `working_memory_id` | `WorkingMemory` | `id` | UNIQUE |
| `entity_name` | `Entity` | `name` | UNIQUE |
| `dynamic_tool_name` | `DynamicTool` | `name` | UNIQUE |
| `agent_job_id` | `AgentJob` | `id` | UNIQUE |

### Indexes

| Name | Node | Property | Type | Notes |
|------|------|----------|------|-------|
| `note_embeddings` | `Note` | `embedding` | VECTOR | 1024-dim cosine similarity (bge-m3) |
| `note_content_fulltext` | `Note` | `content` | FULLTEXT | BM25 hybrid search |
| `note_next_review` | `Note` | `next_review_at` | RANGE | Spaced repetition query |
| `working_memory_session` | `WorkingMemory` | `session_id` | RANGE | Session context lookup |
| `entity_type_idx` | `Entity` | `entity_type` | RANGE | Filter by entity type |
| `dynamic_tool_idx` | `DynamicTool` | `created_at` | RANGE | Recency ordering |
| `agent_job_status` | `AgentJob` | `status` | RANGE | Queue coordinator polling |
| `agent_job_priority` | `AgentJob` | `priority` | RANGE | Priority-ordered dispatch |
| `agent_job_created` | `AgentJob` | `created_at` | RANGE | FIFO ordering within priority |
