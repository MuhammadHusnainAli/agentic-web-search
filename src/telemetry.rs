//! Structured logging setup.

use tracing_subscriber::{EnvFilter, fmt, prelude::*};

use crate::config::LogFormat;

/// Install the global tracing subscriber. `RUST_LOG` overrides the default filter.
pub fn init(format: &LogFormat) {
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,tower_http=info,chromiumoxide=error,hyper=warn"));
    let registry = tracing_subscriber::registry().with(filter);
    match format {
        LogFormat::Json => registry.with(fmt::layer().json().flatten_event(true)).init(),
        LogFormat::Pretty => registry.with(fmt::layer().compact()).init(),
    }
}
