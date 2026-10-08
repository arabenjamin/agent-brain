# Source Quality Plan — the graph is only as good as what enters it

**Status: Phase 0 done (2026-10-06); Phases 1–5 planned, nothing built.**

The model and the code decide how well the brain reasons over its knowledge.
They cannot fix knowledge that came from the wrong place. This plan is about
**intake**: which sources feed the graph, how much each is trusted to assert,
and how the brain keeps finding the right sources on its own — using free data
only.

## 1. Where the graph's knowledge comes from (measured 2026-10-06, last 30 days)

### Claims by origin

| Origin | Claims | Verification attempted | Corroborated |
|---|---|---|---|
| Video learning (15 YouTube channels) | **2,073** (87%) | 279 (13%) | 88 |
| Daily news | 161 | 28 | 21 |
| Intelligence-agency monitor (vetted list) | 58 | 37 (64%) | **27 (73%)** |
| Off-grid networking monitor | 38 | 8 | 2 |
| SLM benchmark watch | 16 | 8 | 4 |
| OSINT article feeds (started 2026-10-06) | 8 | 0 | 0 |

Top asserting sources: NewsNation 247, 60 Minutes 207, Adam Kinzinger 167,
Louis Rossmann 140, Cappy Army 137, IBM Technology 131, C-SPAN 112,
Channel 5 (Andrew Callaghan) 74, Julian Dorey 70, Hasan Minhaj 57. That is
reporting, politicians, commentary, long-form interviews and comedy, all
extracted identically (3 claims per video, every video).

**Reading:** verification capacity (~80/day) is spent on an intake dominated by
commentary, so 87% of claims are never checked; the one intake that searches a
vetted list produces claims that get checked and mostly hold up.

### "Established knowledge" written from unrestricted search

The curiosity engine (`chains/fill-knowledge-gap.yaml`, `chains/learn.yaml`)
wrote **~800 `semantic` notes** in 30 days — the type that means *the brain
established this* — from searches with no source list. **28% cite no URL.**
Retrieval does not label `semantic` notes, so nothing downstream can tell them
from vetted knowledge.

### Scheduled searches still unrestricted: 16 of 27

- `daily-news.yaml` — 7 searches, `engine: exa`, no list (Detroit/Michigan
  queries included, though `michigan-news` exists).
- `off-grid-networking-monitor.yaml` — 3 searches, no list exists for the beat.
- `fill-knowledge-gap.yaml`, `learn.yaml` — `{{goal_topic}}`, any topic.
- `discover-media-sources.yaml` — unrestricted by design (discovery).
- `weekly-news.yaml` — restricted, but by inline `site:` lists, not SourceLists.

## 2. Principles (each learned the hard way on 2026-10-06)

1. **Measure a source on the brain's real queries before trusting it.** Run the
   actual schedule queries, current setup vs proposed, and class every result
   by SourceList. Currents (0 results on every real query) and Webz (QAnon-style
   and off-topic pages, zero listed outlets) were promoted on quotas and features
   alone and had to be reverted within the hour. GDELT was 98% unlisted outlets.
2. **An engine ranks; a list decides who is trusted.** Quality comes from vetted
   per-domain SourceLists, not from any search API.
3. **Coverage is not corroboration.** A repost under the same headline is one
   source (`check_independence_titled`); a platform post or press-release feed
   is not a source (`platforms`, matched by link host — `markets.ft.com`).
4. **State it, don't infer it.** Freshness, source class and corroboration tier
   are computed and carried in the data; a local model reading timestamps called
   a week-old backfill "current".
5. **Free, primary, structured beats free, secondary, scraped.** A filing, a
   rule, a release note or a paper is the thing itself; news is someone's
   account of it.

## 3. Phases

### Phase 0 — Vetted lists for the standing monitors ✅ (2026-10-06)

Done: `national-security`, `semiconductors`, `batteries-materials` SourceLists,
each tested against its schedule's real queries before use; hardware tripwire on
lists for all five sweeps (split the mixed substitution sweep); intel monitor's
Russian sweep on `national-security`; `osint` list and 8 OSINT article feeds;
`platforms` gained flipboard.com and markets.ft.com; syndication counts once;
Currents/Webz out of the default ladder; GDELT retired; ACLED parked. Verified
by a live tripwire run: every result from a vetted list, report specific and
appropriately sceptical.

### Phase 1 — Extraction proportional to source type (largest lever)

Problem: commentary, interviews and comedy assert as loudly as reporting, and
swamp verification.

1. Add `source_type` to `:MediaSource` (and the `sources-media/*.yaml` schema):
   `reporting` | `primary` | `analysis` | `commentary` | `interview` |
   `entertainment`. Unset = `commentary` (the conservative default).
2. Draft a tag for each of the 15 YouTube channels and 8 article feeds, with a
   one-line reason, **for the user to review** — the watchlist is user-curated
   and the tags are judgement calls (e.g. C-SPAN = primary, 60 Minutes =
   reporting, Hasan Minhaj = entertainment, Julian Dorey = interview).
3. Per type, in `chains/video-learning.yaml` / `chains/article-learning.yaml`:
   `reporting`/`primary` keep claim extraction (3/2); `analysis` 1;
   `commentary`/`interview`/`entertainment` 0 — still summarised and stored as
   `source_record`, so nothing is lost, but no claims enter the verification
   queue. The chain reads the type from the `:Media`→`:MediaSource` link.
4. Carry `source_type` into the `source_record` label so retrieval shows it.

Done when: the share of new claims from commentary/interview/entertainment is
near zero, and the verification attempt rate for new claims rises above 50%.

### Phase 2 — Topic → SourceList routing for every search

Problem: only schedules that name a list get vetted results; gap fills, the
research agent and chat search everything.

1. `sources/topic-routes.yaml`: topic → list, each topic a small set of
   keywords/entities (natsec → `national-security`; chips/HBM/CoWoS →
   `semiconductors`; batteries/minerals → `batteries-materials`; Michigan/Detroit
   → `michigan-news`; ML models/benchmarks → `ml-research`; OSINT → `osint`).
2. `search_web` applies a route when the caller names no `source_list`
   (deterministic keyword match, no LLM), reports which route it applied, and
   falls back unrestricted — with the existing warning — when the routed search
   is empty. An explicit `source_list: none` opts out.
3. Before enabling a route: run the Principle-1 test for it.

Done when: gap-fill and researcher searches on mapped topics return ≥ 80%
listed outlets (measured by the Phase 5 report).

### Phase 3 — Curiosity-engine output held to the same standard

1. A gap fill whose answer cites no URL is stored as `unsourced_synthesis`
   (already a labelled, consolidation-excluded type), not `semantic`.
2. A sourced gap fill is stored as `source_record` with its citations, plus
   claims extracted like any other source.
3. Audit the ~800 `semantic` gap-fill notes from the last 30 days: retype the
   unsourced ones (same method as `scripts/migrate_unsourced_gap_notes.cypher`).

Done when: no `semantic` note is written by a chain without at least one cited
URL.

### Phase 4 — Free primary-source feeds

Each is free, structured, and is the record itself rather than coverage of it.
Each goes through Principle 1 before any schedule relies on it.

| Source | Access | Serves |
|---|---|---|
| arXiv API | keyless | SLM watch (papers, not posts about papers) |
| GitHub Releases API | keyless (rate-limited), token optional | off-grid monitor (Meshtastic, Reticulum, RNode firmware) |
| Federal Register API | keyless | export controls, rules, sanctions notices |
| BIS (bis.gov) | public pages/feeds | export-control rules (already on `semiconductors`) |
| SEC EDGAR | keyless (User-Agent required) | Micron/SK hynix ADR/Samsung-adjacent filings, results |
| ReliefWeb | approved appname (pending) | humanitarian reports (ETL built, paused) |

Route: `ingest_api` for the API shape; the ETL layer (`services/datasets/`) for
anything that is rows rather than documents; article feeds where an RSS exists.
Also build the missing `off-grid` SourceList (meshtastic.org,
reticulum.network, hackaday.com, cnx-software.com, …) under Principle 1.

### Phase 5 — The measurement becomes a standing report

What surfaced Webz was a question from the user. Make it routine:

1. A weekly `source-review` schedule (extend `dataset-review`'s pattern):
   - per source (channel, feed, outlet): claims extracted, attempted,
     corroborated, contradicted, and corroboration rate;
   - per scheduled search: results returned, share from a vetted list,
     SearXNG-fallback count;
   - per list: hit rate and dominant domains (one publisher crowding a list —
     pv magazine + ESS News — is flagged).
2. Stated, not inferred (Principle 4): every judgement in the report is computed
   in the tool; the local model only arranges it.
3. Notifies the user, like the dataset review.

Done when: a bad source is flagged by the report before anyone asks.

## 4. Order and open questions

Order: **Phase 1 → 2 → 3 → 5 → 4.** Phase 1 changes the most of what enters the
graph and needs the user's tagging decisions; Phase 2 generalises what was done
by hand for the monitors; Phase 3 stops the curiosity engine writing
unverified knowledge as established; Phase 5 keeps all of it honest; Phase 4
adds new primary sources once the machinery to judge them is in place.

Open questions for the user:
- Phase 1: the tag for each channel (draft to be proposed), and whether any
  channel should leave the watchlist rather than be re-tagged.
- Phase 2: whether chat searches should route by topic too, or only background
  work.
- Phase 4: which primary feeds first (proposal: GitHub Releases for off-grid and
  arXiv for the SLM watch — both keyless and the monitors are unrestricted or
  secondary today).

## 5. Related

- `project-docs/subsystem-rules.md` § Epistemics, § Web Search, § ETL datasets,
  § Media Learning.
- `project-docs/engineering-history.md` — 2026-10-06 entries (claim
  verification review, GDELT retirement, news engines reverted, source lists).
- Known follow-ups not in a phase: Exa mis-dates archive pages (EE Times) as
  current; `markets.ft.com` still surfaces in `ft.com`-listed searches; gallium /
  minor metals thin on every list (Argus rarely surfaces).
