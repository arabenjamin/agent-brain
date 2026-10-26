//! Media service — transcript acquisition + LLM map-reduce summarization.
//!
//! Owns the `yt-dlp` subprocess boundary and the summarization loop. Pure
//! logic, no MCP types. Phase 1 covers YouTube **captions** (human subs
//! preferred over auto-captions) discovered from `yt-dlp -J` metadata and
//! fetched directly as `json3`. Phase 4 adds a self-hosted **Whisper** fallback
//! for caption-less YouTube videos. Phase 5 adds two more input kinds that ride
//! the same Whisper path: **podcast RSS enclosures** (direct audio URLs) and
//! **local files** (`file://` allow-listed to `MEDIA_DIR`).
//!
//! `fetch_transcript` classifies its input (`MediaInput`) and dispatches:
//! yt-dlp URLs try captions first (Whisper fallback); direct audio URLs and
//! local files always transcribe. Subprocess safety: `yt-dlp` is always invoked
//! with an **argument array** (never a shell string), URLs are scheme-validated,
//! and `file://` inputs are canonicalized and confined to `MEDIA_DIR`.
//!
//! **Articles** (2026-10-06) ride the same summarizer without yt-dlp: an
//! `article_rss` source's items are web pages, fetched by `fetch_article` and
//! reduced to their readable text by `extract_article_text`.

use std::sync::Arc;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use tracing::{debug, warn};

use agent_brain_plugin_api::Llm as LlmProvider;
use agent_brain_plugin_api::protocol::net_policy;

/// A recent item discovered from a channel/playlist/podcast feed.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FeedItem {
    pub video_id: String,
    pub title: String,
    pub url: String,
    pub published_at: String,
    pub channel_id: String,
}

/// What is being summarized, so the summarizer's prompts name it correctly.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Medium {
    Video,
    Article,
}

impl Medium {
    /// How the map pass describes its input ("the transcript of a video").
    fn source_phrase(self) -> &'static str {
        match self {
            Medium::Video => "the transcript of a video",
            Medium::Article => "the text of an article",
        }
    }
    fn noun(self) -> &'static str {
        match self {
            Medium::Video => "video",
            Medium::Article => "article",
        }
    }
}

/// Largest article page read, in bytes. A news "page" can be a 36 MB
/// single-page app (news.theater-strategy.org, 2026-10-06); the article text is
/// never more than a sliver of that.
const ARTICLE_MAX_BYTES: usize = 4 * 1024 * 1024;
/// Article text kept for summarizing: six map windows on the local model.
const ARTICLE_MAX_CHARS: usize = 36_000;
/// Below this, extraction found no article — a paywall, a script-rendered
/// page, or an index page. Permanent for that URL, unlike a network error.
const ARTICLE_MIN_CHARS: usize = 800;
/// Prefix of the error `fetch_article` returns for a page with no article.
pub const ARTICLE_UNEXTRACTABLE: &str = "no extractable article text";

/// Video metadata extracted from `yt-dlp -J`.
#[derive(Debug, Clone)]
pub struct VideoMeta {
    pub id: String,
    pub title: String,
    pub channel: String,
    pub channel_id: String,
    pub url: String,
    pub published_at: String,
    pub duration_secs: i64,
    /// Whether yt-dlp reports this as a CURRENTLY-BROADCASTING live stream
    /// (`is_live`). Load-bearing: a live stream has no end, and the
    /// `MEDIA_MAX_DURATION_SECS` cost guard structurally cannot catch one
    /// because yt-dlp reports `duration: null` for it, which parses to 0 — and
    /// `0 > 10800` is false. See [`MediaService::ingest_media`].
    pub is_live: bool,
    /// The uploader's description, first 3,000 chars (yt-dlp only; empty
    /// elsewhere). The summariser spells names from it: auto-captions run
    /// names and numbers together ("Qwen 3.635B" for Qwen 3.6 35B), and the
    /// description of the same video spelled them right (2026-10-26).
    pub description: String,
}

/// How a transcript target must be acquired, chosen by [`MediaService::classify_input`].
enum MediaInput {
    /// http(s) URL handled by yt-dlp (YouTube and the many sites it supports):
    /// metadata + captions, Whisper only as a fallback.
    YtDlp(String),
    /// http(s) URL pointing directly at an audio/video file (a podcast
    /// enclosure, a hosted `.mp3`/`.mp4`, …). No captions; always transcribed.
    DirectMedia(String),
    /// A local file confined to `MEDIA_DIR`. No captions; always transcribed.
    LocalFile(std::path::PathBuf),
}

pub struct MediaService {
    yt_dlp_path: String,
    caption_lang: String,
    max_duration_secs: i64,
    /// Transcript window size (chars) for the map pass.
    window_chars: usize,
    /// Whisper backend for caption-less media; `None` disables the fallback.
    transcriber: Option<Arc<dyn crate::transcribe::Transcriber>>,
    /// Scratch dir for yt-dlp subtitle/audio downloads (cleaned per call).
    media_dir: std::path::PathBuf,
    http: reqwest::Client,
    /// The host's page readers, `fetch_article`'s fallback for a page our own
    /// fetch cannot read (rendering services in rotation: TinyFish, Anakin).
    reader: Option<Arc<dyn agent_brain_plugin_api::Reader>>,
    /// Where every yt-dlp call is recorded (`external_calls`), with the Task
    /// whose run made it.
    telemetry: Option<Arc<dyn agent_brain_plugin_api::Telemetry>>,
    current_task: Option<Arc<dyn Fn() -> Option<String> + Send + Sync>>,
}

impl MediaService {
    pub fn from_env() -> Self {
        Self {
            yt_dlp_path: std::env::var("YT_DLP_PATH").unwrap_or_else(|_| "yt-dlp".into()),
            caption_lang: std::env::var("MEDIA_CAPTION_LANG").unwrap_or_else(|_| "en".into()),
            max_duration_secs: std::env::var("MEDIA_MAX_DURATION_SECS")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(10800),
            window_chars: 6000,
            transcriber: crate::transcribe::from_env(),
            media_dir: std::env::var("MEDIA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::env::temp_dir()),
            // A media or article URL comes from a model: public hosts only.
            http: net_policy::guarded_client(),
            reader: None,
            telemetry: None,
            current_task: None,
        }
    }

    /// Record every yt-dlp call through the host's telemetry
    /// (YOUTUBE_DATA_API_PLAN Phase 0).
    pub fn with_telemetry(mut self, ctx: &agent_brain_plugin_api::PluginContext) -> Self {
        self.telemetry = ctx.telemetry.clone();
        let c = ctx.clone();
        self.current_task = Some(Arc::new(move || c.current_task()));
        self
    }

    /// One `external_calls` row for a yt-dlp call (or a refusal).
    fn record_call(&self, op: &str, outcome: &str, started: std::time::Instant, subject: &str) {
        if let Some(t) = &self.telemetry {
            t.log_external_call(&agent_brain_plugin_api::ExternalCall {
                service: "yt-dlp".into(),
                op: op.into(),
                outcome: outcome.into(),
                duration_ms: started.elapsed().as_millis() as u64,
                units: None,
                subject: Some(subject.to_string()).filter(|s| !s.is_empty()),
                task_id: self.current_task.as_ref().and_then(|f| f()),
            });
        }
    }

    /// Give `fetch_article` the host's page readers as its fallback.
    pub fn with_reader(mut self, reader: Option<Arc<dyn agent_brain_plugin_api::Reader>>) -> Self {
        self.reader = reader;
        self
    }

    // ========================================================================
    // URL helpers
    // ========================================================================

    /// Extract the first `http(s)` or `file://` URL from arbitrary text (e.g. a
    /// goal like `"watch video: https://youtu.be/abc"` or a local
    /// `"file:///home/agent/media/talk.mp4"`). Returns the whole trimmed string
    /// if it is already a bare URL. (Local paths must not contain spaces or be
    /// percent-encoded — the match stops at the first whitespace.)
    pub fn extract_first_url(text: &str) -> Option<String> {
        let re = Regex::new(r"(?:https?|file)://[^\s)>\]]+").ok()?;
        re.find(text)
            .map(|m| m.as_str().trim_end_matches(['.', ',', ')']).to_string())
    }

    /// Reject anything that isn't an `http`/`https` URL. Used for feed URLs and
    /// the yt-dlp subprocess boundary, where `file://` and shell metacharacters
    /// must never reach the network/subprocess as a "URL". Local-file ingestion
    /// goes through [`classify_input`](Self::classify_input) instead.
    fn validate_url(url: &str) -> anyhow::Result<()> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            anyhow::bail!("unsupported or missing URL (only http/https are allowed): {url}");
        }
        Ok(())
    }

    /// Classify a raw ingest target into the acquisition path it needs.
    ///
    /// - `file://…` → [`MediaInput::LocalFile`] after canonicalizing and
    ///   confirming the path is inside `MEDIA_DIR` (symlink-escape safe).
    /// - an `http(s)` URL whose path ends in a known audio/video extension (a
    ///   podcast enclosure, a hosted `.mp3`/`.mp4`, …) → [`MediaInput::DirectMedia`].
    /// - any other `http(s)` URL → [`MediaInput::YtDlp`] (YouTube et al.).
    fn classify_input(&self, raw: &str) -> anyhow::Result<MediaInput> {
        if let Some(rest) = raw.strip_prefix("file://") {
            return Ok(MediaInput::LocalFile(self.resolve_local_file(rest)?));
        }
        Self::validate_url(raw)?;
        if is_direct_media_url(raw) {
            Ok(MediaInput::DirectMedia(raw.to_string()))
        } else {
            Ok(MediaInput::YtDlp(raw.to_string()))
        }
    }

    /// Resolve the path component of a `file://` URL against the `MEDIA_DIR`
    /// allowlist. `canonicalize` resolves `..` and symlinks, so a link pointing
    /// outside the root is rejected rather than followed.
    fn resolve_local_file(&self, rest: &str) -> anyhow::Result<std::path::PathBuf> {
        // `file:///abs/path` → rest = "/abs/path"; `file://localhost/abs` too.
        let rest = rest.strip_prefix("localhost").unwrap_or(rest);
        let requested = std::path::Path::new(rest);
        let root = self.media_dir.canonicalize().map_err(|e| {
            anyhow::anyhow!(
                "MEDIA_DIR ('{}') is not accessible, cannot resolve local files: {e}",
                self.media_dir.display()
            )
        })?;
        let canon = requested
            .canonicalize()
            .map_err(|e| anyhow::anyhow!("local file not found or unreadable ('{}'): {e}", rest))?;
        if !canon.starts_with(&root) {
            anyhow::bail!(
                "local file '{}' is outside the allowed MEDIA_DIR ('{}')",
                canon.display(),
                root.display()
            );
        }
        Ok(canon)
    }

    /// Borrow the configured transcriber or produce a clear "enable Whisper"
    /// error — the shared gate for every caption-less path.
    fn require_transcriber(
        &self,
        what: &str,
    ) -> anyhow::Result<&Arc<dyn crate::transcribe::Transcriber>> {
        self.transcriber.as_ref().ok_or_else(|| {
            anyhow::anyhow!(
                "{what} requires Whisper transcription, which is disabled \
                 (set WHISPER_PROVIDER + WHISPER_BASE_URL to enable audio transcription)"
            )
        })
    }

    // ========================================================================
    // Metadata + captions (yt-dlp)
    // ========================================================================

    /// Run yt-dlp through the gate ([`YtDlpGate`]): wait for its slot, refuse
    /// while backing off, and start a back-off when the answer is a bot check.
    /// `what` is the op recorded in `external_calls` (probe, metadata,
    /// captions, audio, channel); `subject` is the URL or search asked for.
    async fn yt_dlp(
        &self,
        args: &[&str],
        what: &str,
        subject: &str,
    ) -> anyhow::Result<std::process::Output> {
        let wait = {
            let mut gate = YT_DLP_GATE.lock().await;
            let now = tokio::time::Instant::now();
            if let Some((at, utc)) = gate.backoff_until
                && at > now
            {
                drop(gate);
                self.record_call(what, "backoff", std::time::Instant::now(), subject);
                anyhow::bail!(backoff_message(utc));
            }
            let start = gate.next_start.max(now);
            gate.next_start = start
                + std::time::Duration::from_secs(env_secs("MEDIA_YTDLP_MIN_INTERVAL_SECS", 20));
            start - now
        };
        if !wait.is_zero() {
            debug!(what, wait_ms = wait.as_millis() as u64, "yt-dlp paced");
            tokio::time::sleep(wait).await;
        }
        let started = std::time::Instant::now();
        let mut cmd = tokio::process::Command::new(&self.yt_dlp_path);
        // The yt-proxy service: YouTube past the host's VPN exit, whose
        // addresses its bot check blocks (docker-compose.yml, yt-proxy).
        if let Some(proxy) = std::env::var("YT_DLP_PROXY")
            .ok()
            .filter(|p| !p.trim().is_empty())
        {
            cmd.arg("--proxy").arg(proxy.trim());
        }
        let out = match cmd.args(args).output().await {
            Ok(o) => o,
            Err(e) => {
                self.record_call(what, "error", started, subject);
                anyhow::bail!(
                    "failed to run yt-dlp ('{}') for {what}: {e}. Install yt-dlp or set YT_DLP_PATH.",
                    self.yt_dlp_path
                );
            }
        };
        let bot_check =
            !out.status.success() && is_bot_check(&String::from_utf8_lossy(&out.stderr));
        let outcome = match (out.status.success(), bot_check) {
            (true, _) => "ok",
            (false, true) => "bot_check",
            (false, false) => "error",
        };
        self.record_call(what, outcome, started, subject);
        if bot_check {
            let mins = env_secs("MEDIA_YTDLP_BOT_BACKOFF_MINS", 60);
            let until_utc = chrono::Utc::now() + chrono::Duration::minutes(mins as i64);
            YT_DLP_GATE.lock().await.backoff_until = Some((
                tokio::time::Instant::now() + std::time::Duration::from_secs(mins * 60),
                until_utc,
            ));
            warn!(what, until = %until_utc, "YouTube bot check — yt-dlp backs off");
        }
        Ok(out)
    }

    /// `what`: `probe` (a feed poll checking a candidate) or `metadata`
    /// (a video being watched).
    async fn run_yt_dlp_json(&self, url: &str, what: &str) -> anyhow::Result<Value> {
        let output = self
            .yt_dlp(&["-J", "--no-warnings", "--no-playlist", url], what, url)
            .await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("yt-dlp failed: {}", stderr.trim());
        }
        serde_json::from_slice(&output.stdout)
            .map_err(|e| anyhow::anyhow!("yt-dlp returned unparseable JSON: {e}"))
    }

    /// Fetch only a single video's metadata (`yt-dlp -J` → [`VideoMeta`]) — no
    /// captions, no audio download. Used by `poll_media_sources` to check a
    /// candidate's duration and filter out over-length videos before they become
    /// Tasks (RSS feeds carry no duration, so a probe is the only way to know).
    pub async fn fetch_meta(&self, url: &str) -> anyhow::Result<VideoMeta> {
        net_check(url).await?;
        let v = self.run_yt_dlp_json(url, "probe").await?;
        self.parse_meta(&v)
    }

    /// Resolve a YouTube channel to its `(channel_id, channel_name)` from either
    /// a **URL/handle** (`https://youtube.com/@handle`, `/channel/UC…`) or a
    /// bare **channel name** (`"Machine Learning Street Talk"`). Used by the
    /// autonomous source-discovery flow: web search usually names channels in
    /// snippets without giving their canonical URL, so a name resolves via
    /// `ytsearch1:` (one search hit → its channel). The RSS watch feed needs the
    /// `UC…` id. Reads one entry — cheap, no video download.
    pub async fn resolve_youtube_channel_id(
        &self,
        input: &str,
    ) -> anyhow::Result<(String, String)> {
        let input = input.trim();
        if input.is_empty() {
            anyhow::bail!("empty channel reference");
        }
        let is_url = input.starts_with("http://") || input.starts_with("https://");
        // URL: a channel/tab page — the flat playlist carries the channel_id at
        // top level. Name: search YouTube; the (non-flat) top hit carries the
        // channel_id on its entry.
        let target = if is_url {
            input.to_string()
        } else {
            format!("ytsearch1:{input}")
        };
        let mut args = vec!["-J", "--no-warnings"];
        if is_url {
            args.push("--flat-playlist");
        }
        args.push("--playlist-items");
        args.push("1");
        args.push(&target);
        let output = self.yt_dlp(&args, "channel", &target).await?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            anyhow::bail!("yt-dlp channel resolution failed: {}", stderr.trim());
        }
        let v: Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| anyhow::anyhow!("yt-dlp returned unparseable JSON: {e}"))?;
        // The channel id lives at the top level for a channel/tab URL; fall back
        // to the first entry, and to `id` when it looks like a `UC…` id.
        let entry0 = v
            .get("entries")
            .and_then(|e| e.as_array())
            .and_then(|a| a.first());
        let channel_id = [
            str_field(&v, "channel_id"),
            entry0
                .map(|e| str_field(e, "channel_id"))
                .unwrap_or_default(),
            str_field(&v, "id"),
        ]
        .into_iter()
        .find(|s| s.starts_with("UC") && s.len() >= 10)
        .unwrap_or_default();
        if channel_id.is_empty() {
            anyhow::bail!("could not resolve a channel_id (UC…) from '{input}'");
        }
        let name = first_nonempty(&[
            str_field(&v, "channel"),
            str_field(&v, "uploader"),
            str_field(&v, "title"),
        ]);
        Ok((channel_id, name))
    }

    /// Parse the `yt-dlp -J` blob into [`VideoMeta`].
    fn parse_meta(&self, v: &Value) -> anyhow::Result<VideoMeta> {
        let id = v
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        if id.is_empty() {
            anyhow::bail!("yt-dlp metadata missing video id");
        }
        let title = str_field(v, "title");
        let channel = first_nonempty(&[str_field(v, "channel"), str_field(v, "uploader")]);
        let channel_id = first_nonempty(&[str_field(v, "channel_id"), str_field(v, "uploader_id")]);
        let url = first_nonempty(&[str_field(v, "webpage_url"), str_field(v, "original_url")]);
        let duration_secs = v.get("duration").and_then(|d| d.as_f64()).unwrap_or(0.0) as i64;
        let published_at = format_upload_date(&str_field(v, "upload_date"));
        // `is_live` is true only while the stream is actually broadcasting;
        // `was_live` (a finished stream, now a normal VOD with a real duration)
        // is deliberately NOT treated as live — those are safe to ingest.
        // A scheduled premiere (`live_status: is_upcoming`) has no transcript
        // yet either; treat it like a live stream so it is skipped, not ingested.
        let is_live = v.get("is_live").and_then(|x| x.as_bool()).unwrap_or(false)
            || v.get("live_status").and_then(|x| x.as_str()) == Some("is_upcoming");

        Ok(VideoMeta {
            id,
            title,
            channel,
            channel_id,
            url,
            published_at,
            duration_secs,
            is_live,
            description: str_field(v, "description").chars().take(3000).collect(),
        })
    }

    /// Download a caption track via `yt-dlp` itself (into a per-call scratch
    /// dir) and parse it to plain text. Letting yt-dlp fetch the subtitle file
    /// is far more robust than fetching the timedtext URL directly — auto-
    /// captions in particular require yt-dlp's session handling. `tries` come
    /// from [`caption_tries`], best first; each runs only if the one before
    /// wrote no file. Returns `None` when none did.
    async fn download_captions(
        &self,
        url: &str,
        id: &str,
        tries: &[CaptionTry],
    ) -> anyhow::Result<Option<String>> {
        for t in tries {
            let dir = self
                .media_dir
                .join(format!("brain-media-{id}-{}", uuid::Uuid::new_v4()));
            tokio::fs::create_dir_all(&dir).await.map_err(|e| {
                anyhow::anyhow!("could not create media scratch dir {}: {e}", dir.display())
            })?;
            let dir_s = dir.to_string_lossy().to_string();
            let mut args = vec![
                "--skip-download",
                "--no-warnings",
                "--no-playlist",
                "--write-subs",
            ];
            if t.auto {
                args.push("--write-auto-subs");
            }
            args.extend([
                "--sub-langs",
                &t.langs,
                "--sub-format",
                "json3",
                "-o",
                "%(id)s",
                "-P",
                &dir_s,
                url,
            ]);
            let out = self.yt_dlp(&args, "captions", url).await;

            let text = self.read_first_json3(&dir).await;
            // Best-effort cleanup regardless of outcome.
            let _ = tokio::fs::remove_dir_all(&dir).await;

            match out {
                Ok(o) if !o.status.success() => {
                    let stderr = String::from_utf8_lossy(&o.stderr);
                    warn!(video = %id, langs = %t.langs, stderr = %stderr.trim(), "yt-dlp subtitle download reported failure");
                }
                Err(e) => return Err(e),
                _ => {}
            }
            if let Some(text) = text.filter(|t| !t.trim().is_empty()) {
                return Ok(Some(text));
            }
        }
        Ok(None)
    }

    /// Read and parse the first `*.json3` subtitle file in `dir`, if any.
    async fn read_first_json3(&self, dir: &std::path::Path) -> Option<String> {
        let mut rd = tokio::fs::read_dir(dir).await.ok()?;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()) == Some("json3")
                && let Ok(body) = tokio::fs::read_to_string(&path).await
            {
                return Some(parse_json3(&body));
            }
        }
        None
    }

    /// Fetch metadata + transcript. Returns `(meta, transcript, source)`.
    /// Dispatches on [`classify_input`](Self::classify_input): yt-dlp URLs prefer
    /// captions (Whisper fallback); direct audio URLs (podcast enclosures) and
    /// local files always transcribe via Whisper.
    pub async fn fetch_transcript(&self, url: &str) -> anyhow::Result<(VideoMeta, String, String)> {
        let input = self.classify_input(url)?;
        if !matches!(input, MediaInput::LocalFile(_)) {
            net_check(url).await?;
        }
        match input {
            MediaInput::YtDlp(u) => self.fetch_via_ytdlp(&u).await,
            MediaInput::DirectMedia(u) => self.fetch_via_direct_media(&u).await,
            MediaInput::LocalFile(p) => self.fetch_via_local_file(&p).await,
        }
    }

    /// Captions-first path for yt-dlp-extractable URLs (YouTube et al.).
    async fn fetch_via_ytdlp(&self, url: &str) -> anyhow::Result<(VideoMeta, String, String)> {
        let raw = self.run_yt_dlp_json(url, "metadata").await?;
        let meta = self.parse_meta(&raw)?;

        // ── Live-stream guard ────────────────────────────────────────────────
        // Reject BEFORE the caption path, which is what a live stream slips
        // through: the MEDIA_MAX_DURATION_SECS cost guard below only runs on the
        // no-captions branch, and even when reached it cannot fire, because
        // yt-dlp reports `duration: null` for a live stream (parsed to 0, and
        // `0 > max` is false). A live broadcast also has no end, so the
        // map-reduce summarisation in `ingest_media` has no finite transcript to
        // reduce and the job simply hangs.
        //
        // Observed 2026-09-11: a `watch video:` task on a broadcasting stream
        // held a local job slot for 3 hours across 29 attempts, burning zero LLM
        // calls and never erroring. Failing fast and loudly is the correct
        // outcome — a live stream is not transient, so retrying cannot help.
        if meta.is_live {
            anyhow::bail!(
                "'{}' ({}) is a LIVE broadcast — refusing to ingest. A live stream has no end, so there is no finite transcript to summarise, and it would hold a job slot until the broadcast stops. Re-run this once the stream has ended and YouTube has published it as a normal video with a duration.",
                meta.title,
                meta.id
            );
        }

        debug!(video = %meta.id, "downloading caption track via yt-dlp");
        let tries = caption_tries(&raw, &self.caption_lang);
        if let Some(text) = self.download_captions(url, &meta.id, &tries).await? {
            return Ok((meta, text, "captions".to_string()));
        }

        // No usable captions — Whisper fallback.
        let transcriber =
            self.require_transcriber(&format!("'{}' has no captions and", meta.id))?;
        // Guard here (not just in the skill): transcription is far costlier than
        // caption fetch, so reject over-long audio before downloading it.
        if self.max_duration_secs > 0 && meta.duration_secs > self.max_duration_secs {
            anyhow::bail!(
                "no captions for '{}' and it is {}s long, exceeding MEDIA_MAX_DURATION_SECS={} \
                 (too long to transcribe)",
                meta.id,
                meta.duration_secs,
                self.max_duration_secs
            );
        }
        debug!(video = %meta.id, backend = transcriber.label(), "no captions; transcribing audio");
        let (audio, dir) = self.download_audio(url, &meta.id).await?;
        let result = transcriber.transcribe(&audio).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let text = result?;
        if text.trim().is_empty() {
            anyhow::bail!("Whisper returned an empty transcript for '{}'", meta.id);
        }
        Ok((meta, normalize_ws(&text), "whisper".to_string()))
    }

    /// Podcast/direct-audio path: download the file over HTTP and transcribe it.
    /// There is no caption track and usually no duration up front, so the
    /// `MEDIA_MAX_DURATION_SECS` guard does not apply here.
    async fn fetch_via_direct_media(
        &self,
        url: &str,
    ) -> anyhow::Result<(VideoMeta, String, String)> {
        let transcriber = self.require_transcriber("transcribing this audio URL")?;
        let meta = meta_from_media_url(url);
        debug!(id = %meta.id, backend = transcriber.label(), "transcribing direct media URL");
        let (audio, dir) = self.download_http_media(url, &meta.id).await?;
        let result = transcriber.transcribe(&audio).await;
        let _ = tokio::fs::remove_dir_all(&dir).await;
        let text = result?;
        if text.trim().is_empty() {
            anyhow::bail!("Whisper returned an empty transcript for '{url}'");
        }
        Ok((meta, normalize_ws(&text), "whisper".to_string()))
    }

    /// Local-file path: transcribe a file already on disk under `MEDIA_DIR`.
    async fn fetch_via_local_file(
        &self,
        path: &std::path::Path,
    ) -> anyhow::Result<(VideoMeta, String, String)> {
        let transcriber = self.require_transcriber("transcribing this local file")?;
        let meta = meta_from_local_file(path);
        debug!(id = %meta.id, backend = transcriber.label(), "transcribing local file");
        let text = transcriber.transcribe(path).await?;
        if text.trim().is_empty() {
            anyhow::bail!(
                "Whisper returned an empty transcript for '{}'",
                path.display()
            );
        }
        Ok((meta, normalize_ws(&text), "whisper".to_string()))
    }

    /// Download an HTTP(S) media file (e.g. a podcast enclosure) into a per-call
    /// scratch dir. Returns `(file_path, scratch_dir)`; the caller removes the
    /// dir. The Whisper server decodes the container, so no ffmpeg is needed.
    async fn download_http_media(
        &self,
        url: &str,
        id: &str,
    ) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
        let dir = self
            .media_dir
            .join(format!("brain-media-dl-{id}-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.map_err(|e| {
            anyhow::anyhow!("could not create media scratch dir {}: {e}", dir.display())
        })?;
        let ext = url_media_ext(url).unwrap_or("bin");
        let path = dir.join(format!("{id}.{ext}"));
        let fetch = async {
            let bytes = self
                .http
                .get(url)
                .send()
                .await?
                .error_for_status()?
                .bytes()
                .await?;
            tokio::fs::write(&path, &bytes).await?;
            anyhow::Ok(())
        }
        .await;
        if let Err(e) = fetch {
            let _ = tokio::fs::remove_dir_all(&dir).await;
            return Err(anyhow::anyhow!(
                "failed to download audio from '{url}': {e}"
            ));
        }
        Ok((path, dir))
    }

    /// Download the best audio-only stream via yt-dlp (no `-x`/ffmpeg — the
    /// Whisper server decodes the container itself). Returns `(audio_path,
    /// scratch_dir)`; the caller removes the dir.
    async fn download_audio(
        &self,
        url: &str,
        id: &str,
    ) -> anyhow::Result<(std::path::PathBuf, std::path::PathBuf)> {
        let dir = self
            .media_dir
            .join(format!("brain-audio-{id}-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.map_err(|e| {
            anyhow::anyhow!("could not create audio scratch dir {}: {e}", dir.display())
        })?;
        let dir_s = dir.to_string_lossy().to_string();
        let out = self
            .yt_dlp(
                &[
                    "-f",
                    "bestaudio/best",
                    "--no-warnings",
                    "--no-playlist",
                    "-o",
                    "%(id)s.%(ext)s",
                    "-P",
                    &dir_s,
                    url,
                ],
                "audio",
                url,
            )
            .await?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let _ = tokio::fs::remove_dir_all(&dir).await;
            anyhow::bail!("yt-dlp audio download failed: {}", stderr.trim());
        }
        // Find the downloaded audio file (extension varies: m4a/webm/opus).
        let mut rd = tokio::fs::read_dir(&dir).await?;
        while let Ok(Some(entry)) = rd.next_entry().await {
            let path = entry.path();
            if path.is_file() {
                return Ok((path, dir));
            }
        }
        let _ = tokio::fs::remove_dir_all(&dir).await;
        anyhow::bail!("yt-dlp produced no audio file for '{id}'")
    }

    // ========================================================================
    // Summarization (map-reduce)
    // ========================================================================

    /// Summarize a transcript into structured prose + key concepts. Short
    /// transcripts are summarized in a single pass; long ones are chunked
    /// ("map") and synthesized ("reduce").
    pub async fn summarize(
        &self,
        meta: &VideoMeta,
        transcript: &str,
        llm: &Arc<dyn LlmProvider>,
        medium: Medium,
    ) -> anyhow::Result<(String, Vec<String>)> {
        let windows = chunk_transcript(transcript, self.window_chars);
        debug!(video = %meta.id, windows = windows.len(), "summarizing transcript");

        let notes = if windows.len() <= 1 {
            transcript.to_string()
        } else {
            let mut partials = Vec::with_capacity(windows.len());
            for (i, w) in windows.iter().enumerate() {
                let prompt = format!(
                    "This is segment {}/{} of {} titled \"{}\" (a headline: take \
                     facts from the segment, never the title). \
                     Summarize its key points, claims, and any named concepts as concise \
                     bullet points. Do not add preamble.\n\nSEGMENT:\n{}",
                    i + 1,
                    windows.len(),
                    medium.source_phrase(),
                    meta.title,
                    w
                );
                let s = llm
                    .generate(
                        &prompt,
                        Some("You are a precise note-taker. Output only bullet points."),
                    )
                    .await?;
                partials.push(s);
            }
            partials.join("\n\n")
        };

        // The title is the uploader's headline, not a fact source: local E4B
        // read "CBS Reporter's Abdul El Sayed Post Just Backfired" as "CBS
        // reporter Abdul El-Sayed" (a Senate candidate) in 4/4 runs, and the
        // error rode into a claim and the daily brief. With the title fenced
        // off and roles and pronouns tied to the transcript: 0/4 (2026-10-20).
        // Captions mishear names; the uploader's description spells them.
        let reference = if meta.description.is_empty() {
            String::new()
        } else {
            format!(
                "\n\nTHE UPLOADER'S DESCRIPTION (for spelling only: auto-captions run \
                 names and numbers together, e.g. \"Qwen 3.635B\" for Qwen 3.6 35B; when \
                 a name in the notes matches one here, write it as written here. Its \
                 figures are the uploader's own; ignore sponsor text):\n{}",
                meta.description
            )
        };
        let reduce_prompt = format!(
            "You are summarizing {} {} by {}. Its title is \"{}\"; a title is a \
             headline, often clipped or ambiguous, so take every fact from the \
             notes below, never from the title. Below are notes taken from {}. \
             Write a cohesive 200-400 word prose summary and extract the key \
             concepts. Describe each person only by a role the notes give them, \
             and keep each \"his\", \"her\" and \"their\" attached to the person \
             the notes mean; when it is unclear, use the name.{}\n\nNOTES:\n{}\n\n\
             Return ONLY a JSON object of the form:\n\
             {{\"summary\": \"<prose summary>\", \"key_concepts\": [\"concept\", ...]}}",
            if medium == Medium::Article { "an" } else { "a" },
            medium.noun(),
            meta.channel,
            meta.title,
            if medium == Medium::Article {
                "it"
            } else {
                "its transcript"
            },
            reference,
            notes
        );
        let v = llm
            .generate_json(
                &reduce_prompt,
                Some("Output strict JSON only — no prose, no markdown fences."),
                &["summary", "key_concepts"],
                2,
            )
            .await?;

        let summary = v
            .get("summary")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string();
        let key_concepts = v
            .get("key_concepts")
            .and_then(|x| x.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        Ok((summary, key_concepts))
    }

    /// Cost guard: max video length to summarize (0 = unlimited).
    pub fn max_duration_secs(&self) -> i64 {
        self.max_duration_secs
    }

    // ========================================================================
    // Feed listing (RSS/Atom) for the watch loop
    // ========================================================================

    /// Build the feed URL to poll for a source. YouTube channels/playlists get
    /// their free RSS feed (no API key); a `podcast_rss` source's `reference`
    /// *is* the feed URL, returned as-is. Returns `None` for kinds with no feed.
    pub fn feed_url(kind: &str, reference: &str) -> Option<String> {
        match kind {
            "youtube_channel" => Some(format!(
                "https://www.youtube.com/feeds/videos.xml?channel_id={reference}"
            )),
            "youtube_playlist" => Some(format!(
                "https://www.youtube.com/feeds/videos.xml?playlist_id={reference}"
            )),
            "podcast_rss" | "article_rss" => Some(reference.to_string()),
            _ => None,
        }
    }

    /// Fetch and parse the recent items from a channel/playlist/podcast feed.
    /// YouTube feeds are Atom; podcast feeds are RSS 2.0 with `<enclosure>`
    /// audio URLs — each parser yields the same [`FeedItem`] shape.
    pub async fn list_feed_videos(
        &self,
        kind: &str,
        reference: &str,
    ) -> anyhow::Result<Vec<FeedItem>> {
        let Some(url) = Self::feed_url(kind, reference) else {
            anyhow::bail!("feed listing for kind '{kind}' is not supported");
        };
        // A podcast `reference` is a user/graph-supplied URL — scheme-guard it
        // before fetching. YouTube feed URLs are built above and always https.
        Self::validate_url(&url)?;
        // YouTube throttles the feed endpoint by returning transient 404/429/5xx
        // to an IP it is rate-limiting; the *same* channel_id returns 200 minutes
        // later. Because a poll fans every source out in one run, a whole run can
        // be throttled together — observed 2026-09-08, all ~12 sources 404/500 at
        // one poll, every one returning 200 on a later single fetch. The channel
        // IDs were correct; a single `error_for_status()?` still dropped each
        // source for the whole 12h cycle. Retry a few times with backoff; a
        // genuinely dead feed just costs the (bounded) attempts.
        let mut attempt = 0u32;
        let body = loop {
            attempt += 1;
            match self
                .http
                .get(&url)
                .send()
                .await
                .and_then(|r| r.error_for_status())
            {
                Ok(resp) => break resp.text().await?,
                Err(e) => {
                    let retryable = e.is_timeout()
                        || e.is_connect()
                        || e.status().is_none_or(|s| {
                            let c = s.as_u16();
                            c == 404 || c == 408 || c == 425 || c == 429 || c >= 500
                        });
                    if attempt >= 3 || !retryable {
                        return Err(e.into());
                    }
                    let backoff = std::time::Duration::from_millis(500 * 2u64.pow(attempt - 1));
                    tracing::warn!(
                        url = %url, attempt, error = %e,
                        backoff_ms = backoff.as_millis() as u64,
                        "feed fetch failed (transient); retrying"
                    );
                    tokio::time::sleep(backoff).await;
                }
            }
        };
        Ok(match kind {
            "podcast_rss" => parse_rss_feed(&body),
            "article_rss" => parse_article_feed(&body),
            _ => parse_youtube_feed(&body),
        })
    }

    // ========================================================================
    // Articles
    // ========================================================================

    /// Stable `:Media` id of an article: `art-` + FNV-1a of its URL. The same
    /// function serves poll-time dedup and ingest, so the two line up.
    pub fn article_id(url: &str) -> String {
        format!("art-{:016x}", fnv1a(url))
    }

    /// Fetch a web article and reduce it to readable text.
    ///
    /// Reads at most [`ARTICLE_MAX_BYTES`]. A page whose extracted text is
    /// under [`ARTICLE_MIN_CHARS`] fails with [`ARTICLE_UNEXTRACTABLE`]: a
    /// paywall or a script-rendered page, permanent for that URL.
    pub async fn fetch_article(&self, url: &str) -> anyhow::Result<(VideoMeta, String)> {
        Self::validate_url(url)?;
        // The page to read (read-rules.yaml: an arXiv abstract → the paper);
        // the article keeps `url` for its meta and id.
        let read = crate::read_rules::rules()
            .reading_url(url)
            .unwrap_or_else(|| url.to_string());
        if read != url {
            Self::validate_url(&read)?;
            tracing::info!(url, read = %read, "article read at its reading rule's URL");
        }
        net_check(&read).await?;
        // 1. Our own fetch: free, and enough for server-rendered CMS pages.
        let (html, own_status) = match self.fetch_html(&read).await {
            Ok(h) => (Some(h), None),
            Err(FetchError::Status(code)) => (None, Some(code)),
            Err(FetchError::Other(e)) => return Err(e),
        };
        let own_text = html
            .as_deref()
            .map(extract_article_text)
            .unwrap_or_default();
        if own_text.len() >= ARTICLE_MIN_CHARS {
            let html = html.unwrap_or_default();
            return Ok((article_meta(url, &html), clip_article(own_text)));
        }
        if let Some(code) = own_status
            && !is_block_status(code)
        {
            anyhow::bail!("article fetch failed: HTTP {code}");
        }
        // 2. Blocked, or no article in what we got: the host's page readers
        //    (rendering services in rotation) may still read it
        //    (paywall-lite, script-rendered, bot-blocked).
        let fallback = match &self.reader {
            Some(reader) => match reader.read(&read).await {
                Ok(page) => {
                    let text = markdown_article_text(&page.text);
                    if text.len() >= ARTICLE_MIN_CHARS {
                        let meta = match &html {
                            Some(h) => article_meta(url, h),
                            None => meta_from_markdown(url, &page.text),
                        };
                        tracing::info!(url, chars = text.len(), reader = %page.reader, "article read via the page readers");
                        return Ok((meta, clip_article(text)));
                    }
                    format!(
                        "{} read it but found {} chars of article text",
                        page.reader,
                        text.len()
                    )
                }
                Err(e) => format!("page readers failed: {e}"),
            },
            None => "no page readers for a rendering fallback".to_string(),
        };
        let first = match own_status {
            Some(code) => format!("HTTP {code} to a direct fetch"),
            None => format!("{} chars extracted directly", own_text.len()),
        };
        anyhow::bail!(
            "{ARTICLE_UNEXTRACTABLE} ({first}; {fallback}) — paywalled, script-rendered, or not an article"
        )
    }

    /// GET a page as text, at most [`ARTICLE_MAX_BYTES`]. A non-2xx status is
    /// returned as `FetchError::Status` so the caller can tell "blocked" from
    /// "gone".
    async fn fetch_html(&self, url: &str) -> Result<String, FetchError> {
        let mut resp = self
            .http
            .get(url)
            .header(
                reqwest::header::USER_AGENT,
                "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/537.36 (KHTML, like Gecko) \
                 Chrome/126 Safari/537.36",
            )
            .timeout(std::time::Duration::from_secs(30))
            .send()
            .await
            .map_err(|e| FetchError::Other(e.into()))?;
        if !resp.status().is_success() {
            return Err(FetchError::Status(resp.status().as_u16()));
        }
        let mut bytes: Vec<u8> = Vec::new();
        while let Some(chunk) = resp
            .chunk()
            .await
            .map_err(|e| FetchError::Other(e.into()))?
        {
            bytes.extend_from_slice(&chunk);
            if bytes.len() >= ARTICLE_MAX_BYTES {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&bytes).into_owned())
    }
}

enum FetchError {
    Status(u16),
    Other(anyhow::Error),
}

/// Cut article text to [`ARTICLE_MAX_CHARS`] on a char boundary.
fn clip_article(text: String) -> String {
    if text.len() <= ARTICLE_MAX_CHARS {
        return text;
    }
    let mut cut = ARTICLE_MAX_CHARS;
    while !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text[..cut].to_string()
}

/// Metadata when only a page reader's markdown was readable: the first `# ` heading
/// as title, the host as site.
fn meta_from_markdown(url: &str, md: &str) -> VideoMeta {
    let title = md
        .lines()
        .find_map(|l| l.strip_prefix("# "))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| url.to_string());
    VideoMeta {
        id: MediaService::article_id(url),
        title,
        channel: host_of(url).trim_start_matches("www.").to_string(),
        channel_id: String::new(),
        url: url.to_string(),
        published_at: String::new(),
        duration_secs: 0,
        is_live: false,
        description: String::new(),
    }
}

/// Whether an error from [`MediaService::fetch_article`] is permanent for that
/// URL (no article on the page) rather than a network failure.
pub fn is_unextractable(err: &str) -> bool {
    err.contains(ARTICLE_UNEXTRACTABLE)
}

/// Whether two URLs are on the same site, ignoring a leading `www.`.
pub fn same_site(a: &str, b: &str) -> bool {
    let h = |u: &str| host_of(u).trim_start_matches("www.").to_lowercase();
    let (ha, hb) = (h(a), h(b));
    !ha.is_empty() && ha == hb
}

/// `content` of the first `<meta>` whose `property` or `name` is `key`.
fn meta_content(html: &str, key: &str) -> Option<String> {
    let tag_re = Regex::new(r"(?is)<meta\b[^>]*>").unwrap();
    let key_re = Regex::new(&format!(
        r#"(?i)\b(?:property|name)\s*=\s*["']{}["']"#,
        regex::escape(key)
    ))
    .unwrap();
    let content_re = Regex::new(r#"(?is)\bcontent\s*=\s*(?:"([^"]*)"|'([^']*)')"#).unwrap();
    tag_re
        .find_iter(html)
        .map(|m| m.as_str())
        .filter(|t| key_re.is_match(t))
        .find_map(|t| {
            content_re.captures(t).and_then(|c| {
                c.get(1)
                    .or_else(|| c.get(2))
                    .map(|v| decode_entities(v.as_str().trim()))
            })
        })
        .filter(|v| !v.is_empty())
}

/// Title, site and publish time of an article page, from its meta tags.
fn article_meta(url: &str, html: &str) -> VideoMeta {
    let title = meta_content(html, "og:title")
        .or_else(|| {
            Regex::new(r"(?is)<title[^>]*>(.*?)</title>")
                .unwrap()
                .captures(html)
                .map(|c| decode_entities(c[1].trim()))
        })
        .unwrap_or_else(|| url.to_string());
    let site = meta_content(html, "og:site_name")
        .unwrap_or_else(|| host_of(url).trim_start_matches("www.").to_string());
    let published_at = meta_content(html, "article:published_time")
        .or_else(|| {
            Regex::new(r#"(?is)<time\b[^>]*\bdatetime\s*=\s*["']([^"']+)["']"#)
                .unwrap()
                .captures(html)
                .map(|c| c[1].to_string())
        })
        .unwrap_or_default();
    VideoMeta {
        id: MediaService::article_id(url),
        title,
        channel: site,
        channel_id: String::new(),
        url: url.to_string(),
        published_at,
        duration_secs: 0,
        is_live: false,
        description: String::new(),
    }
}

/// Decode the HTML entities article text actually carries.
fn decode_entities(s: &str) -> String {
    let named = s
        .replace("&nbsp;", " ")
        .replace("&rsquo;", "\u{2019}")
        .replace("&lsquo;", "\u{2018}")
        .replace("&rdquo;", "\u{201D}")
        .replace("&ldquo;", "\u{201C}")
        .replace("&mdash;", "\u{2014}")
        .replace("&ndash;", "\u{2013}")
        .replace("&hellip;", "\u{2026}");
    let numeric = Regex::new(r"&#(x[0-9a-fA-F]+|[0-9]+);").unwrap();
    let decoded = numeric.replace_all(&named, |c: &regex::Captures| {
        let n = &c[1];
        let code = if let Some(hex) = n.strip_prefix('x').or_else(|| n.strip_prefix('X')) {
            u32::from_str_radix(hex, 16).ok()
        } else {
            n.parse().ok()
        };
        code.and_then(char::from_u32)
            .map(|ch| ch.to_string())
            .unwrap_or_default()
    });
    // `&amp;` last, so "&amp;lt;" stays the literal text "&lt;".
    unescape_xml(&decoded)
}

/// Readable text of an article page.
///
/// Drops non-content blocks (scripts, navigation, headers, footers, sidebars,
/// forms, figures), takes the largest `<article>` — else `<main>`, else
/// `<body>` — and keeps lines of at least six words that are not wholly the
/// text of one link, so menus, bylines, share buttons and related-story cards
/// fall away and paragraphs stay. Regex, not a DOM parser: the
/// sources are server-rendered CMS pages, and a page this cannot read fails
/// [`ARTICLE_MIN_CHARS`] visibly rather than yielding a wrong summary.
pub fn extract_article_text(html: &str) -> String {
    let mut h = Regex::new(r"(?s)<!--.*?-->")
        .unwrap()
        .replace_all(html, " ")
        .into_owned();
    for tag in [
        "script", "style", "noscript", "svg", "nav", "header", "footer", "aside", "form", "iframe",
        "figure", "button", "select",
    ] {
        let re = Regex::new(&format!(r"(?is)<{tag}\b[^>]*>.*?</{tag}\s*>")).unwrap();
        h = re.replace_all(&h, " ").into_owned();
    }
    let largest = |tag: &str| -> Option<String> {
        Regex::new(&format!(r"(?is)<{tag}\b[^>]*>(.*?)</{tag}\s*>"))
            .unwrap()
            .captures_iter(&h)
            .map(|c| c[1].to_string())
            .max_by_key(|s| s.len())
    };
    let region = largest("article")
        .filter(|s| s.len() > 2_000)
        .or_else(|| largest("main"))
        .or_else(|| largest("body"))
        .unwrap_or_else(|| h.clone());
    // Text that lies wholly inside one link is navigation: "related
    // articles" cards (title + excerpt wrapped in a single <a>), "read the
    // summary in Chinese". It is another story's headline, which claim
    // extraction would otherwise take as an assertion of this one. A real
    // paragraph contains links but is longer than any of them.
    let link_texts: Vec<String> = Regex::new(r"(?is)<a\b[^>]*>(.*?)</a>")
        .unwrap()
        .captures_iter(&region)
        .map(|c| {
            let inner = Regex::new(r"(?s)<[^>]+>").unwrap().replace_all(&c[1], " ");
            decode_entities(&inner)
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|t| t.split(' ').count() >= 4)
        .collect();
    let breaks = Regex::new(r"(?i)</(?:p|div|h[1-6]|li|blockquote|tr|section|dd|dt)\s*>|<br\s*/?>")
        .unwrap()
        .replace_all(&region, "\n");
    let stripped = Regex::new(r"(?s)<[^>]+>")
        .unwrap()
        .replace_all(&breaks, " ");
    let decoded = decode_entities(&stripped);
    keep_article_lines(&decoded, &link_texts)
}

/// The paragraph filter shared by both extractors: lines of at least six
/// words that are not wholly one link's text, cookie notices or unrendered
/// `{{templates}}`, deduplicated.
fn keep_article_lines(text: &str, link_texts: &[String]) -> String {
    let mut lines: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.split_whitespace().collect::<Vec<_>>().join(" ");
        let lower = line.to_lowercase();
        if lower.contains("cookie") || line.contains("{{") {
            continue;
        }
        if link_texts.iter().any(|t| t.contains(&line)) {
            continue;
        }
        if line.split(' ').count() >= 6 && !lines.contains(&line) {
            lines.push(line);
        }
    }
    lines.join("\n")
}

/// Readable text of an article from a page reader's markdown: images dropped, links
/// reduced to their text, block markers stripped, then the same paragraph
/// filter as HTML extraction (a line that was wholly a link is navigation).
pub fn markdown_article_text(md: &str) -> String {
    let image = Regex::new(r"!\[[^\]]*\]\([^)]*\)").unwrap();
    let link = Regex::new(r"\[([^\]]*)\]\([^)]*\)").unwrap();
    let no_images = image.replace_all(md, "");
    let link_texts: Vec<String> = link
        .captures_iter(&no_images)
        .map(|c| c[1].split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|t| t.split(' ').count() >= 4)
        .collect();
    let mut out = String::new();
    for line in no_images.lines() {
        let line = line.trim();
        // A line that is nothing but links is a menu or a related-stories row.
        if !line.is_empty()
            && link
                .replace_all(line, "")
                .trim()
                .trim_matches(['|', ',', '-', '*', ' '])
                .is_empty()
            && link.is_match(line)
        {
            continue;
        }
        let text = link.replace_all(line, "$1");
        let text = text
            .trim_start_matches(['#', '>', '*', '-', '+', ' '])
            .replace("**", "")
            .replace("__", "");
        out.push_str(&text);
        out.push('\n');
    }
    keep_article_lines(&decode_entities(&out), &link_texts)
}

/// A status that means "this site refuses automated readers", which a
/// rendering scraper can often get past — not "the page is gone".
fn is_block_status(status: u16) -> bool {
    matches!(status, 401 | 403 | 429 | 451 | 503)
}

/// Parse an article feed (RSS 2.0 `<item>` or Atom `<entry>`) into
/// [`FeedItem`]s in feed order. `url` is the article's page and `video_id` is
/// [`MediaService::article_id`] of it, matching ingest-time `:Media` ids.
fn parse_article_feed(xml: &str) -> Vec<FeedItem> {
    let cdata = |raw: &str| -> String {
        let raw = raw.trim();
        let inner = raw
            .strip_prefix("<![CDATA[")
            .and_then(|r| r.strip_suffix("]]>"))
            .unwrap_or(raw);
        decode_entities(inner.trim())
    };
    let field = |block: &str, tag: &str| -> Option<String> {
        Regex::new(&format!(r"(?is)<{tag}\b[^>]*>(.*?)</{tag}>"))
            .unwrap()
            .captures(block)
            .map(|c| cdata(&c[1]))
            .filter(|v| !v.is_empty())
    };
    let atom_link = Regex::new(r#"(?is)<link\b[^>]*\bhref\s*=\s*["']([^"']+)["'][^>]*>"#).unwrap();

    let (blocks, atom): (Vec<&str>, bool) = if xml.contains("<item") {
        (xml.split("<item").skip(1).collect(), false)
    } else {
        (xml.split("<entry").skip(1).collect(), true)
    };
    let mut items = Vec::new();
    for block in blocks {
        let end = if atom { "</entry>" } else { "</item>" };
        let block = block.split(end).next().unwrap_or(block);
        let link = if atom {
            atom_link
                .captures_iter(block)
                .find(|c| {
                    let tag = c.get(0).map(|m| m.as_str()).unwrap_or("");
                    !tag.contains("rel=\"") || tag.contains("rel=\"alternate\"")
                })
                .map(|c| decode_entities(c[1].trim()))
        } else {
            field(block, "link").or_else(|| field(block, "guid"))
        };
        let Some(link) = link.filter(|l| l.starts_with("http://") || l.starts_with("https://"))
        else {
            continue;
        };
        let title = field(block, "title").unwrap_or_default();
        let published_at = ["pubDate", "published", "updated", "dc:date"]
            .iter()
            .find_map(|t| field(block, t))
            .unwrap_or_default();
        items.push(FeedItem {
            video_id: MediaService::article_id(&link),
            url: link,
            title,
            published_at,
            channel_id: String::new(),
        });
    }
    items
}

// ============================================================================
// Free functions (unit-tested)
// ============================================================================

fn str_field(v: &Value, key: &str) -> String {
    v.get(key)
        .and_then(|x| x.as_str())
        .unwrap_or_default()
        .to_string()
}

fn first_nonempty(candidates: &[String]) -> String {
    candidates
        .iter()
        .find(|s| !s.is_empty())
        .cloned()
        .unwrap_or_default()
}

/// `"20240131"` → `"2024-01-31"`; passthrough otherwise.
fn format_upload_date(s: &str) -> String {
    if s.len() == 8 && s.chars().all(|c| c.is_ascii_digit()) {
        format!("{}-{}-{}", &s[0..4], &s[4..6], &s[6..8])
    } else {
        s.to_string()
    }
}

/// One caption download: the `--sub-langs` value, and whether auto-captions
/// may answer it.
#[derive(Debug, PartialEq)]
struct CaptionTry {
    langs: String,
    auto: bool,
}

/// The caption downloads to try for a video, best first, from its yt-dlp
/// metadata (`subtitles` are uploaded by a person, `automatic_captions` are
/// YouTube's speech recognition):
///
/// 1. the uploaded tracks in `lang`, uploaded only;
/// 2. `<lang>-orig`, the speech-recognition track itself.
///
/// Measured 2026-10-17..20 (YOUTUBE_DATA_API_PLAN §2): the old single pattern
/// `en,en-.*,en.*` asked for YouTube's `en` rendition first, which answered
/// HTTP 429 for 46% of watched videos, every time, while the same videos'
/// `en-orig` downloaded at once. yt-dlp stops at the 429 before writing
/// `en-orig`, so each of those videos fell to audio + Whisper. The old pattern
/// stays only for metadata that lists neither track.
fn caption_tries(meta: &Value, lang: &str) -> Vec<CaptionTry> {
    let in_lang = |key: &str| -> Vec<String> {
        let mut ks: Vec<String> = meta
            .get(key)
            .and_then(|s| s.as_object())
            .map(|o| {
                o.keys()
                    .filter(|k| *k == lang || k.starts_with(&format!("{lang}-")))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default();
        ks.sort();
        ks
    };
    // `--sub-langs` entries are regexes; track names are taken literally.
    let literal = |k: &str| -> String {
        k.chars()
            .flat_map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                    vec![c]
                } else {
                    vec!['\\', c]
                }
            })
            .collect()
    };
    let mut tries = Vec::new();
    let uploaded = in_lang("subtitles");
    if !uploaded.is_empty() {
        tries.push(CaptionTry {
            langs: uploaded
                .iter()
                .map(|k| literal(k))
                .collect::<Vec<_>>()
                .join(","),
            auto: false,
        });
    }
    let orig = format!("{lang}-orig");
    if in_lang("automatic_captions").contains(&orig) {
        tries.push(CaptionTry {
            langs: literal(&orig),
            auto: true,
        });
    }
    if tries.is_empty() {
        tries.push(CaptionTry {
            langs: format!("{lang},{lang}-.*,{lang}.*"),
            auto: true,
        });
    }
    tries
}

/// Parse a YouTube `json3` caption body into plain text.
fn parse_json3(body: &str) -> String {
    let v: Value = match serde_json::from_str(body) {
        Ok(v) => v,
        Err(_) => return String::new(),
    };
    let mut out = String::new();
    if let Some(events) = v.get("events").and_then(|e| e.as_array()) {
        for ev in events {
            if let Some(segs) = ev.get("segs").and_then(|s| s.as_array()) {
                for seg in segs {
                    if let Some(t) = seg.get("utf8").and_then(|u| u.as_str()) {
                        out.push_str(t);
                    }
                }
            }
        }
    }
    normalize_ws(&out)
}

/// Collapse runs of whitespace to single spaces and trim.
fn normalize_ws(s: &str) -> String {
    s.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Parse a YouTube Atom feed into [`FeedItem`]s (newest first, as delivered).
fn parse_youtube_feed(xml: &str) -> Vec<FeedItem> {
    let vid_re = Regex::new(r"<yt:videoId>([^<]+)</yt:videoId>").unwrap();
    let chan_re = Regex::new(r"<yt:channelId>([^<]+)</yt:channelId>").unwrap();
    let title_re = Regex::new(r"<title>([^<]*)</title>").unwrap();
    let pub_re = Regex::new(r"<published>([^<]+)</published>").unwrap();

    let mut items = Vec::new();
    for entry in xml.split("<entry>").skip(1) {
        let Some(video_id) = vid_re.captures(entry).map(|c| c[1].to_string()) else {
            continue;
        };
        let title = title_re
            .captures(entry)
            .map(|c| unescape_xml(&c[1]))
            .unwrap_or_default();
        let published_at = pub_re
            .captures(entry)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        let channel_id = chan_re
            .captures(entry)
            .map(|c| c[1].to_string())
            .unwrap_or_default();
        items.push(FeedItem {
            url: format!("https://www.youtube.com/watch?v={video_id}"),
            video_id,
            title,
            published_at,
            channel_id,
        });
    }
    items
}

fn unescape_xml(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
}

/// Audio/video file extensions that mark a URL as a direct media file (a
/// podcast enclosure, a hosted clip) rather than a page yt-dlp should scrape.
const MEDIA_EXTS: &[&str] = &[
    "mp3", "m4a", "aac", "ogg", "oga", "opus", "wav", "flac", "mp4", "m4v", "mov", "webm", "mkv",
];

/// The lowercased file extension of a URL's path (ignoring `?query`/`#frag`),
/// if it is a known media extension.
fn url_media_ext(url: &str) -> Option<&'static str> {
    let path = url.split(['?', '#']).next().unwrap_or(url);
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    MEDIA_EXTS.iter().copied().find(|e| *e == ext)
}

/// True when an http(s) URL points directly at a media file (see [`MEDIA_EXTS`]).
fn is_direct_media_url(url: &str) -> bool {
    url_media_ext(url).is_some()
}

/// Deterministic 64-bit FNV-1a hash — stable across builds/Rust versions, so a
/// derived `:Media` id dedups the same URL/path on every run (unlike
/// `DefaultHasher`, whose output is not guaranteed stable).
fn fnv1a(s: &str) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in s.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

/// The last path segment of a URL/path with its extension dropped, prettified
/// into a rough title. Empty → `"Untitled"`.
fn title_from_path(path: &str) -> String {
    let seg = path
        .split(['?', '#'])
        .next()
        .unwrap_or(path)
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or(path);
    let stem = seg.rsplit_once('.').map(|(s, _)| s).unwrap_or(seg);
    let pretty = stem.replace(['_', '-'], " ");
    let pretty = normalize_ws(&pretty);
    if pretty.is_empty() {
        "Untitled".to_string()
    } else {
        pretty
    }
}

/// The host of an http(s) URL (for `channel`/provenance), or `""`.
fn host_of(url: &str) -> String {
    url.split_once("://")
        .map(|(_, rest)| rest)
        .and_then(|rest| rest.split(['/', '?', '#']).next())
        .unwrap_or("")
        .to_string()
}

/// Synthesize [`VideoMeta`] for a direct audio URL (podcast enclosure). The id
/// is a stable hash of the URL so re-ingesting the same episode dedups.
fn meta_from_media_url(url: &str) -> VideoMeta {
    VideoMeta {
        id: format!("pod-{:016x}", fnv1a(url)),
        title: title_from_path(url),
        channel: host_of(url),
        channel_id: String::new(),
        url: url.to_string(),
        published_at: String::new(),
        duration_secs: 0,
        // A published enclosure is a finished file, never a live broadcast.
        is_live: false,
        description: String::new(),
    }
}

/// Synthesize [`VideoMeta`] for a local file. The id hashes the canonical path.
fn meta_from_local_file(path: &std::path::Path) -> VideoMeta {
    let path_str = path.to_string_lossy();
    VideoMeta {
        id: format!("file-{:016x}", fnv1a(&path_str)),
        title: title_from_path(&path_str),
        channel: "local".to_string(),
        channel_id: String::new(),
        url: format!("file://{path_str}"),
        published_at: String::new(),
        duration_secs: 0,
        // A file on disk is finite by construction.
        is_live: false,
        description: String::new(),
    }
}

/// Parse an RSS 2.0 podcast feed into [`FeedItem`]s (feed order, newest first).
/// Each item's `url` is its `<enclosure>` audio URL and its `video_id` is the
/// same stable hash [`meta_from_media_url`] derives, so poll-time dedup lines up
/// with ingest-time `:Media` ids. Items without an audio enclosure are skipped.
fn parse_rss_feed(xml: &str) -> Vec<FeedItem> {
    let enclosure_re =
        Regex::new(r#"<enclosure\b[^>]*\burl\s*=\s*["']([^"']+)["'][^>]*>"#).unwrap();
    let title_re = Regex::new(r"(?s)<title>(?:<!\[CDATA\[(.*?)\]\]>|(.*?))</title>").unwrap();
    let pub_re = Regex::new(r"<pubDate>([^<]+)</pubDate>").unwrap();

    let mut items = Vec::new();
    for item in xml.split("<item>").skip(1) {
        let item = item.split("</item>").next().unwrap_or(item);
        let Some(enclosure_url) = enclosure_re
            .captures(item)
            .map(|c| unescape_xml(c[1].trim()))
            .filter(|u| u.starts_with("http://") || u.starts_with("https://"))
        else {
            continue;
        };
        let title = title_re
            .captures(item)
            .map(|c| {
                let raw = c
                    .get(1)
                    .or_else(|| c.get(2))
                    .map(|m| m.as_str())
                    .unwrap_or("");
                unescape_xml(raw.trim())
            })
            .unwrap_or_default();
        let published_at = pub_re
            .captures(item)
            .map(|c| c[1].trim().to_string())
            .unwrap_or_default();
        items.push(FeedItem {
            video_id: format!("pod-{:016x}", fnv1a(&enclosure_url)),
            url: enclosure_url,
            title,
            published_at,
            channel_id: String::new(),
        });
    }
    items
}

/// Greedily pack a transcript into windows of ~`window_chars`, breaking on
/// sentence boundaries where possible. A single window is returned when the
/// transcript already fits.
fn chunk_transcript(transcript: &str, window_chars: usize) -> Vec<String> {
    let transcript = transcript.trim();
    if transcript.len() <= window_chars {
        return if transcript.is_empty() {
            vec![]
        } else {
            vec![transcript.to_string()]
        };
    }
    let mut windows = Vec::new();
    let mut current = String::new();
    for sentence in split_sentences(transcript) {
        if !current.is_empty() && current.len() + sentence.len() > window_chars {
            windows.push(std::mem::take(&mut current));
        }
        // A single sentence longer than the window is hard-split.
        if sentence.len() > window_chars {
            for piece in sentence.as_bytes().chunks(window_chars) {
                windows.push(String::from_utf8_lossy(piece).to_string());
            }
            continue;
        }
        if !current.is_empty() {
            current.push(' ');
        }
        current.push_str(&sentence);
    }
    if !current.is_empty() {
        windows.push(current);
    }
    windows
}

fn split_sentences(text: &str) -> Vec<String> {
    // Split after ". " while keeping the delimiter attached to the sentence.
    let mut out = Vec::new();
    let mut start = 0;
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b'.' && i + 1 < bytes.len() && bytes[i + 1] == b' ' {
            out.push(text[start..=i].trim().to_string());
            start = i + 1;
        }
    }
    if start < text.len() {
        out.push(text[start..].trim().to_string());
    }
    out.retain(|s| !s.is_empty());
    out
}

/// Whether a yt-dlp failure means the video simply has not aired yet — a
/// scheduled premiere or an upcoming live event. Not transient on the scale of
/// a job's retries, not permanent either: skip it now, pick it up later.
/// YouTube's "Sign in to confirm you're not a bot": an IP whose requests look
/// automated. Measured 2026-10-16: it hit bursts (3-4 yt-dlp calls in one
/// minute) from a Mullvad exit, while calls spaced minutes apart from the same
/// kind of address went through.
pub fn is_bot_check(err: &str) -> bool {
    let e = err.to_ascii_lowercase().replace('\u{2019}', "'");
    e.contains("confirm you're not a bot") || e.contains("sign in to confirm")
}

/// Every yt-dlp call passes one process-wide gate: starts are spaced
/// `MEDIA_YTDLP_MIN_INTERVAL_SECS` apart (default 20), and a bot check stops
/// them for `MEDIA_YTDLP_BOT_BACKOFF_MINS` (default 60), so a burst cannot
/// re-trigger it. While backing off a call fails at once and says until when;
/// the feed poll skips video items, which come back on a later poll.
struct YtDlpGate {
    next_start: tokio::time::Instant,
    backoff_until: Option<(tokio::time::Instant, chrono::DateTime<chrono::Utc>)>,
}

static YT_DLP_GATE: std::sync::LazyLock<tokio::sync::Mutex<YtDlpGate>> =
    std::sync::LazyLock::new(|| {
        tokio::sync::Mutex::new(YtDlpGate {
            next_start: tokio::time::Instant::now(),
            backoff_until: None,
        })
    });

fn env_secs(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// When YouTube's bot back-off ends, if one is on.
pub async fn youtube_backoff_until() -> Option<chrono::DateTime<chrono::Utc>> {
    let gate = YT_DLP_GATE.lock().await;
    gate.backoff_until
        .filter(|(at, _)| *at > tokio::time::Instant::now())
        .map(|(_, utc)| utc)
}

fn backoff_message(until: chrono::DateTime<chrono::Utc>) -> String {
    format!(
        "YouTube bot check: backing off until {} UTC; yt-dlp is not called before then \
         (the feed poll proposes skipped videos again later)",
        until.format("%H:%M")
    )
}

pub fn is_not_yet_available(err: &str) -> bool {
    let e = err.to_ascii_lowercase();
    e.contains("live event will begin")
        || e.contains("premieres in")
        || e.contains("premiere will begin")
        || e.contains("is_upcoming")
}

/// The network policy for a URL a model handed the media tools: an IP-literal
/// host, then where a name resolves. yt-dlp makes its own lookup afterwards,
/// so for that path this narrows the window rather than closing it; the
/// reqwest paths are also guarded on every connection.
async fn net_check(url: &str) -> anyhow::Result<()> {
    net_policy::check_url_str(url).map_err(|why| anyhow::anyhow!("Not fetched: {why}"))?;
    if let Ok(u) = url::Url::parse(url)
        && let Some(url::Host::Domain(host)) = u.host()
    {
        net_policy::check_host(host)
            .await
            .map_err(|why| anyhow::anyhow!("Not fetched: {why}"))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn caption_tries_skip_the_rendition_youtube_rate_limits() {
        let langs = |v: &Value| {
            caption_tries(v, "en")
                .into_iter()
                .map(|t| (t.langs, t.auto))
                .collect::<Vec<_>>()
        };
        // Speech recognition only (CkKwQocFGts, 2026-10-20): `en-orig`, never `en`.
        let asr = serde_json::json!({
            "subtitles": {},
            "automatic_captions": {"en": [], "en-orig": [], "fr": []},
        });
        assert_eq!(langs(&asr), vec![("en-orig".to_string(), true)]);
        // An uploaded track first, uploaded only; then `en-orig` (ecCrC56NorY).
        let uploaded = serde_json::json!({
            "subtitles": {"en-uYU-mmqFLq8": [], "live_chat": [], "de": []},
            "automatic_captions": {"en": [], "en-orig": []},
        });
        assert_eq!(
            langs(&uploaded),
            vec![
                ("en-uYU-mmqFLq8".to_string(), false),
                ("en-orig".to_string(), true)
            ]
        );
        // Track names are literal in yt-dlp's regex list.
        let dotted = serde_json::json!({"subtitles": {"en-x.y": []}});
        assert_eq!(langs(&dotted), vec![("en-x\\.y".to_string(), false)]);
        // Metadata naming neither keeps the old pattern.
        assert_eq!(
            langs(&serde_json::json!({})),
            vec![("en,en-.*,en.*".to_string(), true)]
        );
    }

    #[test]
    fn an_upcoming_premiere_is_recognised() {
        assert!(is_not_yet_available(
            "yt-dlp failed: ERROR: [youtube] zbGepvgR5qQ: This live event will begin in 7 hours."
        ));
        assert!(is_not_yet_available(
            "ERROR: [youtube] x: Premieres in 2 days"
        ));
        // The bot check is its own outcome, with both apostrophes YouTube uses.
        assert!(is_bot_check(
            "ERROR: [youtube] abc: Sign in to confirm you're not a bot. Use --cookies-from-browser"
        ));
        assert!(is_bot_check("Sign in to confirm you\u{2019}re not a bot"));
        assert!(!is_bot_check("ERROR: [youtube] abc: Video unavailable"));
        assert!(!is_not_yet_available(
            "ERROR: [youtube] x: Video unavailable"
        ));
        let svc = MediaService::from_env();
        let upcoming = serde_json::json!({"id": "u1", "title": "t", "live_status": "is_upcoming"});
        assert!(svc.parse_meta(&upcoming).unwrap().is_live);
    }

    /// `is_live` is the only thing standing between a live broadcast and a job
    /// that hangs until the stream ends: yt-dlp reports `duration: null` for one,
    /// which parses to 0, so the MEDIA_MAX_DURATION_SECS guard can never fire.
    /// `was_live` (a finished stream, now an ordinary VOD with a real duration)
    /// must NOT be treated as live or the finished recording becomes uningestable.
    #[test]
    fn parse_meta_flags_live_but_not_finished_streams() {
        let svc = MediaService::from_env();

        let live = serde_json::json!({
            "id": "f9f6JURECo4",
            "title": "25th Anniversary Commemoration Coverage",
            "is_live": true,
            "duration": serde_json::Value::Null,
        });
        let m = svc.parse_meta(&live).expect("live metadata parses");
        assert!(m.is_live, "a broadcasting stream must be flagged live");
        assert_eq!(
            m.duration_secs, 0,
            "null duration parses to 0 — which is exactly why the duration cap \
             cannot catch a live stream and is_live has to"
        );

        // A finished stream is a normal video and stays ingestable.
        let finished = serde_json::json!({
            "id": "abc123",
            "title": "Yesterday's stream",
            "is_live": false,
            "was_live": true,
            "duration": 3600.0,
        });
        let m = svc.parse_meta(&finished).expect("finished metadata parses");
        assert!(!m.is_live, "was_live must not be treated as live");
        assert_eq!(m.duration_secs, 3600);

        // Absent field (most videos, and non-YouTube extractors) is not live.
        let plain = serde_json::json!({"id": "x1", "title": "t", "duration": 120.0});
        let m = svc.parse_meta(&plain).expect("plain metadata parses");
        assert!(!m.is_live, "absent is_live must default to not-live");
    }

    #[test]
    fn parse_meta_keeps_the_description_capped() {
        let svc = MediaService::from_env();
        let long = "Qwen 3.6 35B A3B ".repeat(400);
        let v = serde_json::json!({"id": "d1", "title": "t", "description": long});
        let m = svc.parse_meta(&v).expect("parses");
        assert!(m.description.starts_with("Qwen 3.6 35B A3B"));
        assert_eq!(m.description.chars().count(), 3000);
        let none = serde_json::json!({"id": "d2", "title": "t"});
        assert!(
            svc.parse_meta(&none)
                .expect("parses")
                .description
                .is_empty()
        );
    }

    #[test]
    fn extracts_first_url_from_goal() {
        assert_eq!(
            MediaService::extract_first_url("watch video: https://youtu.be/abc123 please"),
            Some("https://youtu.be/abc123".to_string())
        );
        assert_eq!(
            MediaService::extract_first_url("https://www.youtube.com/watch?v=xyz"),
            Some("https://www.youtube.com/watch?v=xyz".to_string())
        );
        assert_eq!(MediaService::extract_first_url("no url here"), None);
    }

    #[test]
    fn trailing_punctuation_stripped() {
        assert_eq!(
            MediaService::extract_first_url("see (https://example.com/v)."),
            Some("https://example.com/v".to_string())
        );
    }

    #[test]
    fn rejects_non_http_urls() {
        assert!(MediaService::validate_url("file:///etc/passwd").is_err());
        assert!(MediaService::validate_url("https://youtu.be/x").is_ok());
    }

    #[test]
    fn upload_date_formatting() {
        assert_eq!(format_upload_date("20240131"), "2024-01-31");
        assert_eq!(format_upload_date(""), "");
        assert_eq!(format_upload_date("2024-01-31"), "2024-01-31");
    }

    #[test]
    fn parses_json3_captions() {
        let body = r#"{"events":[
            {"segs":[{"utf8":"Hello"},{"utf8":" world"}]},
            {"segs":[{"utf8":"\n"}]},
            {"segs":[{"utf8":"second line"}]}
        ]}"#;
        assert_eq!(parse_json3(body), "Hello world second line");
    }

    #[test]
    fn json3_garbage_is_empty() {
        assert_eq!(parse_json3("not json"), "");
    }

    #[test]
    fn parses_youtube_atom_feed() {
        let xml = r#"<feed>
          <entry>
            <yt:videoId>VID1</yt:videoId>
            <yt:channelId>CHAN</yt:channelId>
            <title>First &amp; Best</title>
            <published>2024-01-01T00:00:00+00:00</published>
          </entry>
          <entry>
            <yt:videoId>VID2</yt:videoId>
            <yt:channelId>CHAN</yt:channelId>
            <title>Second</title>
            <published>2024-01-02T00:00:00+00:00</published>
          </entry>
        </feed>"#;
        let items = parse_youtube_feed(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].video_id, "VID1");
        assert_eq!(items[0].title, "First & Best");
        assert_eq!(items[0].url, "https://www.youtube.com/watch?v=VID1");
        assert_eq!(items[0].channel_id, "CHAN");
        assert_eq!(items[1].video_id, "VID2");
    }

    #[test]
    fn feed_url_kinds() {
        assert_eq!(
            MediaService::feed_url("youtube_channel", "UC123"),
            Some("https://www.youtube.com/feeds/videos.xml?channel_id=UC123".into())
        );
        assert_eq!(
            MediaService::feed_url("youtube_playlist", "PL9"),
            Some("https://www.youtube.com/feeds/videos.xml?playlist_id=PL9".into())
        );
        assert_eq!(
            MediaService::feed_url("podcast_rss", "https://x/feed.xml"),
            Some("https://x/feed.xml".into())
        );
        assert_eq!(MediaService::feed_url("local_file", "x"), None);
    }

    #[test]
    fn extracts_file_url_from_goal() {
        assert_eq!(
            MediaService::extract_first_url("watch video: file:///home/agent/media/talk.mp4 now"),
            Some("file:///home/agent/media/talk.mp4".to_string())
        );
    }

    #[test]
    fn direct_media_url_detection() {
        assert!(is_direct_media_url("https://cdn.example.com/ep/42.mp3"));
        assert!(is_direct_media_url(
            "https://cdn.example.com/a.MP4?token=x&y=1"
        ));
        assert!(is_direct_media_url("https://x/audio.m4a#t=10"));
        // yt-dlp targets — no media extension on the path.
        assert!(!is_direct_media_url("https://www.youtube.com/watch?v=abc"));
        assert!(!is_direct_media_url("https://youtu.be/abc123"));
        assert!(!is_direct_media_url(
            "https://example.com/podcasts/episode-42"
        ));
    }

    #[test]
    fn stable_media_id_for_url() {
        // Deterministic across calls (dedup relies on this).
        let a = meta_from_media_url("https://cdn.example.com/ep/42.mp3");
        let b = meta_from_media_url("https://cdn.example.com/ep/42.mp3");
        assert_eq!(a.id, b.id);
        assert!(a.id.starts_with("pod-"));
        assert_eq!(a.title, "42");
        assert_eq!(a.channel, "cdn.example.com");
        // Different URL → different id.
        assert_ne!(
            a.id,
            meta_from_media_url("https://cdn.example.com/ep/43.mp3").id
        );
    }

    #[test]
    fn local_file_meta() {
        let m = meta_from_local_file(std::path::Path::new("/home/agent/media/My_Talk.m4a"));
        assert!(m.id.starts_with("file-"));
        assert_eq!(m.title, "My Talk");
        assert_eq!(m.channel, "local");
        assert_eq!(m.url, "file:///home/agent/media/My_Talk.m4a");
    }

    #[test]
    fn parses_an_rss_article_feed() {
        let xml = r#"<rss><channel><title>Bellingcat</title>
            <item><title><![CDATA[Tracking a Drone &amp; Its Operators]]></title>
              <link>https://www.bellingcat.com/news/2026/10/06/drone/</link>
              <pubDate>Tue, 06 Oct 2026 16:00:41 +0000</pubDate></item>
            <item><title>No link here</title></item>
            <item><title>Guid only</title><guid isPermaLink="true">https://c4ads.org/reports/x/</guid></item>
            </channel></rss>"#;
        let items = parse_article_feed(xml);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].title, "Tracking a Drone & Its Operators");
        assert_eq!(
            items[0].url,
            "https://www.bellingcat.com/news/2026/10/06/drone/"
        );
        assert_eq!(items[0].published_at, "Tue, 06 Oct 2026 16:00:41 +0000");
        assert_eq!(items[0].video_id, MediaService::article_id(&items[0].url));
        assert!(items[0].video_id.starts_with("art-"));
        assert_eq!(items[1].url, "https://c4ads.org/reports/x/");
    }

    #[test]
    fn parses_an_atom_article_feed_preferring_the_alternate_link() {
        let xml = r#"<feed><entry><title>Report</title>
            <link rel="self" href="https://x.org/feeds/1"/>
            <link rel="alternate" type="text/html" href="https://x.org/2026/report.html"/>
            <published>2026-10-01T10:00:00Z</published></entry></feed>"#;
        let items = parse_article_feed(xml);
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].url, "https://x.org/2026/report.html");
        assert_eq!(items[0].published_at, "2026-10-01T10:00:00Z");
    }

    #[test]
    fn article_feeds_use_the_reference_as_the_feed_url() {
        assert_eq!(
            MediaService::feed_url("article_rss", "https://www.bellingcat.com/feed/").as_deref(),
            Some("https://www.bellingcat.com/feed/")
        );
    }

    #[test]
    fn extracts_article_paragraphs_and_drops_page_furniture() {
        let html = r#"<html><head><title>T</title><script>var x = "a b c d e f g";</script></head>
            <body><nav><a>Home</a> <a>About us and our very long menu item here</a></nav>
            <header><h1>Site banner with many many words in it</h1></header>
            <article><h1>Short title</h1>
              <p>The first paragraph of the investigation explains what was found and how.</p>
              <figure><figcaption>A caption that should not appear in the text at all</figcaption></figure>
              <p>A second paragraph &mdash; with entities &amp; a quote &#8220;like this&#8221; inside it.</p>
              <div class="share">Share</div>
            </article>
            <footer><p>Copyright notice that has more than six words in it</p></footer></body></html>"#;
        let text = extract_article_text(html);
        assert!(text.contains("The first paragraph of the investigation"));
        assert!(text.contains(
            "A second paragraph \u{2014} with entities & a quote \u{201C}like this\u{201D}"
        ));
        for gone in [
            "var x",
            "menu item",
            "Site banner",
            "caption",
            "Copyright",
            "Share",
        ] {
            assert!(!text.contains(gone), "{gone} leaked: {text}");
        }
    }

    #[test]
    fn rendered_markdown_becomes_article_text() {
        let md = "# Toronto-Based VPN Provider Plans to Quit Canada\n\n\
            ### Date Published\n\nOctober 6, 2026\n\n\
            [Law & Policy](https://citizenlab.ca/focus-area/law-policy/) | [bill c-22](https://citizenlab.ca/topic/bill-c-22/)\n\n\
            ![logo](https://x/logo.png)\n\n\
            Psiphon, a software company spun out of the [Citizen Lab](https://citizenlab.ca), plans to **leave Canada** if the bill passes.\n\n\
            - The bill would require providers to retain metadata for one full year.\n\n\
            [Read our earlier analysis of lawful access in Canada](https://citizenlab.ca/older)\n\n\
            We use cookies to improve your experience on this website today.";
        let text = markdown_article_text(md);
        assert!(text.contains(
            "Psiphon, a software company spun out of the Citizen Lab, plans to leave Canada"
        ));
        assert!(
            text.contains("The bill would require providers to retain metadata"),
            "list marker stripped: {text}"
        );
        for gone in [
            "Law & Policy",
            "logo",
            "Read our earlier analysis",
            "cookies",
            "Date Published",
        ] {
            assert!(!text.contains(gone), "{gone} leaked: {text}");
        }
    }

    #[test]
    fn only_refusals_trigger_the_rendering_fallback() {
        for code in [401, 403, 429, 451, 503] {
            assert!(is_block_status(code));
        }
        for code in [404, 410, 500] {
            assert!(
                !is_block_status(code),
                "{code} means gone or broken, not blocked"
            );
        }
    }

    #[test]
    fn markdown_metadata_takes_the_first_heading() {
        let m = meta_from_markdown(
            "https://www.understandingwar.org/backgrounder/x",
            "intro\n# Russian Offensive Campaign Assessment\nbody",
        );
        assert_eq!(m.title, "Russian Offensive Campaign Assessment");
        assert_eq!(m.channel, "understandingwar.org");
        assert_eq!(
            m.id,
            MediaService::article_id("https://www.understandingwar.org/backgrounder/x")
        );
        assert!(clip_article("é".repeat(ARTICLE_MAX_CHARS)).len() <= ARTICLE_MAX_CHARS);
    }

    #[test]
    fn related_story_cards_are_not_article_text() {
        // C4ADS/OCCRP shape: a teaser whose title and excerpt are one link.
        let html = r#"<main>
            <p>The report maps a procurement network across three provinces and two shell firms.</p>
            <p>It cites <a href="/a">customs records filed in 2025</a> as the primary evidence for the finding.</p>
            <div class="card"><a href="/other"><h3>Tweaking the Record: Mapping a Chinese Fentanyl Network</h3>
              <div class="excerpt">C4ADS built out the larger networks behind synthetic drug sales here</div></a></div>
            </main>"#;
        let text = extract_article_text(html);
        assert!(text.contains("procurement network"));
        assert!(
            text.contains("customs records filed in 2025"),
            "inline links stay: {text}"
        );
        assert!(!text.contains("Fentanyl"), "card title leaked: {text}");
        assert!(
            !text.contains("synthetic drug sales"),
            "card excerpt leaked: {text}"
        );
    }

    #[test]
    fn article_meta_reads_og_tags_in_any_attribute_order() {
        let html = r#"<meta content="Inside the Network" property="og:title">
            <meta property='og:site_name' content='C4ADS'>
            <meta property="article:published_time" content="2026-09-29T13:17:38+00:00">"#;
        let m = article_meta("https://c4ads.org/reports/x/", html);
        assert_eq!(m.title, "Inside the Network");
        assert_eq!(m.channel, "C4ADS");
        assert_eq!(m.published_at, "2026-09-29T13:17:38+00:00");
        assert_eq!(
            m.id,
            MediaService::article_id("https://c4ads.org/reports/x/")
        );
        let bare = article_meta(
            "https://www.citizenlab.ca/2026/10/x/",
            "<title>Fallback &amp; Title</title>",
        );
        assert_eq!(bare.title, "Fallback & Title");
        assert_eq!(bare.channel, "citizenlab.ca");
    }

    #[test]
    fn same_site_ignores_www_and_paths() {
        assert!(same_site(
            "https://www.bellingcat.com/feed/",
            "https://bellingcat.com/news/a"
        ));
        assert!(!same_site(
            "https://www.occrp.org/en/feed",
            "https://icij.org/x"
        ));
        assert!(!same_site("", ""));
    }

    #[test]
    fn unextractable_errors_are_recognised() {
        assert!(is_unextractable(&format!(
            "{ARTICLE_UNEXTRACTABLE} (12 chars) — paywalled"
        )));
        assert!(!is_unextractable("error sending request: timed out"));
    }

    #[test]
    fn parses_rss_podcast_feed() {
        let xml = r#"<rss><channel>
          <item>
            <title>Episode One</title>
            <enclosure url="https://cdn.example.com/ep/1.mp3" type="audio/mpeg" length="123"/>
            <pubDate>Wed, 02 Oct 2024 10:00:00 GMT</pubDate>
          </item>
          <item>
            <title><![CDATA[Episode & Two]]></title>
            <enclosure length="99" type="audio/mpeg" url="https://cdn.example.com/ep/2.mp3"/>
            <pubDate>Thu, 03 Oct 2024 10:00:00 GMT</pubDate>
          </item>
          <item>
            <title>No enclosure, skipped</title>
          </item>
        </channel></rss>"#;
        let items = parse_rss_feed(xml);
        assert_eq!(items.len(), 2, "items without an enclosure are skipped");
        assert_eq!(items[0].title, "Episode One");
        assert_eq!(items[0].url, "https://cdn.example.com/ep/1.mp3");
        assert_eq!(items[1].title, "Episode & Two");
        assert_eq!(items[1].url, "https://cdn.example.com/ep/2.mp3");
        // Feed id must match the ingest-time id so dedup lines up.
        assert_eq!(items[0].video_id, meta_from_media_url(&items[0].url).id);
    }

    #[test]
    fn short_transcript_single_window() {
        let windows = chunk_transcript("Short text.", 6000);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0], "Short text.");
    }

    #[test]
    fn empty_transcript_no_windows() {
        assert!(chunk_transcript("   ", 6000).is_empty());
    }

    #[test]
    fn long_transcript_chunks_on_sentences() {
        let sentence = "This is a sentence. ".repeat(100); // ~2000 chars
        let windows = chunk_transcript(&sentence, 500);
        assert!(windows.len() > 1, "expected multiple windows");
        // No window materially exceeds the target (sentence-boundary packing).
        for w in &windows {
            assert!(w.len() <= 520, "window too large: {}", w.len());
        }
    }
}
