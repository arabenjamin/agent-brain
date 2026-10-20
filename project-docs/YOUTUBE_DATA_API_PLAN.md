# YouTube Data API plan

Make fewer YouTube calls instead of routing more of them. yt-dlp keeps only
the calls that fetch content (captions, audio); discovery and metadata move to
the YouTube Data API v3's free quota. Planned 2026-10-17, after the bot-check
outage that `yt-proxy` (dev note `media-yt-dlp/...yt-proxy...`) worked around.

## 1. Why

On 2026-10-17 YouTube's bot check refused yt-dlp from the host's Mullvad exit
on the first call of each hour. `yt-proxy` sends yt-dlp out of the LAN
instead, which works, but:

- the host rule it needs lives outside compose and is lost on reboot, and
  nothing says so when it is gone;
- YouTube now sees the owner's home address, so heavy fetching risks flagging
  that address too (and the owner's own browsing with it);
- every call we make is one more chance of a check, wherever it comes from.

The durable fix is fewer calls. A user-agent change would not help: yt-dlp
already presents itself as YouTube's own clients, and the check judges the
address and a proof-of-origin token. The PO-token provider plugin's own docs
say it no longer gets past the check in most cases (bgutil-ytdlp-pot-provider
on PyPI).

## 2. The count

### Measured (interim, 2026-10-17 22:46 → 2026-10-20 11:06, 2.5 days)

`lookup youtube_calls` over `external_calls` (Phase 0). The week ends
2026-10-24; this is the first 2.5 days, all through `yt-proxy`.

| Op | Calls | ok | errors | bot checks | repeats | ~30 days |
|---|---|---|---|---|---|---|
| probe (poll, new feed item) | 50 | 50 | 0 | 0 | 0 | ~600 |
| metadata (`ingest_media`) | 51 | 51 | 0 | 0 | 1 | ~610 |
| captions | 51 | 27 | **24** | 0 | 1 | ~610 |
| audio (Whisper fallback) | 24 | 23 | 1 | 0 | 1 | ~290 |
| channel | 1 | 1 | 0 | 0 | 0 | a few |
| **Total** | **177** (≈70/day) | | | **0** | | **~2,100, 58% metadata** |

- **No bot checks and no backoffs** since `yt-proxy`.
- **No live re-probes**: probe repeats are 0 (the estimate's worry).
- **Every Whisper run was a caption 429, none a video without captions.**
  All 24 audio calls follow a failed caption call for the same video
  (23 videos, one retried): 46% of watched videos went to Whisper. A video
  with no captions makes yt-dlp exit 0 and is never counted as an error;
  each error still in the logs is `Unable to download video subtitles for
  'en': HTTP Error 429`.
- **The 429 is per track, not pacing.** Failed and successful caption calls
  had the same load before them (5.6 vs 5.8 yt-dlp calls in the prior
  10 minutes). Re-run by hand on 2026-10-20 hours later, two failed videos
  429'd again on `en`; the same videos with `--sub-langs en-orig` (the
  original speech-recognition track) downloaded at once (202 KB and 59 KB
  json3). The brain's pattern `en,en-.*,en.*` asks for `en` (YouTube's
  translated rendition) first, and yt-dlp stops at its 429 before writing
  `en-orig`. See Phase 0b.

### Estimated (2026-10-17, before Phase 0)

The brain did not record yt-dlp calls; this is graph counts (last 30 days) ×
the code paths in `plugins/media/crate/src/service.rs` and `skill.rs`. Retries
are not included, so the real numbers are higher.

| Call | Trigger | Kind | ~30 days |
|---|---|---|---|
| Metadata probe (`-J`) | `poll_media_sources`, every new feed item (duration + live check before a task) | metadata | ~493 (482 runs + 11 too long) **+ live re-probes** (a live broadcast is re-probed every poll and not recorded) |
| Metadata (`-J`) | `ingest_media`, every run | metadata | ~482 |
| Captions (`--write-subs`) | `ingest_media`, after metadata | content | ~460 |
| Audio download | no captions → Whisper | content | 190 |
| Channel resolution | `discover-media-sources` (4 runs) | metadata | a few |
| **Total** | | | **~1,630 (≈54/day), ~60% metadata** |

From the graph: 482 video-learning runs (454 completed, 28 failed); 465
`:Media` (264 captions, 190 Whisper, 11 skipped too long); 105 media-watch
polls; 15 active YouTube channels. Channel feeds are fetched as RSS over plain
HTTP and are not bot-checked.

## 3. What the Data API can and cannot do

- Free quota 10,000 units a day per project; it cannot be bought, only
  requested ([quota costs](https://developers.google.com/youtube/v3/determine_quota_cost)).
- `videos.list`: 1 unit for up to 50 ids → duration (`contentDetails`),
  title/channel/published (`snippet`), live state (`liveBroadcastContent`,
  `liveStreamingDetails`). This replaces every metadata call above.
- `channels.list` with `forHandle`: 1 unit → channel id (channel resolution).
- `playlistItems.list` on a channel's uploads playlist: 1 unit. Not needed
  while RSS works; a fallback if RSS is ever blocked.
- **Not transcripts.** `captions.download` needs edit permission on the video
  (owner only), so captions and audio stay with yt-dlp.
- Auth for public data is an API key, no OAuth.

Expected use: ~15 sources × 4 polls a day → at most ~60 `videos.list` units,
plus a few channel lookups: about 1% of the quota.

## 4. Phases

### Phase 0: count the calls (no key)

Record every yt-dlp call where the gate already sees it (`yt_dlp()` in
`service.rs`): `what` (metadata, captions, audio, channel resolution), the
caller (poll or ingest), ok, bot check, duration. A DuckDB table in the
telemetry store, so the count drills down to its rows. Run a week with no other
change and replace §2's estimate with the measured table, including the live
re-probes.

**Built 2026-10-17.** `external_calls` (DuckDB, `project-docs/schema.md`),
written through the plugin API's `Telemetry::log_external_call` from the one
gate in `plugins/media/crate/src/service.rs`; ops `probe`, `metadata`,
`captions`, `audio`, `channel`; outcomes `ok`, `error`, `bot_check`, `backoff`.
`lookup youtube_calls` (days, default 7) counts them per op with repeated
subjects. The baseline week ends 2026-10-24; Phase 1 waits for it.

First rows (2026-10-17, one poll, three videos): 3 probes, 3 metadata,
3 captions, 1 audio, all through `yt-proxy`, no bot checks. One caption
download got **HTTP 429** and the run fell back to audio + Whisper: a
rate-limited caption costs a heavier call. The week's count should say how
many Whisper runs were 429s (a `captions` error, then `audio` for the same
subject) rather than videos with no captions; if many, the caption call's
pacing matters more than the metadata calls.

**Interim result 2026-10-20** (§2 Measured): the 429s are 100% of the Whisper
runs and come from asking for the wrong caption track, not from pacing.

### Phase 0b: ask for the original caption track first (no key; proposed 2026-10-20)

`download_captions` asks for `{lang}-orig` alone first, and only if that
writes no file, the current pattern (a video with only uploaded captions has
no `-orig` track). Expected: the ~46% of videos that now download audio and
run Whisper get YouTube's own transcript instead: ~290 fewer audio calls a
month, no GPU Whisper time for them, and a better transcript than
faster-whisper-base. A manual-captions-only video costs one extra light call.
The outcome stays countable: `captions` errors should fall to near zero.

### Phase 1: one yt-dlp call per watched video (no key)

`ingest_media` makes two calls today (metadata, then captions). One call does
both: `-j --no-simulate --skip-download --write-subs --write-auto-subs`
prints the metadata JSON and writes the caption files. Checked by hand
2026-10-17 on one video through `yt-proxy`. The language pattern must stay as
narrow as the brain's (`en.*` pulled every translated track and hit a 429).
Saves ~1 call per watched video (~480 a month). The live-stream guard still
runs before anything is summarised (it reads the same JSON).

### Phase 2: metadata from the Data API (key)

- A small client in the media plugin: `videos.list` (batched by 50) and
  `channels.list`. The poll's probe, the live check and channel resolution use
  it when a key is set; without a key, or with the day's quota spent, they fall
  back to yt-dlp as today.
- The key as an `:ApiContext` declared in the media bundle (auth = the env var
  name, `YOUTUBE_API_KEY` in `.env`), not a Rust literal. The request carries
  only public video and channel ids, so `data_class: public`.
- A daily unit ledger with a cap (proposed 5,000, half the quota) so a bug
  cannot spend the quota; past the cap, fall back.
- Live broadcasts are re-checked through the API (1 unit per 50), not yt-dlp.

### Phase 3: the remaining path stays visible

- A bot check while `YT_DLP_PROXY` is set means the bypass is probably gone:
  the warning and a one-time `notify_user` say to check
  `ip rule | grep 5200` (the host rule) instead of only backing off.
- The host rule survives a reboot only if the owner installs it, e.g. as a
  systemd oneshot that runs
  `ip rule add from 172.30.250.0/24 lookup main priority 5200` at boot
  (owner's choice).

## 5. Expected result

| | Calls / 30 days | YouTube sees |
|---|---|---|
| Today | ~1,630 (more with retries and live re-probes) | home address via `yt-proxy` |
| After Phase 1 | ~1,150 | same |
| After Phase 2 | ~670, all content (1 per watched video + audio for no-caption videos) | same, ~60% fewer calls |

Phase 0 replaces these with measured numbers before Phase 1 is judged.

## 6. Not doing

User-agent spoofing; the PO-token plugin; paid residential proxies; the Data
API for captions (owner-only). Browser cookies stay a last resort, and if ever
used, from a throwaway account, never the owner's.

## 7. Decisions for the owner

1. A Google Cloud project with **YouTube Data API v3** enabled, and an API key
   restricted to that API (not the Gemini key; a separate key keeps one
   leak from reaching the other), in `.env` as `YOUTUBE_API_KEY`. Needed for
   Phase 2 only.
2. Keep RSS as the listing source (free, unblocked), with `playlistItems` as
   the fallback only? Recommended: yes.
3. The daily unit cap (proposed 5,000).
4. Install the host rule as a systemd unit, or leave it manual.
