# media

Media learning: ingest_media / ingest_article (yt-dlp captions or Whisper, summarised in-tool), the :MediaSource watchlist (manage_media_source, poll_media_sources, discover_media_sources), spawn_gap_tasks, and the video-learning, article-learning, media-watch and discover-media-sources workflows.

Built before the template and linked to it on 2026-11-06; `.cruft.json`
records the template commit.
`scripts/plugin_template.sh update media` brings a later
template change in as a diff to review.

## Layout

| Path | Holds |
|---|---|
| `plugin.yaml` | name, version, description |
| `read-rules.yaml` | the URL to read instead of the one handed over (an arXiv abstract → the paper) |
| `crate/src/lib.rs` | the `Plugin` impl (`Media`); the `sources-media` seed kind; `start` applies the graph schema and seeds `:MediaSource` from every bundle's files, on create only |
| `crate/src/skill.rs` | `ingest_media`, `ingest_article`, `poll_media_sources`, `manage_media_source` (with the owner's retype and its history), `discover_media_sources`, `spawn_gap_tasks` |
| `crate/src/service.rs` | transcripts and summaries: the `yt-dlp` boundary (captions first), map-reduce summarisation |
| `crate/src/transcribe.rs` | Whisper for caption-less media, through any OpenAI-compatible `/audio/transcriptions` endpoint (`WHISPER_*`) |
| `crate/src/store.rs` | `:Media` (the dedup key and provenance ledger) and `:MediaSource` (the watchlist, each with its tier from the `source-types` list) |
| `crate/src/read_rules.rs` | reads `read-rules.yaml` |
| `sources-media/watchlist.yaml`, `ai-research.yaml`, `osint.yaml` | the shipped channels and feeds, seeded once, then graph-owned |
| `workflows/video-learning.yaml`, `article-learning.yaml` | one video or article: read, summarise, compare with what the brain knows, store a sourced note with its claims |
| `workflows/media-watch.yaml` | every 6 h: new uploads on the active watchlist, one `video-learning` run each (`MEDIA_WATCH_ENABLED`) |
| `workflows/discover-media-sources.yaml` | weekly: channels on the brain's topics, staged inactive for review (`MEDIA_DISCOVERY_ENABLED`) |
| `contexts/intake.yaml` | the agent for a link handed over in chat |
| `views/videos.yaml`, `video_notes.yaml`, `youtube_calls.yaml` | the `lookup` views: what was watched, what it wrote, and the yt-dlp calls by kind |
| `yt-proxy/` | the YouTube egress proxy (tinyproxy, compose `yt-proxy`): yt-dlp's only way out, past the host's VPN exit; `host/` holds the host routing rule it needs |
| `schemas/tools/` | the six tools' input schemas |
| `schemas/graph/schema.cypher` | `Media` and `MediaSource` constraints and indexes |
| `schemas/seeds/` | the generated shapes of `sources-media` and `read-rules.yaml` |
| `schemas/lists/source-types.json` | the closed list of source tiers, written from `SOURCE_TYPES` |

Graph access is in `store.rs`. See `project-docs/SOURCE_QUALITY_PLAN.md` for the tiers.

## Enable and check

On by default (`PLUGINS_ENABLED` in `docker-compose.yml`). yt-dlp is in the brain's image and reaches YouTube only through `YT_DLP_PROXY`. Then
`cargo test -p agent-brain-plugin-media --lib`, `scripts/check_schemas.sh`
and `scripts/check_plugin_deps.sh`, and rebuild the image.
