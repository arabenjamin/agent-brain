# Core and plugins plan

Status: **Phase 0 done 2026-10-10 (staged, not yet deployed); Phases 1–6 planned.** Decided in discussion with the owner; the decisions are in §2 and are not to be re-opened without him.

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

## 6. Phase 2 — workflows: one concept for chains and schedules

**The difference today.** Both are lists of `ChainStep` run by the same queue. They differ on two unrelated axes:

| | Chain (`:SchedulerChain`) | ScheduledTask |
|---|---|---|
| When | a one-off Task whose goal text matches a regex `pattern` (`goal_to_steps`) | a timer (`interval_seconds`) |
| Checks | evaluator, adversarial critic, auto-retrieval appended | verbatim, none |
| Where steps live | template in `chains/` | inline in `schedules/` |

Agents (`run_agent` + a profile) are a third way, where the model chooses the steps.

**The model.** Three separate things:

- **Workflow** — *what*: fixed steps, or `agent:` + `goal:`. Declares `inputs`, `data_class`, `checks`, and step `defaults`.
- **Trigger** — *when*, a list on the workflow: `manual`, `schedule` (interval), `match` (free-text goal pattern, the old chain routing, kept for `create_task` from chat), and later `event`.
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
- Plugin tools register before `DynamicSkill`, which stays last.
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

## 9. Phases 5–6, and measurements

**Phase 5 — chat as a modal** (independent of the others; can run any time). Ctrl-K stays navigation. **Ctrl-J opens an Ask modal** with an agent picker (default the front-door profile), the session list, and the context dock. The Chat workspace and `ChatPanel` go. Old `#/chat…` links and `notify_user`'s `related_session_id` (e.g. a resumable `jobsearch-{{date}}` session) open the modal on that session; nothing that links to chat today may dead-end.

**Phase 6 — consolidation to the second target.** Only after Phase 4 measures what is left. Known candidates: four provider chat loops (`anthropic_`, `ollama_`, `openai_`, `text_loop`) → one OpenAI-wire loop + the Anthropic adapter; the LLM stack (≈7.4K: llm, providers, routing, budget, gate, registry, model config/router, shared_llm); `rest.rs` (2.6K) and per-feature REST clients; search engines as YAML like LLM providers; hand-written tool schemas; splitting `app` into core crates under the per-crate budget.

**Measurements.**

| Date | Phase | Core Rust | of which tests | Plugins Rust | Frontend | Docs loaded up front | Core tools |
|---|---|---|---|---|---|---|---|
| 2026-10-10 | baseline | 95.6K | 17.4K | — | 19.4K | CLAUDE.md 7.7 KB (points into 1.18 MB of docs) | 104 |
| 2026-10-10 | Phase 0 | 87.7K (code 71.1K) | 16.6K | — | 19.3K | CLAUDE.md 7.7 KB (docs −2 stale registries) | 62 |

## 10. Open questions

- Project name — separate session.
- Plugin frontend views: generic plugin panels driven by declared queries, or plugin-owned components?
- `event` triggers: which events (item created, note stored, claim contested)? Not until a workflow needs one.
- Epistemics in core — the owner may revisit after Phase 4.
- Proposals folded into the Feed — done in Phase 0; owner may still prefer files back (§4).

## 11. Working method

Each phase is on `dev`, ends with a §9 measurement row, the docs updated (spec + DevNotes, not new markdown), and a commit message staged for the owner to sign. A phase does not start before the previous one's dead code is gone.
