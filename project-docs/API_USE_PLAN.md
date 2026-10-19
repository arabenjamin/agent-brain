# API Use Plan: an API is registered to be used, not collected

**Status: planned 2026-10-16; reviewed by the owner 2026-10-19 (decisions in §4); nothing built. Next: Phase 0.**

The brain can now find, register and prove a keyless API end to end
(the api-scout workflow on a schedule and api-integrator from chat, both run
by the api-integrator agent since 2026-10-16, with probed endpoints).
What it cannot do is *use* one: nothing connects a registered API to the work
that needs its data. This plan is that connection, and the pruning that keeps
the catalog from becoming a collection.

## 1. Where things stand (measured 2026-10-16)

**The catalog:** 10 `(:ApiContext)` records.

| API | How it got there | Endpoints | Auth | Used by a workflow |
|---|---|---|---|---|
| agent-brain | generated from the codebase | 89 | — | — (self) |
| aidevboard | OpenAPI spec | 27 | key | `ai-dev-board-job-search` (disabled) |
| home_assistant | docs page, LLM-extracted | 20 | bearer | no |
| hacker_news | probed (2026-10-16) | 4 | none | no |
| openviglet_model_catalog | probed (2026-10-16) | 4 | none | no |
| open_meteo | seeded | 0 | none | no |
| brave, google_cse, serpapi | seeded search engines | 0 | key | via `search_web`, not by context |
| github | seeded | 0 | bearer | via the developer plugin |

**The workflows:** 32 of 39 are enabled. **One** calls an API directly:
`daily-news` fetches `api.weather.gov`, which is not a registered API, while
the registered `open_meteo` is used by nothing. Ten enabled workflows get
their outside data from `search_web` instead.

**The vocabulary already exists, half of it.** Workflows carry a `topic` from
the closed `(:Topic)` list (12: ai-ml, cybersecurity, michigan, nia,
off-grid-networking, semiconductors, …), but 21 enabled workflows have none.
A topic says *what about*, not *what kind of data*: "weather" and "model
metadata" are not topics, and the same topic is served by a news feed, a
dataset and a reference catalog in different ways.

**Reading:** discovery and registration work; demand does not exist. Every
API added today is one more record nothing reads.

**Since (2026-10-19):** 11 records. `jobs-board-remoteok` (added 2026-10-17)
is the first registered API a workflow calls (`tech-hiring-trends-weekly`).
A second unregistered direct call joined `daily-news`'s weather:
`vulnerability-watch` reads the CISA KEV feed. Both are Phase 2's "through the
catalog" candidates.

## 2. Principles

1. **Need first.** An API is worth registering only against a need: a
   workflow step that wants its data. Discovery looks for needs nothing
   covers, not for interesting APIs.
2. **Closed vocabulary.** Matching works only on fixed lists (the typed
   relations lesson, HBI §10). Tags are chosen, never free text.
3. **Decide once, at design time.** Which API a workflow step uses is a
   reviewed change to the workflow, drafted by `schedule-author` and approved
   by the owner, not a model's choice on every run.
4. **Choose by measurement between equals.** Only when two APIs serve the same
   need is there a runtime choice, and it is made like search rotation:
   a ranked ladder with a ledger of real results, not a model's opinion.
5. **Unused is a signal.** An API no workflow calls for a set time gets an
   item asking to retire it. An API that needs a key is the owner's item to
   get the key for (see the Vault todo); the brain only learns its name.

## 3. Phases

### Phase 0: tag by hand, measure nimble (no code)

- The **data-kind** list, closed, 9 entries (owner, 2026-10-19): `news-feed`,
  `reference-catalog`, `forecast`, `dataset`, `job-postings`, `device-state`,
  `code-hosting`, `search`, `advisories` (security and government notices:
  CISA KEV, OFAC actions).
- Tag the 10 APIs by hand with topics (0..n, from `(:Topic)`) and a data
  kind. Tag the 10 search-using workflow steps with the need each one has
  (topic + kind), and the 21 topic-less workflows where it is obvious.
- Measure nimble against the hand tags, as for aliases (HBI §11.10): one
  fixed-choice question per axis, the API's description and endpoint
  summaries as the text, struct-ordered request. It goes in only if its
  confident answers are right; a "can't tell" counts as untagged.
- Exit: the two lists, the hand tags, and nimble's score with a yes/no.
- **Draft (2026-10-19):** `eval/api_tags_golden.yaml` holds the kinds, the
  13 APIs (11 registered, 2 to register), 37 step needs across 12 workflows,
  and topics for the 20 topic-less workflows; 17 tags are marked `guess` for
  the owner to settle. It is also nimble's answer key.

### Phase 1: tags in the graph

- `(:ApiContext)-[:SERVES]->(:Topic)` and an `api_kind` property; `kind` and
  `topics` on define_api_context and ingest_api, checked against the lists.
  The tagger is the owner's hand edit, else nimble if Phase 0 said yes.
- A workflow step may declare its need (`needs: {topic, kind}`); steps that
  call `search_web` for a beat are the first candidates.
- `list_apis` shows tags and "used by" (the workflows whose steps name the
  context), so an unused API is visible where the agents look.
- api-integrator proposes tags at registration and says which.

### Phase 2: the first two real uses

Both chosen so the payoff is checkable, not assumed:

- **openviglet_model_catalog → model-metadata drift.** A weekly workflow
  compares `models.yaml` context windows and output limits for the models the
  brain routes to with the catalog; differences become one Feed item with the
  rows. Success: it finds a real difference, or confirms none, with counts.
- **hacker_news → the ai-ml beat.** A separate small weekly workflow (owner,
  2026-10-19: inside the bi-weekly `slm-benchmark-watch` two weeks is one
  sample) reads top stories via `http_request`, keeps the ai-ml ones, and
  compares what it finds with the same beat's web-search step, for several
  runs. Success is measured: new relevant items per run vs. the search step,
  and search allowance saved.
- Also: `daily-news` weather moves to a registered API (`open_meteo` or a
  registered `weather_gov`), and `vulnerability-watch`'s CISA KEV feed is
  registered (`cisa_kev`, kind `advisories`), so no direct call bypasses the
  catalog.

### Phase 3: matching, as proposals

- A weekly pass finds needs (tagged steps) that a registered API's tags cover
  but that do not use it, and lists each match in one weekly digest item (owner, 2026-10-19): "`<api>` could
  feed step `<id>` of `<workflow>` instead of `search_web`." Its ask says
  what approving does: `schedule-author` drafts the changed workflow,
  disabled, for review.
- It also lists needs no API covers: that list becomes the api-scout workflow's input
  (Phase 4).

### Phase 4: demand-driven discovery and retirement

- The api-scout workflow searches for the uncovered needs from Phase 3, not its fixed
  interest areas. An API that needs a key becomes an owner item ("get a key
  at <url>; it would feed <workflow>"), which is where the Vault plan lands.
- An API used by no workflow for 60 days (and not a seeded engine) gets a
  "retire it?" item with its probe history; 60, not 30, so a monthly
  workflow's API is never caught between runs (owner, 2026-10-19).
- Between APIs that serve the same need, a ladder with a ledger, built on the
  search-rotation machinery, only once a real pair exists.

## 4. Owner decisions (2026-10-19)

1. **Data kinds:** the draft eight plus `advisories` (nine). Today's
   vulnerability-watch needs that kind, and none of the eight fit it.
2. **Topic-less APIs: yes.** An API may serve no topic; a need with a kind
   and no topic matches on the kind alone. No topics are invented for
   matching (no `weather`, no `model-catalog`).
3. **Hacker News trial:** a separate small weekly workflow, not a step in
   `slm-benchmark-watch`.
4. **Retirement window:** 60 days unused.
5. **Phase 3 matches:** one weekly digest item, not one item per match.

## 5. What this is not

- Not a runtime "pick any API" tool for the chat. The chat already reaches an
  API with `http_request` and `list_apis`; this plan is about workflows.
- Not a client generator. A Python client is for a person and is written
  only when asked (dev note `http/a-registered-api-is-proven-by-a-call-not-by-a-client-file`).
