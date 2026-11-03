# Outside review plan

What we take from the first outside review, what we answer, and the order we
fix things in. Planned 2026-10-28. A friend cloned the repo and had Fable
review it cold; the review is in `workspace/fable-review.md`. Every claim
below was checked against the code and the live brain on 2026-10-28.
Decisions for the owner are in §9.

## 1. The review in one paragraph

The engineering is good (tests, strict lint, eval sets, measured config
choices); the problem is sprawl. No center, a lot of code that copes with an
8 GB GPU and free tiers, no way to tell what is actually running, heavy to
operate, docs that drift. Security defaults need fixing before anyone else
runs it. One suggestion from its own system: make every change the brain
makes to itself versioned, reasoned and checked by the storage layer, not the
prompt. Without the personal content, it says, "no center" mostly goes away:
"an agent runtime for small GPUs that handles privacy and spend for you."

## 2. What we found when we checked

| Claim | Verdict | Evidence |
|---|---|---|
| Main endpoint has no login, bound to every interface | **True** | `docker-compose.yml:198` blanks `MCP_API_KEY`; `"3001:3000"` publishes on 0.0.0.0 |
| Anyone can claim to be the owner and spend money | **True, and worse** | No header needed for chat: `RouteContext::new` makes every `Purpose::Chat` call `Initiator::Owner` (`crates/llm/src/routing.rs:161`). The header (`http_transport.rs:861`) only matters for REST |
| Neo4j on the default password, open on every interface | **True** | `NEO4J_AUTH=neo4j/password` (`docker-compose.yml:5`); 7689 and 7476 published on 0.0.0.0 |
| Hardcoded `"openclaw"` key in the HBI | **True** | `hbi-frontend/src/api/config.ts:12`; harmless today only because auth is off |
| Also open, not in the review | **True** | HBI on `"3002:80"`, all interfaces; its nginx proxies `/mcp`, `/chat`, `/api` straight to the brain |
| A runtime save silently shadows the YAML | **True in design, rare in practice** | 3 of 55 live workflows are runtime-owned, all brain-authored with no YAML file. But workflow and profile writes keep no history, need no reason, and last writer wins (`workflows::save`, `profiles::write`) |
| Core ships personal workflows | **True** | 9 of the 26 `plugins/core/workflows` are the owner's interests (§5) |
| Detroit and personal names in crates | **Mostly tests and doc comments** | `views.rs` test tz, `clock.rs` docs, test strings. Cosmetic |
| Huge files | **True** | `work/queue.rs` 3,098; `retrieve/http.rs` 2,708; `api/rest.rs` 2,423; `skills/knowledge.rs` 2,419 lines |
| Deployment knowledge lives in compose comments | **True** | 280 of 588 lines of `docker-compose.yml` are comments |
| Can't tell what's running | **Partly** | `/health` returns only `{"status":"healthy"}`; no build or commit identity |

Size today: core crates ~90K lines of Rust, plugin crates ~25K, 14 plugins,
180 plugin YAML files, 200 commits in the last 30 days.

## 3. What we answer

- **The constraint code is the product, not coping.** Privacy-first routing,
  spend caps, free-tier pacing and residency on an 8 GB card are what make a
  small local model usable for real work. The review's own reframing names
  them as the niche.
- **The graph is the center, it just isn't written down where a reader
  looks.** The thesis since the start: a model works best with its context
  1/3 to 2/3 full, and the right graph queries fill it better than a stack of
  markdown. Retrieval eval (recall@10 0.84 → 0.92), the `lookup` views and
  the dev notes are that thesis at work. §5 writes it down.
- **806 dev notes are by design.** Rules live in the graph so an agent gets
  the ones it needs by query, not all of them. The reviewer saw only the seed
  file `devdocs/devnotes.jsonl`.
- **Most of the stack is the design.** Neo4j and DuckDB each do a job nothing
  else in the stack does; the answer to their operating cost is documentation
  and a smaller core (§5, §8). Corrected 2026-11-01: llama.cpp and Ollama do not
  each do a unique job. Measured, llama.cpp alone can serve every local model,
  so one engine is chosen at install (`LOCAL_ENGINE_PLAN.md`).
- **Plugin coupling is already narrow.** Only `crates/host/Cargo.toml` names
  plugin crates (10 of them); no other core crate depends on a plugin.
- **Harness² does not write profiles.** An adopted edit is a todo the owner
  copies into YAML by hand, so the self-edit risk today is workflows written
  by agents (`manage_workflow`), not Harness².

## 4. Phase 0: security defaults (first, small)

Nothing to design except the trust boundary (§9.1).

1. **Bind to loopback or the tailnet.** `3001` → `127.0.0.1:3001` (host
   scripts, `.mcp.json`, `dev_docs.sh` all call localhost). `7689`/`7476` →
   `127.0.0.1`. `3002` (HBI) → the tailnet IP, the way the home-lab stack
   binds (`HOMELAB_MONITORING_PLAN.md`).
2. **Turn auth on.** `MCP_API_KEY` from `.env`, not blanked in compose. The
   browser does not hold the key: the HBI's nginx adds `Authorization` and
   `x-brain-initiator: owner` server-side, so whoever can reach 3002 is the
   owner and nobody else is. Remove the `"openclaw"` fallback. Add the
   bearer header to `.mcp.json` and `scripts/session.env`.
3. **Owner only when authenticated.** Chat no longer defaults to
   `Initiator::Owner`; it is the owner's when the request authenticated and,
   for the HBI, carries the header. With auth off (dev), nothing is the
   owner's, so a paid model never runs on an open port.
4. **Neo4j password from `.env`.** `NEO4J_AUTH` applies only when the data
   volume is first created, so the live database needs a one-time
   `ALTER CURRENT USER SET PASSWORD` before the compose change. Back up with
   a snapshot first.
5. **A test that fails on an open default.** A unit test over
   `docker-compose.yml`: no `ports:` entry without a bind address, no literal
   `NEO4J_AUTH` password, `MCP_API_KEY` not blanked.

Done when: an unauthenticated request to 3001 from the host gets 401; 3001,
7689 and 7476 refuse from another tailnet machine; the HBI works unchanged;
a chat with no key cannot route to a paid provider (unit test).

**Built 2026-10-29, deployed 2026-11-01 (`f19240a`).** As planned, with these choices:

- The HBI binds `${HBI_BIND:-127.0.0.1}`: loopback until the owner sets the
  tailnet IP in `.env`, so a fresh clone never starts open.
- `RouteContext::new` makes everything the brain's; the transport upgrades
  it. A chat turn is the owner's when it authenticated (`ChatRequest.owner`,
  `#[serde(skip)]`); MCP and REST need the key and the HBI's header.
- nginx's config is a template (`BRAIN_API_KEY`, `NGINX_ENVSUBST_FILTER`).
  It also forwards `/v1` for Conduit, which loses 3001; there the client's
  own key passes through.
- `scripts/brain_key.sh` gives host scripts the key (environment, else
  `.env`); `dev_docs.sh`, `session.env`, `pause_cloud_schedules.sh`, two
  probe scripts and the fine-tuning harness and GPU lease (workspace) use it.
- `scripts/rotate_neo4j_password.sh` moves the live database to `.env`'s
  password; it runs before the deploy.
- Guards: `config::tests::compose_publishes_nothing_open` (fails on the old
  compose), `the_owner_header_proves_nothing_without_auth`,
  `a_chat_body_cannot_claim_the_owner`, the routing tests. Dev notes:
  `deploy/nothing-is-published-open-the-key-decides-who-the-owner-is`, and
  the routing and front-door rules updated.

Deploy order: a new `NEO4J_PASSWORD` in `.env` (and `HBI_BIND` if other
machines use the HBI), then `scripts/rotate_neo4j_password.sh`, then deploy
at once; Conduit's base URL becomes `http://<HBI_BIND>:3002/v1`, and Claude
Code's `.mcp.json` needs the bearer header.

Deployed 2026-11-01. The first start failed: the commit deployed before the
rotation ran, and the brain's login retries kept Neo4j locked until it was stopped.
Stopping the brain, running the script and starting it again fixed it. Checked
after: unauthenticated `/mcp` gets 401, 3001 and Neo4j bind loopback, the HBI binds
the tailnet IP and reaches the API. Not checked: a refused connection from another
tailnet machine, and the Tailscale ACL on 3002 (§9.1).

## 5. Phase 1: write the center down and keep the core to it

1. **The center, in the README's first paragraph** (wording is §9.2): a small
   local model does good work because the graph gives it the right context,
   with privacy and spend enforced underneath; plugins specialize it for a
   doctor, a lawyer, a journalist or an analyst. Everything else is a plugin.
2. **Move the owner's interests to the private bundle**
   (`../agent-brain-personal/personal`, already mounted at
   `/home/agent/plugins-private`):

   | Move | Stay in core | Owner decides |
   |---|---|---|
   | hardware-tripwire, hn-ai-trial, intelligence-agency-claims-monitor, rnd-intake, rust-training-data-scout, sanctions-trade-watch, slm-benchmark-watch, surveillance-privacy-watch, tech-dependency-synthesis | api-match-digest, api-retire-check, behavior-candidates, consolidate, default, fill-knowledge-gap, health-monitor, item-dispatch, model-metadata-drift, overdue-todo-review, program-review, program-reviews, todo-snapshot | vulnerability-watch (a good generic example?), deep-dive / deep-dive-pick / deep-dive-hard (they place Rust in this codebase; generic with that removed?) |

   Same pass over `plugins/core/sources/` and `topics/` (every interest topic
   but `agent-brain` and `_scope`), `views/intel_findings.yaml` and the
   `rnd-scout` context. Core keeps one small example topic and source list so
   a fresh clone shows the shape. Programs that name a moved workflow move
   with it. A moved file's workflow keeps its id and history: the seeder must
   treat a plugin change as the same workflow, not a new one (check before
   moving).
3. **Core builds without plugins, enforced.** Each plugin crate becomes an
   optional dependency of `crates/host` behind a feature, all on by default.
   CI runs `cargo build` and `cargo test --lib` with
   `--no-default-features` on `dev`. A core change that needs a plugin then
   fails CI instead of drifting.
4. **Neutral test fixtures.** Swap Detroit/Michigan in tests and doc
   comments for neutral examples. Cheap; do it in the same pass.

Done when: a fresh clone with no private bundle starts, seeds only generic
workflows, and the no-plugin build passes in CI.

**Built 2026-11-02.** As planned, with these choices, which are the owner's to change (§9.2, §9.5):

- **The center** is §5.1's wording, one sentence added under the README's title. The owner's own tl;dr below it is untouched.
- **`vulnerability-watch` stays in core.** It is a generic example of a monitor, seeded off, and it keeps the `cybersecurity` topic and source list as the shape a fresh clone shows.
- **`deep-dive`, `-pick`, `-hard` and `deep-diver` stay in core, unchanged.** About a dozen core files know them, and "place it in this codebase" is true for anyone running this codebase. They stay Rust-shaped, which is this repo's language, not an interest.
- **`ai-ml` also stays,** because `model-metadata-drift` needs it, so core has three topics rather than one example plus `agent-brain`.

Moved to `agent-brain-personal`: the 9 workflows of §5.2, `rnd-scout`, `intel_findings`, 2 programs (`ai-ml-research`, `research-development`), 5 topics, 7 source lists and `eetimes_rss`. The `ml-lab` program moved to its own plugin. Compose's default `PLUGINS_ENABLED` drops `personal` and `ml-lab`, `TZ` defaults to UTC, and `MEDIA_WATCH_ENABLED` defaults off: this box's `.env` sets all three to what it had.

Checked on the live graph, before and after: 55 workflows with the same id, steps, enabled state, owner and last run, only `plugin` changing on the 9. Programs, topics with their note counts, source lists and APIs identical. `rnd-scout` changed only its plugin.

Checked on a fresh clone with no private bundle: it starts and seeds 30 workflows, core 17, media 4, developer 4, epistemics 2, harness 2, reference 1; topics `agent-brain`, `ai-ml`, `cybersecurity`; source lists `cybersecurity`, `platforms`, `state-media`; programs `brain-ops`, `shared-services`.

Core builds and passes with no plugin compiled: `crates/host` holds each as an optional feature, `plugins` on by default. `cargo tree` shows only `agent-brain-plugin-api` left. CI runs the same build and tests on pushes to `dev` and on pull requests; run here by hand, not yet seen on GitHub. One test needs plugins, since it checks kinds they own, and is gated on the feature.

Fixtures: Detroit and Michigan went from tests, doc comments and one YAML example, and so did a real sheriff's name from a test. Left: Michigan in `demonyms.yaml`, which is data, and "Detroit" among the example names in the entity extraction prompt in `knowledge/entities.rs`, which is live model input, so changing it is not cosmetic.

Still the owner's, in the repo:

- `plugins/media/sources-media/watchlist.yaml`: 21 channels the owner chose. A fresh clone seeds them, inert with polling off. Not moved, because the owner has uncommitted edits in the file. It belongs in the private bundle.
- `agent-api`'s `ai-network` and the fixed yt subnets, which are this box's arrangement, harmless on a clone.

## 6. Phase 2: versioned self-edits, enforced where they are stored

The review's suggestion, scoped to what the brain edits about itself:
workflows, agent profiles and programs. All three already have one write
function (`workflows::save`, `profiles::write`, `programs::save`), which
is where this goes, not the prompt.

1. **Every save writes a revision.** `(:Workflow|:ContextProfile|:Program)
   -[:REVISION]->(:Revision {kind, name, version, definition, content_hash,
   prev_hash, author, reason, based_on, at})`. `author` is the agent, the
   owner (HBI) or `seed:<plugin>/<file>`. The YAML file is the `seed`
   revision; a runtime save stacks on top of it.
2. **Append-only.** Neo4j Community cannot refuse an update the way a
   Postgres trigger can, so the guarantee is in code: one repository function
   creates revisions, none updates or deletes them, and a test fails if any
   Cypher in the workspace `SET`s or `DELETE`s a `:Revision`. `prev_hash`
   chains the hashes so an edit made outside that path shows up.
3. **Compare-and-set.** A save names the version it read (`based_on`); if the
   stored version moved, the save is refused with the current one, and the
   caller re-reads. `manage_workflow`, the REST save routes and the HBI forms
   all send it. The seeder is exempt (it writes `seed` revisions only).
4. **A reason is required** on every non-seed save; it is stored on the
   revision and shown in the HBI.
5. **Revert is a new revision** copying an old definition, with a reason.
6. **"What's running" is a query.** A `lookup` view lists workflows and
   profiles whose live revision is not their seed, with the diff. The seeder,
   which today skips a runtime-owned workflow with a log line, records the
   divergence there instead. Runtime edits reach repo files only through the
   owner (§9.4): the HBI and `scripts/adopt_revision.sh` render the file, the
   owner reviews and commits it, and a start that finds file and live identical
   makes the file the owner again.
7. **Backfill:** one `seed` or `baseline` revision for everything stored
   today.

Done when: two concurrent saves of one workflow leave one revision and one
refusal; a save without a reason is refused; reverting a bad edit is one
call; the view lists the 3 runtime-owned workflows.

**Built 2026-11-02.** All of it checked on the live brain. Four writers racing
on one workflow, 12 rounds: one 200 and three 409 every round, one revision per
round, chain intact. A save without a reason, or an edit without `based_on`, is
refused for all three kinds. A revert is one call, and it brought back a deleted
workflow, paused. The backfill wrote 52 + 13 + 18 `seed` and 3 + 3 `baseline`
revisions with no false drift and left the live state identical to a snapshot
taken first. `running_changes` lists the 3 runtime workflows and 3 runtime
programs. A scratch workflow, program and agent adopted with
`scripts/adopt_revision.sh` went back to `yaml` on the next start.

What the build settled that this section left open:

- A revision records a definition, not state. Pausing a workflow or closing a
  program writes none and needs no reason or version.
- A workflow's name can't change on save any more (programs already refused
  this), since the name is the key of its history.
- A definition and its revision commit in one transaction. The owner node is
  locked before its version is read; `revision_uid` is the backstop.
- A delete leaves a `delete` revision and the history stays, so a new object of
  that name continues it. Files the seeder removes leave no `delete` revision:
  git holds that.
- A start finding a live definition no revision describes records an `outside`
  revision and warns, since nothing can stop a direct write.
- `neo4j_query` with `readonly: false` refuses Cypher naming `:Revision`. Direct
  database access can still reach them; the chain check is what reports that.

## 7. Phase 3: what's running, and operating it

1. **`/health` says what it is:** git commit, build time and image id. The
   post-commit hook passes the commit as a build arg. Auto-deploy on commit
   stays (one operator, one box); the commit on `/health` is what makes it
   traceable.
2. **Compose comments become dev notes** (subsystem `deploy`), one rule or
   history note per topic; compose keeps one-line comments that point to
   them.
3. **README refresh** around the §5 center: what it is, what core does, how
   a plugin specializes it, how to run it without the private bundle.

**Built 2026-11-02.** `/health` reports `build` {commit, dirty, built_at,
container}, baked into the image by `scripts/build_identity.sh`; the image's
`org.opencontainers.image.revision` label holds the commit too. Compose went from
572 lines (257 of them comments) to 341 (22 comments) with `docker compose config
--no-interpolate` identical before and after; 11 `deploy` dev notes hold what
the comments said, and comments already covered by a note point there.

Three things differ from the text above. An image can't contain its own id, and
passing it at `up` would make any plain `docker compose up -d` recreate the brain,
so `/health` reports the container, which `docker inspect` maps to the image.
`dirty` was added: the image is built from the working tree, so a commit alone
can understate what is in it. And `:BrainVersion` now reads the baked commit, since
reading the mounted repo's HEAD named a commit the process did not contain after a
failed rebuild. The cost: every commit now gives a new image, so every commit
restarts the brain, docs-only ones included.

## 8. Phase 4: size, measured

1. **Measure in tokens, not lines.** A script reports the token count of
   core crates, each plugin, and the workspace; run in CI and printed, not
   gating yet.
2. **A budget** (§9.3), then a gate: core may not grow past it without the
   owner raising it.
3. **Split big files only where it cuts what an agent must read to change
   one thing:** `work/queue.rs`, `retrieve/http.rs`, `api/rest.rs`,
   `skills/knowledge.rs` are first in line. A split that only moves lines
   does not count.

**Built 2026-11-03.**

`scripts/size_budget.sh` reports tokens for core, each plugin and the repo, and fails when core is over `scripts/core_token_budget`. It runs in CI on `dev` and pull requests and in the pre-commit hook. Tokens are bytes / 4, which needs nothing installed: on Rust a real tokenizer gives 4.24 bytes a token (cl100k_base, all tracked Rust) or 3.70 (Gemma, one 8 KB sample).

| Measured 2026-11-03 | Tokens |
|---|---|
| Core code (`crates/`, no tests) | 723,868 |
| Core data (`plugins/core`) | 53,432 |
| **Core, what the budget counts** | **777,300** |
| Core tests, not counted | 170,886 |
| Everything tracked | 2.38M |

**The budget is 778,000** (§9.3): today's number, rounded up to the next thousand so a small fix doesn't need the owner. Core can only grow when the owner edits that file.

Against the old line budget: core's crates are 73.8K lines, up from 59.2K on 2026-10-11, and `work` is 10.4K against a hard limit of 8K. `size_budget.sh` was never in CI, so nothing caught it, though the dev note said it ran there. The per-crate limit is gone, replaced by the single token gate.

The four files, tokens to read each (tests included):

| File | Before | After |
|---|---|---|
| `api/rest.rs` | 23,379 | shared state and helpers 3,209 + one of 8 resources, 1,314 to 4,743 |
| `retrieve/http.rs` | 26,720 | `http_request` 5,459, `define_api_context` 4,694, `ingest_api` 16,852 |
| `skills/knowledge.rs` | 26,475 | dispatch, prune, consolidate 7,234, `store` 4,042, `recall` 8,715, `reason` 6,910 |
| `work/queue.rs` | 32,768 | coordinator and running a job 19,954, `templates` 6,197, `gates` 4,667, `ops` 2,101 |

Each split is along something a change can touch alone: a resource, a tool, the template language. `rest.rs` has the best evidence: 9 of its last 11 commits touch one resource. `knowledge.rs` has the least, with 4 commits in the history I could follow. `queue.rs` stays the largest, because the coordinator and running a job change together.

Only whole items moved. Checking every non-import line against the originals, the differences are visibility (`pub(super)`), the new module scaffolding and comment headers; one test helper is now in two test modules. The split costs 1,374 tokens of imports and module lines. Unit tests stay at 1,157, and clippy with `-D warnings` is clean.

## 9. Decisions for the owner

1. **Trust boundary.** Parked (owner, 2026-11-02). The owner is the only one
   on the tailnet, so a Tailscale ACL is low priority. Customers, organizations
   and user logins are a later question, and core does not need to care about
   them.
2. **The center sentence.** Used §5.1's for now (2026-11-02). §5.1's wording, or the review's "an agent runtime
   for small GPUs that handles privacy and spend for you." Recommended: ours,
   since the graph is what the code is built around; theirs describes how it
   is delivered.
3. **Size budget.** Set at the recommendation for now (2026-11-03): 778,000
   tokens, today's number. The owner can change it in
   `scripts/core_token_budget`.
4. **Runtime edits back to YAML?** Decided (owner, 2026-11-02): yes, through a
   human. The brain never writes or commits a repo file. It shows the diff and
   renders the file; the owner writes it (HBI copy, or
   `scripts/adopt_revision.sh`), reviews it and commits, so git holds the
   history and a file can be rolled back. Once the file matches what is live,
   the next start hands the workflow back to the file.
5. **The borderline workflows in §5.2.** Kept in core for now (2026-11-02; reasons in §5).

## 10. Order

Phase 0 now (no design beyond §9.1). Phase 1 next, since it settles what
core is before Phase 2 versions it. Phase 3 can run beside either. Phase 4
after Phase 1, so the measured core is the real one.
