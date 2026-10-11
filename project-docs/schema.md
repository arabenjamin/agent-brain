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

Search failures written by `search_web` (engine errors, empty results). Query it with `duckdb_query`.

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
| `workflow` | String? | The `:Workflow` this task is a run of, by name; absent = the default workflow. Indexed (`task_workflow`). Runs created before 2026-10-10 were backfilled from their goal prefix or run-record context |
| `inputs_json` | String? | The run's inputs, a JSON object; steps read them as `{{inputs.<name>}}` |
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
| `topics_locked` | Boolean? | The user set its topics (`PUT /api/notes/{id}/topics`). The classifier skips it, and each new version is stamped on it |
| `entities_at` / `entities_found` | DateTime? / Int? | A claim's entity extraction by the idle-time claim loop (`spawn_claim_entity_backfill`), and how many entities it found (0 is stamped too, so a claim naming nothing is not retried) |
| `relations_at` / `relations_found` | DateTime? / Int? | A claim read by `extract_relations`, and how many relationships it proposed from it (`relations_at` is set even when none, so a claim is read once unless re-read by `note_id`) |
| `topics_at` | DateTime? | Native `DATETIME`, UTC — when it was last classified |
| `topics_failed` | Integer? | Classification failures; at 3 the assigner leaves the note alone (counted as `failed`) |
| `provenance` | String | Who chose the words: `user_input` (a person: the HBI, an outside MCP client's call), `synthesis_inference` (a model: every queue job, chat turn and sub-agent), `system_record` (the brain's own code: scheduler dispatches, claim evidence, code-version notes), `core_training`. Decided by the caller (`knowledge::effective_provenance`): the brain's own work cannot claim `user_input`. Shown to the model by recall |
| `produced_by_task` | String? | The Task that owned the job that wrote it (`__owner_task_id`; for a schedule, its run-record task). Set on every note written inside a job from 2026-10-09, including claims and notes written inside a `run_agent` loop |
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

A domain the brain tracks. The core bundle's `topics/*.yaml` seeds them, synced by `init-db`, and the HBI can create, edit and delete them (`POST /api/topics`, `PUT|DELETE /api/topics/{name}`, `POST /api/topics/{name}/reset`). An edited topic is `managed_by: 'runtime'`, and the sync leaves it alone. A YAML-owned topic removed from YAML is deleted with its edges. The local model classifies notes against the descriptions. The workflows that watch a topic are the ones whose `Workflow.topic` names it (until 2026-10-09 a `schedules:` list in the topic file; `init-db` removes the old property).

| Property | Type | Description |
|----------|------|-------------|
| `name` | String | Slug, unique (`us-politics`) |
| `label` | String | Display name |
| `description` | String | What belongs here — also the classifier's definition |
| `source_lists` | String[] | SourceLists its searches use |
| `version` | String | Hash of every topic's name + description and the skip scope |
| `managed_by` | String | `yaml` (its file applies) or `runtime` (edited in the HBI; its file, if any, no longer applies) |
| `assign` | String | `classifier` (the local model decides) or `rules` (only `members` rules and the user; left out of the classifier prompt) |
| `members` | String | JSON list of member rules `{note_type?, source_context?, content_contains?}` (`assign: rules`) |
| `reference` | String? | A page about the topic (an http(s) URL) set by the user or `reference:` in YAML. Not part of `version`: editing it classifies nothing again |
| `created_at` / `synced_at` / `updated_at` | DateTime | Native `DATETIME`, UTC (`updated_at`: last HBI edit) |

#### `RetiredTopic`

`{name, retired_at}`: a topic that `topics/*.yaml` declares and the user deleted in the HBI. While it exists, `init-db` does not recreate the topic. Reset (`POST /api/topics/{name}/reset`) and re-creating the topic both remove it.

#### `ArchSnapshot`

What the brain was made of at one moment (`services/architecture.rs`), taken by `snapshot_architecture`: weekly on `brain-api-sync`, once at the first start, or by hand. The newest 104 are kept. The JSON blobs are strings because Neo4j has no map-of-lists property.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID, unique |
| `taken_at` | DateTime | Native `DATETIME`, UTC |
| `trigger` | String | `schedule` \| `manual` \| `first` |
| `version` | String | The commit the brain was running (`BrainVersion.sha`), or empty |
| `inventory` | String (JSON) | Sorted name sets: `skills` (skill → tools), `routes`, `agents`, `chains`, `schedules`, `mcp_servers`, `declared_tools`, `providers`, `components` |
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
| `entity_type` | String | The most common type across its mentions (`MENTIONS.type`), or the seed's type when `type_source: 'seed'`, or the user's when `type_source: 'manual'` |
| `type_source` | String? | `seed` for a curated canonical from `entities/*.yaml` (name and type fixed; never pruned) · `manual` when the user set the type or answered a type question (the vote never changes it; "let the brain decide" removes it) |
| `kind` | String? | The fine kind (`country`, `municipality`, `news outlet`, … from `entities/kinds.yaml`), HBI plan Phase 19, `services/entity_kinds.rs` |
| `kind_source` | String? | `reference` (from the linked WikiItem's `kind`, only when its `kind_type` agrees with `entity_type`) · `manual` (set by the user; kept in snapshots) |
| `kind_qid` | String? | The WikiItem a `reference` kind came from |
| `type_question_qid` | String? | Set while the linked WikiItem's `kind_type` disagrees with `entity_type` and the user hasn't answered: the Feed's "entity types to check" |
| `type_kept_qids` | String[]? | Items the user answered "keep" against: they never ask again |
| `distinct` | Boolean? | `true` once a person split it back out (unmerge): never folded automatically again |
| `wiki_checked_at` | DateTime? | Last reference lookup (`services/wiki.rs`). A `none` is due again after 90 days |
| `wiki_result` | String? | `proposed` (a link was suggested or set) · `none` (nothing fit) |
| `wiki_candidates` | String[]? | Wikidata ids the last lookup saw, in order (the drawer's "choose another", with no new search) |
| `wiki_version` | String? | The pick prompt's version for the last lookup |
| `wiki_pending_at` | DateTime? | An on-demand lookup is queued (cleared when it runs; ignored after 15 minutes) |
| `created_at` | DateTime | Native Neo4j `DATETIME`, UTC |

#### `WikiItem`

A Wikidata item, shared by every entity linked to it (HBI plan Phase 18, `services/wiki.rs`). Written for every candidate a lookup sees, so a drawer can offer them without searching again.

| Property | Type | Description |
|----------|------|-------------|
| `qid` | String | Wikidata id (`Q1068951`), unique by MERGE |
| `label` / `description` | String | English, else Wikidata's language-neutral `mul` label (Q937, Albert Einstein, has no `en` label) |
| `kinds` | String[] | Labels of its instance-of (P31) values, at most 3 |
| `class_qids` | String[]? | Its instance-of (P31) ids, at most 6 (Phase 19; backfilled for older items when linked) |
| `kind` / `kind_type` | String? | The kind its classes walk up to, and the coarse type that kind implies (`services/entity_kinds.rs`); null when no root is reached |
| `wikimedia_page` | Boolean? | A disambiguation page, list or set index: a proposed link to it is withdrawn |
| `kind_version` | String? | Hash of the `kinds.yaml` the kind was derived with; a different hash re-derives it |
| `aliases` | String[] | English and `mul` aliases, at most 8 |
| `wiki_title` | String? | English Wikipedia article title; absent when it has none |
| `summary` / `summary_at` | String? / DateTime? | The article's first paragraph (REST `page/summary`), fetched when a drawer shows it, cached 30 days |
| `fetched_at` | DateTime | Last refreshed from Wikidata |

#### `WikiClass`

A Wikidata class on the way from an item's instance-of to a kind (HBI plan Phase 19, `services/entity_kinds.rs`), cached so later walks cost no requests. 1,544 were fetched for the first 226 linked items.

| Property | Type | Description |
|----------|------|-------------|
| `qid` | String | Wikidata id, unique by MERGE |
| `label` | String | English or `mul` label |
| `parents` | String[] | Its subclass-of (P279) ids; empty for a root or a missing class |
| `fetched_at` | DateTime | When it was fetched |

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

#### `Workflow`

A named job chain (CORE_AND_PLUGINS_PLAN Phase 2; replaced `ScheduledTask` and `SchedulerChain` on 2026-10-10). Runs on its schedule when it has one, and by name with inputs whenever something asks (`manage_workflow action=run`, `create_task workflow=…`, REST, the media and gap pipelines, the perception scan). Each run is a `:Task` with `workflow` set. Seeded from each loaded plugin bundle's `workflows/*.yaml` by `services/workflow_seeder.rs`. Template variables in steps: `{{task_id}}`, `{{goal}}`, `{{date}}` / `{{now}}` / `{{weekday}}` (local wall clock), `{{inputs.<name>}}`, `{{_prev}}`, `{{steps.<id>}}`. Rule: `dev_docs get workflows/model`.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID, unique (a migrated ScheduledTask kept its id) |
| `name` | String | Unique slug, the file stem; what callers run it by |
| `description` | String? | What it does |
| `enabled` | Boolean | A disabled workflow neither fires on its schedule nor runs by name (Run now still works) |
| `interval_seconds` | Integer? | Schedule period; null = runs only by name |
| `steps` | String | JSON-encoded `Vec<ChainStep>` (file `defaults:` and `data_class` already applied) |
| `inputs` | String | JSON array of `{name, description?, required}`; a run missing a required one is refused when created |
| `checks` | String[] | Opt-in: `evaluator` (score the result, re-queue below threshold), `critic` (adversarial review before it starts) |
| `evaluation_rubric` | String? | What the evaluator scores against; the run's `success_criteria` otherwise |
| `is_default` | Boolean | The one workflow a task with no `workflow` runs (free-text goals) |
| `last_run_at` / `next_run_at` | DateTime? | Native `DATETIME`, UTC; `next_run_at` null without a schedule |
| `created_at` / `updated_at` | DateTime | Native `DATETIME`, UTC |
| `managed_by` | String | `yaml` (force-synced from `workflows/<name>.yaml` on startup; `enabled` is set only at creation) or `runtime` (graph-owned, the seeder never touches it) |
| `paused_reason` | String? | Why it is disabled, when that was deliberate |
| `topic` | String? | The `topics/*.yaml` topic it works on; absent = spans topics |
| `kind` | String? | `monitor` · `brief` · `learning` · `maintenance` · `dataset` (`WORKFLOW_KINDS`); required in a file that has a schedule |
| `plugin` | String? | The plugin bundle a yaml-owned workflow was seeded from (`core`, …); null for one created at runtime |
| `unavailable` | String? | Why it cannot run now: a tool not registered, an MCP server missing or disabled, its plugin not enabled. Set or cleared at every startup by `services/plugins.rs`; dispatch and Run now refuse it, its timer skips it, the Feed counts it |

#### `ContextProfile` (an agent)

An agent profile: its tool allowlist, prompt, model preference, data class. The graph is the authority (CORE_AND_PLUGINS_PLAN Phase 3b, `services/profiles.rs`): seeded from each loaded bundle's `contexts/*.yaml`, edited in the HBI through `/api/contexts`. Rule: `dev_docs get profiles/graph`.

| Property | Type | Description |
|----------|------|-------------|
| `name` | String | Unique; what chat, `run_agent` and `delegate` name it by |
| `definition` | String | The whole profile as JSON (`system_prompt`, `sections`, `tools`, `model_preference`, `data_class`, `spawnable`, `explicit_only`, …) |
| `managed_by` | String | `yaml` (rewritten from its file on every start) or `runtime` (saved in the HBI; the seeder leaves it alone until `POST /api/contexts/{name}/reset`) |
| `plugin` / `source` | String | The bundle and file it is seeded from |
| `description`, `model_preference`, `provider_hint`, `spawnable`, `specialty`, `allows_all`, `synced_at` | various | Mirrored by the self-model sync for queries; `(:ContextProfile)-[:ALLOWS]->(:ToolDef)` mirrors the allowlist |

#### `Todo` (an item)

The one record for work that wants doing, the user's or an agent's (HBI plan §9.2). REST `/todos`, the `manage_todo` tool, `notify_user` with `needs: "action"` and the proposal writer all write it through `services/items.rs`.

| Property | Type | Description |
|----------|------|-------------|
| `id` | String | UUID, unique |
| `title` / `description` | String | What it asks |
| `status` | String | `pending` · `in_progress` · `awaiting_approval` (an agent that writes, not yet started) · `awaiting_review` (an agent finished) · `done` (`TODO_STATUSES`) |
| `priority` | Integer | 0 urgent … 3 low |
| `tags` | String | JSON array |
| `due_at` | DateTime? | A calendar date stored at 00:00 UTC; overdue once it is before today's *local* date |
| `assignee` | String? | `user`, `agent:<spawnable profile>` or `agent:auto`, checked against the live roster when written; only the user assigns an agent. Absent = the user's own (every todo before 2026-10-09) |
| `origin_kind` / `origin_ref` | String? | Who asked: `user`, `schedule`, `proposal` (ref = filename), `agent` (ref = the notification id). An item is found again by its origin, so it is never made twice |
| `prompt` | String? | What an agent is told; falls back to title + description |
| `session_id` | String? | Its thread, a chat session (`todo-<id>` when the HBI starts one) |
| `snoozed_until` | DateTime? | Hidden from the Feed until then |
| `dispatched_at` / `dispatch_count` / `result_note_id` | DateTime? / Integer / String? | Agent runs: when the last started, how many (at most 3, sending back counts), the note holding the last result |
| `model` | String? | A catalog model its agent runs on; absent = the agent's own |
| `review_comment` | String? | The user's comment when they sent a result back; folded into the next run's prompt |
| `approved_dispatch` | Integer? | The run number the user approved. Single-use: an agent that asks first runs only when it equals `dispatch_count + 1` |
| `closed_reason` | String? | `became a schedule` when a schedule drafted from it was turned on |

Edges: `(:Todo)-[:SPAWNED {run}]->(:Task)` for each agent run (and "Hand to the brain"); `(:Todo)-[:BECAME]->(:Workflow)` for a workflow drafted from it.
| `created_at` / `updated_at` | DateTime | Native `DATETIME`, UTC |

#### `AgentNotification`

What `notify_user` writes. A Feed row: a read, or, with `needs_action`, represented by its item.

| Property | Type | Description |
|----------|------|-------------|
| `id`, `message`, `context`, `related_session_id` | String | The message, a short label, the conversation to continue |
| `read` / `read_at` | Boolean / DateTime | Opening it in the Feed marks it read |
| `needs_action` / `item_id` | Boolean? / String? | Set by `needs: "action"`: it asked for something, and `item_id` is the item it created |
| `created_at` | DateTime | Native `DATETIME`, UTC |

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

#### `DevNote`

Developer documentation in the graph (`services/dev_notes.rs`; rules: `dev_docs get dev-notes/rules`). Separate from `:Note` on purpose, with its own indexes `dev_note_embeddings` and `dev_note_fulltext`. Seeded create-only from `devdocs/devnotes.jsonl`.

| Property | Type | Description |
|---|---|---|
| `id` | String | Unique (`dev_note_id` constraint) |
| `kind` | String | `rule` · `history` · `decision` · `plan` |
| `subsystem`, `title`, `content` | String | Fulltext-indexed |
| `paths` | List<String> | Files the note is about |
| `source` | String | Where it came from (a former doc, a YAML file, `dev_docs`) |
| `date` | String | Local date written (`YYYY-MM-DD`) |
| `embedding` | List<Float> | bge-m3 of title + first 4000 chars; backfilled at startup |

#### `RemovedTool`

A tombstone for a tool the brain no longer has (`services/self_model.rs`). Written at startup when a `:ToolDef` vanishes from the registry, annotated from `architecture/removed-tools.yaml`, and deleted if the tool is registered again. `list_capabilities(kind=tools)` lists them under `removed`.

| Property | Type | Description |
|---|---|---|
| `name` | String | Tool name; MERGE key |
| `removed_at` | DateTime | When the sync first saw it gone (UTC) |
| `skill`, `description` | String? | Copied from the vanished `:ToolDef` |
| `reason` | String? | Evidence for the removal (from the YAML) |
| `commit` | String? | Last commit that still had it: `git show <commit>:<path>` recovers it |
| `replaced_by` | String? | What to use instead |

---

### Relationships

| Relationship | Pattern | Properties | Description |
|-------------|---------|------------|-------------|
| `RELATES_TO` | `(Note)→(Note)` | `similarity: float` | Auto-created when cosine similarity ≥ 0.75 |
| `SUMMARIZED_BY` | `(Note)→(Note)` | — | Source note pointing to its consolidated summary |
| `REFLECTS_ON` | `(Note)→(Task)` | — | Reflection or outcome note linked to its task |
| `PART_OF` | `(Note)→(Note)` | — | Semantic chunk pointing to its parent note |
| `MENTIONS` | `(Note)→(Entity)` | `count: int`, `surface: string` (as written), `type: string` (as extracted), `via: string?` (the name it resolved or was merged through, including a demonym: "chinese" on a mention of China), `merged_from: string?`, `absorbed: ["<entity id>:<count>"]?` (counts folded in from a merged entity the note also mentioned) | Entity mention extracted from note content. `via`/`absorbed` are what an unmerge reverses |
| `ALIAS_OF` | `(EntityAlias)→(Entity)` | — | The alias's canonical entity |
| `IN_TOPIC` | `(Note)→(Topic)` | `rank: int` (1 = main topic, 2 = as central), `method: 'llm' \| 'rule' \| 'manual'`, `assigned_at: datetime` | Written by the topic assigner (`llm`, or `rule` for an `assign: rules` topic's member rule), or by the user (`manual`, with the note `topics_locked`); an unlocked note's edges are replaced whenever it is classified again |
| `NOT_IN_TOPIC` | `(Entity)→(Topic)` | `at: datetime` | The user keeps the entity off that topic's map and lists; carried to the survivor by an entity merge |
| `RELATES` | `(Entity)→(Entity)` | `type` (from `relations/*.yaml`), `method: 'manual' \| 'llm'`, `status: 'active' \| 'proposed' \| 'rejected'`, `evidence: [note id]`, `model` (llm), `created_at`, `decided_at`, `seen_again` (a rejected one proposed again), `from_merge_review` (folded from `RELATED_ENTITY`) | A typed relationship (HBI plan Phase 17, `services/relations.rs`). The user's are active; extracted ones are proposals until accepted or rejected; a rejected one is kept so it is never proposed again. A symmetric type is stored once, from the lower id |
| `REFERS_TO` | `(Entity)→(WikiItem)` | `status: 'proposed' \| 'confirmed' \| 'rejected'`, `method: 'llm' \| 'manual'`, `why`, `query` (the model-suggested search term, when used), `model`, `at`, `decided_at` | The entity's reference link (HBI plan Phase 18). The model's pick is proposed; the user confirms, rejects, chooses another or pastes a URL. Confirming one rejects any other, so an entity has one reference. A rejected item is never offered to the model for that entity again. A proposed link to a `wikimedia_page` item is set `rejected` by `entity_kinds` with `why` saying so (Phase 19). Two entities linked (proposed or confirmed) to one item are a `wikidata` merge candidate |
| `ALIAS_VERDICT` | `(Entity)→(Entity)` (candidate `from` → `into`) | `verdict: 'same' \| 'related' \| 'different'`, `p_same`, `p_related`, `p_different` (floats), `model`, `version` (prompt version), `at: datetime` | The alias judge's opinion on a merge candidate (`services/alias_judge.rs`). It only orders the candidate list. A verdict from another model or prompt version is judged again, and a merge deletes it with the node |
| `NOT_SAME` | `(Entity)→(Entity)` | `decided_at: datetime` | A person answered a merge proposal "different things": never proposed again |
| `RELATED_ENTITY` | `(Entity)→(Entity)` | `decided_at: datetime` | Related but not the same (`trump administration` → `donald trump`): never proposed as a merge |
| `DERIVED_FROM` | `(Note{inference})→(Note)` | — | Inference note citing its source notes |
| `SUBTASK_OF` | `(Task)→(Task)` | — | Sub-task of a parent task |
| `DEPENDS_ON` | `(Task)→(Task)` | — | Task ordering dependency |

---

### Constraints

| Name | Node | Property | Type |
|------|------|----------|------|
| `workflow_id` | `Workflow` | `id` | UNIQUE |
| `workflow_name` | `Workflow` | `name` | UNIQUE |
| `working_memory_id` | `WorkingMemory` | `id` | UNIQUE |
| `entity_name` | `Entity` | `name` | UNIQUE |
| `entity_alias_name` | `EntityAlias` | `name` | UNIQUE |
| `topic_name` | `Topic` | `name` | UNIQUE |
| `arch_snapshot_id` | `ArchSnapshot` | `id` | UNIQUE |
| `agent_job_id` | `AgentJob` | `id` | UNIQUE |
| `todo_id` | `Todo` | `id` | UNIQUE |
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
| `agent_job_status` | `AgentJob` | `status` | RANGE | Queue coordinator polling |
| `agent_job_priority` | `AgentJob` | `priority` | RANGE | Priority-ordered dispatch |
| `agent_job_created` | `AgentJob` | `created_at` | RANGE | FIFO ordering within priority |
| `agent_job_parent` | `AgentJob` | `parent_job_id` | RANGE | Chain unpark / cancel-children lookups |
