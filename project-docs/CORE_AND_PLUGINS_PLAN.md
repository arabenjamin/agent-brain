# Core and plugins plan

Status: **Phase 3c (plugin-api crate) done 2026-10-10: `crates/plugin-api`, ACLED moved out of `app` as the `osint-data` plugin, a plugin cannot shadow a tool, CI checks plugin dependencies; verified live. Phase 3b (profiles in the graph) done 2026-10-10: the graph owns profiles, the HBI writes it, prompts are sections with `requires`, the tool catalogue is generated, plugins extend profiles; verified live. Phase 3a (bundle loader) done 2026-10-10: every seed dir is in `plugins/core/`, one loader, workflows marked unavailable when what they name is missing; verified live. Phase 2c part 1 (rule dev notes rewritten for workflows) done 2026-10-10; part 2 (a week of runs compared) due from 2026-10-17. Phase 2b (workflows) done 2026-10-10: chains and schedules are one `:Workflow` model, live graph migrated, verified live. Phase 2a (named step outputs + step defaults) done 2026-10-10: deployed and verified live on daily-news (32 steps → 18, every step completed, claims linked to the stored brief). Phase 1 (docs into the graph) done 2026-10-10: deployed and verified live (650 notes seeded and embedded in ~2 min, search/get/write round trip, cleanup run, retrieval eval unchanged 0.880 / 0.800). Phase 0 done and deployed 2026-10-10 (`bb402f3`; cleanup script run, 43 tombstones, retrieval eval unchanged at recall@10 0.880 / MRR 0.800); Phases 1–6 planned.** Decided in discussion with the owner; the decisions are in §2 and are not to be re-opened without him.

## 1. Why

The project set out to give an LLM the *right* context from queries over a graph, instead of a book of markdown. Its own codebase now has the problem it was built to solve:

| Measured 2026-10-10 | Size | ≈ tokens |
|---|---|---|
| Rust (`crates/`) | 95.6K lines (`app` 89.8K: services 42K, skills 24K, clients 15K); ~17K are inline tests | 0.9–1M |
| Frontend (`hbi-frontend/src`) | 19.4K lines | ~200K |
| `project-docs/` | 1.18 MB (`engineering-history.md` + `subsystem-rules.md` = 420 KB) | ~300K |
| Registered tools | 104, of which **34 never called** since telemetry began (2026-06-12) | |
| Seeds | 18 chains, 28 schedules, 21 agent profiles; 21 seed files carry the owner's personal data | |

No frontier window holds the Rust alone, and the owner's working rule — an LLM performs best with its context 1/3 to 2/3 full — puts the target far lower. Token cost of working on the code grows with its size, so size is a cost, not a style issue.

Goals, in the owner's words: recapture the core goals and refocus the code on them; keep the good things; not custom software that only works for one person; a plugin system so a doctor, a lawyer, a journalist or an accountant can specialise the brain; the brain helps build its own plugins; the brain is not a chatbot.

## 2. Decisions (2026-10-10)

1. **Carve in place**, no rewrite. Hard-won behaviour (retrieval tuning, privacy routing, queue durability) lives in the code and its tests.
2. **Dead code is removed on evidence, every phase** (§4). Nothing is kept "in case"; git history is the archive.
3. **The Feed is core** (items, todos, `notify_user`, dispatch). If the brain is not a chatbot, the Feed is how it talks.
4. **Epistemics (claims, relations) stays in core for now, written as an in-tree plugin** against the plugin API. If the owner later decides it is not core, moving it is a folder move.
5. **Plugin code: both kinds.** Declarative YAML bundles and out-of-process MCP plugins for anyone; in-process Rust plugin crates for heavy, graph-bound work. **An in-process plugin crate may depend only on `plugin-api`** (plus `protocol`/`models` and external crates), never on `app` or `repository`. CI enforces it.
6. **Docs move into the graph.** The graph is the authority for engineering history and subsystem rules; git keeps a seed export for bootstrap and backup that no agent loads; `project-docs/` shrinks to a short spec.
7. **Chat becomes a hotkey modal** with an agent picker; the Chat workspace goes.
8. **Project name**: separate session, out of scope here.
9. **The graph is the runtime authority for everything declarative; git seeds it; a seed admits only what is available** (owner, 2026-10-10). Profiles, prompts, workflows, DevNotes, plugin bundles: all live in the graph and are seeded from git when the graph is new or empty. A seeded item that names a tool, server or plugin that is not available does not arrive broken and is not silently dropped. See §7 "Seeding rule". A deletion leaves a tombstone in the graph, so the brain remembers what is gone and why (§4).

## 3. Size budget

The whole repo does not need to fit a window; **the unit of work does**.

| Target | When |
|---|---|
| Core tools ≤ 70 | end of Phase 0 |
| Docs an agent loads up front (CLAUDE.md + spec) ≤ 40 KB | end of Phase 1 |
| Core Rust ≤ ~60K lines (extraction + deletion alone) | end of Phase 4 |
| Core Rust ≤ ~32K lines ≈ 1/3 of a 1M window; every core crate ≤ 6K soft / 8K hard; core tools ≤ 40 | end of Phase 6 |

Honest note: extraction only moves code out of the core's context; it does not shrink the total. The second half of the core target needs the consolidation in Phase 6, not just moving files. An estimate of what stays core after Phase 4 (storage, knowledge, LLM stack, queue/scheduler/runner, chat loop, MCP/HTTP, Feed, core skills) comes to ~55–60K.

`scripts/size_budget.sh` (Phase 0) prints lines per crate and per plugin, with and without tests, and fails over the hard limit. It runs in the `dev` CI job beside format + unit tests. Every phase ends with the table re-measured and pasted into §9.

## 4. Phase 0 — dead-code sweep

Cheapest phase, immediate reduction, and it shrinks what later phases must move.

**Three signals; a deletion needs at least one, written next to it:**

- **Runtime** — tool-call history. `interactions.tools_used` in telemetry DuckDB (61K rows since 2026-06-12) plus `(:AgentJob).tool_name` in the graph. Saved as a query run through the existing `duckdb_query` tool, so it costs no new Rust. **First verify coverage**: confirm chat-turn, sub-agent, queue-job and REST calls all land in one of the two sources before trusting a zero. Known gap: `gpu_lease` is driven by a script over REST.
- **Compile time** — `pub` hides dead code from rustc. Narrow visibility to `pub(crate)` crate by crate and let `dead_code` find the rest; `cargo-machete` for unused dependencies. Run the tools in a Rust container, not installed on the host.
- **Config** — seeds pointing at retired things.

**Candidates from the 2026-10-10 measurement** (never called in four months unless noted). The owner reviews the list; "decide" items need his call.

| Group | Tools | Proposed |
|---|---|---|
| Git writes | `git_commit`, `git_push`, `git_create_pr`, `git_create_branch`, `git_status`, `get_git_diff` | delete |
| Codebase writes | `write_codebase_file`, `get_file_tree` | delete (the rest of codebase/git → developer pack, Phase 4) |
| WebSocket | `ws_connect`, `ws_send`, `ws_receive`, `ws_close` | delete the skill |
| Procedural memory | `store_procedure`, `search_procedures`, `record_outcome`; `execute_procedure` (8 calls, last 09-07) | delete the tools and the `Procedure` model; **`procedure_executor.rs` kept** (it runs dynamic tools until Phase 3) |
| Model/worker admin | `use_model`, `reload_models`, `set_worker_config` | delete; models.yaml + restart is the path |
| Job introspection | `manage_job`, `get_job_result`, `update_job_progress` | delete `get_job_result`, `update_job_progress`; **`manage_job` kept** (it backs the HBI cancel/retry buttons) |
| Chains | `manage_chain` | delete in Phase 2 (workflows replace it) |
| Session/goal helpers | `decompose_goal`, `analyze_gaps`, `digest_experiences`, `summarise_session`, `list_sessions` | delete |
| Proposals | `write_proposal`, `read_proposal`, `dismiss_proposal`, `list_proposals` (150 calls, every one returned empty: `./proposals` and `proposals/applied` hold **zero files**, ever) | **keep the capability, fold it into the Feed** (owner to confirm): a brain suggestion is a `:Todo` item with `origin: proposal`, which `write_proposal` already creates. Delete the markdown-file side (`PROPOSALS_DIR`, its compose mount, read/list/dismiss); the Feed reads and dismisses |
| Misc | `resource`, `snapshot_architecture` | delete `resource` (and its registry); **`snapshot_architecture` kept** — added 2026-10-08 on a weekly schedule, so zero calls meant "not yet due", not dead |
| Datasets | `reliefweb_reports`, `datasets/reliefweb.rs`, `schedules/reliefweb-sync.yaml`; GDELT retired 2026-10-06 but `datasets/gdelt.rs`, `gdelt_events`, `schedules/gdelt-sync.yaml` remain | **delete both** (owner, 2026-10-10) |
| GPU | `gpu_lease` (script/REST only) | **keep** (owner, 2026-10-10: future fine-tuning). Not a tool an agent needs; candidate to leave REST-only + script |
| Agent constructor | `construct_agent` (5 calls, all 2026-06-12): superseded by `run_agent` + `select_agent`, `schedule-author`, and `manage_plugin` (Phase 3) | **delete** `constructor.rs` (885) + `AgentSpec` handling in `queue.rs` |
| Media helpers | `fetch_transcript` (7; `ingest_media` fetches the same captions), `list_channel_videos` (16; `poll_media_sources` uses the same `list_feed_videos`) | **delete the tools** |
| Media watchlist | `manage_media_source` (5): the only way to activate sources `discover-media-sources` stages | **keep**; moves to the media pack |
| Scheduler tick | `run_scheduler_tick` (18; Settings "Run a tick now") | **fold** into `scheduler_control` `action: tick`; the button keeps working |
| Recency / self-inspection | `list_notes` (17; `recall_recent` covers it), `context` (3; `list_capabilities` covers it), `get_usage` (3; `duckdb_query` on `model_usage`, `get_llm_budget`), `list_codebase_files` (1; `search_codebase`) | **delete**; fix the allowlists and prompt lines naming them |
| API contexts | `define_api_context` (4): manual path for an API with no spec | **keep until Phase 3**, then folds into bundle entries + `manage_plugin` |
| MCP servers | `mcp_list_servers` (1) | **fold**: `mcp_list_tools` with no `server` lists servers |

Deleting a tool deletes its code path, its frontend surface, its schemas, its seed references, its docs, and its tests.

**Tombstones, not just git.** Git keeps deleted code, but code recovered from git has drifted from everything around it, and nothing in the running brain knows it was ever there. So `self_model.rs` stops deleting a vanished `:ToolDef` and marks it `removed: true` with `removed_at`, a `reason` (the evidence above), the `commit` to recover from, and `replaced_by` when there is one (`list_notes` → `recall_recent`). Then:
- a prompt, allowlist or workflow step that names a removed tool is flagged at seed and in `audit`, with `replaced_by` as the fix;
- `list_capabilities` can say "`list_notes` was removed on 2026-10-.., use `recall_recent`" instead of "no such tool";
- the same tombstone shape covers removed workflows, profiles and plugins later.

Until the Phase 3 seeding rule exists, Phase 0 fixes the prompts by hand: the `general.yaml` system prompt (19 KB in one string) names `context`, `get_usage`, `list_notes`, `get_file_tree`, `decompose_goal`, `record_outcome`, `summarise_session` and `manage_chain`, and its hand-written "Key tools available" list is exactly the drift the seeding rule removes.

**A tool younger than 30 days cannot be judged dead** (the `snapshot_architecture` false zero). Check `git log --reverse -S'"<tool>"'` for the introduction date beside the call count.

**Exit:** owner-approved list applied; `cargo test --lib` green; `eval-retrieval` unchanged; size table in §9.

### Phase 0 result (2026-10-10)

**Coverage verified first.** Every production path (chat via `write_guard`, queue jobs, sub-agents, MCP/REST via `brain_core`, context pre-loads) dispatches through the one `ToolHandler`, built with telemetry at `brain_core.rs`; a zero in `interactions` is a true zero for tools. REST handlers that call services directly are not tool calls.

**Removed:** 43 tools (41 deleted, 2 folded); 104 → 62 registered (61 static + the `list_tasks` built-in). Whole skills: `GitSkill`, `WsSkill`, `ResourceSkill` (+ `resource_registry`), `SleepSkill` (+ `SleepService`, training export), `ContextSkill`, `ConstructorSkill` (+ the `AgentSpec` learning edge), `DatasetSkill` (+ `services/datasets/`, `DatasetStore`, `datasets/`, three schedules). Also: the `websocket` cargo feature and `tokio-tungstenite`; `PROPOSALS_DIR`, its mount and the startup backfill (`write_proposal` now puts the whole proposal in the Feed item); `SearchConfig` (unread), dead `BrainEvent` variants, a duplicate `auth_middleware`; dead trait methods on `KnowledgeStore`/`TaskStore`/`WorkingMemoryStore` with their impls and mocks; the graph-viz export; orphaned repository functions. Config signal: `chains/git-history.yaml` (its diff step never ran) and `contexts/developer.yaml` (every tool it existed for is gone). Stale hand-written registries `project-docs/tools.md` and `architecture.md` (claimed 61 tools against 104).

**Compile-time pass.** Making the lib's modules `pub(crate)` and compiling `main.rs` into it as a module (so `main` is the root) makes rustc report every unreachable `pub` item: 57 at first. The domain ones were removed. Left for Phase 6, listed so they are not rediscovered: unused MCP transport/protocol scaffolding (error variants, `with_config` builders, `McpTransport::send`), `LlmClient::new`/`health_check`, `LlmConfig::new`/`with_max_tokens`, `SchedulerService::new`/`shutdown`, `RestAdapter` builders, `Topics::from_defs`, `Rules::from_yaml`, three test-only claims helpers (`evidence_domains`, `source_is_in_results`, `check_independence`). `cargo-machete` not yet run.

**Tombstones** are a separate `:RemovedTool` label, not a flag on `:ToolDef`: ten readers count `:ToolDef`, and a flag would have needed every one of them to filter. Reasons come from `architecture/removed-tools.yaml`. A profile allowlist naming an unregistered tool now logs a warning with the replacement (it used to be dropped silently by the `ALLOWS` MATCH). The HBI schedule editor's dropdown now filters its curated tool list to the live registry instead of always showing it.

**Findings for later phases:**
- **Snapshot restore has no caller.** `SnapshotService::restore_snapshot` is unreachable: snapshots are taken, but nothing restores one. Phase 1 needs a restore path anyway (the DevNote export rides on snapshots).
- **Nothing writes `llm_config` after startup** now that `use_model` is gone, so the `Arc<RwLock<Option<LlmConfig>>>` hot-swap machinery is static. Phase 6.
- **Pre-existing:** `hbi-frontend` `entities.test.ts > labels every proposal reason` fails on `HEAD` too (6 reasons vs 5 labels); one clippy warning in `alias_judge.rs`.

**After deploy:** run `scripts/phase0_cleanup.cypher` (seeders never prune: three schedules, one chain, four built-in dynamic tools stay in the graph otherwise), then `eval-retrieval` and `list_capabilities(kind=tools)`.

## 5. Phase 1 — docs into the graph

**Model.** A separate label, `:DevNote {id, kind: rule|history|decision, subsystem, paths[], title, content, date, embedding}`. It is a separate label, not a `note_type`, because dev history must never surface in the brain's own retrieval about the world (a news question about GDELT must not return our GDELT incident). The hybrid vector+BM25 retrieval is parameterised by label and gets its own indexes on `:DevNote`; `search_notes` and every `:Note` query stay untouched.

**Access.** One `dev_docs` tool: `search` (text, optional `subsystem`/`path`), `get`, `write`. Allowed for `self-analyst` and coding agents (Claude Code already reaches the brain through `.mcp.json`). Not in the general profile.

**Seed export.** `devdocs/devnotes.jsonl`, written by an `export-devdocs` CLI on the snapshot code path. `init-db` seeds from it when the graph has none (`ON CREATE` only: the graph is the authority and a seed never overwrites it). Re-exported in the same change that writes a DevNote, so git always has a current copy. No agent reads it; if the brain is down, grepping it is the fallback.

**Migration.** Each chunk of `engineering-history.md`, `subsystem-rules.md`, completed plan docs, and the incident comment blocks in seed YAML (`daily-news.yaml`: 137 of 543 lines) goes into one of three bins:

- rule, still true, needed to change code safely → short spec (if every change needs it) or DevNote `kind: rule`
- why / incident / measurement → DevNote `kind: history`, linked by `subsystem` + `paths`
- describes code that no longer exists → dropped (git keeps it)

A finished plan becomes one `kind: decision` DevNote and is deleted; `PLANS_INDEX.md` lists active plans only, one line each.

**What remains in `project-docs/`:** a spec of the core (what it is, workflow format, plugin API, schema and timestamps, env), plus active plan docs. CLAUDE.md is rewritten *smaller*, pointing at `dev_docs`; the owner signs off on the wording.

**Exit:** CLAUDE.md + spec ≤ 40 KB; a coding agent asked "why does the queue do X" gets the relevant notes from `dev_docs`; graph wipe + `init-db` restores DevNotes from the export.

### Phase 1 result (2026-10-10)

- **Built:** `services/dev_notes.rs` (`:DevNote`, own vector + fulltext indexes, RRF hybrid search, create-only seed, startup embedding backfill, export after every write); the `dev_docs` tool (search/get/write; `self-analyst` allowlist); `scripts/dev_docs.sh` for coding agents, because an MCP client lists only the front-door profile's tools (calls are not filtered, listing is); `restore-snapshot` CLI (closes the Phase 0 finding).
- **Migrated:** 25 docs (1.0 MB) → 590 notes split by heading (`rule` 52, `history` 147, `decision` 317, `plan` 74); 59 YAML comment blocks (729 lines) from chains/schedules → notes, each replaced by `# Why: dev_docs get <id>` (parsed YAML verified identical before/after). `project-docs/` 1.19 MB → 142 KB of reference files: env, schema, cli, architecture_context, USAGE, README_DUCKDB, FINE_TUNING_PLAYBOOK, PLANS_INDEX (now active plans only), this plan and SOURCE_QUALITY_PLAN.
- **Not bin-sorted.** The plan's third bin ("describes code that no longer exists → drop") was not applied note by note: everything moved, because a note costs nothing until queried and judging 590 sections is better done when a query surfaces a stale one (`write` replaces it).
- **Removed with it:** the `doc-update` schedule (it rewrote a git-log digest into STATUS.md every 12h; `git log` is the changelog) and `scripts/self_learn.py` / `self_reflect.py` (hand-written architecture notes listing removed tools; they called `decompose_goal`).
- **Citation convention instead of 36 edits:** code comments citing a former doc (`HBI_REDESIGN_PLAN.md §9.2`) stay; `dev_docs` `subsystem` matches a note's source, so the name finds the notes.
- **CLAUDE.md** rewritten around dev notes, 7.7 KB → 7.6 KB.
- **After deploy:** run `scripts/phase1_cleanup.cypher` (the doc-update schedule node).

## 6. Phase 2 — workflows: one concept for chains and schedules

**The difference today.** Both are lists of `ChainStep` run by the same queue. They differ on two unrelated axes:

| | Chain (`:SchedulerChain`) | ScheduledTask |
|---|---|---|
| When | a one-off Task whose goal text matches a regex `pattern` (`goal_to_steps`) | a timer (`interval_seconds`) |
| Checks | evaluator, adversarial critic, auto-retrieval appended | verbatim, none |
| Where steps live | template in `chains/` | inline in `schedules/` |

Agents (`run_agent` + a profile) are a third way, where the model chooses the steps.

**Measured 2026-10-10 (one-off Tasks by chain pattern, 60 days):** `fill-knowledge-gap` 890, `video-learning` 644, `article-learning` 75 — pipelines the brain triggers itself, passing an input (topic, URL) through goal text; `consolidate` 29, `document` 25, `prioritize` 12, `deep-dive` 11, `learn` 10, `codebase` 5, `review` 4, `diagnose` 1 — keyword routes guessing at free-text goals by substring; `meta-learn`, `run-agent`, `schedule-review`, `ui-frontend` 0 (`deep-dive-hard` 0 but 7 days old). So a "chain" is either a pipeline with inputs or a keyword router, and a schedule is a pipeline on a timer.

**Decided (owner, 2026-10-10):** one `:Workflow` label replaces both node types (not just one format over two labels); pipelines are called by name with inputs, a free-text goal goes to one default, and chains with no runs in 60 days are deleted unless under 30 days old; the evaluator and critic are opt-in per workflow (`checks:`), not on by default.

**Sequence:** 2a named step outputs + step defaults (additive; rewrite daily-news); 2b the `:Workflow` model, `manage_workflow`, REST, HBI editor, migration of the live graph including runtime-owned schedules; 2c delete the old paths (done inside 2b; 2c is now the dev-note sweep and the week-of-runs comparison).

**Phase 2a result (2026-10-10):** a step may carry an `id`; later steps read it as `{{steps.<id>}}` or `{{steps.<id>.<path>}}`, and a step that did not complete resolves to a visible marker. References must name an earlier step (rejected at upsert, REST and lint; a unit test lints every seed file). A seed file's `defaults:` is merged into each step. daily-news dropped its WorkingMemory staging (14 push_context steps and both cleanup queries): 32 steps → 18. Rule: `dev_docs get chains/named-step-outputs`.

**Phase 2b design (2026-10-10).** The ScheduledTask machinery becomes the Workflow machinery (model, repository, seeder, tool, REST, HBI), with the schedule made optional; chains fold into it. Nothing is built twice.

- `(:Workflow {name})`, `name` a unique slug (the file stem). Keeps every ScheduledTask property (`enabled`, `next_run_at`, `last_run_at`, `paused_reason`, `topic`, `kind`, `managed_by`) and adds `inputs` (declared names), `checks` (`evaluator`, `critic`), `evaluation_rubric` and `is_default`. `interval_seconds` null = run by name only.
- One seed dir, `workflows/*.yaml` (`WORKFLOWS_DIR`): `every: 1d` replaces `interval_seconds`; `inputs:`, `checks:`, `default: true`; steps keep their format. `chains/` and `schedules/` are deleted.
- A run is a `:Task` with `workflow` and `inputs_json`. Steps see `{{inputs.<name>}}`; a missing required input refuses the run when it is created. A Task with no workflow runs the default workflow. Run records are found by `t.workflow`, not by parsing their `context`; old run records are backfilled.
- Callers name their workflow: media → `video-learning` / `article-learning` / `fill-knowledge-gap`, gap detection → `fill-knowledge-gap`, perception scan → `consolidate` / `codebase` / `diagnose`, the deep-dive schedule → `deep-dive`. `create_task` gains `workflow` + `inputs`.
- `manage_workflow` (list, get, upsert, delete, audit, run) replaces `manage_chain` and `manage_scheduled_task` (both tombstoned). REST `/api/work/workflows*` replaces `/api/work/schedules*` and `/api/scheduler/chains`.
- Retired: `meta-learn`, `run-agent`, `schedule-review`, `ui-frontend` (no runs in 60 days), and the keyword routes `document`, `prioritize`, `learn`, `review`. The 60-day goals that hit them were mostly misroutes, such as an LLM's "No section titled 'Priority Research Topic' was found" routed to `prioritize`.
- Checks: the evaluator ran once in 60 days and the critic never, so no migrated workflow opts in.
- Live migration: stop the brain, snapshot, then one Cypher script copies every ScheduledTask (including the 4 runtime-owned ones) into a `:Workflow` with its id and timers, so nothing fires early. It backfills `Task.workflow` and deletes the old nodes. Then deploy.

**Phase 2b result (2026-10-10).** Built as designed, with the old paths deleted in the same change rather than in 2c: keeping both would have meant two seeders writing nodes one dispatcher reads.
- Code: `crates` +1,905 / −2,931 lines, frontend +234 / −729; seed YAML 3,709 → 3,182 lines in one dir (`workflows/`, 33 files). Unit tests 870 pass; frontend 96 of 97, the one failure (`entities.test.ts`) predates this work. Static tools 61: `manage_workflow` in, `manage_chain` and `manage_scheduled_task` out (tombstoned).
- Also removed: chain seeding on the first tick, `goal_to_steps` regex routing, `goal_topic` / `file_slug`, the run-record context parser, `/api/scheduler/chains`, the HBI Routing-chains view, and an unused duplicate REST router (`RestAdapter::routes`).
- Callers name their workflow: media polling, gap spawning, gap detection, the perception scan (repeated failures and dead jobs → `diagnose`, consolidation and staleness → `consolidate`, codebase → `codebase`, queue backlog → default), the evaluator and critic retries (same workflow and inputs), and the three workflows that start runs (`news-analysis`, `tech-dependency-synthesis`, `brain-exercise` → `fill-knowledge-gap`; `deep-dive-pick` → `deep-dive`). The schedule that was `schedules/deep-dive.yaml` is `deep-dive-pick`, as its name clashed with the chain.
- Migration (`scripts/phase2b_migrate.cypher`, brain stopped, old nodes backed up to the snapshot volume as `phase2b-scheduledtask-schedulerchain-2026-10-10.txt`): 28 schedules → workflows with ids and timers kept, 0 unmapped; 749 schedule runs and 1,746 one-off pipeline runs backfilled with `workflow` + `inputs` (all inputs parse); 28 + 17 old nodes deleted. On deploy nothing fired early; the 9 former chains were created, the rest synced.
- Verified live: list; a refused run (missing input, unknown name with the names that exist); `fill-knowledge-gap` run by name with a topic (13 jobs, completed); a free-text `create_task` on the default workflow (completed); a rejected upsert (bad name; undeclared input); Run now from the HBI's REST path on `todo-snapshot` (completed, timers advanced); `/api/work/workflows`, `/workflows`, topic mix, architecture, feed. The HBI was rebuilt but not clicked through.
- Two fixes found live: a run got a second `update_task` when its workflow closes itself before a cleanup step (now only appended when no step closes the run's own task); an unknown workflow name now lists the enabled ones.
- Behaviour to know: `diagnose` keeps its confidence guardrail, so every diagnosis run stops as blocked for review, as dead-job tasks already did. Repeated-tool-failure tasks, which went to the retired `meta-learn` chain, now land there too.

**Phase 2c.** Part 1 done 2026-10-10: the 25 `rule` dev notes that still described chains and schedules (ownership, externalized chains, evaluator and critic, curiosity engine, media, HBI, self-improvement loop, architecture) are rewritten against `workflows/model`, keeping their ids and their former-doc `source` (a `dev_docs write` that replaces a note now keeps its source, so a cited former doc stays searchable by name). History notes are left as written: they record what was true then. Part 2, from 2026-10-17: compare a week of runs before and after per workflow (same tool sequence, same success rate), the plan's exit test.

**The model.** Three separate things:

- **Workflow** — *what*: fixed steps, or `agent:` + `goal:`. Declares `inputs`, `data_class`, `checks`, and step `defaults`.
- **Trigger** — *when*, a list on the workflow: `manual` (by name, with inputs) and `schedule` (interval); later `event`. No `match` trigger: a free-text `create_task` goal runs the one default workflow.
- **Run** — one execution with its inputs. The existing `:Task` label stays as the run record (renaming it is churn with no gain), gaining `workflow` and `inputs`.

`:SchedulerChain` and `:ScheduledTask` merge into `:Workflow`; `managed_by` (yaml/runtime) carries over unchanged. Callers that build goal strings to hit a regex (`spawn_gap_tasks`, perception scan) call `run_workflow(name, inputs)` instead.

**Named step outputs.** `{{_prev}}` only reaches one step back, so `daily-news.yaml` spends 14 of its 34 steps on `push_context`, plus a gather and two cleanups, to carry data forward through `:WorkingMemory`. Steps get an `id`; `{{steps.<id>}}` (and `{{steps.<id>.<field>}}`) resolve from the same run's jobs. AgentJob gains `run_id` and `step_id`. `{{_prev}}` stays as an alias. A resolved output over its channel size is marked truncated, never cut silently.

**Step defaults.** `daily-news` repeats `priority / max_attempts / provider_hint` on all 34 steps; a workflow-level `defaults:` block holds them once.

```yaml
name: daily-brief
triggers: [{ schedule: 1d }]
defaults: { provider_hint: ollama, max_attempts: 2 }
steps:
  - { id: metro,   tool: search_web,   args: { query: "latest Detroit metro news", recency: day, news: true } }
  - { id: weather, tool: http_request, args: { url: "…" }, optional: true }
  - id: brief
    tool: reason
    provider_hint: ollama-cloud
    args: { question: "…", context: "{{steps.metro}}\n{{steps.weather}}" }
  - { tool: store_note, args: { content: "{{steps.brief}}", note_type: news } }
```

**Migration is one-shot.** A script converts all chain and schedule YAML and the live graph nodes; then the old formats, `manage_chain`, the two seeders and `goal_to_steps`' regex-only path are deleted. No dual path survives. The `schemas/` envelopes merge into one `workflow` schema; `lint_chain_steps` is rewritten for named outputs (the "reason past step 1 needs `{{_prev}}`" rule becomes "a reason step must reference a step output").

**Exit:** every former chain and schedule runs as a workflow with the same tool sequence (compare a week of runs before and after); `daily-news` roughly halves; seed + validation code smaller than before.

## 7. Phase 3 — plugin API and bundle loader

**A plugin is a folder:**

```
plugins/<name>/
  plugin.yaml        # name, version, description, requires (env vars, plugins, MCP servers), data_class default
  workflows/ agents/ sources/ schemas/ topics/ mcp-servers/
  graph.cypher       # optional constraints/indexes, applied by init-db
  crate/             # optional in-process Rust crate (depends on plugin-api only)
```

The core ships as a bundle too (`plugins/core/`), so there is one loader. The five seeders (`chain_`, `schedule_`, `source_`, `media_source_`, `mcp_server_seeder.rs`, ~750 lines) collapse into it. Enabled plugins come from `PLUGINS_ENABLED` (passed through in `docker-compose.yml`), with `PLUGINS_DIR` defaulting to `./plugins`; plugins outside the repo are mounted.

**`plugin-api` crate.** `Plugin` trait (`manifest`, `tools() -> Vec<Arc<dyn Skill>>`, optional `routes()` nested under `/api/plugins/<name>/`, optional background `start()`), and `PluginContext`: narrowed host services — graph (read/write Cypher through the guard), knowledge (store, search), LLM (`generate`, `generate_json` under the routing policy with the plugin's purpose and data class), HTTP, clock, notify. **The host API is the same set of capabilities the core exposes as MCP tools**, so an in-process plugin and an MCP plugin can do the same things.

**Rules.**
- A plugin tool may not shadow a core tool or another plugin's tool; the loader refuses loudly at startup.
- Plugin tools register before the declared tools (a bundle's `tools/`), which stay last.
- In-tree plugins compile into the binary and are enabled at runtime. Cargo features only if build size ever matters.
- **The proof of the boundary: the brain starts and passes the core test suite with no plugins enabled.**

**Seeding rule (§2.9).** The loader is the one seeder, and it applies the same rule to every kind of item:
- **The graph is the runtime authority.** Each seeded node carries `managed_by: yaml|runtime`, as ScheduledTasks already do. A runtime edit (UI, chat, `manage_plugin`) writes the graph and flips it to `runtime`; the seeder never overwrites a runtime-owned node. **Profiles change the most:** today `contexts/*.yaml` is read into memory at startup and the UI writes YAML files back to disk, with the graph only a mirror (the 2026-09-12 duplicate-front-door incident came from that). After this, the UI writes the graph, and git is the seed.
- **Admit only what is available.** Tool allowlists are intersected with the live registry; the dropped names are reported, not silently lost. A workflow whose steps name an unavailable tool or MCP server is seeded **disabled**, with the reason, and a Feed item says so: never run broken, never silently dropped. (This settles the old "no seeder skip-on-boot" concern: the objection was to *silent* dropping.)
- **Prompts are sections, not one string.** A profile's prompt is a list of sections, each optionally `requires: [tools]`; a section whose tools are not all available is left out. The tool catalogue ("Key tools available") is **generated** from the allowlist and the registry, never hand-written. A plugin contributes sections to a profile the same way, so disabling the plugin removes its prompt text too (the JOB APPLICATION RULE belongs to the `personal` pack this way).
- **Lint against tombstones.** Prose that names a removed tool is flagged with its `replaced_by`.
- **Empty graph:** everything seeds from git, filtered as above. **Non-empty:** only yaml-owned nodes are refreshed.

**Declarative tools.** `DynamicTool` (an HTTP template stored in the graph) becomes a `tools:` entry in a bundle, and `dynamic.rs` merges into the loader.

**Brain-built plugins.** A `manage_plugin` tool writes declarative bundles into a runtime plugin dir (`managed_by: runtime`), each file validated against its schema before it is saved. This replaces `manage_dynamic_tool` and the authoring half of `construct_agent`. Code plugins written by the brain (an MCP server built and tested in `rust-sandbox`) are later and frontier-only; not in this phase.

**Frontend views of plugins.** Open (§10). Interim: in-tree plugin UI lives under `hbi-frontend/src/plugins/<name>/` and renders only when `/api/plugins` reports the plugin enabled.

**Exit:** loader + `plugin-api` in place; core runs plugin-less; the `core` bundle loads every seed the old seeders did; profiles are graph-authoritative; disabling a plugin removes its tools, workflows and prompt sections with no warnings left behind; a fresh empty graph seeds clean.

**Sequence (2026-10-10).** Phase 3 is four slices, each deployable alone:
- **3a bundle loader (YAML).** Every seed dir moves to `plugins/core/` with a `plugin.yaml`; one loader finds the enabled bundles and hands each kind's files to its reader. The source, MCP-server and media-source seeders fold into it; the ten `*_DIR` env vars become `PLUGINS_DIR` + `PLUGINS_ENABLED`. Availability: a workflow whose step names a tool not in the registry, an MCP server that is missing or disabled, or whose plugin is not enabled is marked `unavailable` (the reason, on the node), refused by dispatch and the timer, and counted by the Feed's existing "workflows with steps that can't run" digest. `GET /api/plugins` lists each bundle, its status and what it contributed.
- **3b profiles in the graph.** `:ContextProfile` becomes the authority (seeded from bundles, `managed_by`), the UI writes the graph, prompts become sections with `requires:`, the tool catalogue is generated, allowlists are intersected with the registry and the dropped names reported.
- **3c `plugin-api` crate.** The `Plugin` trait and `PluginContext`; in-process plugin tools register before `DynamicSkill`; a tool name claimed twice refuses startup; the core test suite runs with no plugins enabled.
- **3d declarative tools and `manage_plugin`.** `DynamicTool` and the Rust-literal built-ins in `seed_built_ins` become bundle `tools:` entries; `manage_plugin` writes runtime bundles.

**3a result (2026-10-10).** Built as designed. `services/plugins.rs` (manifest, `requires`, load order, `GET /api/plugins`, ON-CREATE seeding, availability) replaced `source_seeder`, `media_source_seeder` and `mcp_server_seeder`; ten `*_DIR` env vars became `PLUGINS_DIR` + `PLUGINS_ENABLED` (plus a dead `RELIEFWEB_APPNAME`). `Workflow` gains `plugin` and `unavailable`; the Feed's broken-workflow digest now counts the stored reason instead of recomputing it. Verified live: core loads 33 workflows, 18 profiles, 3 schemas; a throwaway `probe` bundle loaded, its workflow ran by name, and after a restart without it the workflow was refused with "plugin 'probe' is not enabled" (probe removed). The first start flagged one real case: `self-test-dead-tools`, a runtime-owned schedule from April in the old `tool`/`args` shape naming the removed `list_notes`, already disabled. Rule: `dev_docs get plugins/loader`.

**3b result (2026-10-10).** Built as designed (`services/profiles.rs`). Verified live: all 18 profiles seeded yaml-owned; `general` renders its four sections and a generated 31-tool catalogue (22,981 chars, from 21,031 with the hand-written list of 19); a description-only save of `deep-diver-hard` kept `explicit_only` and its tools, went runtime-owned, and reset restored the file; deleting a yaml-owned profile is refused with its path; a runtime profile was created and deleted without touching the repo; a probe plugin's `extends: general` added the section whose tool exists and not the one whose tool does not, and both vanished after a restart without the plugin. The startup lint found `schedule-author` telling its model to check tools "with `context`" (removed in Phase 0); fixed. Rule: `dev_docs get profiles/graph`.

**3c result (2026-10-10).** Built as designed. `osint-data` (640 code lines) left `app`; `app` lost the ACLED skill, service and registration. Verified live: with no credentials the bundle is skipped with "needs env ACLED_EMAIL, ACLED_PASSWORD" and the registry has 61 tools; with placeholder credentials it loads, `acled_events` registers (62) and runs end to end (ACLED rejects the credentials, as it should); `check_plugin_deps.sh` fails a plugin crate given a `models` dependency and passes once it is removed. Found live and fixed: switching a plugin off tombstoned its tool as removed; the self-model sync now records `ToolDef.plugin` and drops such a tool without a tombstone. Rule: `dev_docs get plugins/api`.

**3c design (2026-10-10).** The boundary, proved by a real move: ACLED (`acled_events`, the whole `osint-data` pack now that GDELT and ReliefWeb are gone) leaves `app` for `plugins/osint-data/crate/`.
- `crates/plugin-api` (`agent-brain-plugin-api`): `trait Plugin { name(); tools(&PluginContext) -> Vec<Arc<dyn Skill>> }`, re-exporting the protocol types a tool needs. A plugin crate depends on it and external crates only; `scripts/check_plugin_deps.sh` (in CI) fails otherwise.
- **`PluginContext` grows with its consumers.** It starts with `env` (and the plugin's name), which is all ACLED uses. The plan's list (graph, knowledge, LLM, HTTP, notify) is added pack by pack in Phase 4, each capability over the existing core service, when the pack that needs it moves. Building them now would ship five unused interfaces, which Phase 0 would delete.
- In-tree plugin crates compile into the binary (`services/plugins.rs` lists them) and are instantiated only when their bundle loaded: `requires.env` keeps `osint-data` off until ACLED's credentials are set, as the old `from_env` check did, but now visibly. Their tools register before `DynamicSkill`. **A plugin tool whose name is already taken refuses the plugin, not the brain**: the plugin's tools are left out, the reason is logged at ERROR and shown by `GET /api/plugins`. A bundle with a `crate/` that this binary did not compile is reported.
- `cargo test --lib -p agent-brain` is the core suite, run with no plugin enabled; each plugin crate carries its own tests.

**3d design (2026-10-10; owner chose bundle Cypher tools and building `manage_plugin` now).**
- Measured: the graph's only `DynamicTool` is the core's own `list_tasks` (one Cypher step, 240 uses); `manage_dynamic_tool` was called 8 times ever (2026-09-07, 09-26) and left nothing behind; 7 of 8 `:Procedure` nodes are orphans from February and March.
- **A `tools/` seed kind:** `{name, description, kind: cypher, input_schema, cypher, limit?}`. It runs through `neo4j_query`'s own read path (read-only, schema guard, LIMIT injection, `{count, rows}`), so `list_tasks` (now `plugins/core/tools/list_tasks.yaml`) answers exactly as before. Arguments become Cypher parameters; every declared property is required or has a default. Tools are read from the loaded bundles at start (like profile extensions); one whose name is taken is refused with the reason. Only `cypher` exists: a kind arrives with its first tool.
- **Deleted:** `DynamicSkill`, `manage_dynamic_tool` (tombstone → `manage_plugin`), the procedure executor, `seed_built_ins`, and the live `:DynamicTool` / `:Procedure` nodes (backed up first). The five `:ApiContext` seeds hardcoded in Rust become an `apis/` seed kind (ON CREATE, like sources).
- **`manage_plugin` (list, get, write, delete)** writes brain-authored bundles to `PLUGINS_RUNTIME_DIR` (`plugins-runtime/` in the repo, so they show in `git status`). Every file is checked by the parser that loads it (workflow steps against the live tools and their schemas, tools against the Cypher guard), and nothing is written unless all pass. **A runtime bundle never loads by itself:** it waits for the owner to add it to `PLUGINS_ENABLED`, and each write puts a review item in the Feed. A plugin can add prompt sections to the front-door profile, so a bundle the brain was talked into writing must not take effect without a person. It cannot touch a bundle it did not write.

**3d result (2026-10-11).** Built as designed (`skills/bundle_tools.rs`, `skills/plugin_manager.rs`). Deleted `DynamicSkill`, the procedure executor, `manage_dynamic_tool` (tombstoned; `execute_procedure`'s tombstone now points at `manage_plugin`), `/api/tools/dynamic`, snapshot procedures and the `DynamicTool`/`Procedure` constraints; `seed_built_ins` became `seed_bundles`. `scripts/phase3d_cleanup.cypher` removed the 9 nodes (backup in `backups/`). Verified live:
- `list_tasks` from `plugins/core/tools/` returns the same `{count, rows}` with the same 12 columns.
- The five ApiContexts are unchanged.
- The registry shows `list_tasks` as a core declared tool.
- One bad `manage_plugin` write was refused with all four problems listed: a core-only kind, a writing query, a taken tool name, an unknown step tool. A shipped name was refused.
- A valid bundle (a declared tool plus a workflow calling it) was written with a Feed item and stayed unloaded. Added to `PLUGINS_ENABLED`, it loaded and its tool answered. Deleted and restarted without it, the tool was gone with no tombstone and its workflow was marked unavailable (probe removed).
- The profile lint flagged `toolsmith`, which was still about dynamic tools; it is rewritten around `manage_plugin`.

Rules: `dev_docs get plugins/declared-tools`, `dev_docs get plugins/manage-plugin`.

**3a decisions.** Directory names stay as they are (`contexts/`, `sources-media/`): renaming to `agents/` is churn the code does not need. `architecture/` (components, layout, tombstones) is read from the core bundle only: it describes the core. Rule notes that cite `workflows/*.yaml` stay correct, read as bundle-relative.

**3b design (2026-10-10).**
- `(:ContextProfile {name})` (until now a mirror rebuilt by the self-model sync) holds the profile: `definition` (the profile as JSON), `managed_by`, `plugin`, `source` (the file). Startup seeds it from every loaded bundle's `contexts/` like workflows: absent or yaml-owned → written from the file; runtime-owned → left alone (WARN); a yaml-owned profile whose file is gone (plugin disabled, file deleted) is removed. The service loads profiles from the graph; with no Neo4j it still reads the files.
- The HBI writes the graph, never the repo: a save lays the body over the stored profile (fields a form does not know survive; today a UI save drops `explicit_only`, `retrieval_exclude`, `side_effects`), sets `managed_by: runtime`, and `POST /api/contexts/{name}/reset` hands it back to its file. Deleting a yaml-owned profile is refused with the file's path: it would come back on restart.
- **Sections.** A profile may carry `sections: [{name, text, requires: [tools]}]` after its `system_prompt`; a section whose tools are not all available to the profile (allowlist ∩ registry) is left out. `generate: tools` renders the tool catalogue from the allowlist and the registry descriptions. A bundle file with `extends: <profile>` adds sections to another profile; they are rebuilt from the loaded bundles at every start, so disabling the plugin removes them.
- **Lint.** At startup each profile is checked against the registry and the `:RemovedTool` tombstones: allowlist names not registered, and prose that names a removed tool (with its `replaced_by`). Reported in the log and by `GET /api/contexts`.
- `general.yaml` becomes sections: the hand-written "Key tools available" list becomes the generated catalogue (it names 19 of the profile's 31 tools and drifts with every change), and the JOB APPLICATION RULE `requires: [mcp_call]` (it moves to the `personal` pack in Phase 4).

## 8. Phase 4 — extraction, one pack at a time

Order: smallest blast radius first. Each pack: move it, compile against `plugin-api` only, run with it disabled, run `eval-retrieval`, delete what is left behind, re-measure.

| Order | Pack | Contents (≈ lines today) |
|---|---|---|
| 1 | `osint-data` | datasets mod + ACLED (≈1.5K after GDELT/ReliefWeb are deleted) |
| 2 | `media` | MediaService, MediaSkill, transcribe, media sources (3.6K) |
| 3 | `reference` | wiki, wiki background, entity kinds, alias judge (3.6K) |
| 4 | `developer` | codebase, git reads, architecture view (≈3.5K) |
| 5 | `personal` | the owner's seeds: Detroit brief, job search + résumé rules, sports filter, NIA topic, off-grid monitor, his email in User-Agents. YAML only; lives in its own private repo, mounted |
| 6 | `epistemics` | claims, claim rewrite, relations (≈4.4K); in-tree, enabled by default (§2.4) |

Entities, topics, the Feed (items, todos, dispatch), search, HTTP, MCP client, Neo4j/DuckDB query and sandboxed `execute_code` stay core. Topics are generic grouping (a lawyer's matters, a doctor's conditions); revisit only if extraction shows otherwise.

**Exit:** no personal data in the core repo; core ≤ ~60K lines; every pack can be disabled.

**Pack 1, `osint-data`: done in 3c.** The datasets module went with GDELT and ReliefWeb in Phase 0; ACLED is the whole pack.

**Pack 2, `media`: design (2026-10-11).** Measured coupling: `skills/media.rs` (1,216 lines), `services/media.rs` (1,990), `services/transcribe.rs` (110) and `repository/src/media.rs` (286) reach the core through six calls only: graph reads and writes on `:Media`/`:MediaSource` (and two `:Task` reads), `store_note`, LLM `generate`/`generate_json`, telemetry's `log_knowledge_gap`, `workflows::start_run`, and the local-day clock. No REST route or HBI view is media's; `search.rs` (the graph-first "watched already?" block) and the Feed's `media_ingested` count read `:Media` and simply find none when the pack is off.
- **`PluginContext` gains typed host capabilities**, each a trait in `plugin-api` implemented by the app over the existing service (`services/plugin_host.rs`): `graph` (Cypher with JSON params and JSON rows; reads and writes, never schema-free: a plugin's writes are its own labels by convention, checked in review), `knowledge` (`store_note`), `llm` (`generate`, `generate_json`, through the shared routed provider), `workflows` (`start_run`), `telemetry` (`log_gap`), `clock` (`local_day_start_utc`), and `files(kind)` (every loaded bundle's files of a seed kind). Only what media uses; the next pack adds its own.
- **`Plugin` gains optional hooks:** `start(ctx)` (async, after the registry is built: media creates its constraints and indexes and seeds `:MediaSource`), `seed_kinds()` + `check_seed(kind, text)` (the seed kinds a plugin owns; `manage_plugin` asks it to check a file of that kind), and `retrieval_tools()` (tools that count as retrieval for the chat write guard, which listed `ingest_media` by name).
- **`sources-media/` becomes the media plugin's kind**, read from every loaded bundle (a `personal` pack can add channels); the core seeder no longer writes `:MediaSource`. The `Media`/`MediaSource` constraints and indexes leave `init_schema` for the plugin's `start`.
- **Moves:** the four media workflows (`video-learning`, `article-learning`, `media-watch`, `discover-media-sources`) and `sources-media/*.yaml` to `plugins/media/`; `spawn_gap_tasks` goes with them (only the media workflows call it). `yt-dlp`/`ffmpeg` stay in the image. Compose enables `media` by default, so behaviour is unchanged.
- **Check:** with `media` off, the registry loses its six tools, the four workflows show unavailable, nothing warns; with it on, `ingest_media` on a real video and `poll_media_sources` behave as before; `eval-retrieval` unchanged.

**Pack 2 result (2026-10-11).** Built as designed. `plugins/media/crate/` holds the skill, service, transcriber and a new `store.rs` (the repository's `:Media`/`:MediaSource` queries, rewritten over the host graph); `app` and `repository` lost all four files, the media constraints and indexes, the `sources-media` seeder and `ingest_media` in the write guard's list. neo4rs's `json` feature is on (JSON params → Bolt). `cargo test --lib` at the root runs 847 core + 36 media tests; `check_plugin_deps.sh` passes for both plugins. Verified live:
- With `media` on, it loads and registers its six tools; `start` created its schema and found the 32 sources already seeded. `manage_media_source list` reads through the host. A known video was deduplicated. A fresh 38 North article went through the whole path in 53 s: fetch, LLM summary through the routed provider, a `:Media` node with a native `ingested_at`, linked `FROM_SOURCE` to `38-north`.
- With `media` off, the six tools were gone with no tombstones, the four workflows were marked unavailable with their reasons, and no profile warned. With it back on, all four were available again.
- `eval-retrieval`: recall@10 0.880, MRR 0.800, unchanged.

Rule: `dev_docs get plugins/api` (host capabilities, `start`, owned seed kinds, retrieval tools).

**Pack 3, `reference`: design (2026-10-11; owner chose plugin routes nested under the plugin, the alias judge as its own plugin, and the HBI left in place).** Measured: `services/wiki.rs` (1,362 lines), `entity_kinds.rs` (801), `wiki_background.rs` (573), `skills/wiki.rs` (170), `clients/wiki.rs` (253, ten REST routes) and `alias_judge.rs` (391). Data: 501 entities checked against Wikidata, 357 `REFERS_TO` links, 275 entities with a `kind` (274 from references, 1 by hand); `link_entities` ran in 93 turns (the `entity-linking` workflow), `wiki_background` in 51. Unlike media, core code *calls into* this pack: `entity_resolution` ranks merge candidates with `alias_judge::rank`, `main.rs` spawns the judge, and the HBI's entity drawer, merge review, relation review and related-entities views call the wiki routes.
- **Routes.** `Plugin::routes(ctx) -> Option<axum::Router>` (plugin-api re-exports `axum`). The host serves every accepted plugin's router under `/api/plugins/<name>/…` through one dispatch route, so a plugin cannot collide with a core route, its routes vanish with it (404 "plugin not loaded"), and the API-key layer covers them. The ten routes become `/api/plugins/reference/entities/{id}/wiki…`, `/entities/types`, `/entities/{id}/type…`, `/wiki/describe`, `/wiki/coverage`.
- **New host capabilities** (only what these two use): `entities` (the merge-candidate list and the demonym words from the core's entity rules), `busy()` (foreground work is running: idle loops back off), `llm.model_name()` (the step-pinned model, for provenance), `workflows.enqueue_tool_step(tool, args)` (the on-demand lookup reuses the `entity-linking` step and its pinned model), `knowledge.push_session_entry` (`wiki_background` banks into a session), and `ctx.root()` (the bundle's folder: `kinds.yaml` moves from the core's `entities/` into `plugins/reference/`).
- **A neo4rs-shaped shim in plugin-api** (`query(..).param(..)`, `execute`, `run`, `row.get::<T>`) over the JSON graph, so the ~37 typed query sites port line for line; packs 4 and 6 reuse it.
- **`alias-judge`, its own plugin:** the idle-time loop moves to its `start`; `rank` (ordering stored verdicts) stays in core `entity_resolution`, so with the plugin off candidates are simply unranked.
- **`reference`:** the two tools, the routes, entity kinds, and the `entity-linking` workflow. Core keeps reading what is stored (`kind`, `REFERS_TO`, the snapshot's manual kinds) and finds nothing new when it is off.
- **HBI:** URLs updated; a 404 from a plugin route renders as "reference lookups are off" rather than an error. The frontend split stays open (§10).

**Pack 3 result (2026-10-11).** Built as designed, with three differences:
- **The HBI uses `pluginApi(name)`.** Wiki hints in core review lists fall back to none on a 404. The drawer's reference and type sections show the brain's own message ("plugin 'reference' is not loaded").
- **Background loops are gated.** `ctx.background_allowed()` gates them because the judge used to start only under the HTTP transport.
- **The dispatch route forwards a fresh request.** Found live: the outer route's path parameters, left in the request extensions, made the plugin's `Path` extractor fail with a 500.

Results:
- `app` lost six files (3,550 lines) and ten routes. `kinds.yaml` moved to the bundle root. One obsolete test that checked `kinds.yaml` against the alias seed was dropped. Clippy's last warning (in the judge's test) went with the move.
- Tests: 826 core, 17 reference, 3 alias-judge and 36 media; `check_plugin_deps.sh` passes for four plugins.

Verified live:
- All three plugins load. The judge started (`nimble`).
- With the old routes captured first, the new `entities/{id}/wiki`, `wiki/coverage` and `wiki/describe` answer byte-identically. `entities/types` now answers where the old binary was already failing on the moved file.
- An on-demand lookup went route → `enqueue_tool_step` → a job on the step's pinned `gpt-oss:120b-cloud` → `link_entities` → stored (`none`, 7 candidates).
- Later lookups were refused by the routing policy: the Ollama Cloud account limit was reached until 11:51 UTC. That is the existing quota behaviour; the proposal path waits for quota.
- A manual type set and unlock round-tripped; the test entity was restored.
- With `reference` and `alias-judge` off: the tools were gone with no tombstones, `entity-linking` was unavailable, the routes answered 404 with the reason, the judge did not start, and no profile warned. Back on, all available.
- `eval-retrieval` 0.880 / 0.800, unchanged.

Rule: `dev_docs get plugins/api`.

## 9. Phases 5–6, and measurements

**Phase 5 — chat as a modal** (independent of the others; can run any time). Ctrl-K stays navigation. **Ctrl-J opens an Ask modal** with an agent picker (default the front-door profile), the session list, and the context dock. The Chat workspace and `ChatPanel` go. Old `#/chat…` links and `notify_user`'s `related_session_id` (e.g. a resumable `jobsearch-{{date}}` session) open the modal on that session; nothing that links to chat today may dead-end.

**Phase 6 — consolidation to the second target.** Only after Phase 4 measures what is left. Known candidates: four provider chat loops (`anthropic_`, `ollama_`, `openai_`, `text_loop`) → one OpenAI-wire loop + the Anthropic adapter; the LLM stack (≈7.4K: llm, providers, routing, budget, gate, registry, model config/router, shared_llm); `rest.rs` (2.6K) and per-feature REST clients; search engines as YAML like LLM providers; hand-written tool schemas; splitting `app` into core crates under the per-crate budget.

**Measurements.**

| Date | Phase | Core Rust | of which tests | Plugins Rust | Frontend | Docs loaded up front | Core tools |
|---|---|---|---|---|---|---|---|
| 2026-10-10 | baseline | 95.6K | 17.4K | — | 19.4K | CLAUDE.md 7.7 KB (points into 1.18 MB of docs) | 104 |
| 2026-10-10 | Phase 0 | 87.7K (code 71.1K) | 16.6K | — | 19.3K | CLAUDE.md 7.7 KB (docs −2 stale registries) | 62 |
| 2026-10-10 | Phase 1 | 88.3K (code 71.7K) | 16.7K | — | 19.3K | CLAUDE.md 7.6 KB; project-docs 1.19 MB → 142 KB | 63 |
| 2026-10-10 | Phase 2 | 88.1K (code 71.4K) | 16.7K | — | 18.8K | CLAUDE.md 7.6 KB | 61 |
| 2026-10-10 | Phase 3a | 88.6K (code 71.8K) | 16.8K | — | 18.8K | CLAUDE.md 7.6 KB | 61 |
| 2026-10-10 | Phase 3b | 89.5K (code 72.5K) | 17.0K | — | 18.9K | CLAUDE.md 7.6 KB | 61 |
| 2026-10-10 | Phase 3c | 89.1K (code 72.1K) | 17.0K | 0.8K (osint-data) | 18.9K | CLAUDE.md 7.6 KB | 61 (+1 with osint-data) |

## 10. Open questions

- Project name — separate session.
- Plugin frontend views: generic plugin panels driven by declared queries, or plugin-owned components?
- `event` triggers: which events (item created, note stored, claim contested)? Not until a workflow needs one.
- Epistemics in core — the owner may revisit after Phase 4.
- Proposals folded into the Feed — done in Phase 0; owner may still prefer files back (§4).

## 11. Working method

Each phase is on `dev`, ends with a §9 measurement row, the docs updated (spec + DevNotes, not new markdown), and a commit message staged for the owner to sign. A phase does not start before the previous one's dead code is gone.
