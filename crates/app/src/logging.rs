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

use crate::config::{Config, LogFormat};

// ── Log buffer ────────────────────────────────────────────────────────────────

#[derive(Clone, Serialize)]
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

    pub fn recent(&self, limit: usize, min_level: Option<&str>) -> Vec<LogEntry> {
        let entries = self.entries.lock().unwrap();
        let level_rank = |l: &str| match l {
            "ERROR" => 4,
            "WARN" => 3,
            "INFO" => 2,
            "DEBUG" => 1,
            _ => 0,
        };
        let min_rank = min_level.map(|l| level_rank(&l.to_uppercase())).unwrap_or(0);
        entries
            .iter()
            .rev()
            .filter(|e| level_rank(&e.level) >= min_rank)
            .take(limit)
            .cloned()
            .collect()
    }
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
            self.extra.push((field.name().to_string(), value.to_string()));
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
        self.extra.push((field.name().to_string(), value.to_string()));
    }
    fn record_u64(&mut self, field: &tracing::field::Field, value: u64) {
        self.extra.push((field.name().to_string(), value.to_string()));
    }
    fn record_bool(&mut self, field: &tracing::field::Field, value: bool) {
        self.extra.push((field.name().to_string(), value.to_string()));
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
pub fn init(config: &Config) {
    init_with_buffer(config, None);
}

pub fn init_with_buffer(config: &Config, buffer: Option<Arc<LogBuffer>>) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

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

    match config.logging.format {
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

/// Initialize logging with defaults (for tests or simple usage).
/// Logs go to stderr to avoid interfering with stdout.
pub fn init_default() {
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new("info"))
        .with(fmt::layer().with_writer(io::stderr).pretty())
        .try_init();
}
