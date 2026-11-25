//! One tracing setup for every binary.
//!
//! Filter with `VANTA_LOG` (falls back to `RUST_LOG`), e.g.
//! `VANTA_LOG=vanta_loader=trace,info`. Debug and trace output exist
//! only in builds with the `diagnostics` feature; a plain release build
//! logs info and above whatever the filter asks for.

use tracing_subscriber::EnvFilter;
use tracing_subscriber::filter::LevelFilter;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

/// The most verbose level this build can log.
const CAP: LevelFilter = if cfg!(feature = "diagnostics") {
    LevelFilter::TRACE
} else {
    LevelFilter::INFO
};

/// The directive filter for this run: `VANTA_LOG`, then `RUST_LOG`, then
/// `default`; a value that does not parse falls back to `default`.
fn filter_from(default: &str, value: Option<&str>) -> EnvFilter {
    value
        .and_then(|v| EnvFilter::try_new(v).ok())
        .unwrap_or_else(|| EnvFilter::new(default))
}

pub fn init(default_level: &str) {
    let filter = filter_from(
        default_level,
        std::env::var("VANTA_LOG")
            .or_else(|_| std::env::var("RUST_LOG"))
            .ok()
            .as_deref(),
    );
    let _ = tracing_subscriber::registry()
        .with(CAP)
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .with_target(true)
                // Time since the process started: a monotonic clock, like a
                // console's own boot time. The `time` feature provides it.
                .with_timer(tracing_subscriber::fmt::time::Uptime::default())
                .with_line_number(true)
                .with_writer(std::io::stderr),
        )
        .try_init();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_filter_that_does_not_parse_falls_back_to_the_default() {
        let f = filter_from("warn", Some("vanta_loader[=trace"));
        assert_eq!(f.max_level_hint(), Some(LevelFilter::WARN));
    }

    #[test]
    fn a_good_filter_is_honoured() {
        let f = filter_from("info", Some("vanta_loader=trace"));
        assert_eq!(f.max_level_hint(), Some(LevelFilter::TRACE));
    }

    #[test]
    fn no_filter_uses_the_default_level() {
        let f = filter_from("debug", None);
        assert_eq!(f.max_level_hint(), Some(LevelFilter::DEBUG));
    }
}
