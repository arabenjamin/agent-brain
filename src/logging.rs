use tracing_subscriber::{
    EnvFilter,
    fmt::{self, format::FmtSpan},
    layer::SubscriberExt,
    util::SubscriberInitExt,
};

use crate::config::{Config, LogFormat};

/// Initialize the tracing subscriber for logging.
pub fn init(config: &Config) {
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(&config.log_level));

    let subscriber = tracing_subscriber::registry().with(env_filter);

    match config.log_format {
        LogFormat::Json => {
            subscriber
                .with(
                    fmt::layer()
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
                        .pretty()
                        .with_span_events(FmtSpan::CLOSE)
                        .with_target(true),
                )
                .init();
        }
    }
}

/// Initialize logging with defaults (for tests or simple usage).
pub fn init_default() {
    let _ = tracing_subscriber::registry()
        .with(EnvFilter::new("info"))
        .with(fmt::layer().pretty())
        .try_init();
}
