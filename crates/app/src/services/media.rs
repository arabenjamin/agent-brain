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

use std::sync::Arc;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use tracing::{debug, warn};

use crate::services::traits::LlmProvider;

/// Structured summary returned by `ingest` — the payload chains reason over.
#[derive(Debug, Clone, Serialize)]
pub struct MediaSummary {
    pub video_id: String,
    pub title: String,
    pub channel: String,
    pub channel_id: String,
    pub url: String,
    pub published_at: String,
    pub duration_secs: i64,
    /// `captions` | `whisper`
    pub transcript_source: String,
    pub summary: String,
    pub key_concepts: Vec<String>,
    pub transcript_len: usize,
}

/// A recent item discovered from a channel/playlist/podcast feed.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct FeedItem {
    pub video_id: String,
    pub title: String,
    pub url: String,
    pub published_at: String,
    pub channel_id: String,
}

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
    transcriber: Option<Arc<dyn crate::services::transcribe::Transcriber>>,
    /// Scratch dir for yt-dlp subtitle/audio downloads (cleaned per call).
    media_dir: std::path::PathBuf,
    http: reqwest::Client,
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
            transcriber: crate::services::transcribe::from_env(),
            media_dir: std::env::var("MEDIA_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(|_| std::env::temp_dir()),
            http: reqwest::Client::new(),
        }
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
    ) -> anyhow::Result<&Arc<dyn crate::services::transcribe::Transcriber>> {
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

    async fn run_yt_dlp_json(&self, url: &str) -> anyhow::Result<Value> {
        let output = tokio::process::Command::new(&self.yt_dlp_path)
            .args(["-J", "--no-warnings", "--no-playlist", url])
            .output()
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "failed to run yt-dlp ('{}'): {e}. Install yt-dlp or set YT_DLP_PATH.",
                    self.yt_dlp_path
                )
            })?;
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
        let v = self.run_yt_dlp_json(url).await?;
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
        let output = tokio::process::Command::new(&self.yt_dlp_path)
            .args(&args)
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run yt-dlp for channel resolution: {e}"))?;
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

        Ok(VideoMeta {
            id,
            title,
            channel,
            channel_id,
            url,
            published_at,
            duration_secs,
        })
    }

    /// Download the caption track via `yt-dlp` itself (into a per-call scratch
    /// dir) and parse it to plain text. Letting yt-dlp fetch the subtitle file
    /// is far more robust than fetching the timedtext URL directly — auto-
    /// captions in particular require yt-dlp's session handling. Returns `None`
    /// when the video has no caption track. Human subs are preferred over
    /// auto-captions (yt-dlp writes `<id>.<lang>.json3`, auto to the same name;
    /// we pass `--write-subs` before `--write-auto-subs` so manual wins).
    async fn download_captions(&self, url: &str, id: &str) -> anyhow::Result<Option<String>> {
        let dir = self
            .media_dir
            .join(format!("brain-media-{id}-{}", uuid::Uuid::new_v4()));
        tokio::fs::create_dir_all(&dir).await.map_err(|e| {
            anyhow::anyhow!("could not create media scratch dir {}: {e}", dir.display())
        })?;

        // Match the exact lang plus regional variants (en, en-US, en-orig, …).
        let lang_pat = format!("{lang},{lang}-.*,{lang}.*", lang = self.caption_lang);
        let out = tokio::process::Command::new(&self.yt_dlp_path)
            .args([
                "--skip-download",
                "--no-warnings",
                "--no-playlist",
                "--write-subs",
                "--write-auto-subs",
                "--sub-langs",
                &lang_pat,
                "--sub-format",
                "json3",
                "-o",
                "%(id)s",
                "-P",
                &dir.to_string_lossy(),
                url,
            ])
            .output()
            .await;

        let text = self.read_first_json3(&dir).await;
        // Best-effort cleanup regardless of outcome.
        let _ = tokio::fs::remove_dir_all(&dir).await;

        match out {
            Ok(o) if !o.status.success() => {
                let stderr = String::from_utf8_lossy(&o.stderr);
                warn!(video = %id, stderr = %stderr.trim(), "yt-dlp subtitle download reported failure");
            }
            Err(e) => anyhow::bail!(
                "failed to run yt-dlp ('{}') for captions: {e}",
                self.yt_dlp_path
            ),
            _ => {}
        }
        Ok(text.filter(|t| !t.trim().is_empty()))
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
        match self.classify_input(url)? {
            MediaInput::YtDlp(u) => self.fetch_via_ytdlp(&u).await,
            MediaInput::DirectMedia(u) => self.fetch_via_direct_media(&u).await,
            MediaInput::LocalFile(p) => self.fetch_via_local_file(&p).await,
        }
    }

    /// Captions-first path for yt-dlp-extractable URLs (YouTube et al.).
    async fn fetch_via_ytdlp(&self, url: &str) -> anyhow::Result<(VideoMeta, String, String)> {
        let raw = self.run_yt_dlp_json(url).await?;
        let meta = self.parse_meta(&raw)?;

        debug!(video = %meta.id, "downloading caption track via yt-dlp");
        if let Some(text) = self.download_captions(url, &meta.id).await? {
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
        let out = tokio::process::Command::new(&self.yt_dlp_path)
            .args([
                "-f",
                "bestaudio/best",
                "--no-warnings",
                "--no-playlist",
                "-o",
                "%(id)s.%(ext)s",
                "-P",
                &dir.to_string_lossy(),
                url,
            ])
            .output()
            .await
            .map_err(|e| anyhow::anyhow!("failed to run yt-dlp for audio: {e}"))?;
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
    ) -> anyhow::Result<(String, Vec<String>)> {
        let windows = chunk_transcript(transcript, self.window_chars);
        debug!(video = %meta.id, windows = windows.len(), "summarizing transcript");

        let notes = if windows.len() <= 1 {
            transcript.to_string()
        } else {
            let mut partials = Vec::with_capacity(windows.len());
            for (i, w) in windows.iter().enumerate() {
                let prompt = format!(
                    "This is segment {}/{} of the transcript of a video titled \"{}\". \
                     Summarize its key points, claims, and any named concepts as concise \
                     bullet points. Do not add preamble.\n\nSEGMENT:\n{}",
                    i + 1,
                    windows.len(),
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

        let reduce_prompt = format!(
            "You are summarizing a video titled \"{}\" by {}. Below are notes taken from its \
             transcript. Write a cohesive 200-400 word prose summary and extract the key \
             concepts.\n\nNOTES:\n{}\n\nReturn ONLY a JSON object of the form:\n\
             {{\"summary\": \"<prose summary>\", \"key_concepts\": [\"concept\", ...]}}",
            meta.title, meta.channel, notes
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
            "podcast_rss" => Some(reference.to_string()),
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
        let body = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(match kind {
            "podcast_rss" => parse_rss_feed(&body),
            _ => parse_youtube_feed(&body),
        })
    }
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

#[cfg(test)]
mod tests {
    use super::*;

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
