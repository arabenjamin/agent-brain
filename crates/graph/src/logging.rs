use std::collections::VecDeque;
use std::io;
use std::sync::{Arc, Mutex};

use chrono::Utc;
use serde::Serialize;
use tracing::Subscriber;
use tracing_subscriber::{
    EnvFilter,
    fmt::{self, format::FmtSpan},
    layer::{Context, Layer, SubscriberExt},
    registry::LookupSpan,
    util::SubscriberInitExt,
};

use serde::Deserialize;

// ── Configuration ──────────────────────────────────────────────────────────

/// Structured logging configuration.
#[derive(Debug, Clone)]
pub struct LoggingConfig {
    pub level: String,
    pub format: LogFormat,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum LogFormat {
    #[default]
    Pretty,
    Json,
}

// ── Log buffer ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    pub target: String,
    pub message: String,
}

pub struct LogBuffer {
    entries: Mutex<VecDeque<LogEntry>>,
    capacity: usize,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            entries: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        })
    }

    fn push(&self, entry: LogEntry) {
        let mut entries = self.entries.lock().unwrap();
        if entries.len() >= self.capacity {
            entries.pop_front();
        }
        entries.push_back(entry);
    }

    /// Newest first, filtered; with counts so a capped page says it is capped.
    pub fn query(&self, f: &LogFilter, limit: usize) -> LogPage {
        let entries = self.entries.lock().unwrap();
        let mut matched = 0usize;
        let mut page = Vec::new();
        for e in entries.iter().rev() {
            if f.matches(e) {
                matched += 1;
                if page.len() < limit {
                    page.push(e.clone());
                }
            }
        }
        LogPage {
            entries: page,
            matched,
            buffered: entries.len(),
            capacity: self.capacity,
            oldest: entries.front().map(|e| e.timestamp.clone()),
        }
    }

    pub fn recent(&self, limit: usize, min_level: Option<&str>) -> Vec<LogEntry> {
        let entries = self.entries.lock().unwrap();
        let level_rank = |l: &str| match l {
            "ERROR" => 4,
            "WARN" => 3,
            "INFO" => 2,
            "DEBUG" => 1,
            _ => 0,
        };
        let min_rank = min_level
            .map(|l| level_rank(&l.to_uppercase()))
            .unwrap_or(0);
        entries
            .iter()
            .rev()
            .filter(|e| level_rank(&e.level) >= min_rank)
            .take(limit)
            .cloned()
            .collect()
    }
}

/// What `GET /api/logs` asks of the buffer.
#[derive(Debug, Default, Clone)]
pub struct LogFilter {
    /// Lowest level shown (`ERROR` > `WARN` > `INFO` > `DEBUG`).
    pub min_level: Option<String>,
    /// Keep only targets starting with one of these (empty: all).
    pub include: Vec<String>,
    /// Drop targets starting with one of these.
    pub exclude: Vec<String>,
    /// Routine-activity targets: shown only at WARN or above, so their
    /// failures stay visible while their chatter does not.
    pub quiet: Vec<String>,
    /// Case-insensitive text in the message or target.
    pub text: Option<String>,
    /// Only lines after this timestamp (RFC 3339, as the buffer stores them).
    pub after: Option<String>,
}

fn level_rank(l: &str) -> u8 {
    match l {
        "ERROR" => 4,
        "WARN" => 3,
        "INFO" => 2,
        "DEBUG" => 1,
        _ => 0,
    }
}

impl LogFilter {
    /// `target=a,b,-c` → include a and b, exclude c.
    pub fn parse_targets(spec: &str) -> (Vec<String>, Vec<String>) {
        let (mut inc, mut exc) = (Vec::new(), Vec::new());
        for t in spec.split(',').map(str::trim).filter(|t| !t.is_empty()) {
            match t.strip_prefix('-') {
                Some(x) if !x.is_empty() => exc.push(x.to_string()),
                Some(_) => {}
                None => inc.push(t.to_string()),
            }
        }
        (inc, exc)
    }

    pub fn matches(&self, e: &LogEntry) -> bool {
        let min = self
            .min_level
            .as_deref()
            .map(|l| level_rank(&l.to_uppercase()))
            .unwrap_or(0);
        if level_rank(&e.level) < min {
            return false;
        }
        if !self.include.is_empty()
            && !self
                .include
                .iter()
                .any(|p| e.target.starts_with(p.as_str()))
        {
            return false;
        }
        if self
            .exclude
            .iter()
            .any(|p| e.target.starts_with(p.as_str()))
        {
            return false;
        }
        if level_rank(&e.level) < 3 && self.quiet.iter().any(|p| e.target.starts_with(p.as_str())) {
            return false;
        }
        if let Some(a) = &self.after
            && e.timestamp.as_str() <= a.as_str()
        {
            return false;
        }
        if let Some(t) = &self.text {
            let t = t.to_lowercase();
            if !e.message.to_lowercase().contains(&t) && !e.target.to_lowercase().contains(&t) {
                return false;
            }
        }
        true
    }
}

/// A page of the buffer: the newest matches first, and what the buffer holds.
#[derive(Debug, Serialize)]
pub struct LogPage {
    pub entries: Vec<LogEntry>,
    /// Lines in the buffer matching the filter (the page may hold fewer).
    pub matched: usize,
    /// Lines the buffer holds now, and its capacity.
    pub buffered: usize,
    pub capacity: usize,
    /// The oldest line still in the buffer: nothing before it can be shown.
    pub oldest: Option<String>,
}

// ── Tracing layer that writes to the ring buffer ──────────────────────────────

struct RingBufferLayer {
    buffer: Arc<LogBuffer>,
}

struct MessageVisitor {
    message: String,
    extra: Vec<(String, String)>,
}

impl tracing::field::Visit for MessageVisitor {
    fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
        } else {
            self.extra
                .push((field.name().to_string(), value.to_string()));
        }
    }
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        let s = format!("{value:?}");
        if field.name() == "message" {
            self.message = s;
        } else {
            self.extra.push((field.name().to_string(), s));
        }
    }
    fn record_i64(&mut self, field: &tracing::field::Field, value: i64) {
        self.extra
            .push((field.name().to_string(), value.to_string()));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.extra
            .push((field.name().to_string(), value.to_string()));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.extra
            .push((field.name().to_string(), value.to_string()));
    }
}

impl<S: Subscriber + for<'a> LookupSpan<'a>> Layer<S> for RingBufferLayer {
    fn on_event(&self, event: &tracing::Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        let mut visitor = MessageVisitor {
            message: String::new(),
            extra: Vec::new(),
        };
        event.record(&mut visitor);

        let mut message = visitor.message;
        for (k, v) in visitor.extra {
            if !message.is_empty() {
                message.push(' ');
            }
            message.push_str(&format!("{k}={v}"));
        }

        self.buffer.push(LogEntry {
            timestamp: Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            level: meta.level().to_string().to_uppercase(),
            target: meta.target().to_string(),
            message,
        });
    }
}

// ── Public init functions ─────────────────────────────────────────────────────

/// Initialize logging, optionally capturing to an in-memory ring buffer.
///
/// IMPORTANT: Logs are written to stderr, not stdout.
/// This is critical for MCP stdio transport where stdout is reserved
/// for JSON-RPC protocol messages.
pub fn init(config: &LoggingConfig) {
    init_with_buffer(config, None);
}

pub fn init_with_buffer(config: &LoggingConfig, buffer: Option<Arc<LogBuffer>>) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.level));

    let subscriber = tracing_subscriber::registry().with(env_filter);

    macro_rules! maybe_buffer {
        ($sub:expr) => {
            if let Some(buf) = buffer {
                $sub.with(RingBufferLayer { buffer: buf }).init();
            } else {
                $sub.init();
            }
        };
    }

    match config.format {
        LogFormat::Json => {
            let sub = subscriber.with(
                fmt::layer()
                    .with_writer(io::stderr)
                    .json()
                    .with_span_events(FmtSpan::CLOSE)
                    .with_target(true)
                    .with_thread_ids(true),
            );
            maybe_buffer!(sub);
        }
        LogFormat::Pretty => {
            let sub = subscriber.with(
                fmt::layer()
                    .with_writer(io::stderr)
                    .pretty()
                    .with_span_events(FmtSpan::CLOSE)
                    .with_target(true),
            );
            maybe_buffer!(sub);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(level: &str, target: &str, msg: &str, ts: &str) -> LogEntry {
        LogEntry {
            timestamp: ts.into(),
            level: level.into(),
            target: target.into(),
            message: msg.into(),
        }
    }

    #[test]
    fn filters_level_targets_text_and_time() {
        let (inc, exc) =
            LogFilter::parse_targets("agent_brain::services, -agent_brain::services::scheduler,-");
        assert_eq!(inc, vec!["agent_brain::services"]);
        assert_eq!(exc, vec!["agent_brain::services::scheduler"]);
        let f = LogFilter {
            min_level: Some("info".into()),
            include: inc,
            exclude: exc,
            text: Some("LEASE".into()),
            after: Some("2026-10-08T10:00:00Z".into()),
            ..Default::default()
        };
        assert!(f.matches(&e(
            "WARN",
            "agent_brain::services::gpu_lease",
            "GPU lease expired",
            "2026-10-08T11:00:00Z"
        )));
        assert!(
            !f.matches(&e(
                "DEBUG",
                "agent_brain::services::gpu_lease",
                "lease",
                "2026-10-08T11:00:00Z"
            )),
            "below level"
        );
        assert!(
            !f.matches(&e(
                "INFO",
                "agent_brain::services::scheduler",
                "lease",
                "2026-10-08T11:00:00Z"
            )),
            "excluded target"
        );
        assert!(
            !f.matches(&e("INFO", "neo4rs::pool", "lease", "2026-10-08T11:00:00Z")),
            "not an included target"
        );
        assert!(
            !f.matches(&e(
                "INFO",
                "agent_brain::services::gpu_lease",
                "lease",
                "2026-10-08T09:00:00Z"
            )),
            "before `after`"
        );
        assert!(
            !f.matches(&e(
                "INFO",
                "agent_brain::services::queue",
                "job done",
                "2026-10-08T11:00:00Z"
            )),
            "no text match"
        );
    }

    #[test]
    fn quiet_targets_keep_only_their_warnings() {
        let f = LogFilter {
            quiet: vec!["agent_brain::services::scheduler".into()],
            ..Default::default()
        };
        assert!(!f.matches(&e("INFO", "agent_brain::services::scheduler", "tick", "t")));
        assert!(f.matches(&e(
            "WARN",
            "agent_brain::services::scheduler",
            "auto-paused",
            "t"
        )));
        assert!(f.matches(&e(
            "INFO",
            "agent_brain::services::gpu_lease",
            "released",
            "t"
        )));
    }

    #[test]
    fn a_page_counts_every_match_and_reports_the_horizon() {
        let b = LogBuffer::new(3);
        for i in 0..5 {
            b.push(e(
                "INFO",
                "t",
                &format!("m{i}"),
                &format!("2026-10-08T10:0{i}:00Z"),
            ));
        }
        let p = b.query(&LogFilter::default(), 2);
        assert_eq!(
            p.entries
                .iter()
                .map(|e| e.message.as_str())
                .collect::<Vec<_>>(),
            vec!["m4", "m3"]
        );
        assert_eq!((p.matched, p.buffered, p.capacity), (3, 3, 3));
        assert_eq!(p.oldest.as_deref(), Some("2026-10-08T10:02:00Z"));
    }
}
