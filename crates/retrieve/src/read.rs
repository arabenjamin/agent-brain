//! `read_page`: a URL to readable text, spread across the free page readers.
//!
//! Until 2026-10-16 agents read pages through Exa's paid `web_fetch_exa`
//! (over the MCP client), spending the same $10 monthly credit as Exa search,
//! and `ingest_article` fell back to Anakin's 300 a month alone. The readers
//! are now YAML (`page_readers.rs`) with free allowances, and each read goes
//! first to the reader within its paced allowance with the fewest reads today
//! (`search_rotation.rs`, the same pacing as `search_web`), then the rest,
//! then our own direct fetch. Raw files go to the direct fetch first. Every
//! attempt is a row in the `read_usage` ledger.

use std::time::Instant;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use tracing::{info, warn};

use agent_brain_protocol::{Skill, ToolCallResult, ToolDefinition, parse_args};
use agent_brain_repository::{Ledger, ReadAttempt, TelemetryClient};

use crate::page_readers::{self, MIN_TEXT_CHARS, ReaderSpec};
use crate::search_rotation::{self, Rung, Standing};

/// How long a reader that reported a spent allowance is tried last.
const QUOTA_COOLDOWN_HOURS: i64 = 6;

const DEFAULT_MAX_CHARS: usize = 8_000;
const MAX_MAX_CHARS: usize = 30_000;

/// A page as `read_page` returns it.
#[derive(Debug, Clone)]
pub struct PageRead {
    pub url: String,
    pub reader: String,
    pub title: Option<String>,
    /// The whole text the reader returned.
    pub text: String,
    /// Every reader passed over before this one, with why.
    pub passed_over: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct ReadInput {
    url: String,
    #[serde(default)]
    max_chars: Option<usize>,
    #[serde(default)]
    reader: Option<String>,
}

fn classify(error: &str) -> &'static str {
    let lower = error.to_lowercase();
    if lower.contains("quota exhausted")
        || lower.contains("insufficient")
        || lower.contains("credits")
    {
        "quota_exhausted"
    } else if lower.contains("429")
        || lower.contains("too many requests")
        || lower.contains("rate limit")
    {
        "rate_limited"
    } else if lower.contains("not configured") {
        "not_configured"
    } else if lower.contains("http 401")
        || lower.contains("http 403")
        || lower.contains("http 451")
        || lower.contains("http 503")
        // TinyFish's word for "the site refused us" (Reuters, 2026-10-16).
        || lower.contains("target_http_error")
    {
        "blocked"
    } else if lower.contains("request failed") || lower.contains("timed out") {
        "network_error"
    } else {
        "api_error"
    }
}

/// Who is reading, as `search_web` records it.
fn caller() -> (String, Option<String>, Option<String>) {
    let purpose = agent_brain_llm::routing::ROUTE_CONTEXT
        .try_with(|c| c.purpose.as_str().to_string())
        .unwrap_or_else(|_| "direct".to_string());
    let tool = agent_brain_llm::ctx::CURRENT_TOOL
        .try_with(|t| t.clone())
        .ok()
        .flatten();
    (purpose, tool, agent_brain_llm::ctx::current_owner_task())
}

pub struct ReadSkill {
    client: Client,
    telemetry: Option<TelemetryClient>,
}

impl ReadSkill {
    pub fn new(telemetry: Option<TelemetryClient>) -> Self {
        Self {
            client: Client::new(),
            telemetry,
        }
    }

    fn def() -> ToolDefinition {
        ToolDefinition {
            name: "read_page".to_string(),
            description: "Read a web page (or raw file) as clean text: an article, a docs \
                 page, a source file on raw.githubusercontent.com. Spreads reads across \
                 free page readers that render JavaScript (falling back to a direct fetch), \
                 so use it whenever a search result's snippet is not enough. Returns the \
                 title and up to max_chars of text (default 8000), marked when cut. Not for \
                 calling an API (use http_request)."
                .to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The page's http(s) URL"},
                    "max_chars": {"type": "integer", "minimum": 500, "maximum": MAX_MAX_CHARS, "description": "Most characters of text returned (default 8000)"},
                    "reader": {"type": "string", "description": "Try this reader first (the rest still back it up). Omit: the rotation chooses."}
                },
                "required": ["url"]
            }),
        }
    }

    /// The readers in the order this URL will try them, and how the first
    /// was chosen (`direct` for a raw file, `pinned`, `rotation`, `ladder`).
    fn ladder(&self, url: &str, pinned: Option<&str>) -> (Vec<&'static ReaderSpec>, &'static str) {
        let readers: Vec<&'static ReaderSpec> = page_readers::readers()
            .iter()
            .filter(|r| page_readers::configured(r))
            .collect();
        let exhausted = self
            .telemetry
            .as_ref()
            .and_then(|t| {
                t.readers_with_recent_errors("quota_exhausted", QUOTA_COOLDOWN_HOURS)
                    .ok()
            })
            .unwrap_or_default();
        let now = chrono::Utc::now();
        let rungs: Vec<Rung> = readers
            .iter()
            .map(|r| {
                let mut rung = Rung {
                    name: r.name.clone(),
                    standing: None,
                    day_calls: 0,
                    weight: r.budget.as_ref().and_then(|b| b.weight).unwrap_or(1.0),
                };
                let Some(b) = &r.budget else { return rung };
                if exhausted.contains(&r.name) {
                    rung.standing = Some(Standing::Spent);
                    return rung;
                }
                match self.telemetry.as_ref().map(|t| {
                    t.provider_use(
                        Ledger::Read,
                        &r.name,
                        search_rotation::period_start(b.per, now),
                    )
                }) {
                    Some(Ok(u)) => {
                        rung.day_calls = u.day_calls;
                        rung.standing = Some(search_rotation::standing(b, &u, now));
                    }
                    Some(Err(e)) => {
                        warn!(reader = %r.name, error = %e, "read rotation: ledger read failed")
                    }
                    None => rung.standing = Some(Standing::OnPace),
                }
                rung
            })
            .collect();
        let first = readers
            .iter()
            .find(|r| page_readers::first_for(r, url))
            .map(|r| r.name.clone());
        let head = pinned.map(str::to_string).or(first.clone());
        let (order, rotated) = search_rotation::order(&rungs, head.as_deref());
        let how = if pinned.is_some() {
            "pinned"
        } else if first.is_some() {
            "direct"
        } else if rotated {
            "rotation"
        } else {
            "ladder"
        };
        let by_name = |n: &String| readers.iter().copied().find(|r| &r.name == n);
        (order.iter().filter_map(by_name).collect(), how)
    }

    /// Read `url`, trying readers until one returns enough text. `Err` names
    /// every reader tried and why each failed.
    pub async fn read(&self, url: &str, pinned: Option<&str>) -> Result<PageRead, String> {
        let url = url.trim();
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Err(format!("not an http(s) URL: {url:?}"));
        }
        let (ladder, how) = self.ladder(url, pinned);
        if ladder.is_empty() {
            return Err("no page reader is configured".into());
        }
        let (purpose, tool, task_id) = caller();
        let call_id = uuid::Uuid::new_v4().to_string();
        let mut passed_over: Vec<String> = Vec::new();
        for (i, reader) in ladder.iter().enumerate() {
            let started = Instant::now();
            let outcome = page_readers::read(&self.client, reader, url).await;
            let mut row = ReadAttempt {
                call_id: call_id.clone(),
                reader: reader.name.clone(),
                url: url.to_string(),
                duration_ms: Some(started.elapsed().as_millis() as i64),
                picked: if i == 0 { how } else { "fallback" }.to_string(),
                purpose: Some(purpose.clone()),
                tool: tool.clone(),
                task_id: task_id.clone(),
                ..Default::default()
            };
            match outcome {
                Ok(page) => {
                    let chars = page.text.chars().count();
                    row.chars = Some(chars as i64);
                    row.units = Some(page.units);
                    if chars < MIN_TEXT_CHARS {
                        // Billed, but not a read: a script shell or a wall.
                        row.success = true;
                        row.error_kind = Some("too_little_text".into());
                        self.record(&row);
                        passed_over.push(format!("{} ({chars} chars of text)", reader.name));
                        continue;
                    }
                    row.success = true;
                    self.record(&row);
                    if !passed_over.is_empty() {
                        info!(url, reader = %reader.name, passed_over = ?passed_over, "read_page fell through");
                    }
                    return Ok(PageRead {
                        url: url.to_string(),
                        reader: reader.name.clone(),
                        title: page.title,
                        text: page.text,
                        passed_over,
                    });
                }
                Err(e) => {
                    let kind = classify(&e);
                    row.error_kind = Some(kind.to_string());
                    self.record(&row);
                    warn!(url, reader = %reader.name, error = %e, "page reader failed");
                    passed_over.push(format!(
                        "{} ({kind}: {})",
                        reader.name,
                        e.chars().take(160).collect::<String>()
                    ));
                }
            }
        }
        Err(format!(
            "No reader could read {url}. Tried: {}",
            passed_over.join("; ")
        ))
    }

    fn record(&self, row: &ReadAttempt) {
        if let Some(t) = &self.telemetry
            && let Err(e) = t.record_read_attempt(row)
        {
            warn!(reader = %row.reader, error = %e, "Failed to record page read");
        }
    }

    /// Each budgeted reader against its allowance now, and the ledger's
    /// per-reader totals, for `get_search_usage`.
    pub fn report(&self, window_hours: Option<i64>) -> Value {
        let Some(t) = &self.telemetry else {
            return json!({"note": "telemetry is not configured"});
        };
        let now = chrono::Utc::now();
        let rotation: Vec<Value> = page_readers::readers()
            .iter()
            .filter(|r| page_readers::configured(r))
            .filter_map(|r| {
                let b = r.budget.as_ref()?;
                let u = t
                    .provider_use(Ledger::Read, &r.name, search_rotation::period_start(b.per, now))
                    .ok()?;
                Some(json!({
                    "reader": r.name,
                    "per": format!("{:?}", b.per).to_lowercase(),
                    "free": b.free,
                    "used": search_rotation::used(b, &u),
                    "allowed_so_far": (search_rotation::allowance_so_far(b, now) * 1000.0).round() / 1000.0,
                    "standing": format!("{:?}", search_rotation::standing(b, &u, now)),
                    "reads_today": u.day_calls,
                }))
            })
            .collect();
        let stats = t
            .get_read_stats(window_hours)
            .unwrap_or_else(|e| json!({"error": e.to_string()}));
        json!({"rotation": rotation, "ledger": stats})
    }

    async fn handle(&self, arguments: Option<Value>) -> ToolCallResult {
        let input: ReadInput = match parse_args(arguments) {
            Ok(i) => i,
            Err(e) => return e,
        };
        let max = input
            .max_chars
            .unwrap_or(DEFAULT_MAX_CHARS)
            .clamp(500, MAX_MAX_CHARS);
        match self.read(&input.url, input.reader.as_deref()).await {
            Ok(page) => {
                let total = page.text.chars().count();
                let mut text: String = page.text.chars().take(max).collect();
                if total > max {
                    text.push_str(&format!(
                        "\n\n[… cut at {max} of {total} characters; raise max_chars (up to {MAX_MAX_CHARS}) to read more]"
                    ));
                }
                ToolCallResult::success_json(json!({
                    "url": page.url,
                    "title": page.title,
                    "reader": page.reader,
                    "chars": total,
                    "truncated": total > max,
                    "passed_over": page.passed_over,
                    "text": text,
                }))
            }
            Err(e) => ToolCallResult::error(e),
        }
    }
}

#[async_trait]
impl Skill for ReadSkill {
    fn name(&self) -> &str {
        "Page Reader"
    }

    fn tools(&self) -> Vec<ToolDefinition> {
        vec![Self::def()]
    }

    async fn execute(&self, tool_name: &str, arguments: Option<Value>) -> Option<ToolCallResult> {
        match tool_name {
            "read_page" => Some(self.handle(arguments).await),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_classified_for_the_ledger() {
        assert_eq!(
            classify("tinyfish read failed: quota exhausted — HTTP 402"),
            "quota_exhausted"
        );
        assert_eq!(classify("HTTP 429 Too Many Requests"), "rate_limited");
        assert_eq!(
            classify("direct read failed: HTTP 403 Forbidden"),
            "blocked"
        );
        assert_eq!(classify("Request failed: timed out"), "network_error");
        assert_eq!(classify("tinyfish: invalid_url"), "api_error");
    }

    #[tokio::test]
    async fn a_non_http_url_is_refused_before_any_reader() {
        let skill = ReadSkill::new(None);
        let e = skill.read("file:///etc/passwd", None).await.unwrap_err();
        assert!(e.contains("not an http(s) URL"));
    }
}
