use crate::config::settings::LoggingSettings;
use crate::types::TraceId;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::Path;
use tracing::Level;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Initializes dual-output logging: human-readable stdout and optional JSON file.
pub fn init_logging(settings: &LoggingSettings) -> io::Result<()> {
    let log_level = parse_log_level(&settings.level);
    let env_filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(log_level.as_str()));

    let stdout_layer = tracing_subscriber::fmt::layer()
        .with_target(true)
        .with_thread_ids(true)
        .with_span_events(FmtSpan::NEW | FmtSpan::CLOSE)
        .with_filter(env_filter.clone());

    let registry = tracing_subscriber::registry().with(stdout_layer);

    if settings.json_log_enabled {
        let log_path = Path::new(&settings.log_file);
        if let Some(parent) = log_path.parent() {
            fs::create_dir_all(parent)?;
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;

        let json_layer = tracing_subscriber::fmt::layer()
            .json()
            .with_writer(file)
            .with_current_span(false)
            .with_span_list(true)
            .with_filter(env_filter);

        registry.with(json_layer).init();
    } else {
        registry.init();
    }

    Ok(())
}

fn parse_log_level(level: &str) -> Level {
    match level.to_lowercase().as_str() {
        "error" => Level::ERROR,
        "warn" => Level::WARN,
        "debug" => Level::DEBUG,
        "trace" => Level::TRACE,
        _ => Level::INFO,
    }
}

/// Creates a tracing span with a correlation trace_id for downstream log correlation.
pub fn correlation_span(operation: &str) -> tracing::Span {
    let trace_id = TraceId::generate();
    tracing::info_span!(
        "operation",
        trace_id = %trace_id,
        operation = operation,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_log_level_handles_known_values() {
        assert_eq!(parse_log_level("ERROR"), Level::ERROR);
        assert_eq!(parse_log_level("debug"), Level::DEBUG);
        assert_eq!(parse_log_level("unknown"), Level::INFO);
    }
}
