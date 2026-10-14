//! Diagnostics logging.
//!
//! Progress, warnings and errors go to stderr so that stdout stays reserved for
//! machine-consumable output (`--json`, rendered tables). Colour is applied only
//! when stderr is a terminal and `NO_COLOR` is unset.

use std::io::{IsTerminal, Write};

/// Verbosity threshold.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    /// Warnings and errors only.
    Warn,
    /// Normal operational output.
    Info,
    /// Extra detail useful when diagnosing a node.
    Debug,
    /// Everything, including per-path decisions.
    Trace,
}

/// ANSI escape used for a given prefix.
#[derive(Debug, Clone, Copy)]
enum Style {
    Red,
    Yellow,
    Green,
    Blue,
    Gray,
}

impl Style {
    fn code(self) -> &'static str {
        match self {
            Style::Red => "\x1b[31m",
            Style::Yellow => "\x1b[33m",
            Style::Green => "\x1b[32m",
            Style::Blue => "\x1b[34m",
            Style::Gray => "\x1b[90m",
        }
    }
}

/// Logger handle passed by reference through the engine.
#[derive(Debug)]
pub struct Log {
    level: Level,
    color: bool,
    /// Suppresses everything except errors, used by `--quiet`.
    silent: bool,
}

impl Log {
    /// Build a logger honouring `NO_COLOR` and terminal detection.
    pub fn new(level: Level, silent: bool) -> Log {
        let color = std::env::var_os("NO_COLOR").is_none()
            && std::env::var("TERM").map(|t| t != "dumb").unwrap_or(true)
            && std::io::stderr().is_terminal();
        Log {
            level,
            color,
            silent,
        }
    }

    /// A logger that prints nothing but errors.
    pub fn quiet() -> Log {
        Log {
            level: Level::Warn,
            color: false,
            silent: false,
        }
    }

    /// Current verbosity threshold.
    pub fn level(&self) -> Level {
        self.level
    }

    fn emit(&self, level: Level, style: Style, tag: &str, message: &str) {
        if level > self.level {
            return;
        }
        let mut stderr = std::io::stderr().lock();
        if self.color {
            let code = style.code();
            let _ = writeln!(stderr, "{code}[{tag}]\x1b[0m {message}");
        } else {
            let _ = writeln!(stderr, "[{tag}] {message}");
        }
    }

    /// A failure the operator must act on.
    pub fn error(&self, message: impl AsRef<str>) {
        if self.silent {
            return;
        }
        self.emit(Level::Warn, Style::Red, "error", message.as_ref());
    }

    /// Something suspicious that does not stop the run.
    pub fn warn(&self, message: impl AsRef<str>) {
        if self.silent {
            return;
        }
        self.emit(Level::Warn, Style::Yellow, "warn", message.as_ref());
    }

    /// Progress or result information.
    pub fn info(&self, message: impl AsRef<str>) {
        if self.silent {
            return;
        }
        self.emit(Level::Info, Style::Blue, "info", message.as_ref());
    }

    /// Confirmation that a step succeeded.
    pub fn ok(&self, message: impl AsRef<str>) {
        if self.silent {
            return;
        }
        self.emit(Level::Info, Style::Green, " ok ", message.as_ref());
    }

    /// Section banner for multi-step commands.
    pub fn step(&self, message: impl AsRef<str>) {
        if self.silent {
            return;
        }
        self.emit(Level::Info, Style::Gray, "step", message.as_ref());
    }

    /// Detail for the current step.
    pub fn detail(&self, message: impl AsRef<str>) {
        if self.silent {
            return;
        }
        self.emit(Level::Info, Style::Gray, " .. ", message.as_ref());
    }

    /// Verbose diagnostic output.
    pub fn debug(&self, message: impl AsRef<str>) {
        self.emit(Level::Debug, Style::Gray, "dbg ", message.as_ref());
    }

    /// Per-path tracing.
    pub fn trace(&self, message: impl AsRef<str>) {
        self.emit(Level::Trace, Style::Gray, "trc ", message.as_ref());
    }
}

impl Default for Log {
    fn default() -> Self {
        Log::new(Level::Info, false)
    }
}

/// Shorten `text` to at most `max` characters, marking elision with an ellipsis.
pub fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    let count = text.chars().count();
    if count <= max {
        return text.to_string();
    }
    if max <= 3 {
        return text.chars().take(max).collect();
    }
    let mut out: String = text.chars().take(max - 3).collect();
    out.push_str("...");
    out
}

/// Collapse a value onto one line, for a table cell.
///
/// File contents (a grub file, a journal) and reasons can be multi-line; dropping
/// them into a cell would break the layout of everything around them, so the text is
/// flattened first and then elided.
pub fn flatten(text: &str, max: usize) -> String {
    let joined: Vec<&str> = text.split_whitespace().collect();
    truncate(&joined.join(" "), max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_are_ordered() {
        assert!(Level::Warn < Level::Info);
        assert!(Level::Info < Level::Debug);
        assert!(Level::Debug < Level::Trace);
    }

    #[test]
    fn truncate_keeps_short_strings() {
        assert_eq!(truncate("eth0-rx-0", 20), "eth0-rx-0");
        assert_eq!(truncate("", 4), "");
    }

    #[test]
    fn truncate_marks_elision_within_budget() {
        let out = truncate("nvme0q12-completion", 10);
        assert_eq!(out.chars().count(), 10);
        assert!(out.ends_with("..."));
    }

    #[test]
    fn truncate_handles_tiny_budgets() {
        assert_eq!(truncate("abcdef", 2), "ab");
        assert_eq!(truncate("abcdef", 0), "");
    }

    #[test]
    fn truncate_is_multibyte_safe() {
        assert_eq!(truncate("caf\u{e9}", 8), "caf\u{e9}");
        assert_eq!(
            truncate("caf\u{e9}\u{e9}\u{e9}\u{e9}", 5).chars().count(),
            5
        );
    }

    #[test]
    fn flatten_collapses_file_contents_into_one_line() {
        assert_eq!(flatten("quiet\nisolcpus=2-3\n", 40), "quiet isolcpus=2-3");
        assert_eq!(flatten("  a   b  ", 10), "a b");
        assert_eq!(flatten("", 10), "");
        let long = flatten(
            "# comment\nGRUB_TIMEOUT=5\nGRUB_CMDLINE_LINUX=\"quiet\"\n",
            12,
        );
        assert_eq!(long.chars().count(), 12);
        assert!(long.ends_with("..."), "{long}");
        assert!(!long.contains('\n'));
    }
}
