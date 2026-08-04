//! Media service — transcript acquisition + LLM map-reduce summarization.
//!
//! Owns the `yt-dlp` subprocess boundary and the summarization loop. Pure
//! logic, no MCP types. Phase 1 covers YouTube **captions** (human subs
//! preferred over auto-captions) discovered from `yt-dlp -J` metadata and
//! fetched directly as `json3`. Whisper transcription for caption-less media
//! (Phase 4) and podcast/local ingestion (Phase 5) are stubbed with clean
//! errors so the seams exist without half-working behavior.
//!
//! Subprocess safety: `yt-dlp` is always invoked with an **argument array**
//! (never a shell string), and URLs are scheme-validated before use.

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
    /// Caption track URL (already normalized to `fmt=json3`), if any.
    pub caption_url: Option<String>,
}

pub struct MediaService {
    yt_dlp_path: String,
    caption_lang: String,
    max_duration_secs: i64,
    /// Transcript window size (chars) for the map pass.
    window_chars: usize,
    /// `none` disables Whisper fallback (Phase 4).
    whisper_provider: String,
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
            whisper_provider: std::env::var("WHISPER_PROVIDER").unwrap_or_else(|_| "none".into()),
            http: reqwest::Client::new(),
        }
    }

    // ========================================================================
    // URL helpers
    // ========================================================================

    /// Extract the first `http(s)` URL from arbitrary text (e.g. a goal like
    /// `"watch video: https://youtu.be/abc"`). Returns the whole trimmed string
    /// if it is already a bare URL.
    pub fn extract_first_url(text: &str) -> Option<String> {
        let re = Regex::new(r"https?://[^\s)>\]]+").ok()?;
        re.find(text)
            .map(|m| m.as_str().trim_end_matches(['.', ',', ')']).to_string())
    }

    /// Reject anything that isn't an `http`/`https` URL (no `file://`, no shell
    /// metacharacters reaching the subprocess as a "URL").
    fn validate_url(url: &str) -> anyhow::Result<()> {
        if !(url.starts_with("http://") || url.starts_with("https://")) {
            anyhow::bail!("unsupported or missing URL (only http/https are allowed): {url}");
        }
        Ok(())
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

    /// Parse the `yt-dlp -J` blob into [`VideoMeta`], selecting the best caption
    /// track for `self.caption_lang` (manual subs preferred over auto-captions).
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
        let caption_url = select_caption_url(v, &self.caption_lang);

        Ok(VideoMeta {
            id,
            title,
            channel,
            channel_id,
            url,
            published_at,
            duration_secs,
            caption_url,
        })
    }

    async fn fetch_caption_text(&self, caption_url: &str) -> anyhow::Result<String> {
        let body = self
            .http
            .get(caption_url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_json3(&body))
    }

    /// Fetch metadata + transcript. Returns `(meta, transcript, source)`.
    /// Falls back to Whisper only when captions are absent (Phase 4 — errors
    /// cleanly unless `WHISPER_PROVIDER` is configured).
    pub async fn fetch_transcript(&self, url: &str) -> anyhow::Result<(VideoMeta, String, String)> {
        Self::validate_url(url)?;
        let raw = self.run_yt_dlp_json(url).await?;
        let meta = self.parse_meta(&raw)?;

        if let Some(ref cap_url) = meta.caption_url {
            debug!(video = %meta.id, "fetching caption track");
            let text = self.fetch_caption_text(cap_url).await?;
            if !text.trim().is_empty() {
                return Ok((meta, text, "captions".to_string()));
            }
            warn!(video = %meta.id, "caption track was empty, falling through");
        }

        // No usable captions — Whisper fallback (Phase 4).
        if self.whisper_provider == "none" {
            anyhow::bail!(
                "no captions available for '{}' and Whisper transcription is disabled \
                 (set WHISPER_PROVIDER to enable audio transcription)",
                meta.id
            );
        }
        anyhow::bail!(
            "Whisper provider '{}' is configured but audio transcription is not yet \
             implemented (Phase 4)",
            self.whisper_provider
        )
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

    /// Build the free YouTube RSS feed URL for a source (no API key required).
    /// Returns `None` for kinds without a supported feed yet (e.g. podcasts).
    pub fn feed_url(kind: &str, reference: &str) -> Option<String> {
        match kind {
            "youtube_channel" => Some(format!(
                "https://www.youtube.com/feeds/videos.xml?channel_id={reference}"
            )),
            "youtube_playlist" => Some(format!(
                "https://www.youtube.com/feeds/videos.xml?playlist_id={reference}"
            )),
            _ => None,
        }
    }

    /// Fetch and parse the recent items from a channel/playlist feed.
    pub async fn list_feed_videos(
        &self,
        kind: &str,
        reference: &str,
    ) -> anyhow::Result<Vec<FeedItem>> {
        let Some(url) = Self::feed_url(kind, reference) else {
            anyhow::bail!(
                "feed listing for kind '{kind}' is not supported yet (Phase 5: podcasts/local)"
            );
        };
        let body = self
            .http
            .get(&url)
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        Ok(parse_youtube_feed(&body))
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

/// Select a caption track URL from yt-dlp metadata, preferring human
/// `subtitles` over `automatic_captions`, and normalize it to `fmt=json3`.
fn select_caption_url(v: &Value, lang: &str) -> Option<String> {
    for field in ["subtitles", "automatic_captions"] {
        let Some(map) = v.get(field).and_then(|m| m.as_object()) else {
            continue;
        };
        // Exact lang match first, then any track whose key starts with `lang`
        // (e.g. "en" matching "en-US").
        let key = map
            .keys()
            .find(|k| k.as_str() == lang)
            .or_else(|| map.keys().find(|k| k.starts_with(lang)))?;
        let tracks = map.get(key).and_then(|t| t.as_array())?;
        // Prefer an existing json3 track, else take the first and force json3.
        let chosen = tracks
            .iter()
            .find(|t| t.get("ext").and_then(|e| e.as_str()) == Some("json3"))
            .or_else(|| tracks.first())?;
        let base = chosen.get("url").and_then(|u| u.as_str())?;
        return Some(force_fmt_json3(base));
    }
    None
}

/// Rewrite a timedtext caption URL to request the `json3` format.
fn force_fmt_json3(url: &str) -> String {
    // Drop any existing fmt= parameter, then append fmt=json3.
    let re = Regex::new(r"([?&])fmt=[^&]*").unwrap();
    let stripped = re.replace_all(url, "$1").to_string();
    let stripped = stripped.trim_end_matches(['?', '&']);
    let sep = if stripped.contains('?') { '&' } else { '?' };
    format!("{stripped}{sep}fmt=json3")
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
    fn force_json3_fmt() {
        assert_eq!(
            force_fmt_json3("https://x/api/timedtext?v=1"),
            "https://x/api/timedtext?v=1&fmt=json3"
        );
        assert_eq!(
            force_fmt_json3("https://x/api/timedtext?v=1&fmt=vtt"),
            "https://x/api/timedtext?v=1&fmt=json3"
        );
        assert_eq!(
            force_fmt_json3("https://x/api/timedtext"),
            "https://x/api/timedtext?fmt=json3"
        );
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
    fn selects_manual_caption_over_auto() {
        let v: Value = serde_json::from_str(
            r#"{
                "subtitles": {"en": [{"ext": "vtt", "url": "https://s/manual?fmt=vtt"}]},
                "automatic_captions": {"en": [{"ext": "json3", "url": "https://s/auto?fmt=json3"}]}
            }"#,
        )
        .unwrap();
        let url = select_caption_url(&v, "en").unwrap();
        assert!(
            url.starts_with("https://s/manual"),
            "should prefer manual subs: {url}"
        );
        assert!(url.ends_with("fmt=json3"));
    }

    #[test]
    fn selects_lang_prefix_match() {
        let v: Value = serde_json::from_str(
            r#"{"automatic_captions": {"en-US": [{"ext": "json3", "url": "https://s/x"}]}}"#,
        )
        .unwrap();
        assert!(select_caption_url(&v, "en").is_some());
    }

    #[test]
    fn no_caption_returns_none() {
        let v: Value =
            serde_json::from_str(r#"{"subtitles": {}, "automatic_captions": {}}"#).unwrap();
        assert!(select_caption_url(&v, "en").is_none());
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
            MediaService::feed_url("podcast_rss", "https://x/feed"),
            None
        );
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
