//! The write journal: what `apply` did, and what `rollback` has to undo.
//!
//! The journal is a TOML document under `paths.state_dir`. TOML rather than JSON
//! because the suite already owns a reader and writer for it, so the journal costs
//! no new parser - and because it stays greppable next to the config that produced
//! it:
//!
//! ```toml
//! [journal]
//! version = 1
//! started_at = 1760000000
//! node = "matching-engine-01"
//! source = "/etc/isolera.toml"
//!
//! [[journal.records]]
//! path = "/proc/irq/124/smp_affinity_list"
//! value = "2,3"
//! previous = "0-3\n"
//! applied = true
//! reason = "isolera: irq overrides pattern matching-engine-*"
//! ```

use crate::config::Config;
use crate::error::{Error, Kind, Result};
use crate::sysroot::{SysRoot, WriteRecord};
use crate::toml::{self, Table, Value};

/// File name inside `paths.state_dir`.
pub const JOURNAL_FILE: &str = "journal.toml";

/// Journal format version; a file from another version is rejected, not guessed.
pub const VERSION: i64 = 1;

/// One apply run's worth of writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Journal {
    /// Format version of the file that was read (always [`VERSION`] when written).
    pub version: i64,
    /// When the run started, in Unix seconds.
    pub started_at: u64,
    /// When the run was rolled back, if it has been.
    pub rolled_back_at: Option<u64>,
    /// Node label the run targeted.
    pub node: String,
    /// Configuration file the run used.
    pub source: String,
    /// Every write, in the order it happened.
    pub records: Vec<WriteRecord>,
}

impl Journal {
    /// An empty journal for a new run.
    pub fn new(node: &str, source: &str) -> Journal {
        Journal {
            version: VERSION,
            started_at: crate::grub::now_seconds(),
            rolled_back_at: None,
            node: node.to_string(),
            source: source.to_string(),
            records: Vec::new(),
        }
    }

    /// Logical path of the journal for a plan.
    pub fn path(cfg: &Config) -> String {
        format!(
            "{}/{}",
            cfg.paths.state_dir.trim_end_matches('/'),
            JOURNAL_FILE
        )
    }

    /// Append a record.
    pub fn push(&mut self, record: WriteRecord) {
        self.records.push(record);
    }

    /// Append several records.
    pub fn extend(&mut self, records: &[WriteRecord]) {
        self.records.extend(records.iter().cloned());
    }

    /// True when there is nothing to undo.
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    /// Records rollback may act on, newest first.
    ///
    /// Newest first matters: two writes to the same path must be undone in reverse
    /// order, or the first restore would be clobbered by the second.
    pub fn restorable(&self) -> Vec<&WriteRecord> {
        self.records.iter().rev().collect()
    }

    /// True when the run has already been rolled back.
    pub fn rolled_back(&self) -> bool {
        self.rolled_back_at.is_some()
    }

    /// Render the journal as TOML.
    pub fn render(&self) -> String {
        let mut rows: Vec<Value> = Vec::new();
        for record in &self.records {
            let mut row: Table = Table::new();
            row.insert("path".to_string(), Value::String(record.path.clone()));
            row.insert("value".to_string(), Value::String(record.value.clone()));
            if let Some(previous) = &record.previous {
                row.insert("previous".to_string(), Value::String(previous.clone()));
            }
            row.insert("reason".to_string(), Value::String(record.reason.clone()));
            row.insert("applied".to_string(), Value::Bool(record.applied));
            row.insert("created".to_string(), Value::Bool(record.created));
            row.insert("reversible".to_string(), Value::Bool(record.reversible));
            row.insert("unchanged".to_string(), Value::Bool(record.unchanged));
            rows.push(Value::Table(row));
        }

        let mut journal: Table = Table::new();
        journal.insert("version".to_string(), Value::Integer(self.version));
        journal.insert(
            "started_at".to_string(),
            Value::Integer(self.started_at as i64),
        );
        journal.insert("node".to_string(), Value::String(self.node.clone()));
        journal.insert("source".to_string(), Value::String(self.source.clone()));
        if let Some(at) = self.rolled_back_at {
            journal.insert("rolled_back_at".to_string(), Value::Integer(at as i64));
        }
        if !rows.is_empty() {
            journal.insert("records".to_string(), Value::Array(rows));
        }

        let mut root: Table = Table::new();
        root.insert("journal".to_string(), Value::Table(journal));
        toml::to_document(&root)
    }

    /// Parse a journal, failing loudly on anything unexpected.
    pub fn parse(text: &str) -> Result<Journal> {
        let root = toml::parse(text)?;
        let table = root
            .get("journal")
            .and_then(Value::as_table)
            .ok_or_else(|| malformed("missing the [journal] table"))?;
        let version = table
            .get("version")
            .and_then(Value::as_int)
            .unwrap_or(VERSION);
        if version != VERSION {
            return Err(malformed(format!(
                "version {version} was written by another Isolera build (this one writes {VERSION})"
            )));
        }
        let mut journal = Journal {
            version,
            started_at: table
                .get("started_at")
                .and_then(Value::as_int)
                .unwrap_or(0)
                .max(0) as u64,
            rolled_back_at: table
                .get("rolled_back_at")
                .and_then(Value::as_int)
                .filter(|at| *at > 0)
                .map(|at| at as u64),
            node: string_field(table, "node"),
            source: string_field(table, "source"),
            records: Vec::new(),
        };
        if let Some(Value::Array(rows)) = table.get("records") {
            for row in rows {
                let row = row
                    .as_table()
                    .ok_or_else(|| malformed("a record is not a table"))?;
                let path = string_field(row, "path");
                if path.is_empty() {
                    return Err(malformed("a record has no path"));
                }
                journal.records.push(WriteRecord {
                    path,
                    value: string_field(row, "value"),
                    previous: match row.get("previous") {
                        Some(Value::String(text)) => Some(text.clone()),
                        _ => None,
                    },
                    reason: string_field(row, "reason"),
                    applied: bool_field(row, "applied"),
                    created: bool_field(row, "created"),
                    reversible: bool_field(row, "reversible"),
                    unchanged: bool_field(row, "unchanged"),
                });
            }
        }
        Ok(journal)
    }
}

/// A malformed-journal error.
fn malformed(detail: impl AsRef<str>) -> Error {
    Error::new(
        Kind::Io,
        format!("journal is unusable: {}", detail.as_ref()),
    )
}

fn string_field(table: &Table, key: &str) -> String {
    table
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

fn bool_field(table: &Table, key: &str) -> bool {
    table.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Read the journal for a plan, if the node has one.
pub fn load(sysroot: &SysRoot, cfg: &Config) -> Result<Option<Journal>> {
    match sysroot.read_optional(&Journal::path(cfg)) {
        Some(text) => Journal::parse(&text).map(Some),
        None => Ok(None),
    }
}

/// Write the journal out.
///
/// The file is written through the sysroot like any other, but the record of its
/// own write is appended *after* the content is rendered, so the journal never
/// lists itself: rolling back would otherwise have to delete its own input.
pub fn save(sysroot: &SysRoot, cfg: &Config, journal: &Journal) -> Result<Option<WriteRecord>> {
    let path = Journal::path(cfg);
    let text = journal.render();
    let reason = format!("isolera: write journal for {}", cfg.source);
    sysroot.write(&path, &text, &reason)
}

/// The journal file is Isolera's own bookkeeping, never a target of rollback.
pub fn is_journal_path(cfg: &Config, path: &str) -> bool {
    path == Journal::path(cfg)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    fn record(path: &str, value: &str, previous: Option<&str>) -> WriteRecord {
        WriteRecord {
            path: path.to_string(),
            value: value.to_string(),
            previous: previous.map(str::to_string),
            reason: "isolera: test".to_string(),
            applied: true,
            created: previous.is_none(),
            reversible: true,
            unchanged: false,
        }
    }

    #[test]
    fn journal_round_trips_through_toml() {
        let mut journal = Journal::new("test-node", "/etc/isolera.toml");
        journal.started_at = 1_760_000_000;
        journal.push(record(
            "/proc/irq/124/smp_affinity_list",
            "2,3",
            Some("0-3\n"),
        ));
        journal.push(record("/proc/sys/kernel/watchdog", "0", Some("1\n")));

        let text = journal.render();
        assert!(text.starts_with("[journal]\n"), "{text}");
        assert!(text.contains("[[journal.records]]"));
        assert!(
            text.contains("previous = \"0-3\\n\""),
            "values keep their newlines: {text}"
        );

        let parsed = Journal::parse(&text).unwrap();
        assert_eq!(parsed, journal);
        assert_eq!(parsed.records[0].previous.as_deref(), Some("0-3\n"));
        assert!(parsed.records[0].reversible);
        assert_eq!(parsed.node, "test-node");
        assert_eq!(
            Journal::parse(&parsed.render()).unwrap(),
            journal,
            "render is stable"
        );
    }

    #[test]
    fn an_empty_journal_round_trips() {
        let journal = Journal::new("test-node", "fixture.toml");
        let parsed = Journal::parse(&journal.render()).unwrap();
        assert!(parsed.is_empty());
        assert_eq!(parsed.records.len(), 0);
        assert!(!parsed.rolled_back());
    }

    #[test]
    fn a_journal_from_another_version_is_rejected() {
        let err = Journal::parse("[journal]\nversion = 99\n").unwrap_err();
        assert!(err.message().contains("version 99"), "{}", err.message());
        assert!(
            err.message().contains("another Isolera"),
            "{}",
            err.message()
        );
        assert!(Journal::parse("nothing here").is_err());
        assert!(
            Journal::parse("[journal]\nrecords = 3\n").is_ok(),
            "no records means nothing to undo"
        );
    }

    #[test]
    fn rollback_order_is_newest_first() {
        let mut journal = Journal::new("node", "cfg");
        journal.push(record("/a", "1", Some("0")));
        journal.push(record("/a", "2", Some("1")));
        let order: Vec<&str> = journal
            .restorable()
            .iter()
            .map(|r| r.value.as_str())
            .collect();
        assert_eq!(order, vec!["2", "1"], "the last write is undone first");
    }

    #[test]
    fn save_and_load_go_through_the_sysroot() {
        let node = testing::node();
        let cfg = testing::config();
        let mut journal = Journal::new("test-node", &cfg.source);
        journal.push(record("/proc/sys/kernel/watchdog", "0", Some("1\n")));
        save(&node.sysroot, &cfg, &journal).unwrap();

        assert_eq!(Journal::path(&cfg), "/var/lib/isolera/state/journal.toml");
        let loaded = load(&node.sysroot, &cfg).unwrap().unwrap();
        assert_eq!(loaded.records.len(), 1);
        assert!(is_journal_path(&cfg, "/var/lib/isolera/state/journal.toml"));
        assert!(!is_journal_path(&cfg, "/proc/sys/kernel/watchdog"));

        // A second save with identical content is a no-op.
        assert!(save(&node.sysroot, &cfg, &loaded).unwrap().is_none());
    }

    #[test]
    fn a_missing_journal_is_not_an_error() {
        let node = testing::node();
        let cfg = testing::config();
        assert!(load(&node.sysroot, &cfg).unwrap().is_none());
        assert!(load(&node.dry, &cfg).unwrap().is_none());
    }
}
