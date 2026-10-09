# Database Schema

Complete schema reference for both storage backends.

---

## Schema Version Policy

The Neo4j schema is initialised by `cargo run -- init-db` (`Neo4jClient::init_schema()`). There is no formal migration system — constraints and indexes use `CREATE IF NOT EXISTS`, so `init-db` is safe to re-run.

When you add a new node type or index:
1. Add the `CREATE CONSTRAINT` / `CREATE INDEX` statement to `init_schema()` in `repository/client.rs`.
2. Re-run `cargo run -- init-db` against the running Neo4j instance.
3. Update this file under the appropriate section.
4. If the change removes a node type that existing deployments may have, document the manual Cypher cleanup here.

There is no `schema_version` node — the schema is defined entirely by the Rust source. Treat `init_schema()` as the migration script.

---

## DuckDB — Telemetry & Operational Data

File path configured via `TELEMETRY_DB_PATH` env var. Schema is auto-initialised by `TelemetryClient::new()`.

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

### Timestamps: one representation, always

**Every temporal property is a native Neo4j `DATETIME` stored in UTC.** Never write one as a string.

Cypher compares a datetime to a string as **null**, not `false` — the predicate is neither true nor false, so `WHERE` drops the row and the query succeeds with zero results. There is no error and no warning, and zero rows is indistinguishable from "there is no such data". The graph carried both representations until 2026-08-14, which silently broke `find_similar_tasks` (returned 0 instead of 815, disabling task dedup) and made a chat session report an empty memory on a day with 504 new notes.

Practical rules:

- **Writing:** wrap the parameter — `SET t.updated_at = datetime($now)`, `CREATE (t:Task {created_at: datetime($ts)})`. For an optional value that may arrive as `''`, use `CASE WHEN $x = '' THEN null ELSE datetime($x) END`; `datetime('')` throws.
- **Comparing:** compare against a temporal, never a literal — `WHERE n.created_at >= datetime('2026-08-13')`, `WHERE j.created_at >= datetime() - duration({days: 1})`. Do not wrap the property (`datetime(n.created_at)` is a redundant no-op).
- **Reading into a `String` model field:** project `toString(prop) AS prop`. Reads that pull a whole node (`RETURN j`) have no such hook and must go through `repository::node_ts`, which accepts either representation and normalises to `…Z`.
- **Model structs stay `String`** (ISO-8601). The REST and MCP payloads are unchanged by this; only the storage type moved.

Enforced by `Neo4jClient::string_timestamp_violations()` — warned at startup from `main.rs`, asserted empty by the `no_string_timestamps` integration test. Migration: `scripts/migrate_timestamps_to_datetime.cypher`.

### Node Types

#### `Task`

High-level goals tracked by the autonomous scheduler.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID |
| `goal` | String | Human-readable goal statement |
| `context` | String? | Optional extra context |
| `status` | String | `created` \| `in_progress` \| `completed` \| `failed` \| `blocked` |
| `created_at` | DateTime | Native Neo4j `DATETIME`, UTC |
| `updated_at` | DateTime | Native Neo4j `DATETIME`, UTC |

#### `Note`

Long-term memory entries with hybrid vector+BM25 retrieval and spaced-repetition scheduling.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID |
| `content` | String | The note body |
| `note_type` | String | `semantic` \| `episodic` \| `reflection` \| `consolidated` \| `outcome` \| `inference` |
| `embedding` | Float[] | 1024-dim bge-m3 vector (nullable — absent if no embed model configured). Measured 2026-10-08: 223 notes hold an **empty** list and 3 a 4096-dim one; neither is searchable, so test `size(n.embedding) = 1024`, not `IS NOT NULL` |
| `access_count` | Integer | Number of times retrieved |
| `last_accessed_at` | DateTime | Native Neo4j `DATETIME`, UTC |
| `next_review_at` | DateTime | Native Neo4j `DATETIME`, UTC — spaced repetition due date |
| `review_interval_days` | Float? | Current review interval |
| `source_context` | String? | Where/how the note was created |
| `event_at` | DateTime | Native Neo4j `DATETIME`, UTC — when the described event occurred (episodic notes) |
| `created_at` | DateTime | Native Neo4j `DATETIME`, UTC |
| `topics_version` | String? | The topic-definitions hash it was classified against (`services/topics.rs`). Equal to the current version and no `IN_TOPIC` ⇒ assessed, fits no topic; anything else ⇒ waiting |
| `topics_at` | DateTime? | Native `DATETIME`, UTC — when it was last classified |
| `topics_failed` | Integer? | Classification failures; at 3 the assigner leaves the note alone (counted as `failed`) |
| `provenance` | String | Who chose the words: `user_input` (a person: the HBI, an outside MCP client's call), `synthesis_inference` (a model: every queue job, chat turn and sub-agent), `system_record` (the brain's own code: scheduler dispatches, claim evidence, code-version notes), `core_training`. Decided by the caller (`knowledge::effective_provenance`): the brain's own work cannot claim `user_input`. Shown to the model by recall |
| `provenance_was` / `provenance_relabelled_at` | String? / DateTime? | Set by `scripts/fix_note_provenance.cypher` (2026-10-08) on the 12,269 notes it relabelled: the old value (`none` when there was none) |

Claim notes (`note_type: 'claim'`, `services/claims.rs`) also carry:

| Property | Type | Description |
|----------|------|-------------|
| `claim_status` / `claim_kind` / `asserted_by` / `asserted_at` | | Epistemic status, see `subsystem-rules.md` § Epistemics |
| `about` | String[] | The named things the claim is about, as the claim names them. Extraction and the rewrite check that each is named in the claim (`standalone_problems`) |
| `source_url` / `source_title` / `source_outlet` | String? | The article or video its `ASSERTED_IN` source records (`source_reference`), on the claim so a reader needn't follow the edge. The topic classifier reads the title with the claim |
| `standalone_checked_at` | DateTime? | When the stand-alone check last ran on it (new claims: at extraction). Unset ⇒ `rewrite-claims` will check it |
| `rewritten_at` / `content_v1` | DateTime? / String? | Set when `rewrite-claims` replaced its text: the original stays in `content_v1` |
| `ambiguous` / `ambiguous_reason` | Boolean? / String? | It could not be restated so it stands alone; the reason names what is missing. Its text is unchanged |

#### `Topic`

A domain the brain tracks, one per `topics/*.yaml` (`TOPICS_DIR`), synced by `init-db`; a topic removed from YAML is deleted with its edges. The local model classifies notes against the descriptions.

| Property | Type | Description |
|----------|------|-------------|
| `name` | String | Slug, unique (`us-politics`) |
| `label` | String | Display name |
| `description` | String | What belongs here — also the classifier's definition |
| `schedules` | String[] | ScheduledTask names that watch it |
| `source_lists` | String[] | SourceLists its searches use |
| `version` | String | Hash of every topic's name + description and the skip scope |
| `created_at` / `synced_at` | DateTime | Native `DATETIME`, UTC |

#### `ArchSnapshot`

What the brain was made of at one moment (`services/architecture.rs`), taken by `snapshot_architecture`: weekly on `brain-api-sync`, once at the first start, or by hand. The newest 104 are kept. The JSON blobs are strings because Neo4j has no map-of-lists property.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID, unique |
| `taken_at` | DateTime | Native `DATETIME`, UTC |
| `trigger` | String | `schedule` \| `manual` \| `first` |
| `version` | String | The commit the brain was running (`BrainVersion.sha`), or empty |
| `inventory` | String (JSON) | Sorted name sets: `skills` (skill → tools), `routes`, `agents`, `chains`, `schedules`, `mcp_servers`, `dynamic_tools`, `providers`, `components` |
| `counts` | String (JSON) | Category → count |
| `diff` | String (JSON) | Category → `{added, removed}` against `previous_id`; only categories that changed |
| `summary` | String | The diff in one line, written by code, not a model |
| `previous_id` | String | The snapshot it was compared with; empty for the first |

#### `Entity`

Named entities extracted from note content (8 types: `person`, `organisation`, `location`, `technology`, `tool`, `concept`, `url`, `date`). One real thing is one node: extraction resolves each name through `services/entity_resolution.rs` (alias → exact name → normalised `key`) before creating anything. See `subsystem-rules.md` § Entity resolution.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID |
| `name` | String | Lowercased, unique |
| `key` | String | Normalised comparison key: lowercase, letters/digits only, a leading article dropped, a leading title dropped before a full name (`u.s. senate` → `ussenate`). Two entities with one key are one thing. `~<name>` on a `distinct` entity |
| `entity_type` | String | The most common type across its mentions (`MENTIONS.type`), or the seed's type when `type_source: 'seed'` |
| `type_source` | String? | `seed` for a curated canonical from `entities/*.yaml` (name and type fixed; never pruned) |
| `distinct` | Boolean? | `true` once a person split it back out (unmerge): never folded automatically again |
| `created_at` | DateTime | Native Neo4j `DATETIME`, UTC |

#### `EntityAlias`

Another name for an entity: curated in `entities/*.yaml`, or left behind by a merge. Write-time extraction resolves through it, and an unmerge restores from it.

| Property | Type | Description |
|----------|------|-------------|
| `name` | String | Lowercased, unique |
| `key` | String | As `Entity.key` |
| `case_sensitive` | Boolean | Resolves only when the note's text has it in capitals (`US`, not the pronoun `us`) |
| `method` | String | `seed` · `normalise` (same key) · `proposal` (a person merged it in the HBI) · `restored` |
| `merged_from_id` | String? | The merged-away entity's id (what an unmerge recreates; the HBI resolves old links through it) |
| `entity_type` | String? | The merged-away entity's type |
| `entity_created_at` | DateTime? | The merged-away entity's `created_at` |
| `merged_at` | DateTime? | Native `DATETIME`, UTC |

#### `Procedure`

Named multi-step workflows stored as JSON step arrays and executed by `ProcedureSkill`.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID |
| `name` | String | Unique name |
| `description` | String? | Human-readable summary |
| `steps` | String | JSON array of procedure steps |
| `created_at` | DateTime | Native Neo4j `DATETIME`, UTC |

#### `WorkingMemory`

Session-scoped scratchpad entries. Each HTTP session or stdio invocation has its own session_id.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID, unique |
| `session_id` | String | Groups entries by session |
| `content` | String | Entry body |
| `role` | String | `user` \| `assistant` \| `system` |
| `turn_index` | Integer | Ordering within the session |
| `created_at` | DateTime | Native Neo4j `DATETIME`, UTC |

#### `DynamicTool`

Runtime-defined MCP tools created and dispatched by `DynamicSkill`.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID |
| `name` | String | Unique tool name (registered at runtime) |
| `description` | String | Shown in `tools/list` |
| `input_schema` | String | JSON schema for tool arguments |
| `created_at` | DateTime | Native Neo4j `DATETIME`, UTC |

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
| `created_at` | DateTime | Native Neo4j `DATETIME`, UTC |
| `updated_at` | DateTime | Native Neo4j `DATETIME`, UTC |
| `started_at` | DateTime | Native Neo4j `DATETIME`, UTC |
| `completed_at` | DateTime | Native Neo4j `DATETIME`, UTC |

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
| `last_run_at` | DateTime | Native Neo4j `DATETIME`, UTC — timestamp of last successful dispatch |
| `next_run_at` | DateTime | Native Neo4j `DATETIME`, UTC — next due time |
| `created_at` | DateTime | Native Neo4j `DATETIME`, UTC |
| `updated_at` | DateTime | Native Neo4j `DATETIME`, UTC |

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

#### `SchedulerChain`

Stored job-chain templates matched by the scheduler's `goal_to_steps()` query. Seeded from `chains/*.yaml` by `init-db` (and auto-seeded on first scheduler tick if none exist). Supports `{{task_id}}`, `{{goal}}`, `{{goal_topic}}`, `{{date}}`, `{{now}}`, `{{weekday}}`, `{{file_slug}}` template substitutions in step arguments. The three time vars are **local** wall-clock (`services/clock.rs`), not UTC — stored node timestamps remain UTC.

| Property | Type | Description |
|----------|------|-------------|
| `name` | String | Unique chain name; MERGE key |
| `pattern` | String | Primary CONTAINS keyword matched against task goal |
| `patterns` | String? | JSON array of additional OR-matched keywords |
| `priority` | Integer | Lower = matched first; default chain uses 9999 |
| `steps` | String | JSON-encoded `Vec<ChainStep>` template |
| `description` | String? | What this chain does |
| `no_evaluator` | Boolean | If true, skip evaluator step even when task has `success_criteria` |
| `no_adversarial` | Boolean | If true, skip adversarial pre-flight even when task has `success_criteria` |
| `evaluation_rubric` | String? | Overrides `Task.success_criteria` as the evaluator goal text |

---

### Relationships

| Relationship | Pattern | Properties | Description |
|-------------|---------|------------|-------------|
| `RELATES_TO` | `(Note)→(Note)` | `similarity: float` | Auto-created when cosine similarity ≥ 0.75 |
| `SUMMARIZED_BY` | `(Note)→(Note)` | — | Source note pointing to its consolidated summary |
| `REFLECTS_ON` | `(Note)→(Task)` | — | Reflection or outcome note linked to its task |
| `PART_OF` | `(Note)→(Note)` | — | Semantic chunk pointing to its parent note |
| `MENTIONS` | `(Note)→(Entity)` | `count: int`, `surface: string` (as written), `type: string` (as extracted), `via: string?` (the name it resolved or was merged through), `merged_from: string?`, `absorbed: ["<entity id>:<count>"]?` (counts folded in from a merged entity the note also mentioned) | Entity mention extracted from note content. `via`/`absorbed` are what an unmerge reverses |
| `ALIAS_OF` | `(EntityAlias)→(Entity)` | — | The alias's canonical entity |
| `IN_TOPIC` | `(Note)→(Topic)` | `rank: int` (1 = main topic, 2 = as central), `method: 'llm'`, `assigned_at: datetime` | Written by the topic assigner; replaced whenever the note is classified again |
| `NOT_SAME` | `(Entity)→(Entity)` | `decided_at: datetime` | A person answered a merge proposal "different things": never proposed again |
| `RELATED_ENTITY` | `(Entity)→(Entity)` | `decided_at: datetime` | Related but not the same (`trump administration` → `donald trump`): never proposed as a merge |
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
| `entity_alias_name` | `EntityAlias` | `name` | UNIQUE |
| `topic_name` | `Topic` | `name` | UNIQUE |
| `arch_snapshot_id` | `ArchSnapshot` | `id` | UNIQUE |
| `dynamic_tool_name` | `DynamicTool` | `name` | UNIQUE |
| `agent_job_id` | `AgentJob` | `id` | UNIQUE |
| `todo_id` | `Todo` | `id` | UNIQUE |
| `scheduler_chain_id` | `SchedulerChain` | `id` | UNIQUE |
| `note_id` | `Note` | `id` | UNIQUE |
| `task_id` | `Task` | `id` | UNIQUE |

### Indexes

| Name | Node | Property | Type | Notes |
|------|------|----------|------|-------|
| `note_embeddings` | `Note` | `embedding` | VECTOR | 1024-dim cosine similarity (bge-m3) |
| `task_status` | `Task` | `status` | RANGE | Scheduler polls `status='created'` every tick |
| `task_created` | `Task` | `created_at` | RANGE | Recency ordering in `list_tasks` |
| `note_content_fulltext` | `Note` | `content` | FULLTEXT | BM25 hybrid search |
| `note_next_review` | `Note` | `next_review_at` | RANGE | Spaced repetition query |
| `working_memory_session` | `WorkingMemory` | `session_id` | RANGE | Session context lookup |
| `entity_type_idx` | `Entity` | `entity_type` | RANGE | Filter by entity type |
| `entity_key_idx` | `Entity` | `key` | RANGE | Write-time resolution by normalised key |
| `entity_id_idx` | `Entity` | `id` | RANGE | HBI drawer, merges and candidates look entities up by id |
| `entity_alias_key_idx` | `EntityAlias` | `key` | RANGE | Write-time resolution by normalised key |
| `arch_snapshot_taken_idx` | `ArchSnapshot` | `taken_at` | RANGE | Newest snapshot, history |
| `dynamic_tool_idx` | `DynamicTool` | `created_at` | RANGE | Recency ordering |
| `agent_job_status` | `AgentJob` | `status` | RANGE | Queue coordinator polling |
| `agent_job_priority` | `AgentJob` | `priority` | RANGE | Priority-ordered dispatch |
| `agent_job_created` | `AgentJob` | `created_at` | RANGE | FIFO ordering within priority |
| `agent_job_parent` | `AgentJob` | `parent_job_id` | RANGE | Chain unpark / cancel-children lookups |
