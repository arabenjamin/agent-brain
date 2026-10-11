//! Whisper transcription backends (Phase 4) — speech-to-text for caption-less
//! media. Configured via env, mirroring the LLM-provider pattern.
//!
//! The default (and only) backend is [`HttpTranscriber`], which POSTs audio to
//! any **OpenAI-compatible** `/audio/transcriptions` endpoint. This intentionally
//! targets a **self-hosted** Whisper server (e.g. faster-whisper-server /
//! speaches running as the `whisper` sidecar) — no external SaaS. Point
//! `WHISPER_BASE_URL` at your own server; `WHISPER_API_KEY` is optional.
//!
//! Env:
//! - `WHISPER_PROVIDER` — `none` (default, disabled) | `http` | `local` | `openai`
//!   (the last three are aliases: all mean "POST to WHISPER_BASE_URL").
//! - `WHISPER_BASE_URL` — e.g. `http://whisper:8000/v1`
//! - `WHISPER_MODEL` — e.g. `Systran/faster-whisper-base`
//! - `WHISPER_API_KEY` — optional bearer token.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use tracing::debug;

#[async_trait]
pub trait Transcriber: Send + Sync {
    /// Transcribe the audio file at `path` to plain text.
    async fn transcribe(&self, path: &Path) -> anyhow::Result<String>;
    /// Human-readable backend label for logs/telemetry.
    fn label(&self) -> &str;
}

/// POSTs audio to an OpenAI-compatible `/audio/transcriptions` endpoint.
pub struct HttpTranscriber {
    /// Base URL including any `/v1` suffix, no trailing slash.
    base_url: String,
    model: String,
    api_key: Option<String>,
    http: reqwest::Client,
}

impl HttpTranscriber {
    pub fn new(base_url: String, model: String, api_key: Option<String>) -> Self {
        Self {
            base_url: base_url.trim_end_matches('/').to_string(),
            model,
            api_key,
            // CPU transcription of a long video can take minutes — be patient.
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(900))
                .build()
                .unwrap_or_default(),
        }
    }
}

#[async_trait]
impl Transcriber for HttpTranscriber {
    async fn transcribe(&self, path: &Path) -> anyhow::Result<String> {
        let bytes = tokio::fs::read(path).await?;
        let filename = path
            .file_name()
            .and_then(|f| f.to_str())
            .unwrap_or("audio")
            .to_string();

        let part = reqwest::multipart::Part::bytes(bytes)
            .file_name(filename)
            .mime_str("application/octet-stream")?;
        let form = reqwest::multipart::Form::new()
            .part("file", part)
            .text("model", self.model.clone())
            .text("response_format", "text");

        let url = format!("{}/audio/transcriptions", self.base_url);
        debug!(url = %url, model = %self.model, "posting audio to Whisper endpoint");
        let mut req = self.http.post(&url).multipart(form);
        if let Some(ref key) = self.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req.send().await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("Whisper endpoint returned {status}: {}", body.trim());
        }
        // response_format=text → the body is the transcript.
        Ok(resp.text().await?.trim().to_string())
    }

    fn label(&self) -> &str {
        "http"
    }
}

/// Build a transcriber from the environment, or `None` when disabled/unconfigured.
pub fn from_env() -> Option<Arc<dyn Transcriber>> {
    let provider = std::env::var("WHISPER_PROVIDER").unwrap_or_else(|_| "none".into());
    match provider.as_str() {
        "http" | "local" | "openai" => {
            let base_url = std::env::var("WHISPER_BASE_URL").ok()?;
            let model = std::env::var("WHISPER_MODEL")
                .unwrap_or_else(|_| "Systran/faster-whisper-base".into());
            let api_key = std::env::var("WHISPER_API_KEY")
                .ok()
                .filter(|k| !k.is_empty());
            Some(Arc::new(HttpTranscriber::new(base_url, model, api_key)))
        }
        _ => None,
    }
}
