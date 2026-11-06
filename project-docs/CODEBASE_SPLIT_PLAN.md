# Codebase Split Plan: small units, one schema home, stamped plugins

**Status: planned 2026-11-05 from todo 9e1a512f. § 8 decided by the owner
the same day (the recommendations, 1–6; 7 waits on Phase 3's list). Phase 0
built 2026-11-06. Phase 1 built 2026-11-06: tool, graph, DuckDB and seed schemas, and closed lists. Phase 2 under way: the template, its script and its check built 2026-11-06; linking the 13 older plugins to it started the same day (websocket first).**

The owner's goal (2026-11-05): keep the code small enough that an LLM can pull
what it needs into context. The repo only grows, so break it up. Alongside:
give the schemas one home so they're easy to review, change and validate, and
use cookiecutter so every plugin follows one stamped pattern.

## 1. Where we are (measured 2026-11-05)

### Growth

| Date | Rust, tokens (crates + plugins, tests included) | Everything tracked |
|---|---|---|
| 2026-08-01 | 357K | 669K |
| 2026-09-01 | 483K | 889K |
| 2026-10-01 | 669K | 1.23M |
| 2026-11-01 | 1.11M | 2.37M |
| 2026-11-05 | 1.14M | 2.43M |

Rust has tripled in three months, about +200K tokens a month. Core (what
`scripts/size_budget.sh` counts) went from 777K on 11-03 to 784K; the budget
went from 778K to 900K. At this rate the new budget lasts weeks, not months.

### What "fits in context" means here

The whole of core will never fit: it's 784K tokens. Even a 1M-token window
leaves no room to work. So the useful goal is narrower: **what an agent must
read to change one thing**, meaning one unit plus the contracts it touches,
fits with room to spare. A local 16K model never reads code. The readers that
matter are cloud models with 128K to 200K windows.

On that measure, plugins already work and core crates don't. A plugin is built
against `plugin-api`, whose whole reach (`plugin-api`, `protocol`, `decision`,
`engine`) is **21K tokens**. A core crate can reach everything it depends on:

| Unit | Own code | Code it can reach (own + transitive deps) |
|---|---|---|
| `media` plugin | 40K | 61K |
| `epistemics` plugin | 35K | 56K |
| `llm` | 48K | 121K |
| `knowledge` | 58K | 221K |
| `work` | 102K | 323K |
| `retrieve` | 79K | 402K |
| `api` (HBI REST) | 70K | 419K |
| `skills` | 62K | 560K |
| `host` | 40K | 657K (it compiles in every plugin) |
| `app` | 58K | 916K, all of core and every plugin |

"Can reach" is an upper bound: an agent reads the public items it needs, not
whole crates. But it's the honest measure of how far a change can spread, and
the gap between the plugins (21K of contract) and `work` (323K) is the
problem.

### Where the bulk is

The core files over 15K tokens are `repository/telemetry.rs` 20.9K,
`work/queue.rs` 20.4K, `chat/agent_runner.rs` 20.2K, `work/programs.rs`
19.1K, `retrieve/search.rs` 18.6K, `host/views.rs` 18.2K,
`retrieve/http/ingest.rs` 16.9K, `graph/entity_resolution.rs` 16.6K,
`scheduler/scheduler.rs` 16.1K and `work/validation.rs` 15.8K.

### Unused tools

Of 70 registered tools, **29 ran in no workflow step and no agent run in the
last 30 days**: `select_agent`, `snapshot_architecture`,
`analyze_own_structure`, `get_git_log`, `list_workspace_files`,
`write_codebase_doc`, `write_proposal`, `list_tasks`, `define_api_context`,
`ingest_api`, `adversarial_plan_review`, `synthesize_knowledge`,
`mcp_list_tools`, `geocode`, `discover_media_sources`, `manage_media_source`,
`get_llm_budget`, `list_endpoints`, `reflect_on_work`, `get_search_usage`, the
four `ws_*`, `dead_letter`, `enqueue_jobs`, `manage_job`, `area_traffic` and
`scheduler_control`. Front-door chat turns and direct MCP calls aren't in that
count (`manage_media_source` was called directly today), so this is a list to
review, not a list to delete. Earlier lesson: code shrinks by removing
features, not by deleting dead code.

### Where schemas live today

| Kind | Where | Count |
|---|---|---|
| Tool input schemas | inline `json!` in each skill's Rust | 74 places |
| Seed-file shapes | serde structs with `deny_unknown_fields`, spread over 12 crates | 53 structs |
| Seed JSON Schemas | `plugins/core/schemas/` (`workflow.json`, `program.json`), checked on save | 2 |
| Neo4j constraints and indexes | Rust string constants in 8 files (`repository/client.rs` and plugin stores) | 48 statements |
| DuckDB tables | `repository/telemetry.rs`, `maps/tracks/store.rs` | 14 tables |
| Graph docs | `project-docs/schema.md`, by hand | |
| HBI types | `hbi-frontend/src/lib/*.ts`, mirrored by hand | |

Today's example: adding the claim status `contested` meant editing the Rust
enum, the `lookup` view's status list (`host/views.rs`), `status.ts` and
`schema.md`. That's four copies of one closed list, and nothing checks they
agree.

### How uniform the plugins are

There are 13 plugins: 10 with a Rust crate, 3 data-only (`github`, `harness`,
`ml-lab`). They share `plugin.yaml` and the `Plugin` trait, but nothing else
is fixed. Seed directories are named by habit (`sources`, `sources-media`,
`layers`, `kinds.yaml`, `parallel.yaml`). Crate modules differ (`store.rs` vs
`db.rs` vs `runs.rs`, `skill.rs` vs `claim_skill.rs`). Two have their own
Python or Rust sidecar directories. Adding a Rust plugin means editing three
places by hand: `crates/host/Cargo.toml` (dependency and feature) and
`crates/host/src/plugins.rs`.

## 2. Principles

1. **The unit of work is what must fit, not the repo.** Every unit you'd
   change on its own (a plugin, a core crate, a service) plus the contracts it
   touches stays under a stated token limit (§ 8.1).
2. **A boundary is a contract.** It pays off when the reader can stop at it:
   `plugin-api` is 7.6K tokens and nobody reads the host behind it. A split
   with no narrow contract only moves lines.
3. **Processes split for runtime reasons, not for reading.** Context size is
   won at the contract. A network boundary adds latency, failure modes,
   versioning and deployment, all on one home machine. Take that cost only
   where the runtime gains something (isolation, its own resources, its own
   lifecycle), and measure it on a pilot first.
4. **One closed list, one file.** A schema, status list or table definition
   lives in one place. Everything else is generated from it or checked
   against it.
5. **A plugin is stamped, not hand-built.** The template decides the layout,
   and a check catches a plugin that drifts from it.
6. **Removing beats moving.** Before moving a subsystem, ask whether it's
   used.

## 3. On microservices

The todo says "break the monolith into microservices". My recommendation is a
**modular core built from plugins, with a process boundary piloted once**,
not microservices across the board:

- **What the goal needs:** narrow contracts. Plugins prove it: 21K of
  contract, and the unit stays small. Turning core subsystems into plugins
  shrinks core and caps reach without a network.
- **What microservices add:** one more container per service, a host API over
  the wire (the graph, the note store and model calls, with privacy and
  budget still decided by the brain), versioning between services, and
  partial failures. On an 8 GB GPU and one Neo4j, every service still shares
  those two resources, so isolation buys little.
- **Where a process boundary is right:** the brain already runs eight
  sidecars (llama.cpp, whisper, both sandboxes, yt-proxy, ml-runner, the HBI,
  Neo4j). Each one has a runtime reason. A plugin with its own long-running
  loop and its own store, like the maps track collectors, is the next
  plausible case.
- **The wire format exists.** The brain is both an MCP server and an MCP
  client (`mcp_call`, `:McpServer`). An out-of-process plugin is an MCP
  server the brain calls, so Phase 4 tests that path, with numbers.

## 4. Phases

### Phase 0: measure what one change must read (built 2026-11-06)

1. ✅ `scripts/size_budget.sh` prints each unit's own code and its reach: its
   own code plus every workspace crate it depends on, transitively, through
   normal path dependencies read from `cargo metadata`, each crate counted
   once. That covers 27 units (17 core crates, 10 plugin crates), sorted by
   reach.
2. ✅ The limits are in `scripts/unit_token_limits` (own 60,000, reach
   100,000; § 8.1). A unit over either limit is flagged. They're reported,
   not gated, until the over-limit units have a Phase 3 plan. The core budget
   is unchanged, and the budget line stays last, so the pre-commit hook reads
   it as before.
3. ✅ It also prints the plugin contract (what `plugin-api` reaches) and the
   growth of Rust code by month from git history, skipped in a shallow clone
   (CI's checkout).

First run, 2026-11-06: **11 of 27 units are over a limit, all of them core
crates**: `app`, `host`, `skills`, `api`, `retrieve`, `chat`, `scheduler`,
`work`, `knowledge`, `graph`, `llm`. Every plugin passes; the largest,
`media`, has 40K own and 62K reach. The plugin contract is 21,462 tokens.
`repository` (49K own, 52K reach) is the largest core crate under both limits.

### Phase 1: schemas in one place per bundle

Every bundle gets one `schemas/` directory: `plugins/core/schemas/` for core,
`plugins/<name>/schemas/` for a plugin. The directory already exists and is
already loaded and checked; it just holds two files today. Inside it:

| Subdir | Holds | Source of truth |
|---|---|---|
| `tools/<tool>.json` | each tool's input schema, descriptions included | the file. Rust loads it with `include_str!`, so nothing changes at runtime |
| `seeds/<kind>.json` | each seed kind's shape (workflows, views, apis, sources-media, …) | the Rust struct. The JSON is generated (`schemars`) and committed, so it can be reviewed and diffed |
| `graph/*.cypher` | Neo4j constraints and indexes, per bundle | the file. `init-db` and plugin `start` run it |
| `duckdb/<database>.sql` | DuckDB tables, one file per database (`telemetry.sql`, maps' `tracks.sql`) | the file |
| `lists/<name>.json` | closed lists (claim statuses, source tiers, note types) | the Rust constant or enum. The file is written from it by a test; tool schemas fill from the constant, the HBI is generated from the file |

1. ✅ **Inventory**, by a script: `scripts/check_schemas.sh --inventory`
   counts each kind of schema in files and still inline.
2. **Move by kind**, one commit each: tools first (the most reviewed text,
   the descriptions models read), then graph, telemetry, seeds, lists.
   - ✅ **Tools, built 2026-11-06.** 66 tool schemas moved to
     `schemas/tools/<tool>.json` in 8 bundles, loaded with `tool_schema!(core
     "name")` / `tool_schema!(plugin "name")` (`crates/protocol/src/tool_schema.rs`)
     through `include_str!`, so a missing file fails the build. Seven use
     placeholders: a value the code owns is written `"${NAME}"` and filled
     from the Rust constant (`"TODO_STATUSES" => TODO_STATUSES`), so the
     constant is still the one source. The conversion was proven exact: a
     temporary test per tool asserted that the old `json!` value and the
     loaded file are equal (66 of 66), and then it was removed. Five schemas
     stay in Rust because they're built from data at runtime: `lookup` (the
     views), `search_web` (the engine seeds), `deep_research` (its
     processors), `execute_code` (the sandboxes), `ws_receive` (a formatted
     default). `list_tasks` is a declared tool, and its schema was already in
     its YAML. Effect: core code went from 728,085 to 714,687 tokens, and
     `skills` dropped under the own-code limit (62K to 52K). The text now
     counts as core data, so core's total barely moves; what an agent changing
     the handler must read shrinks.
   - ✅ **Graph, built 2026-11-06.** The 47 Neo4j constraints and indexes
     (the inventory first said 48, but one was a DuckDB index) moved from Rust
     strings in 8 files to `schemas/graph/schema.cypher` in 5 bundles (core 33,
     developer 5, media 5, maps 2, research 2), with their comments.
     `graph_schema!(core)` / `graph_schema!(plugin)` reads the file at compile
     time (`crates/protocol/src/graph_schema.rs`). Core's
     `Neo4jClient::init_schema` and each plugin's `start` apply it, the plugins
     through one helper, `plugin_api::graph::apply_schema`, where there were
     four styles before. Proven equal: every bundle's parsed statements matched
     the old Rust strings in order (47 of 47). `repository` gains a dependency
     on `protocol` for the reader (its reach goes from 52K to 60K, still under
     the limit). Compared with the live graph:
     - six file indexes live under auto-generated names (an equivalent
       already existed), so nothing is missing;
     - `task_created` was live and documented but in no code. It's added, so
       a fresh install gets it (48 statements now);
     - **the original fork's retired labels** held 7 constraints, 6 indexes
       and 185 nodes (`Parameter` 117, `Schema` 66) that no code touched.
       Dropped 2026-11-06 on the owner's word (§ 8.8), after a backup.

     `schema.md`'s constraint and index tables had drifted (a documented
     index no code created, a dozen real ones missing). They're replaced by a
     pointer to the files.
   - ✅ **DuckDB, built 2026-11-06.** The two databases' schemas moved from
     one raw SQL string each to `schemas/duckdb/<database>.sql`:
     `plugins/core/schemas/duckdb/telemetry.sql` (10 tables, 21 `ADD COLUMN
     IF NOT EXISTS` migrations, with their comments) and
     `plugins/maps/schemas/duckdb/tracks.sql` (3 tables and an index). The
     directory is `duckdb/` named by database, not `telemetry/` as § Phase 1's
     table first said, because the maps file isn't telemetry.
     `duckdb_schema!(core "telemetry")` / `(plugin "tracks")` reads each at
     compile time, and `execute_batch` runs it as before. Proven equal line by
     line (170 and 16 lines). The 19 telemetry tests that open a real
     in-memory database pass. The inventory's 14 tables were 13; the 14th was
     a test's `CREATE TABLE zz`.
   - ✅ **Seed shapes, built 2026-11-06.** 22 JSON Schemas generated from
     the Rust types that parse each seed kind, committed as
     `plugins/<bundle>/schemas/seeds/<kind>.json` (core 16, maps 2, media 2,
     epistemics 2). `schemars` 1.2 is a dev-dependency only: the types carry
     `#[cfg_attr(test, derive(schemars::JsonSchema))]`, so it never reaches
     the binary (`cargo tree -e normal` has no `schemars`). One test per
     owning file (13 tests in 8 crates) regenerates each schema. It fails if
     the committed file differs (`SCHEMA_REGENERATE=1` rewrites it), and it
     validates every shipped seed file of the kind against the schema: 106
     files on 2026-11-06, 22 of them the private bundle's, which is checked
     when it's checked out beside the repo (CI has only the repo's). The
     shared check is `protocol::seed_schema_check`, behind a test-only
     feature that plugins reach through `plugin-api`. Doc comments become
     `description`s, `deny_unknown_fields` becomes `additionalProperties:
     false`, and defaults and required fields carry over.
     - Validation caught the generator's first mistake: `contexts/` holds
       three shapes (profiles, the boot/init protocols, and profile
       extensions with `extends:`). They're split as the loader splits them,
       into three schemas.
     - Two fields are another crate's type (`data_class`,
       `retrieval_exclude` on profiles), which a test-only derive can't
       reach. They were opaque in the schema until the closed-lists step (below).
     - **Seven kinds accept unknown keys** (no `additionalProperties:
       false`): `contexts`, `topics`, `entities`, `mcp-servers`, `sources`,
       and the protocol and topic-scope files. A typo in an agent profile is
       ignored, not reported. No shipped file has an ignored key today, so
       making them strict breaks no seed. The profiles stored in the graph
       need the same check first (§ 8.9).
     - Coverage: `check_schemas.sh` holds a table naming each seed directory
       and bundle-root file's schema. A new kind without a line fails. Still
       without a schema: `architecture` (core's self-model files) and four
       single files (`relations.yaml`, `kinds.yaml`, `parallel.yaml`,
       `layout.yaml`). `workflows` and `programs` keep their hand-written
       schemas, which are checked on every save.
   - ✅ **Closed lists, built 2026-11-06.** Ten lists, each written by its
     owning crate's test to `plugins/<bundle>/schemas/lists/<name>.json`
     (`{name, description, values}`) and drift-checked like the seed
     schemas: core's `note-types`, `code-written-note-types`,
     `todo-statuses`, `todo-origins`, `program-kinds`, `program-statuses`,
     `workflow-kinds`, `data-classes` (`crates/work/src/closed_lists.rs`),
     epistemics' `claim-statuses` (from `ClaimStatus::ALL`; an exhaustive
     match makes a new variant fail to compile until it's listed) and media's
     `source-types`. The Rust constant stays the source, not a YAML file as
     the table first said: the code needs the values at compile time, and a
     list read from a file would need a second check that the code handles
     every value. Every whole copy now takes the list or is checked against
     it:
     - tool schemas: `claim` status, `store_note` note_type (the list minus
       the two code-written types, as before), `manage_workflow` kind and
       data_class are `"${NAME}"` placeholders, proven equal to the old
       literals;
     - the hand-written `program.json` and `workflow.json` keep their enums
       (other code serves them as files), compared with the list by a test;
     - the HBI imports `src/lib/lists.generated.ts` (`scripts/gen_lists.sh`).
       `status.ts`, `noteTypes.ts`, the todo model and schedule kinds use
       `satisfies Record<List, …>`, so a new value fails `tsc` until it has
       a colour or label;
     - core's `lookup` view can't import the plugin's enum; its copy is
       checked against the epistemics file.

     The check found two copies that had drifted. `lookup`'s claim statuses
     lacked `disputed`, so a view filtering on it was refused (fixed). The
     HBI's note colours lacked `news_raw` and `meta_learning_result` (now
     named, still muted). The profile fields left opaque by the seed step
     (`data_class`, `retrieval_exclude`) are now described from their lists,
     so a profile naming an unknown class or note type fails the seed test.
     `check_schemas.sh` checks each list file's shape and owner, that the
     generated TS is current, and that no tool schema or HBI file spells a
     whole list out. Generated files (`schemas/seeds/`, `schemas/lists/`)
     no longer count against the core budget (owner, 2026-11-06): core is
     787,284 tokens, with 17,220 generated reported apart.
3. **One check**, `scripts/check_schemas.sh`, run in CI and pre-commit.
   ✅ Built for tools: every file is JSON and loaded by exactly one call, its
   placeholders are exactly the ones the call fills, and no schema is written
   inline outside test code except the five listed; each file is checked
   against the JSON Schema meta-schema by a unit test
   (`crates/host/src/schema_files.rs`). ✅ Then, as each kind moved: graph
   and DuckDB statements idempotent and read by their bundle; every shipped
   seed file validated against its kind's schema; committed generated files
   matching a fresh generation; each closed list matching what reads it (the
   `contested` case). Not done: parsing Cypher and SQL (the live start is
   the parse), and "every registered tool has exactly one schema file",
   which the one-call-per-file rule covers for all but the five data-built
   schemas.
4. ✅ `schema.md` points to the files for constraints and lists instead of
   repeating them.

Done when: adding a claim status is one edit plus regenerating, and the check
fails if a copy disagrees.

### Phase 2: one plugin template, stamped with cookiecutter

1. **`templates/plugin/`**, a cookiecutter template with two variants
   (`kind: data` and `kind: rust`):
   - `cookiecutter.json`: `name`, `description`, `kind`, `seed_kinds`
     (multi-choice), `owns_graph_labels`, `needs_env`;
   - the layout: `plugin.yaml`, `README.md`, `schemas/` (§ Phase 1),
     `workflows/`, `contexts/`, `views/`, and for Rust `crate/` with
     `lib.rs` (the `Plugin` impl), `skill.rs` (tools), `store.rs` (graph
     access) and `tests`;
   - a `post_gen_project` hook that does the deterministic edits a person
     forgets: registers a Rust plugin in `crates/host/Cargo.toml` and
     `plugins.rs`, runs `cargo check -p`, `check_schemas.sh` and the
     shipped-seed test, and prints what is left to do.
2. **Run it without installing anything on the host:**
   `uvx cookiecutter@2.7.1 templates/plugin` (cookiecutter 2.7.1, Python ≥
   3.10; the host has `uv` and Python 3.11). It's wrapped as
   `scripts/new_plugin.sh`.
3. **Keep plugins on the template with `cruft`** (2.16.0): each plugin
   records the template commit it came from in `.cruft.json`. `cruft check`
   in CI flags a plugin that has drifted, and `cruft update` brings a
   template change to every plugin as a reviewable diff. That makes the
   uniformity hold over time, not only at creation.
4. **Re-stamp the 13 existing plugins**, one per commit, onto the template's
   layout (module names, seed directory names, `schemas/`). Each move goes
   through the shipped-seed test and the full unit tests.
5. **The brain's own use** (§ 8.6): a `scaffold_plugin` step that stamps
   from the same template into a staging directory (not the repo) inside the
   Python sandbox, which has no network. It validates the result and offers
   the diff to the owner. This keeps § 9.4 of OUTSIDE_REVIEW_PLAN: the brain
   never writes or commits a repo file. It's what api-integrator or a program
   review would use when a proposal needs a new bundle.

✅ **Steps 1–3 built 2026-11-06**, with four changes from the text above,
each found by trying it:
- **The repo edits are the script's, not a hook's.** cruft re-runs the
  template's hooks on every update, inside a temporary copy, so a hook that
  edits `crates/host` would run in the wrong place. The hooks only check the
  answers (`pre_gen_project.py`: names, kinds, a data plugin with tools) and
  shape the plugin itself (`post_gen_project.py`: drop `crate/` for a data
  plugin, write one `schemas/tools/<tool>.json` per tool and the graph file).
  `scripts/plugin_template.sh new` does the 8 hand edits a Rust plugin used to
  need (host `Cargo.toml` ×3, `plugins.rs` ×2, the Dockerfile's dependency
  stage ×3), refuses a tool name that is already taken (dispatch is first
  match; it checks the schema files, the Rust names and the live brain), then
  formats and runs the plugin's tests, clippy, `check_plugin_deps.sh` and
  `check_schemas.sh`. The script is `plugin_template.sh new|update|check`
  rather than `new_plugin.sh`.
- **`cruft check` can't see drift with the template in this repo.** It
  compares commit hashes, so every commit, even one that doesn't touch the
  template, makes every plugin "out of date". `plugin_template.sh check` asks
  git instead: did `templates/plugin` change since the commit the plugin's
  `.cruft.json` records? It needs no network or uvx, and it runs in CI (now
  with `fetch-depth: 0`) and pre-commit. cruft stays for `create` and the
  three-way `update`, which keeps the plugin's own edits and marks a
  conflict. `.cruft.json` records the template as `.`, so the file is the
  same on every checkout. cruft reads the committed template, so a template
  change is committed before it's stamped or applied.
- **No `store.rs`, no `seed_kinds` answer.** The stamped crate has to pass
  clippy with `-D warnings`, and an unused graph-access skeleton is dead
  code. The README says where graph access goes. Seed directories are
  created when there's a seed to put in them; `check_schemas.sh` already
  fails on an unknown kind.
- **A stamped tool answers "not built yet" as an error** (`isError: true`)
  until it's written, and its description says so. The plugin isn't on until
  it's in `PLUGINS_ENABLED`.

Tried in a scratch clone: a Rust plugin with two tools, a label and an env
var stamped, registered and passed its 2 tests and every check; a template
change after that was flagged by `check`, and `update` brought it in while
keeping the plugin's own edit; a data plugin got only `plugin.yaml`,
`README.md` and `.cruft.json`; `lookup`, `search_web`, `list_tasks` and
`store_note` were each refused as taken. The Dockerfile edit was checked as
text, not built.

**Step 4, started 2026-11-06: link, don't re-stamp.** A plugin built before
the template is put on it by `plugin_template.sh link <name>`. That reads its
answers from its own files (plugin.yaml's description and env, the tools with
a schema file or an inline one, the labels its graph file names), records them
with `cruft link`, and adds the template's `README.md`. It changes no other
file. Then by hand: the README's layout table lists the real modules, and
`lib.rs` gets the template's `name_matches_plugin_yaml` test. `check` fails on
a bundle with neither a `.cruft.json` nor a place in the script's
`NOT_YET_LINKED`, which loses one name per commit. Two changes from the text
above:
- **No module or seed-directory renames.** Dev notes and history cite paths
  such as `plugins/epistemics/crate/src/claims.rs`, and the seed kinds
  (`sources-media`, `layers`) are read by the private bundle too. A rename
  would break those for a name. The README's layout table says where each
  thing is instead.
- **What's the plugin's own is skipped by updates.** Every `.cruft.json`
  (stamped or linked) lists `crate/src/skill.rs`, `schemas/tools` and
  `schemas/graph` in cruft's `skip`. Once written they're the plugin's, and a
  template change to the skeleton would only conflict. Tried in a scratch
  clone: a template change to `skill.rs` and `README.md` reached the README
  and left the linked plugin's `skill.rs` alone.

The template's dispatch test (call every tool) isn't added to older plugins:
called with no arguments, a real tool may go to the network.

Linked 2026-11-06, in four commits: `websocket`; the data plugins `github`,
`harness`, `ml-lab`; the small Rust ones `alias-judge`, `osint-data`,
`sandbox`, `research`, `reference`. Left, one commit each: developer,
epistemics, maps, media.

Done when: a new plugin is one command, every plugin passes
`plugin_template.sh check`, and none was hand-built.

### Phase 3: shrink core: remove, then move into plugins

1. **The review list.** For each of the 29 unused tools (§ 1) and each
   subsystem over 10K tokens: its use over 60 days (workflow steps, agent
   runs, chat turns and direct calls, all counted), what it would cost to
   remove it, and the plugin it could become. The owner decides each one:
   remove, move, or keep.
2. **Candidates to look at first** (by size and how narrow the edge is; none
   decided):
   - Programs: `work/programs.rs` 19.1K, `skills/program`, the `/api/programs`
     resource.
   - The developer-introspection tools left in core.
   - The HBI REST `api` crate (70K, reach 419K). It's a natural boundary
     because only the HBI uses it, but it calls deep into `work` and
     `scheduler`.
   - Telemetry readers.
3. **Each move goes through the template** (Phase 2) and must cut core's
   measured size. A move that leaves core the same size doesn't count.
4. **When core shrinks, lower `scripts/core_token_budget`** to the new size,
   rounded up, so the space freed isn't refilled by habit.

Done when: core is below 700K tokens, and no core crate is over the reach
limit, or the owner has kept it over with a reason recorded.

### Phase 4: one process boundary, piloted and measured

1. **The host API over the wire.** A narrow set of brain endpoints a service
   may call: the graph read through the schema guard, the note store,
   `generate` through the routing policy (privacy and budget stay in the
   brain, so no service holds a provider key), and telemetry. They're
   versioned, and the brain's API key is bound to the internal network.
2. **A `kind: service` template variant:** an MCP server in its own
   container on `brain-internal`, a `:McpServer` seed, a health check.
3. **The pilot is `osint-data`** (6.4K tokens, tools only, one outside API).
   It's the smallest real plugin, so it isolates the cost of the boundary
   itself. Measure tool latency against in-process, memory, the code it took,
   and how failures look (service down, slow, wrong version).
4. **The decision gate:** stop at one, or take the next with a runtime
   reason. The maps track collectors are the best next case: their own loop,
   their own DuckDB, their own outside feeds.

Done when: the pilot's numbers are in this plan and the owner has called the
gate.

### Phase 5 (optional): plugins in their own repos

`agent-brain-personal` is the precedent. Only if, after Phases 0 to 3, the
monorepo is still a problem for the tools that read it. A plugin in its own
repo is stamped by the same template, pinned by version, and built in CI
against `plugin-api`. Cross-repo changes cost more, so this waits until
`plugin-api` has stopped changing every week.

## 5. What this does not change

- **The rules in CLAUDE.md.** Privacy before budget, routing, workflow step
  rules and timestamps all stay as they are, and a service keeps them because
  it calls the brain for models (§ Phase 4.1).
- **One Neo4j and one GPU.** No service gets its own graph or engine.
- **The brain never writes repo files**
  (OUTSIDE_REVIEW_PLAN § 9.4); `scaffold_plugin` stages a diff.

## 6. Risks

- **The template ossifies a bad layout.** Mitigation: re-stamp two very
  different plugins (`media`, `github`) first, and fix the template before
  the rest.
- **Generated schemas drift from hand-edited ones.** That's why the check
  fails on drift, not warns.
- **A pilot service that works but nobody needs.** The gate is a real stop;
  stopping after one counts as a result.
- **Moving code into plugins grows `plugin-api`.** Every new host capability
  a moved subsystem needs is a contract change; count `plugin-api`'s size in
  Phase 0 and treat it as the scarcest budget.

## 7. Order

**Phase 0 → 1 → 2 → 3**, with Phase 4 any time after Phase 2 (it needs the
template variant) and Phase 5 last, if at all. Phase 1 before Phase 2,
because the template's `schemas/` layout should be the real one. Phase 3
after Phase 2, because each move uses the template.

## 8. Decisions for the owner

**Decided 2026-11-05 (owner): the recommendations, 1 to 6.** Decision 7 is
made per item once Phase 3's 60-day usage list exists.

1. **The limit per unit.** Decided: own code ≤ 60K tokens and reach ≤
   100K, so one unit plus its contracts and a working session fits a 200K
   window and still fits a 128K one. Plugins pass today; `work` (102K own,
   323K reach) and most core crates don't.
2. **Microservices.** Decided: plugins plus one measured process pilot
   (§ 3, Phase 4), rather than committing to services now.
3. **The schema home.** Decided: `plugins/core/schemas/` and
   `plugins/<name>/schemas/` (one pattern for core and plugins, already
   loaded), rather than a new top-level `schemas/`.
4. **The source of truth for seed shapes.** Decided: the Rust structs,
   with JSON Schema generated and committed. The other way, hand-written
   JSON with structs generated from it, needs code generation in the build.
5. **Cookiecutter or cargo-generate.** Decided: cookiecutter (as asked)
   with cruft. The templates mix YAML, Python and Rust, and cruft's drift
   check is what keeps plugins uniform. cargo-generate (0.26.0) is the
   Rust-native equivalent, but it has no drift check.
6. **The brain using the template.** Decided: yes, to a staged diff only
   (Phase 2.5), after the template has stamped the existing plugins.
8. **The retired labels' leftovers in the live graph** (found 2026-11-06,
   § Phase 1): 7 constraints, 7 indexes and 185 nodes on `ApiCredential`,
   `Endpoint`, `HealingEvent`, `ModelSpec`, `Parameter`, `Resource`, `Schema`
   and `DynamicTool`, from the agent-api fork, read and written by no code.
   Decided (owner, 2026-11-06): drop them. Done the same day:
   `scripts/drop_fork_leftovers_2026-11-06.cypher`, backup in
   `workspace/backups/fork-leftovers-2026-11-06.json`.
9. **Strict seed kinds** (found 2026-11-06, § Phase 1): add
   `deny_unknown_fields` to the seven kinds that accept unknown keys, so a
   typo fails the seed test instead of being dropped. Recommended, after
   checking the profiles stored in the graph for extra keys. Open.
7. **Phase 3's list.** The owner's call per item (remove, move or keep),
   once the 60-day usage is in.
