//! Output plumbing shared by every command.
//!
//! Isolera prints either a human-readable table or a JSON document. The JSON is
//! wrapped in a small envelope so a provisioning script can tell one document from
//! another and refuse a version it does not understand, instead of parsing a bare
//! object and hoping:
//!
//! ```json
//! {"schema":1,"kind":"plan","node":"matching-engine-01","version":"1.0.0","data":{}}
//! ```

use crate::error::{Error, Kind, Result};

/// JSON schema version for every document Isolera emits.
pub const JSON_SCHEMA: u32 = 1;

/// How a command should render its result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// Aligned tables for a terminal or a runbook.
    Text,
    /// A machine-readable JSON document.
    Json,
}

impl Format {
    /// Parse an `--output` value.
    pub fn parse(text: &str) -> Result<Format> {
        match text.trim().to_ascii_lowercase().as_str() {
            "text" | "table" | "human" => Ok(Format::Text),
            "json" => Ok(Format::Json),
            other => Err(Error::new(
                Kind::Usage,
                format!("unknown output format {other:?}; expected \"text\" or \"json\""),
            )),
        }
    }

    /// The flag name, for messages.
    pub fn as_str(self) -> &'static str {
        match self {
            Format::Text => "text",
            Format::Json => "json",
        }
    }

    /// True for JSON output.
    pub fn is_json(self) -> bool {
        self == Format::Json
    }
}

/// Choose between the text and JSON rendering of a result.
pub fn render(format: Format, text: String, json: String) -> String {
    match format {
        Format::Text => text,
        Format::Json => json,
    }
}

/// Wrap a command's JSON payload in the Isolera envelope.
pub fn envelope(kind: &str, node: &str, payload: &str) -> String {
    format!(
        "{{\"schema\":{JSON_SCHEMA},\"kind\":{},\"node\":{},\"version\":{},\"data\":{payload}}}",
        crate::sysroot::json_string(kind),
        crate::sysroot::json_string(node),
        crate::sysroot::json_string(crate::VERSION),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_parse_with_aliases() {
        assert_eq!(Format::parse("text").unwrap(), Format::Text);
        assert_eq!(Format::parse("TABLE").unwrap(), Format::Text);
        assert_eq!(Format::parse("human").unwrap(), Format::Text);
        assert_eq!(Format::parse(" json ").unwrap(), Format::Json);
        assert!(Format::Json.is_json());
        assert!(!Format::Text.is_json());
        assert_eq!(Format::Json.as_str(), "json");

        let err = Format::parse("yaml").unwrap_err();
        assert_eq!(err.kind(), Kind::Usage);
        assert!(err.message().contains("yaml"), "{}", err.message());
    }

    #[test]
    fn render_picks_the_right_body() {
        assert_eq!(render(Format::Text, "a".into(), "{}".into()), "a");
        assert_eq!(render(Format::Json, "a".into(), "{}".into()), "{}");
    }

    #[test]
    fn the_envelope_is_valid_and_versioned() {
        let document = envelope("plan", "test-node", "{\"writes\":0}");
        assert!(document.starts_with("{\"schema\":1,\"kind\":\"plan\",\"node\":\"test-node\""));
        assert!(document.ends_with("\"data\":{\"writes\":0}}"));
        assert!(document.contains(&format!("\"version\":\"{}\"", crate::VERSION)));
        assert_eq!(document.matches('{').count(), document.matches('}').count());

        let quoted = envelope("verify", "a\"b", "{}");
        assert!(quoted.contains("\"node\":\"a\\\"b\""), "{quoted}");
    }
}
