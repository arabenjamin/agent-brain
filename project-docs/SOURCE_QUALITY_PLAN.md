# Source Quality Plan — the graph is only as good as what enters it

**Status: Phase 0 done (2026-10-06). Source tiers set by the owner (§ 3a) and
Phase 1 built and staged (2026-11-05): channel tiers, extraction per tier,
re-tiering with history. Next: the re-rank proposal, with Phase 5.**

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

The curiosity engine (`workflows/fill-knowledge-gap.yaml`, and `chains/learn.yaml` until it was retired in Phase 2b)
wrote **~800 `semantic` notes** in 30 days — the type that means *the brain
established this* — from searches with no source list. **28% cite no URL.**
Retrieval does not label `semantic` notes, so nothing downstream can tell them
from vetted knowledge.

### Scheduled searches still unrestricted: 16 of 27

- `daily-news.yaml` — 7 searches, `engine: exa`, no list (Detroit/Michigan
  queries included, though `michigan-news` exists).
- `off-grid-networking-monitor.yaml` — 3 searches, no list exists for the beat.
- `fill-knowledge-gap.yaml` — `{{inputs.topic}}`, any topic (`learn.yaml` is retired).
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

## 3a. Source tiers (owner, 2026-11-05)

From most to least trusted to assert a fact:

1. **`primary`**: peer-reviewed work or the primary record, i.e. a paper, filing,
   rule, court record, release note, or the proceedings themselves.
2. **`reporting`**: original reporting by an outlet with editors and named sources.
3. **`reference`**: a tertiary summary such as Wikipedia. It's good for orienting
   and pointing to sources, but it isn't evidence on its own.
4. **`analysis`** and **`commentary`** (one tier): secondary synthesis, opinion,
   interviews, entertainment. `analysis` is careful secondary work; `commentary`
   is someone's argument. They differ only in claim extraction (Phase 1).
5. **`platform`**: a post on a user platform (Reddit, X, Medium, a press-release
   wire). It isn't a source, per Principle 3 and the existing `platforms` list.

A tier is stated in the data, never inferred by a model (Principle 4). It has
four uses:

1. **Retrieval ordering.** Within otherwise equal results, the higher tier
   ranks first, and every retrieved note shows its tier.
2. **Corroboration needs a source above commentary.** Evidence that is only
   `analysis`, `commentary` or `platform` can't make a claim `corroborated`.
   Measured 2026-11-05: 1 of 1,192 corroborated claims rests only on platforms or
   Wikipedia, so the tightening costs almost nothing. The real gap is the 433
   that rest only on *unclassified* domains, which means classifying domains,
   not channels.
3. **Notes carry their source type**, from the `:MediaSource` for media and from
   the domain's class for web evidence.
4. **Low-tier claims are verified first.** A claim from `analysis` gets
   verification priority over one from `reporting`, because it is the one most
   likely to need it.
5. **A low tier is a lead to a higher one** (owner, todo 8f672eb1). A Wikipedia
   article, a commentary video or a platform post is read for what it cites:
   follow its links and references to the paper, filing or report, and store
   and cite that instead. The verifier does the same with low-tier evidence
   before it counts it.

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

### Phase 1 — Extraction proportional to source type (largest lever) — built 2026-11-05, staged

Problem: commentary, interviews and comedy assert as loudly as reporting, and
swamp verification.

1. ✅ `source_type` on `:MediaSource` and in the `sources-media/*.yaml` schema,
   using the tier names in § 3a: `primary` | `reporting` | `analysis` |
   `commentary`. (`reference` and `platform` describe web pages, not channels.)
   Interviews and entertainment are `commentary`: what a guest or a comedian
   says is that person's claim, and nobody vetted it. Unset = `commentary`, the
   conservative default, and that includes a chat hand-off of a video from a
   channel the watchlist doesn't have.
2. ✅ Tags, decided by the owner on 2026-11-05. They are applied by
   `scripts/apply_source_types_2026-11-05.cypher` (run after the deploy) and are
   in the shipped seed files for a fresh install. Items = items ingested in the
   last 30 days.

   | Source | Kind | Items | Type | Why |
   |---|---|---|---|---|
   | c-span | video | 78 | `primary` | The proceedings themselves. Each claim is attributed to its speaker, so a senator's statement is checked as the senator's claim. |
   | citizen-lab | article | 14 | `primary` | Forensic research reports with published evidence: the finding itself, not someone's account of it (owner). |
   | 60-minutes | video | 77 | `reporting` | Original investigative reporting with named sources. |
   | vice-news | video | 8 | `reporting` | Documentary field reporting. |
   | the-record | article | 82 | `reporting` | Cyber and intel news with its own reporters. Owned by Recorded Future, a threat-intel vendor, so its stories about Recorded Future research aren't independent. |
   | occrp | article | 79 | `reporting` | Investigations built on documents and published sources. |
   | icij | article | 0 | `reporting` | Cross-border leak investigations. |
   | bellingcat | article | 15 | `reporting` | Open-source investigations with published methods. |
   | c4ads | article | 0 | `reporting` | Data-driven investigations of illicit networks. |
   | louis-rossmann | video | 32 | `analysis` | Open-source journalism that is also commentary (owner): placed between the two. |
   | channel-5 | video | 21 | `analysis` | Open-source field journalism that is also commentary (owner): placed between the two. |
   | cappy-army | video | 40 | `analysis` | Military analysis from public reporting and imagery. |
   | johnny-harris | video | 2 | `analysis` | Explainer journalism that synthesises others' reporting. |
   | two-minute-papers | video | 6 | `analysis` | Summaries of papers. The paper itself is primary, and arXiv is registered. |
   | andrej-karpathy | video | 0 | `analysis` | A practitioner teaching from his own work. |
   | dfrlab | article | 11 | `analysis` | Think-tank research on influence operations. NATO-aligned (Atlantic Council). |
   | 38-north | article | 1 | `analysis` | Think-tank analysis, including satellite imagery (Stimson Center). |
   | news-nation | video | 80 | `commentary` | Mostly commentary (owner): opinion-hour clips. |
   | adam-kinzinger | video | 66 | `commentary` | A former politician's opinion. |
   | hasan-minhaj | video | 29 | `commentary` | Political comedy. |
   | julian-dorey | video | 18 | `commentary` | Long-form interview podcast. Guests' claims aren't vetted. |
   | ibm-technology | video | 33 | `commentary` | Vendor explainers restating general knowledge. |
   | deep-learning-ai | video | 9 | `commentary` | Course and explainer content. |

   The inactive sources with no items (elastic, clevercast, air-io, intellipaat,
   data-school, machine-learning-street-talk, fireship, ken-jee,
   traversy-media, linking-your-thinking, lex-fridman,
   learning-from-machine-learning) stay unset. Some video `:Media` has no
   `FROM_SOURCE` link (Julian Dorey 7, plus one-offs from arXiv, Reuters and
   PBS), so it reads as unset.

   Effect, estimated from these item counts at today's per-item rates: about
   1,900 new claims every 30 days falls to about 1,030 (489 from
   `primary`/`reporting` video, 380 from articles, 113 from `analysis`, and
   about 47 from the commentary sample). That is well under the verifier's
   ~2,400 per 30 days, against 13% of claims attempted on 2026-10-06.
3. ✅ Per type, `claim` extract takes `source_type` (from `ingest_media` /
   `ingest_article`, which read it through `:Media`→`:MediaSource`) and
   `limits_by_type` from the workflow. `video-learning`: primary/reporting 3,
   analysis 1, commentary 0. `article-learning`: 2, 1, 0. A commentary item is
   still summarised and stored as a `source_record`, so nothing is lost, but
   only a sample of its claims enters verification: `probe_every: 5` gives one
   claim from every fifth item, picked by a stable hash of the note. That
   sample keeps a track record growing for the re-ranking in step 5. Each claim
   stores the tier it was extracted under (`claim.source_type`), as does its
   source note.
4. Show `source_type` in the `source_record` label in retrieval (§ 3a, use 1).
   **Not built.** The notes carry it now; the label doesn't read it yet.
5. **Re-ranking as we watch** (owner, 2026-11-05: "There was a time when I had
   more trust for various outlets… as things change, so does the
   trustworthiness of the outlet"). A tier is a judgement at a date, not a
   fixed fact.
   - ✅ **History is kept.** `manage_media_source` retype changes a tier, needs
     a reason, runs only on the owner's request, and writes a dated
     `(:MediaSource)-[:RETYPED]->(:SourceTypeChange {from, to, at, by,
     reason})`; `history` lists them. A claim keeps the tier it was extracted
     under, so a source that falls from `reporting` doesn't recast its old
     claims, and its record can be read by period.
   - ✅ **Every tier keeps a record.** Commentary yields its 1-in-5 sample;
     analysis yields 1 an item.
   - **The proposal** (built with Phase 5's `source-review`, not yet): per
     source and per tier period, claims attempted, corroborated, contested and
     owner-confirmed refuted over the last 90 days. When a source's record
     diverges from its tier, the review files a Feed item proposing a move,
     with the numbers. Examples: a `reporting` source with owner-confirmed
     refutations, or corroborating well below its tier's median; or a
     `commentary` source whose sample corroborates like reporting. The owner
     answers in the item, and the front door calls retype. Nothing re-ranks
     itself. Thresholds are set from the first month of tiered claims, not
     guessed now.

Done when: the share of new claims from commentary is near the 1-in-5 sample,
and the verification attempt rate for new claims rises above 50%.

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
   - per scheduled search: results returned, share from a vetted list;
   - per list: hit rate and dominant domains (one publisher crowding a list —
     pv magazine + ESS News — is flagged).
2. Stated, not inferred (Principle 4): every judgement in the report is computed
   in the tool; the local model only arranges it.
3. Notifies the user, like the dataset review.
4. Proposes re-tiering a source whose record has moved away from its tier
   (Phase 1 step 5): a Feed item the owner answers, never a change it makes.

Done when: a bad source is flagged by the report before anyone asks.

## 4. Order and open questions

Order: **Phase 1 → 2 → 3 → 5 → 4.** Phase 1 changes the most of what enters the
graph and needs the user's tagging decisions; Phase 2 generalises what was done
by hand for the monitors; Phase 3 stops the curiosity engine writing
unverified knowledge as established; Phase 5 keeps all of it honest; Phase 4
adds new primary sources once the machinery to judge them is in place.

Open questions for the user:
- Phase 1: whether any channel should leave the watchlist rather than sit at
  `commentary` (tiers decided 2026-11-05).
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
