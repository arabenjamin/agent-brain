# Media Learning Plan — Watch & Summarize Videos to Learn and Stay Current

Handoff plan for implementing autonomous video/audio ingestion in the Agent Brain.
Goal: let the brain **watch and summarize videos (and podcasts/audio)** so it can
**learn new concepts** and **stay up to date on topics it already knows**.

This document is the spec. Implement it in phases; each phase is independently
shippable and testable.

---

## 1. Design decisions (locked)

| Decision | Choice |
|----------|--------|
| Transcript source | **Captions first, Whisper fallback.** Pull YouTube/native captions via `yt-dlp` (free, no GPU). Only download audio + transcribe with Whisper when a video has no captions. |
| Source scope | **All of:** YouTube channels/playlists, ad-hoc video URLs, podcast RSS / audio files, local video files. |
| Trigger model | **Both:** autonomous scheduled watch of a channel/feed watchlist **and** on-demand ingestion of arbitrary links. |

These map onto the existing architecture almost 1:1 — the `learn` chain
(`chains/learn.yaml`: search → reason → store_note → update_task) is the exact
shape a video-learning chain takes, and a YouTube channel watchlist is the
`SourceList` pattern (`sources/*.yaml`, graph-owned after first seed).

---

## 2. Why this fits the current architecture

The brain already has every primitive this feature needs:

- **External-fetch skill pattern** — `skills/search.rs` shells to an HTTP API,
  returns JSON, degrades gracefully when unconfigured (logs a knowledge gap).
  Media ingestion is the same shape with `yt-dlp` instead of SerpApi.
- **Fetch → reason → store_note chains** — `chains/learn.yaml` and
  `chains/fill-knowledge-gap.yaml`. A video chain is a drop-in sibling.
- **Task-driven scheduler** — `status=created` Tasks are matched to chains by
  `goal_to_steps()` and dispatched. New videos become Tasks; the scheduler does
  the rest. No new dispatch machinery required.
- **YAML-seeded, graph-owned config lists** — `SourceList` (`sources/*.yaml`),
  `ScheduledTask` (`schedules/*.yaml`), `SchedulerChain` (`chains/*.yaml`). A
  `MediaSource` watchlist follows the `SourceList` seeder pattern exactly.
- **store_note pipeline** — already runs async entity extraction
  (`spawn_extract_entities`), similarity linking, and spaced-repetition
  scheduling. Video summaries flow into this for free, so concepts become
  `:Entity` nodes and relate to existing knowledge automatically.
- **Per-step model routing** — `provider_hint` / `required_capabilities` let the
  cheap map-summaries run on the local model and the final synthesis run on a
  stronger cloud model (see how `daily-news.yaml` uses `provider_hint: ollama-cloud`
  only for the final write step).

**Net:** almost no new *architecture*. The new surface is one skill, one service
module, a couple of graph nodes, and YAML for chains/schedules/watchlist.

---

## 3. New components

### 3.1 `services/media.rs` — transcript acquisition + summarization service

Owns the subprocess boundary and the LLM map-reduce. Pure logic, no MCP types.

Responsibilities:
1. **Metadata** — `yt-dlp -J <url>` → parse JSON for `id`, `title`, `uploader`,
   `channel_id`, `upload_date`, `duration`, `description`, `webpage_url`,
   available `subtitles` / `automatic_captions`.
2. **Captions** — request `en` (configurable) subtitle track in `json3` or `vtt`
   via `yt-dlp --skip-download --write-subs --write-auto-subs --sub-lang en
   --sub-format json3 -o <tmp>`; parse the cue file into plain text (strip
   timestamps/formatting). Prefer human subs over auto-captions when both exist.
3. **Whisper fallback** (Phase 4) — if no caption track: `yt-dlp -x --audio-format
   mp3 -o <tmp>` then transcribe via a pluggable transcriber (see §6).
4. **Map-reduce summarization** — transcripts run long (a 1-hr talk ≈ 8–12k
   words), and the chain is fixed-length, so **the windowing must happen *inside*
   the tool, not as chain steps.** Chunk the transcript (reuse the knowledge
   chunker's philosophy: split on paragraph, then sentence, ~1500-char windows),
   summarize each window ("map") with the local model, then synthesize
   ("reduce") into a structured summary with a stronger model. Return:
   `{ video_id, title, channel, channel_id, url, published_at, duration_secs,
   transcript_source: "captions"|"whisper", summary, key_concepts: [..],
   transcript_len }`. Optionally include the full transcript when asked.

Subprocess rules:
- Invoke `yt-dlp`/`ffmpeg` with **arg arrays, never a shell string** (no
  injection surface). Validate the URL scheme (`http/https`, or a `file://`
  allowlisted to `MEDIA_DIR` for local files).
- Binaries are configurable (`YT_DLP_PATH`, `FFMPEG_PATH`) and default to PATH
  lookup. If the binary is missing, return a clear, actionable error and (like
  `search.rs`) log a tool-config knowledge gap — do **not** panic.
- Temp files go under `MEDIA_DIR` (default: the session scratch/tmp); clean up
  after parse.

### 3.2 `skills/media.rs` — MCP tools

New `MediaSkill`. Holds `Arc<RwLock<Option<LlmConfig>>>` (read per call via
`make_llm()`, same as other skills) plus `Neo4jClient` and the media service.
Tools:

- `ingest_media(url, source_context?, store?)` — the workhorse. Fetch metadata +
  transcript (captions→whisper), run map-reduce summary, upsert a `:Media` node
  for dedup, and return the structured summary. When `store: true`, also writes
  the summary `:Note` and links it (see §4). Default `store: false` so chains
  keep control of storage (consistent with `search_web` returning, `store_note`
  storing).
- `fetch_transcript(url, lang?)` — lower-level: return raw transcript text only
  (no summary). For direct Q&A / RAG grounding.
- `list_channel_videos(source, since?)` — given a channel/playlist/RSS ref,
  return recent videos **filtered to those not yet ingested** (no `:Media` node).
  Uses YouTube's free per-channel RSS feed
  `https://www.youtube.com/feeds/videos.xml?channel_id=<id>` (and
  `?playlist_id=<id>`) — no API key, ~15 latest items with IDs + publish dates.
- `poll_media_sources()` — iterate all active `:MediaSource` nodes, call
  `list_channel_videos` for each, and **`create_task("watch video: <url>", …)`**
  for every new video. This is the autonomous entry point; the scheduler then
  routes each Task through the video-learning chain. (Chains can't loop over a
  dynamic list, so we fan out into Tasks instead — matches the Task-driven core.)
- `manage_media_source(action, name, kind, ref, description?, active?)` —
  runtime CRUD for the watchlist (mirrors `manage_scheduled_task`). YAML is the
  seed source; graph is authority after first create (see §5).

**Registration:** register every tool in BOTH `tool_registry` (for `tools/list`)
AND the `skills` vec (for `tools/call`) in `brain_core.rs::build_skills()` —
forgetting either yields an invisible tool or a dispatch failure. `self_model.rs`
introspection re-syncs the meta-graph automatically at the end of `build_skills`.

### 3.3 Chains

**`chains/video-learning.yaml`** — pattern `"watch video:"` (+ patterns
`"summarize video"`, `"ingest video"`). This is the learning loop:

1. `ingest_media` — `url` extracted from the goal; returns structured summary.
   `provider_hint: ollama` for the map pass; the reduce inside the tool can use a
   `required_capabilities: ["reasoning"]` step-level hint.
2. `search_notes` — query on the returned `key_concepts` to pull *existing*
   related knowledge (this is what turns "summarize" into "learn").
3. `reason` — **context = `{{_prev}}`** (critical: scheduled `reason` steps must
   pass prior output via `context`, never inline in the question, or RAG eats its
   own output → garbage loop; see the reason-RAG-contamination note). Prompt:
   "Given this new video summary and the existing related notes, state (a) what
   is genuinely NEW vs. what the brain already knew, (b) what CHANGED/updated an
   existing understanding, (c) concepts that are important but thinly covered."
   Use `provider_hint: ollama-cloud` for this synthesis step.
4. `store_note` — `note_type: semantic`, `source_context: video_learning`,
   content = the video summary + the new/changed analysis. Links to `:Media`
   (via `ingest_media` storing, or a follow-up `neo4j_query` step creating
   `(:Note)-[:SUMMARIZES]->(:Media)`).
5. `create_task` (conditional) — for each thinly-covered key concept, create a
   `"fill knowledge gap: <concept>"` Task, feeding the **existing** curiosity
   engine (`chains/fill-knowledge-gap.yaml`). This is how watching a video
   triggers deeper autonomous research.
6. `update_task` — mark the video Task `completed`.

Set `no_adversarial: true` (ingestion isn't high-stakes planning). Let the
evaluator run only if the Task carries `success_criteria`.

### 3.4 Schedules

**`schedules/media-watch.yaml`** — a single-step ScheduledTask running
`poll_media_sources` on an interval (default 6h / 21600s). It creates one
`"watch video: <url>"` Task per new video; the scheduler dispatches them through
`chains/video-learning.yaml` on subsequent ticks. Gate the whole thing behind
`MEDIA_WATCH_ENABLED` so deployments without `yt-dlp` don't spawn failing tasks.

---

## 4. Data model additions

New nodes (declare indexes in `repository/client.rs::init_schema`):

- **`:MediaSource`** — the watchlist. `{ name (unique), kind:
  'youtube_channel'|'youtube_playlist'|'podcast_rss', ref (channel_id /
  playlist_id / rss_url), description, active: bool, managed_by: 'yaml'|'runtime' }`.
  YAML-seeded (§5). Analogous to `:SourceList`.
- **`:Media`** — dedup + provenance ledger. `{ id (video_id, unique), url, title,
  channel, channel_id, published_at, duration_secs, transcript_source, ingested_at,
  source_media_name }`. Existence check gates re-ingestion.

Relationships:

- `(:Media)-[:FROM_SOURCE]->(:MediaSource)` — which watchlist entry surfaced it.
- `(:Note)-[:SUMMARIZES]->(:Media)` — the summary note ↔ its source video.
- Reuse existing `(:Note)-[:MENTIONS {count}]->(:Entity)` from the store_note
  pipeline — concepts in the summary become first-class entities and connect to
  everything else the brain knows. No new extraction code needed.
- Optional later: `(:Media)-[:COVERS]->(:Entity)` for direct video→concept lookup.

**"Stay up to date on things it already knows"** falls out of this: the watch
schedule tracks channels about known topics; a new video that `MENTIONS` an
`:Entity` the brain already has, and whose `reason` step flags a CHANGE, gets an
`inference`/`outcome` note capturing *what updated* — surfacing deltas rather than
re-storing what's known.

---

## 5. Watchlist seeding (`sources-media/*.yaml`)

New seed dir `SOURCES_MEDIA_DIR` (default `./sources-media`), a new
`services/media_source_seeder.rs` modeled on `services/source_seeder.rs`:
**seed ON CREATE only**, graph-owned thereafter (runtime edits via
`manage_media_source` / `neo4j_query` persist across restarts; delete a node to
re-seed from YAML). Missing directory is non-fatal.

Example `sources-media/ai-research.yaml`:
```yaml
name: ai-research
description: Channels covering AI/ML research and engineering.
sources:
  - kind: youtube_channel
    ref: UCbfYPyITQ-7l4upoX8nvctg   # channel_id
    description: Two Minute Papers
  - kind: youtube_playlist
    ref: PLxxxxxx
    description: A curated playlist
```
Per the **no-hardcoded-data-in-Rust** rule, all channel IDs / feed URLs live in
YAML + graph, never Rust literals.

---

## 6. Whisper transcriber abstraction (Phase 4)

Mirror the LLM provider pattern (`services/llm.rs` is multi-provider, config-driven).
Add a `Transcriber` trait with implementations selected by env:

- `WHISPER_PROVIDER` = `none` (default) | `whisper-local` (whisper.cpp / faster-whisper
  binary) | `openai` | `groq` (both cheap hosted Whisper).
- `WHISPER_MODEL`, `WHISPER_API_KEY`, `WHISPER_BASE_URL`, `WHISPER_BIN_PATH`.

Only invoked when captions are absent. Podcasts and local files (§ Phase 5)
always use this path.

---

## 7. Config / env vars (add to `config.rs` + CLAUDE.md table)

| Variable | Default | Description |
|----------|---------|-------------|
| `YT_DLP_PATH` | `yt-dlp` | Path to yt-dlp binary. |
| `FFMPEG_PATH` | `ffmpeg` | Path to ffmpeg (audio extraction for Whisper). |
| `MEDIA_DIR` | session scratch | Temp dir for downloads/caption files; also the allowlist root for `file://` local ingestion. |
| `MEDIA_CAPTION_LANG` | `en` | Preferred caption language. |
| `MEDIA_MAX_DURATION_SECS` | `10800` | Skip videos longer than this (cost guard). |
| `MEDIA_WATCH_ENABLED` | `false` | Enable the autonomous watch schedule. |
| `SOURCES_MEDIA_DIR` | `./sources-media` | MediaSource watchlist YAML seeds. |
| `WHISPER_PROVIDER` | `none` | `none`/`whisper-local`/`openai`/`groq`. |
| `WHISPER_MODEL` / `WHISPER_API_KEY` / `WHISPER_BASE_URL` / `WHISPER_BIN_PATH` | — | Whisper config (Phase 4). |

---

## 8. Phased implementation

> **Status (implemented):** Phases 1–3 are built and unit-tested (`MediaService`,
> `MediaSkill` with 5 tools, `:Media`/`:MediaSource` nodes + schema, the
> `media_source_seeder`, `chains/video-learning.yaml`, `schedules/media-watch.yaml`,
> `sources-media/ai-research.yaml`, all wired into `build_skills`). Phase 4
> (Whisper) and Phase 5 (podcasts/local files) are **stubbed seams** — caption-less
> media errors cleanly and non-YouTube feed kinds return a "not supported yet"
> error, rather than half-working. Requires `yt-dlp` on PATH at runtime.

**Phase 1 — Captions MVP, on-demand (highest value, lowest cost).**
`services/media.rs` (metadata + caption fetch + map-reduce summary), `MediaSkill`
with `ingest_media` + `fetch_transcript`, `:Media` dedup node,
`chains/video-learning.yaml` routed by `"watch video:"`, summary stored as a
`semantic` note. Deliverable: paste a URL / create a "watch video:" Task → get a
stored, entity-linked summary.

**Phase 2 — Autonomous channel/playlist watch.** `:MediaSource` node + YAML
seeder + `manage_media_source`, `list_channel_videos` (RSS), `poll_media_sources`,
`schedules/media-watch.yaml`. Deliverable: tracked channels auto-ingest new
uploads.

**Phase 3 — Learning loop.** Add the `search_notes` + compare `reason` +
gap-task-spawning steps to the chain; `SUMMARIZES` links; new-vs-changed
analysis. Deliverable: videos update known topics and trigger curiosity research.

**Phase 4 — Whisper fallback.** `Transcriber` trait + providers; caption-less
YouTube videos become ingestible.

**Phase 5 — Podcasts + local files.** Podcast RSS `kind` in MediaSource; local
`file://` ingestion under `MEDIA_DIR`. Both ride the Phase-4 Whisper path.

---

## 9. Testing

- **Unit (CI-safe, no network/binaries):** caption parsing (json3/vtt fixtures →
  plain text), RSS feed parsing (sample XML fixture), dedup logic, map-reduce
  chunk windowing, URL validation / injection guards. Store fixtures under
  `crates/app/tests/fixtures/media/`.
- **Integration (gated):** put live `yt-dlp`/RSS tests behind an env gate
  (e.g. `MEDIA_IT=1`) and a stable known video, so `cargo test` on CI without
  `yt-dlp` stays green. Mirror how Neo4j integration tests are separated.
- Verify graceful degradation: missing `yt-dlp` → clean error + logged gap, not a
  panic.

---

## 10. Risks & mitigations

- **`yt-dlp` / YouTube fragility & ToS / rate limits** — prefer the free RSS feed
  for discovery (not scraping), poll politely (6h default), cap concurrency via
  the queue's Ollama semaphore. Keep `yt-dlp` a swappable binary.
- **Transcript length → LLM cost** — map on the local model, reduce on cloud;
  `MEDIA_MAX_DURATION_SECS` guard; skip re-ingest via `:Media` dedup.
- **Auto-caption noise** — acceptable for summarization; note
  `transcript_source` so downstream reasoning can weight accordingly.
- **Subprocess security** — arg arrays only, URL-scheme validation, `file://`
  allowlisted to `MEDIA_DIR`.
- **Summary contamination** — the compare `reason` step MUST pass prior output
  via `context: "{{_prev}}"`, never inline in the question (RAG-contamination
  footgun).

---

## 11. Docs to update (per branch strategy: docs first)

- `CLAUDE.md` — new skill in the structure tree + skill count, env var table,
  a "Media Learning" section describing the watch→ingest→learn loop, the new
  nodes/relationships, and `sources-media/` ownership semantics.
- `project-docs/STATUS.md` — tool count + feature status.
- `MEMORY.md` + a topic file — one-line index entry pointing to a
  `media-learning.md` memory capturing the non-obvious wiring (RSS discovery →
  Task fan-out → chain; captions-before-whisper; map-reduce inside the tool).

---

## Appendix A — Full example `chains/video-learning.yaml`

Routed to any Task whose goal starts with `watch video:` (created on-demand or by
`poll_media_sources`). This is the **learning** version, with the new-vs-known
comparison. A minimal "just summarize" variant drops steps 2–6 and stores the
raw summary directly (like `learn.yaml`).

**Why the working-memory bank/reassemble steps:** a chain's `{{_prev}}` carries
**only the immediately preceding step's output**. The final note must contain
BOTH the video summary AND the new-vs-known analysis, and the `reason` step needs
the summary as its `context`. So — exactly as `daily-news.yaml` does — each
intermediate output is banked into a per-task `WorkingMemory` session with
`push_context`, then a `neo4j_query` step reassembles the pieces before the step
that needs more than one of them. The final step deletes the scratch session.

```yaml
# chains/video-learning.yaml
#
# Watch/ingest a video and learn from it: fetch transcript, summarize,
# compare against existing knowledge (NEW vs KNOWN vs CHANGED), and store a
# sourced note linked to a :Media dedup node.
#
# {{_prev}} only carries the PREVIOUS step's output, so intermediate outputs are
# banked in the WorkingMemory session "video-{{task_id}}" and reassembled with a
# neo4j_query step before any step that needs more than one of them. The final
# step deletes the scratch session (same idiom as chains/daily-news.yaml).
name: video-learning
pattern: "watch video:"
patterns:
  - "summarize video"
  - "ingest video"
  - "watch this video"
priority: 40
no_adversarial: true       # ingestion isn't high-stakes planning — skip pre-flight
description: "Watch/ingest a video: fetch transcript, map-reduce summarize, compare against existing knowledge, and store a sourced semantic note linked to its :Media node."
steps:
  # 1. Fetch metadata + transcript (captions → whisper fallback) and map-reduce
  #    summarize INSIDE the tool (chains are fixed-length; transcripts are not).
  #    `url` accepts the raw goal ("watch video: <url>"); the tool extracts the
  #    first http(s) URL. Upserts a :Media node (dedup) and returns structured
  #    JSON: { video_id, title, channel, url, summary, key_concepts, ... }.
  - tool_name: ingest_media
    arguments:
      url: "{{goal}}"
      source_context: video_learning
      store: false
    priority: 1
    max_attempts: 3
    provider_hint: ollama
    description: "Fetch transcript + map-reduce summary"

  # 2. Bank the summary so it survives past the reason step below.
  - tool_name: push_context
    arguments:
      session_id: "video-{{task_id}}"
      content: "## VIDEO SUMMARY\n{{_prev}}"
      role: observation
    priority: 1
    max_attempts: 2
    provider_hint: ollama
    description: "Bank the summary in working memory"

  # 3. Reassemble the banked summary so it can be passed as `context` (not the
  #    push confirmation that {{_prev}} now holds).
  - tool_name: neo4j_query
    arguments:
      cypher: "MATCH (w:WorkingMemory {session_id: 'video-{{task_id}}'}) RETURN w.content AS content ORDER BY w.turn_index"
    priority: 1
    max_attempts: 3
    provider_hint: ollama
    description: "Reassemble summary for the reason step"

  # 4. Compare the new material against what the brain already knows. context =
  #    the summary (NEVER restate it inside `question` — scheduled reason RAGs the
  #    graph off the question and would eat its own output → garbage loop). The
  #    reason tool's internal RAG surfaces topically-related existing notes.
  - tool_name: reason
    arguments:
      question: |
        The CONTEXT contains a summary of a video the brain just watched.
        Compare it against what is already known and write three sections:
        ## NEW — concepts/claims genuinely new to the brain
        ## CHANGED — where this updates or contradicts prior understanding
        ## FOLLOW UP — important concepts that are thinly covered and worth
        researching further (one concise line each; these seed gap tasks)
        Ground every point in the CONTEXT. If nothing is new, say so plainly.
      context: "{{_prev}}"
      store_inference: false
    priority: 2
    max_attempts: 3
    # Synthesis benefits from a stronger model; router picks the best catalog
    # match within CLOUD_TIER. Any hint other than "ollama" uses the active
    # config instead of the weak local model.
    provider_hint: ollama-cloud
    required_capabilities: ["reasoning"]
    description: "New-vs-known comparison"

  # 5. Bank the analysis alongside the summary.
  - tool_name: push_context
    arguments:
      session_id: "video-{{task_id}}"
      content: "## NEW vs KNOWN ANALYSIS\n{{_prev}}"
      role: observation
    priority: 1
    max_attempts: 2
    provider_hint: ollama
    description: "Bank the analysis in working memory"

  # 6. Reassemble summary + analysis into one blob for the stored note.
  - tool_name: neo4j_query
    arguments:
      cypher: "MATCH (w:WorkingMemory {session_id: 'video-{{task_id}}'}) RETURN w.content AS content ORDER BY w.turn_index"
    priority: 1
    max_attempts: 3
    provider_hint: ollama
    description: "Reassemble summary + analysis for storage"

  # 7. Persist the durable semantic note. store_note's async pipeline runs entity
  #    extraction + similarity linking, so key concepts become :Entity nodes and
  #    connect to existing knowledge automatically. A follow-up neo4j_query (or
  #    ingest_media store:true) creates (:Note)-[:SUMMARIZES]->(:Media).
  - tool_name: store_note
    arguments:
      content: "# Video learning: {{goal}}\n\n{{_prev}}"
      note_type: semantic
      source_context: video_learning
    priority: 1
    max_attempts: 3
    provider_hint: ollama
    description: "Store the sourced summary note"

  # 8. Mark the video Task done.
  - tool_name: update_task
    arguments:
      task_id: "{{task_id}}"
      status: completed
    priority: 1
    max_attempts: 3
    provider_hint: ollama
    description: "Complete the task"

  # 9. Clean up the scratch working-memory session.
  - tool_name: neo4j_query
    arguments:
      cypher: "MATCH (w:WorkingMemory {session_id: 'video-{{task_id}}'}) DETACH DELETE w"
      readonly: false
    priority: 0
    max_attempts: 2
    provider_hint: ollama
    description: "Delete scratch session"
```

**Gap-task fan-out (deliberately not a chain step):** the `## FOLLOW UP`
concepts should become `"fill knowledge gap: <concept>"` Tasks so the existing
curiosity engine (`chains/fill-knowledge-gap.yaml`) researches them. A chain
can't loop over a dynamic list, so do this the same way new videos are handled:
have `ingest_media` (or a small dedicated tool) parse the follow-up concepts and
call `create_task` per concept. Keep it out of the linear YAML.

---

## Appendix B — `init_schema` Cypher (add to `crates/repository/src/client.rs`)

Add these to the existing arrays in `Neo4jClient::init_schema` (they run through
the same `constraints.iter().chain(indexes.iter())` loop, and `IF NOT EXISTS`
makes them idempotent):

```rust
// --- in the `constraints` array ---
"CREATE CONSTRAINT media_source_name IF NOT EXISTS FOR (m:MediaSource) REQUIRE m.name IS UNIQUE",
// :Media.id is the platform video/episode id — the dedup key checked before every
// ingest and used to filter already-seen items out of list_channel_videos.
"CREATE CONSTRAINT media_id IF NOT EXISTS FOR (m:Media) REQUIRE m.id IS UNIQUE",

// --- in the `indexes` array ---
// poll_media_sources scans active watchlist entries every tick.
"CREATE INDEX media_source_active IF NOT EXISTS FOR (m:MediaSource) ON (m.active)",
// Per-channel listing / "what have we ingested from this channel".
"CREATE INDEX media_channel_idx IF NOT EXISTS FOR (m:Media) ON (m.channel_id)",
// Recency queries for the watch loop and reporting.
"CREATE INDEX media_ingested_idx IF NOT EXISTS FOR (m:Media) ON (m.ingested_at)",
```

No index is needed for the `SUMMARIZES` / `FROM_SOURCE` relationships — they are
traversed from a `:Media` node already pinned by its unique `id`. The summary
note itself rides the existing `Note` constraints/indexes and the vector +
full-text indexes, so video summaries are searchable via the normal RAG path with
no extra schema.
