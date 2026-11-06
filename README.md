# Agent Brain

A persistent, self-improving ( lol hardly ) autonomous agent ( kinda ) backed by a Neo4j knowledge graph (thats true).

A small local model does good work when the graph gives it the right context, with privacy and spend enforced underneath. Plugins specialize it for a doctor, a lawyer, a journalist or an analyst; everything else is a plugin.

## tl;dr
Human here, this project got out of hand quickly. It started as an mcp server, that grew into something very different. 
Let me just say I dont care for the name, but thats what I picked when I started it. 
Despite my best attempts, this is a managed dumpster fire, though, it may yet have it's uses and some potential. 
Did I vibe code this ? If you want to call it that. I'm certainly a sofware engineer and know my way around software and programming. 
I have been using claude to put this together. Nothing about this is one-shot or from the hip, there has been a lot of planning and replanning, iteration after iteration, and careful evaluation. I've dog-fooded it, and make others do the same.
I have a NVidia3060, and the goal was to get this to work locally on a small GPU, and to some degree of success. Though it looks like it works a lot better with a 5090. One could certainly use this with frontier cloud models and the quality and the richness of the results is definately felt. But the idea was to get this to run on smaller software.



## What core does

Core is what any specialization stands on: store what was learned, find it again, reason over it, run work on a schedule, and enforce privacy and spend underneath all of it. It runs on one local model with no cloud key.

- **Remembers** — notes with hybrid BM25+vector search, spaced repetition, entity extraction. A string the question names as written (a dollar amount, a date, a code such as GBU-12, a version, a model name, a URL, quoted text) is found as a phrase and ranks first; the spans come from tested patterns in `exact-patterns/`, not a model. The same patterns check what the brain writes: a link in a `reason` or chat answer that is in none of the sources it was given is marked `[link not in its sources]`, and a note id written as a URL becomes `note <id>`
- **Reasons** — LLM inference over stored knowledge with derivation tracking
- **Plans** — goal decomposition into ordered sub-tasks, autonomous scheduling
- **Executes** — durable priority job queue with per-provider concurrency
- **Spends only when asked** — a paid model runs only on work the owner started: an authenticated chat turn, an HBI action (the HBI's nginx adds the key and `x-brain-initiator: owner`), and the agents those delegate to. Scheduled and brain-started work never reaches a paid provider, even through a profile's paid pin (it runs on the chat default instead), unless its workflow is marked `paid: true`, which only the owner can set (YAML or HBI; a model-driven save is refused). `deep-dive-hard` is the one approved workflow
- **Extends** — runtime tool definition via stored procedure pipelines
- **Integrates** — OpenAPI spec ingestion, credential injection, HTTP execution
- **Routes** — before a front-door chat turn runs, the local model picks where it goes first (a behavior, an agent, a front-door tool, or a direct answer) under graded rules: what we have stored before the web, and our own names (topic aliases) read with `lookup`. A **behavior** (the routing block's `behaviors:`, such as "what did we learn today") is run by code: the local model fills its slots, `lookup` runs, and the front door phrases the rows with no agent loop. Every turn is logged (`turn:` in DuckDB) and HBI chat answers take a 👍/👎; the weekly `behavior-candidates` workflow mines both for turns an agent answered the same way each time, which are behaviors to write. `CHAT_ROUTER`, `project-docs/env.md`
- **Specialises** — see the next section
- **Keeps its own history** — every change the brain or you make to a workflow, agent or program is a revision with an author and a reason; see [What's running](#whats-running)

## How a plugin specializes it

A plugin is a folder of declarative files, with Rust only if it needs its own tools. No doctor or lawyer plugin ships here: the ones in `plugins/` are one analyst's setup and double as examples.

```
plugins/<name>/
  plugin.yaml      # name, version, description, requires (env keys it needs)
  topics/          # what this domain tracks
  sources/         # who to read
  workflows/       # the work, on a schedule or run by name
  contexts/        # its agents: prompt, tools, model
  programs/        # how the work is organized
  views/  tools/   # named read-only queries over the graph
  crate/           # optional: Rust, for a tool the files above can't express
```

To add one, make the folder, add its name to `PLUGINS_ENABLED` in `.env`, and restart. A plugin of files needs no rebuild; a `crate/` is an optional feature of `crates/host`. `GET /api/plugins` shows what loaded and why not. `core` always loads, and the core crates build and test without any plugin (CI enforces it).

## Running it without the private bundle

The owner's own interests live in a separate private repo, mounted through `PLUGINS_EXTRA_DIRS`. You don't need it. A clone without it seeds only the generic workflows, topics and programs, and `./install.sh` sets that up. To keep your own private bundle the same way, point `PLUGINS_PRIVATE_HOST_DIR` at a folder holding it, and name it in `PLUGINS_ENABLED`.

## Prerequisites

- Rust via rustup: the version is pinned in `rust-toolchain.toml` and installed automatically
- Docker & Docker Compose
- 25 GB of disk for the models. An NVIDIA GPU with at least 6 GB and `nvidia-container-toolkit` is much faster; with none, every feature still works, slowly

## Getting Started

### Quickest path to victory
```bash
./install.sh
```

It generates the secrets in `.env`, picks the build and preset from your card, downloads and verifies the models in `llamacpp/models.manifest`, and starts the stack on loopback. `./install.sh --gpu cpu` runs with no GPU. `./install.sh --engine openai --url ... --model ... --embed-model ...` uses your own OpenAI-wire server instead of llama.cpp. It is safe to run again. `GET /health` shows the engine and which features are on. After that, `docker compose up -d --build` is all you need.

With no cloud key the brain runs on the local model alone. Keys go in `.env` (`project-docs/env.md`).

### Longer path

```bash
# Clone and build
git clone <repo-url>
cd agent-brain
cargo build --release

# Configure: set MCP_API_KEY and NEO4J_PASSWORD in .env, or compose refuses to start
cp .env.example .env

# Start dependencies
docker compose up -d neo4j

# Initialize database
cargo run --release -- init-db

# Run as MCP server (stdio; add --transport http for the HTTP + /chat front door)
cargo run --release -- serve
```

## MCP Tools

The live list is `list_capabilities` (`kind: tools`) or `GET /api/tools`; it
also names tools that were removed and what replaced them. A guide by group:

### Knowledge
`store_note`, `search_notes`, `recall_recent`, `prune_old_notes`, `consolidate_memories`, `reason`, `synthesize_knowledge`

### Tasks
`create_task`, `update_task`, `list_tasks`, `reflect_on_work`

### Job Queue
`enqueue_jobs`, `manage_job`, `dead_letter`

### Scheduler
`scheduler_control` (`action: tick` runs one now), `manage_workflow` (list, get, run by name with inputs, upsert, delete, history, revert, audit). Every boot audits the workflows: one whose every step names a missing tool is paused, and re-enabled by a later audit once its tools are back; `health-monitor` reports any such pause and a drop in notes written.

### Programs
`manage_program` (list, get, upsert, delete, history, revert, reset, audit; review_context, record_review, start_due_reviews for the program-review workflow): the brain's work organized into departments, projects, investigations and cases that nest (the Niche Intelligence Agency over its verticals; Research & Development over AI & ML Research and Off-grid Networking), each owning its workflows. The weekly `rnd-intake` hands the NIA's reports to `rnd-scout`, which files at most two R&D todos for things we could learn from or use. Seeded from `programs/*.yaml`, edited by the tool or `/api/programs`, checked whole on every save (`schemas/program.json` first). Pausing or closing a program pauses its workflows; `program_scorecard` rolls the workflow scorecard up each program. In the HBI: Work › Programs (`GET /api/programs/scorecard` serves the same view). `program-manager` reviews a program, its workflows and the agents they use, and files its proposals as todos (`program-review`; the daily `program-reviews` dispatcher ships off). `agent_scorecard` shows each agent's runs, failures, misfit arguments and cost. See `project-docs/PROGRAMS_PLAN.md`.

### Working Memory
`push_context`, `notify_user`

### Query
`lookup` (runs a named view: the daily brief, open todos, claims, what was learned in a window, an API's endpoints; with no arguments it lists the views), `neo4j_query`, `duckdb_query`, `describe_graph`, `list_capabilities`. A view is a bundle's `views/*.yaml`: a read-only Cypher query with parameters (`kind: cypher`), or a preset over the note spec, which bounds time in local days (`kind: notes`); a notes view's `wide` shape (`learned`: counts by topic) answers a call whose notes outnumber the limit. A cut-off answer says so. It is not its own tool.

### Plugins
`manage_plugin` (drafts a bundle into `PLUGINS_RUNTIME_DIR`; it loads only once you add it to `PLUGINS_ENABLED`). Declared tools (a bundle's `tools/*.yaml`, read-only Cypher) such as `list_tasks`

Plugin tools (each from its bundle in `plugins/`, on by default in compose):
- `media`: `ingest_media`, `ingest_article`, `poll_media_sources`, `manage_media_source`, `discover_media_sources`, `spawn_gap_tasks`. Each watched channel or feed has a tier (`primary` > `reporting` > `analysis` > `commentary`, unset = commentary) that sets how many claims its items yield; commentary gives only a 1-in-5 sample. The owner changes a tier with `manage_media_source` retype, which keeps the history. Hand the chat a video or article link and the front door runs `video-learning` / `article-learning` on it; a run started from chat ends with the `intake` agent, which finds and reads the article or paper it is based on, places it in a program, files a todo only when there is something to act on, and reports back into that chat. `plugins/media/read-rules.yaml` says where to read a page instead of the URL given (an arXiv abstract → the whole paper) and which URL a mirror is known by (a Hugging Face papers page → its arXiv paper); the hand-off checks that a source `intake` names is one it started reading, and retries once
- `reference`: `link_entities`, `wiki_background` (needs `WIKIMEDIA_CONTACT`); routes under `/api/plugins/reference/`; entity coordinates from Wikidata (P625) as native Neo4j points, read on its idle loop, which put an entity on the drawer map. Map tiles are self-hosted Protomaps: `scripts/fetch_map_tiles.sh` fills `./maps` (world to zoom 7 plus the regions in `maps/regions.yaml`), served by the HBI at `/maps/` (`project-docs/MAP_PLAN.md`)
- `alias-judge`: no tools; ranks entity merge candidates on idle time (`ALIAS_JUDGE_MODEL`)
- `developer`: `read_codebase_file`, `search_codebase`, `get_git_log`, `write_codebase_doc`, `write_proposal`, `analyze_own_structure`, `write_workspace_file`, `list_workspace_files`, `snapshot_architecture`, `dev_docs` (the dev notes, `scripts/dev_docs.sh`); the architecture view at `/api/plugins/developer/architecture`
- `epistemics`: `claim` (extract facts, and a named source's opinions, predictions and hypotheticals labelled as theirs; verify against independent web evidence with a local grounding check, and refute only on the cited results' own conflicting words, skipping an attribution the recording itself proves; list; sources), `extract_relations`; typed-relationship routes under `/api/plugins/epistemics/`. Claims are notes, so the core still reads them when it is off
- `osint-data`: `acled_events` (needs `ACLED_EMAIL`, `ACLED_PASSWORD`)
- `harness`: the brain improves its own sub-agents from evidence (`project-docs/HARNESS2_PLAN.md`, after Harness², arXiv 2610.14224). `harness-improve {agent, task, why}` probes the agent as it is, has `harness-editor` write an invariant and a domain candidate, probes both, has `harness-composer` keep, adopt or merge on what the runs show, probes the result, and has `harness-reflector` write `harness/<agent>-<date>.json` (with a proposed lesson) and a todo for the owner; it never adopts a prompt itself, and a `harness-library/<agent>` dev note is written only when the owner adopts. `harness-candidates` (weekly) starts it on the sub-agent with 2+ bad runs. A probe is `run_agent probe: true` (optionally with a candidate `harness`): reads run, every other call is answered by a stub and recorded; every run's trajectory is in `agent_runs`. `run_agent verify` checks the trajectory: `require_tools` (`tool` or `tool:action`), `answer_requires` (a claim in the answer needs the call behind it) and `retry` (once more, told what fell short). Judges run on nemotron-3-ultra (free); `judge_model` and a starting `harness` are inputs
- `ml-lab`: the brain learns machine learning by doing it (`project-docs/ML_LAB_PLAN.md`). `ml-study` (every 3 days) has `ml-student` read the next lesson of a curriculum (Kaggle Learn, Karpathy's Zero to Hero, optimization and search) and run its exercise in the Python sandbox. `squares-report` (daily) reports the `ml-runner` service (`plugins/ml-lab/runner`): the Kaggle Squares in Squares store, every packing checked with Kaggle's own metric, an overnight search on 12 CPUs, and a nightly build that is submitted when it beats the last one. The same runner works the monthly Kaggle Playground Series (tabular ML: a cross-validated model ladder, `playground-report` daily, and `playground-study`, which reads the competition's forum and top notebooks and proposes the ladder's next rung).
- `research`: `deep_research` (needs `PARALLEL_API_KEY`): a cited multi-source report from Parallel's Task API, paid per run and capped monthly (`plugins/research/parallel.yaml`); in chat it returns a run id and the report arrives in the Feed and as a note; the `deep-research` workflow
- `websocket`: `ws_connect`, `ws_send`, `ws_receive`, `ws_close`: a WebSocket connection held across tool calls, for streaming feeds and push APIs `http_request` cannot read. Connects go through the net policy (internal hosts only via `HTTP_ALLOW_INTERNAL`, to the checked address); at most 8 open, long messages cut and marked
- `maps`: `geocode` (a public place name to a point: the brain's located entities, then a `(:Place)` cache, then OpenStreetMap's Nominatim, one request a second; never a private address), and the `lookup` views `places_near` and `notes_near` (what the brain knows, and what its notes say, within a radius; countries and regions only with `broad`). `store_note` takes an optional `lat`/`lon` for a note about an event at a place. Map layers are a seed kind any bundle may ship (`layers/*.yaml`: a read-only query returning `lat`, `lon`, `label`, optionally timed), drawn live by the HBI's **Map** workspace with a time window; places added by hand there are private (never geocoded). Ship (aisstream.io) and aircraft (OpenSky) positions over watch areas (`areas/*.yaml`, the NIA's are private) go to their own DuckDB (`tracks.duckdb`, 14 days) with per-hour coverage, and `area_traffic` answers what was seen with that coverage, so an area no receiver covers reads as unseen, never quiet; watch areas live in the graph and are drawn on the Map (the files are seeds). The Map follows the zoom: layers load for the view and cluster when zoomed out, each area is one bubble until you zoom in to its outline and positions, and "Look here now" takes one OpenSky snapshot of any view. Routes under `/api/plugins/maps/` (`project-docs/MAP_PLAN.md`)
- `github`: no tools; the `ci-watch` workflow (hourly, Brain Ops): the latest GitHub Actions run per workflow and branch of agent-brain, gizmatron and gizmo-server, compared in code with the previous check through the `github` API context; a repo the token cannot read is reported once, never silently empty. A newly failed run opens an action item, a recovered branch sends a note, nothing changed sends nothing. Another repo is one more fetch step (needs `GITHUB_TOKEN`)
- `sandbox`: `execute_code` (below); the bundle also holds the two sandbox images (`python/`, `rust/`)
- `personal`: no tools; the owner's own workflows, agents and topics, from a private repo mounted at `/home/agent/plugins-private` (`PLUGINS_EXTRA_DIRS`); absent elsewhere

A new plugin is stamped from `templates/plugin` (cookiecutter, kept current with cruft; CODEBASE_SPLIT_PLAN Phase 2): `scripts/plugin_template.sh new <name> --kind rust|data --description "..." [--tools a,b] [--graph-labels Thing] [--env KEY]` writes the bundle (`plugin.yaml`, `README.md`, and for Rust `crate/` with the `Plugin` impl, a skill whose tools answer "not built yet", their `schemas/tools/` files and a `schemas/graph/schema.cypher`), registers a Rust one with the host and the Dockerfile, and runs its tests and the repo checks. `update <name>` brings a later template change in as a three-way diff; `check` (CI, pre-commit) fails when a stamped plugin is behind the template. It runs through `uvx`, so nothing is installed. Enabling it is still `PLUGINS_ENABLED`.

### HTTP / API / MCP
`http_request`, `define_api_context`, `ingest_api`, `mcp_list_tools` (no `server` lists the servers), `mcp_call`

### Model
`get_llm_budget` (the GPU lease is REST: `/api/gpu-lease`)

### Code Execution (the `sandbox` plugin)
`execute_code`

Runs Python in an isolated `sandbox` sidecar and returns what it printed, so
quantitative steps compute their numbers instead of narrating them — prose
arithmetic fails silently and confidently, and nothing downstream re-derives it.
The sandbox sits on an `internal: true` Docker network with **no egress**, a
read-only root filesystem, dropped capabilities, none of the brain's
credentials, and no bind mounts; every input must be passed inline in the code.
`numpy`, `sympy`, `pandas`, `scipy`, and `scikit-learn` are available. A failed run returns its traceback
as a *successful* tool call, so the model can fix the code rather than burn a
retry. Offered only when `SANDBOX_URL` (or `RUST_SANDBOX_URL`, for `language: "rust"`) is set.

### Other
`search_web`, `read_page`, `get_search_usage`

`search_web` spreads its searches across the free allowances: each engine file
in `plugins/*/search-engines/` may declare a `budget` (Exa $10/month, Tavily
1,000/month, TinyFish 12,000/day, Serper's one-time 2,500), and each call goes
first to the engine that is within its paced allowance and least used today.
An engine measured better at one tool's job leads that tool's searches while
on pace (`lead_for`: Exa for claim verification), and an engine can be kept
for those tools (`reserved_for`): any other caller reaches it only by naming
it or as the last resort.
The failover ladder (`SEARCH_ENGINE_ORDER`) stays behind that pick, with
Brave last, so one exhausted free tier cannot fail the call (`SEARCH_ROTATION=off` restores the plain ladder).
`get_search_usage` reports each engine's burn, its standing in the rotation,
who searched, and quality hints: dated, in-window and known-source shares, and
how many of its results were later cited as claim evidence.

`read_page` turns a URL into text the same way: the page readers in
`plugins/*/page-readers/` (TinyFish Fetch 1,000/day, Anakin 300/month) are
paced and rotated, with our own direct fetch behind them (and first for raw
files). `ingest_article` uses the same readers when its own fetch is blocked.

## Connect via HTTP

```bash
# Start server
cargo run --release -- serve --transport http

# With custom port and API key
cargo run --release -- serve --transport http --bind 0.0.0.0:8080 --api-key your-secret-key
```

**Endpoints:**
- `POST /mcp` — JSON-RPC requests
- `GET /mcp` — SSE stream
- `POST /chat` — Interactive chat with SSE
- `GET /health` — What is running: the commit, build time and container, and the local engine

### Claude Desktop Config

```json
{
  "mcpServers": {
    "agent-brain": {
      "command": "/path/to/agent-brain",
      "args": ["serve"],
      "env": {
        "NEO4J_URI": "bolt://localhost:7687",
        "NEO4J_PASSWORD": "your-password"
      }
    }
  }
}
```

### Docker

```bash
# .env must set MCP_API_KEY and NEO4J_PASSWORD; compose refuses to start without them.
docker compose up -d --build
```

The brain (3001) and Neo4j (7689, 7476) listen on loopback only; the HBI
(3002) listens on `HBI_BIND` (loopback unless set, e.g. to the tailnet IP),
and its nginx adds the key, so whoever reaches it is the owner. Host scripts
read the key with `scripts/brain_key.sh`. On an existing database, run
`scripts/rotate_neo4j_password.sh` before the first deploy with a new
`NEO4J_PASSWORD`.

### Keeping dependencies current

`Cargo.lock` is committed and the image builds `--locked`, so the container runs exactly what was tested on the host. Update on purpose:

```bash
scripts/update_deps.sh                 # latest compatible crates: build, clippy, tests; restores the lock on failure
scripts/update_deps.sh --rust 1.99.1   # also move the compiler pin (brain + Rust sandbox)
docker compose up -d --build agent-brain   # then commit Cargo.lock
```


## What's running

- **The code:** `GET /health` reports the commit the image was built from, whether the tree had other changes, when it was built, and the container. If that commit isn't `git rev-parse HEAD`, the newest rebuild failed or hasn't finished. Each commit redeploys through `.githooks/post-commit` (`git config core.hooksPath .githooks`), so each one restarts the brain. A build by hand needs `eval "$(scripts/build_identity.sh)"` first, or it reports a null commit.
- **What differs from the repo:** the `running_changes` view (`lookup`) lists each workflow, agent or program whose live definition isn't its file, with the diff, who changed it and why.
- **Going back:** `manage_workflow` and `manage_program` have `history` and `revert`, and the HBI has a History panel on all three. A revert is a new revision.
- **Keeping a runtime edit:** `scripts/adopt_revision.sh <kind> <name>` writes it to its file and shows the diff. The brain never writes or commits a repo file; you review and commit, and the next start hands it back to the file.

## CLI Commands

```
serve            Run as MCP server (default)
init-db          Initialize Neo4j schema
repair-notes     One-off note repair migration
eval-retrieval   Score retrieval against a golden set
todo             Manage todos over HTTP
```

## Environment Variables

| Variable | Default | Description |
|----------|---------|-------------|
| `NEO4J_URI` | `bolt://localhost:7687` | Neo4j connection |
| `NEO4J_PASSWORD` | — | Neo4j password (required) |
| `LOCAL_ENGINE` | `llamacpp` | The local engine, an entry of `engines:` in `models.yaml` |
| `LLM_MODEL` | the engine's generation model | The active provider's model |
| `MCP_TRANSPORT` | `stdio` | Transport type |
| `MCP_HTTP_BIND` | `127.0.0.1:3000` | HTTP bind |
| `MCP_API_KEY` | — | HTTP bearer key; without it nothing is the owner's, so no paid model runs |
| `SANDBOX_URL` | — | Code-execution sandbox endpoint (the `sandbox` plugin). Unset ⇒ no `execute_code` |
| `PLUGINS_DIR` / `PLUGINS_ENABLED` | `plugins` / *(core only)* | Where the seed bundles live, and which load besides `core` (see `project-docs/env.md`) |
| `TZ` | — (⇒ UTC) | Wall-clock timezone (e.g. `America/New_York`). What the brain **says** about time is local; what it **stores** stays UTC. Unset means its "today" is UTC's, which is a different day for part of every day outside UTC |

## Development

```bash
cargo fmt && cargo clippy
cargo test --lib          # unit tests, all crates
cargo test --test '*'     # integration tests (need Neo4j)
```

`CLAUDE.md` holds what applies to most changes; detail on a subsystem is in the dev notes (`scripts/dev_docs.sh search "<question>"`).

## License

MIT
