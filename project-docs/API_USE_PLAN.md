# API Use Plan: an API is registered to be used, not collected

**Status: planned 2026-10-16; reviewed by the owner 2026-10-19 (decisions in §4). Phases 0–4 built 2026-10-19; the HN trial runs weekly until the owner's call; the ladder waits for a real pair.**

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
  The owner accepted the draft as written, guesses included (2026-10-19).
- **nimble measured (2026-10-19, `scripts/measure_api_tags_nimble.py`):** 12
  APIs (10 registered; `cisa_kev` and `weather_gov` from a one-line
  description), one `choice` over the nine kinds and one `noul` per topic,
  confident at ≥ 0.7 (topics: no at ≤ 0.3), about 5 minutes.
  - **Kind: 12 of 12 right, all confident** (lowest 0.95, `cisa_kev`).
  - **Topics: 125 right, 16 can't tell, 3 wrong of 144 pairs.** It found 5 of
    the 6 hand "yes" pairs at ≥ 0.78. Two of the three wrong answers are
    false "yes" on search engines whose description is one line (`brave` →
    geopolitics 0.77, `serpapi` → semiconductors 0.83); 15 of the 16 "can't
    tell" are those two as well. The third is `hacker_news` → ai-ml at 0.19:
    a hand guess nimble disputes (HN is not mainly about AI).
  - **Reading:** yes for kind. Yes for topics when the kind is asked first and
    an API of kind `search` gets no topics (by definition a search engine
    belongs to no beat); then the only disagreement is the HN guess. Small
    sample: 12 APIs, 6 positive topic pairs. Re-run it on each API registered
    from here on before trusting it further.

### Phase 1: tags in the graph

- `(:ApiContext)-[:SERVES]->(:Topic)` and an `api_kind` property; `kind` and
  `topics` on define_api_context and ingest_api, checked against the lists.
  The tagger is the owner's hand edit, else nimble if Phase 0 said yes.
- A workflow step may declare its need (`needs: {topic, kind}`); steps that
  call `search_web` for a beat are the first candidates.
- `list_apis` shows tags and "used by" (the workflows whose steps name the
  context), so an unused API is visible where the agents look.
- api-integrator proposes tags at registration and says which.
- **Built 2026-10-19.**
  - The kinds list is `plugins/core/apis/_kinds.yaml`, mirrored as `(:ApiKind)`
    on every start.
  - `define_api_context` / `ingest_api` take `kind` and `topics`, checked before
    anything is saved. With neither, an untagged API is tagged by nimble
    (`crates/retrieve/src/api_tags.rs`, `API_TAGGER_MODEL`): kind first,
    a `search` API serves no topic, only answers ≥ 0.7 are stored.
  - `list_apis` shows `kind`, `topics`, `tags_by` and `used_by` (steps naming
    the context or calling under its `base_url`), and lists the untagged and
    unused APIs by name.
  - A step's `needs` is checked on every save; a seeded file's bad one is
    dropped with a warning.
  - The owner's tags are applied by `scripts/api_tags_owner.cypher` (10 APIs).
    The 37 needs are in 12 workflows.
  - Verified live: a bad kind and topics on a `search` API are refused with
    nothing saved; nimble tagged a test KEV context `advisories` (0.97) and
    `cybersecurity` (0.99) in about 39 s.
  - Not yet live: api-integrator's new tagging step. The agent is
    runtime-owned and shadows its file; reset it to pick the step up.
  - Not tested live: `ingest_api`'s tags. It shares the code path with
    `define_api_context`.

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

- **Built 2026-10-19.**
  - **Through the catalog:** `cisa_kev` (`apis/cisa_kev.yaml`, advisories,
    cybersecurity) feeds `vulnerability-watch`. `weather_gov` (personal
    bundle; its User-Agent carries the owner's contact) feeds `daily-news`.
    `list_apis` shows both under `used_by`, and `open_meteo` is now the
    unused forecast API (a 60-day retire candidate).
  - **`model-metadata-drift`** (weekly, brain-ops, no model) compares every
    `(:ModelDef)` with openviglet. First run: 10 confirmed, 0 differences; 3
    not in the catalog (Haiku 5.5, Sonnet 5.5, ministral-14b: the catalog
    was last updated 09-26); 12 not comparable (the catalog lists Ollama
    families with no limits; llamacpp is local). Prices are compared only
    where models.yaml charges. A Feed item opens only when the set of
    differences changes.
  - **`hn-ai-trial`** (weekly, ai-ml-research, no model) uses `hn_algolia`,
    registered for it: the Firebase `hacker_news` API needs one call per
    item, which a step cannot fan out. Each run takes one day of HN stories
    with 50+ points, keeps the AI/ML ones by keyword, and runs daily-news's
    own tech search with the same arguments. It records a `TRIAL:` line of
    counts plus both lists. The owner decides after several runs.
  - **Fixed after the first runs:**
    - vulnerability-watch's KEV window is now the prior report's date minus 3
      days; a fixed 14-day window re-listed old entries.
    - A code step drops news lines whose URL the prior report already cited
      (the model repeated them).
    - The trial's keywords no longer match a bare "agent".

### Phase 3: matching, as proposals

- A weekly pass finds needs (tagged steps) that a registered API's tags cover
  but that do not use it, and lists each match in one weekly digest item (owner, 2026-10-19): "`<api>` could
  feed step `<id>` of `<workflow>` instead of `search_web`." Its ask says
  what approving does: `schedule-author` drafts the changed workflow,
  disabled, for review.
- It also lists needs no API covers: that list becomes the api-scout workflow's input
  (Phase 4).

- **Built 2026-10-19: `api-match-digest`** (weekly, brain-ops, no model).
  - **The rule:** kinds must be equal, and `search` APIs are never proposed.
    A need with no topic matches on kind alone. A need with a topic matches
    an API that serves it, or a topic-less API of a non-beat kind.
  - **`beat` kinds** (news-feed, advisories, job-postings) are marked in
    `apis/_kinds.yaml`. A topic-less API of a beat kind matches only
    topic-less needs. Without that, the dry run proposed HN for 26 topical
    news steps, Michigan news included.
  - **First run:** 5 matches.
    - GitHub for off-grid-networking-monitor's firmware-release step.
    - openviglet for slm-benchmark-watch's model-release step.
    - HN for daily-news `business`.
    - openviglet for two rust-training-data-scout paper searches. These two
      are weak: reference-catalog / ai-ml is coarser than "papers".
  - Also on the first run: 30 steps in 12 kind/topic groups that no API
    covers (the Phase 4 input), and 8 steps already through the catalog.
  - **One Feed item** (`needs: action`) opens only when the set of matches
    changes. To take a match, open the item in chat and name it: the front
    door has schedule-author draft the change and saves it as a disabled copy
    for review.
  - **First handoff (2026-10-19):** handing the item straight to
    schedule-author failed. It is read-only by design (it drafts, the
    orchestrator saves), and four drafts in one answer ran past its output
    limit, so nothing was saved. Claude Code wrote the four RSS drafts
    instead: `vulnerability-watch-rss`, `surveillance-privacy-watch-rss`,
    `zero-trust-architecture-research-rss` and `hardware-tripwire-rss`.
    - Each is disabled with no schedule, its feeds added beside the
      searches, and it stores under `*_rss_draft`.
    - Feed and parse tested: 9, 2, 0 and 1 items kept in the last 7 days.
  - **Lesson:** the steps it reads hold placeholders (`{{date}}`,
    `{{_prev}}`) that the match step's own templating filled in, which broke
    the JSON. The query defuses `{{` first.

### Phase 4: demand-driven discovery and retirement

- The api-scout workflow searches for the uncovered needs from Phase 3, not its fixed
  interest areas. An API that needs a key becomes an owner item ("get a key
  at <url>; it would feed <workflow>"), which is where the Vault plan lands.
- An API used by no workflow for 60 days (and not a seeded engine) gets a
  "retire it?" item with its probe history; 60, not 30, so a monthly
  workflow's API is never caught between runs (owner, 2026-10-19).
- Between APIs that serve the same need, a ladder with a ledger, built on the
  search-rotation machinery, only once a real pair exists.

- **Built 2026-10-19.**
  - **Use is recorded.** Every `http_request` through a context stamps
    `last_used_at`; registration sets `created_at`. Existing APIs start their
    clock at `use_tracked_from` (today), because nothing recorded use before.
    `retire_exempt` (search engines, GitHub, the brain's own API) keeps an API
    used outside workflows off the list.
  - **`api-retire-check`** (weekly, brain-ops, no model) lists APIs that no
    enabled workflow names and nothing has called for 60 days, with their
    probe history. A Feed item asking retire / keep / exempt opens only when
    an API newly goes idle. Today the candidates are aidevboard (reached
    through its MCP server, not this context), hacker_news, home_assistant
    and open_meteo; none is due before 2026-12-18.
  - **`api-scout`** works from demand: it takes the latest digest's NEEDS NO
    API COVERS, picks the two largest groups not scouted in its last four
    runs, and gives api-integrator those needs. It names the steps that need
    them and carries a not-again list (GDELT, Currents, Webz, ApiTube,
    SearXNG). A news API must be measured on the step's own query first.
    Key-gated finds become `needs: action` items naming where to get the key,
    the env var, and the steps they would feed.
  - **First scout runs** went to free nemotron / gpt-oss-20b once the reset
    dropped api-integrator's Haiku pin. One asked a question no one reads;
    the other said no outlet serves a keyless RSS feed, without fetching any.
    In fact BleepingComputer, The Record, SecurityWeek and EE Times all serve
    one. The owner registered those four as `news-feed` APIs
    (`apis/*_rss.yaml`) and pinned api-integrator to `ministral-14b-latest`
    until the Ollama Cloud quota resets (about 2026-10-21; OpenRouter has no
    free gpt-oss). The scout's task now requires a fetch behind any "does not
    exist".
  - **The beat rule works both ways:** a topic-less need no longer matches
    topical feeds. The cybersecurity feeds had been proposed for daily-news
    business.
  - **The ladder** waits for a real pair: two APIs serving one need, both in
    use. `weather_gov` and `open_meteo` share the forecast kind, but only
    one is used.

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
