//! Error type shared by every Isolera module.
//!
//! A single error enum keeps the call sites free of conversion boilerplate while
//! still letting the CLI map failures onto distinct exit codes, which matters
//! when the suite is driven from a provisioning script or a CI job.

use std::fmt;
use std::io;

/// Category of failure; also determines the process exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// Bad command line or flag combination.
    Usage,
    /// The configuration file is missing, malformed or inconsistent.
    Config,
    /// Filesystem or sysfs/procfs I/O failed.
    Io,
    /// A required path/node does not exist on this host.
    NotFound,
    /// Operation requires root (or CAP_SYS_NICE / CAP_SYS_ADMIN).
    NotRoot,
    /// The host cannot support the requested operation (wrong kernel/arch).
    Unsupported,
    /// A verification threshold was exceeded.
    Verify,
    /// Internal invariant violated; always a bug in Isolera.
    Internal,
}

impl Kind {
    /// Process exit code associated with this failure category.
    pub fn exit_code(self) -> i32 {
        match self {
            Kind::Internal => 1,
            Kind::Usage => 2,
            Kind::Config => 3,
            Kind::Io => 4,
            Kind::NotRoot => 5,
            Kind::NotFound => 6,
            Kind::Unsupported => 7,
            Kind::Verify => 8,
        }
    }

    /// Short machine-readable label used in JSON reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Usage => "usage",
            Kind::Config => "config",
            Kind::Io => "io",
            Kind::NotFound => "not_found",
            Kind::NotRoot => "not_root",
            Kind::Unsupported => "unsupported",
            Kind::Verify => "verify",
            Kind::Internal => "internal",
        }
    }
}

/// The error type used throughout the crate.
#[derive(Debug, Clone)]
pub struct Error {
    kind: Kind,
    message: String,
}

impl Error {
    /// Build an error of `kind` with a formatted message.
    pub fn new(kind: Kind, message: impl Into<String>) -> Self {
        Error {
            kind,
            message: message.into(),
        }
    }

    /// Failure category.
    pub fn kind(&self) -> Kind {
        self.kind
    }

    /// Human-readable description.
    pub fn message(&self) -> &str {
        &self.message
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for Error {}

impl From<io::Error> for Error {
    fn from(err: io::Error) -> Self {
        Error::new(Kind::Io, err.to_string())
    }
}

impl From<String> for Error {
    fn from(message: String) -> Self {
        Error::new(Kind::Internal, message)
    }
}

impl From<&str> for Error {
    fn from(message: &str) -> Self {
        Error::new(Kind::Internal, message)
    }
}

/// Convenience alias used across the crate.
pub type Result<T> = std::result::Result<T, Error>;

/// Build a [`Kind::Usage`] error.
pub fn usage<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::new(Kind::Usage, message))
}

/// Build a [`Kind::Config`] error.
pub fn config<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::new(Kind::Config, message))
}

/// Build a [`Kind::NotFound`] error.
pub fn missing<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::new(Kind::NotFound, message))
}

/// Build a [`Kind::Unsupported`] error.
pub fn unsupported<T>(message: impl Into<String>) -> Result<T> {
    Err(Error::new(Kind::Unsupported, message))
}

/// Wrap an I/O failure with the path that caused it.
pub fn io_context(path: &str, err: &io::Error) -> Error {
    Error::new(Kind::Io, format!("{path}: {err}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_codes_are_distinct() {
        let kinds = [
            Kind::Internal,
            Kind::Usage,
            Kind::Config,
            Kind::Io,
            Kind::NotRoot,
            Kind::NotFound,
            Kind::Unsupported,
            Kind::Verify,
        ];
        let mut codes: Vec<i32> = kinds.iter().map(|k| k.exit_code()).collect();
        codes.sort_unstable();
        codes.dedup();
        assert_eq!(codes.len(), kinds.len());
    }

    #[test]
    fn io_errors_convert_with_io_kind() {
        let err: Error = io::Error::new(io::ErrorKind::NotFound, "nope").into();
        assert_eq!(err.kind(), Kind::Io);
    }

    #[test]
    fn usage_helper_sets_kind() {
        let err = usage::<()>("bad flag").unwrap_err();
        assert_eq!(err.kind(), Kind::Usage);
        assert_eq!(err.message(), "bad flag");
    }
}
