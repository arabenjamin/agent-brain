use std::io;

use tracing_subscriber::{
    EnvFilter,
    fmt::{self, format::FmtSpan},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

use crate::config::{Config, LogFormat};

/// Initialize the tracing subscriber for logging.
///
/// IMPORTANT: Logs are written to stderr, not stdout.
/// This is critical for MCP stdio transport where stdout is reserved
/// for JSON-RPC protocol messages.
pub fn init(config: &Config) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.logging.level));

    let subscriber = tracing_subscriber::registry().with(env_filter);

    match config.logging.format {
        LogFormat::Json => {
            subscriber
                .with(
                    fmt::layer()
                        .with_writer(io::stderr)
                        .json()
                        .with_span_events(FmtSpan::CLOSE)
                        .with_target(true)
                        .with_thread_ids(true),
                )
                .init();
        }
        LogFormat::Pretty => {
            subscriber
                .with(
                    fmt::layer()
                        .with_writer(io::stderr)
                        .pretty()
                        .with_span_events(FmtSpan::CLOSE)
                        .with_target(true),
                )
                .init();
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
