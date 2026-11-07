//! Log setup shared by every binary.
//!
//! `RUST_LOG` picks what is logged (`default_filter` when unset). `LOG_FORMAT=json` writes one
//! JSON object per line (for Loki, Elasticsearch, Cloud Logging and other collectors); anything
//! else, or unset, keeps the human-readable text format.

use tracing_subscriber::EnvFilter;

/// Environment variable that selects the log format.
pub const FORMAT_ENV: &str = "LOG_FORMAT";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Text,
    Json,
}

impl Format {
    pub fn parse(s: &str) -> Self {
        if s.trim().eq_ignore_ascii_case("json") { Format::Json } else { Format::Text }
    }

    pub fn from_env() -> Self {
        std::env::var(FORMAT_ENV).map(|s| Self::parse(&s)).unwrap_or(Format::Text)
    }
}

/// Install the global subscriber. `with_target` shows the module path (`kube::runtime`) in text
/// logs; JSON logs always include it.
pub fn init(default_filter: &str, with_target: bool) {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| default_filter.into());
    let builder = tracing_subscriber::fmt().with_env_filter(filter);
    match Format::from_env() {
        Format::Json => builder.json().flatten_event(true).with_current_span(false).init(),
        Format::Text => builder.with_target(with_target).init(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats() {
        assert_eq!(Format::parse("json"), Format::Json);
        assert_eq!(Format::parse(" JSON "), Format::Json);
        assert_eq!(Format::parse("text"), Format::Text);
        assert_eq!(Format::parse(""), Format::Text);
        assert_eq!(Format::parse("yaml"), Format::Text);
    }
}
