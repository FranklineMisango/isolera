//! The single gateway between Isolera and the host filesystem.
//!
//! Every path the suite touches goes through [`SysRoot`], which gives three
//! properties that the rest of the code relies on:
//!
//! 1. **Root translation** - all paths are logical absolute paths such as
//!    ``/proc/cmdline``; the root prefix is applied on access. Pointing the root
//!    at ``tests/fixtures/sysroot`` makes the entire suite runnable on a laptop
//!    with no privileges, and at ``/`` on a real node.
//! 2. **Dry-run journaling** - `write` either performs the change or records the
//!    intended change, so `plan` and `apply` share one code path.
//! 3. **Rollback data** - each record keeps the previous value and whether it can
//!    be restored, which is what `isolera rollback` replays.

use std::cell::RefCell;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};

use crate::error::{Error, Kind, Result};

/// How a path is written back to the kernel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// ``/sys`` - write in place, kernel rejects temp+rename.
    Sysfs,
    /// ``/proc`` - write in place, values are live kernel state.
    Procfs,
    /// A regular file on disk; written atomically via temp-file + rename.
    Regular,
}

impl FileKind {
    /// Classify a logical path.
    pub fn of(logical: &str) -> FileKind {
        if logical.starts_with("/sys/") || logical == "/sys" {
            FileKind::Sysfs
        } else if logical.starts_with("/proc/") || logical == "/proc" {
            FileKind::Procfs
        } else {
            FileKind::Regular
        }
    }

    /// ``sysfs``/``procfs``/``file``, for reports.
    pub fn as_str(self) -> &'static str {
        match self {
            FileKind::Sysfs => "sysfs",
            FileKind::Procfs => "procfs",
            FileKind::Regular => "file",
        }
    }
}

/// One intended or completed change, also the rollback unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteRecord {
    /// Logical absolute path (no root prefix).
    pub path: String,
    /// Value Isolera wants.
    pub value: String,
    /// Content before the write, when the file could be read.
    pub previous: Option<String>,
    /// Human-readable justification, surfaced by `plan` and stored in the log.
    pub reason: String,
    /// False for dry-run records.
    pub applied: bool,
    /// True when the file did not exist before and was created.
    pub created: bool,
    /// Whether `rollback` may restore this path.
    pub reversible: bool,
    /// True when the file's contents already matched `value`.
    pub unchanged: bool,
}

impl WriteRecord {
    /// Serialise the record as a single-line JSON object (no serializer crate).
    pub fn to_json(&self) -> String {
        format!(
            "{{\"path\":{},\"value\":{},\"previous\":{},\"reason\":{},\
\"applied\":{},\"created\":{},\"reversible\":{},\"unchanged\":{}}}",
            json_string(&self.path),
            json_string(&self.value),
            self.previous
                .as_deref()
                .map(json_string)
                .unwrap_or_else(|| "null".into()),
            json_string(&self.reason),
            self.applied,
            self.created,
            self.reversible,
            self.unchanged,
        )
    }

    /// True when applying this record would change the node.
    pub fn is_change(&self) -> bool {
        !self.unchanged
    }
}

/// Minimal JSON string escaper (stdlib only, no serde).
pub fn json_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for character in value.chars() {
        match character {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            character if (character as u32) < 0x20 => {
                out.push_str(&format!("\\u{:04x}", character as u32));
            }
            character => out.push(character),
        }
    }
    out.push('"');
    out
}

/// True when a path's previous value can be restored by `rollback`.
///
/// Live kernel statistics under ``/proc`` cannot be "restored"; only the
/// writable tuning surfaces, and everything under ``/sys``, can.
pub fn is_reversible(logical: &str) -> bool {
    if logical == "/proc/cmdline" || logical == "/proc/cmdline~" {
        return false;
    }
    if logical.starts_with("/proc/sys/") || logical.starts_with("/proc/irq/") {
        return true;
    }
    !logical.starts_with("/proc/")
}

/// Root-prefixed filesystem view with dry-run journalling.
#[derive(Debug)]
pub struct SysRoot {
    root: PathBuf,
    dry_run: bool,
    journal: RefCell<Vec<WriteRecord>>,
}

impl SysRoot {
    /// Create a view rooted at `root`.
    pub fn new(root: impl Into<PathBuf>, dry_run: bool) -> SysRoot {
        SysRoot {
            root: root.into(),
            dry_run,
            journal: RefCell::new(Vec::new()),
        }
    }

    /// The real machine.
    pub fn host(dry_run: bool) -> SysRoot {
        SysRoot::new("/", dry_run)
    }

    /// Root prefix as configured.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// True when writes are recorded but not performed.
    pub fn dry_run(&self) -> bool {
        self.dry_run
    }

    /// Physical path for a logical path (``/proc/cmdline`` -> ``<root>/proc/cmdline``).
    pub fn abs(&self, logical: &str) -> PathBuf {
        if self.root == Path::new("/") {
            return PathBuf::from(logical);
        }
        let relative = logical.trim_start_matches('/');
        if relative.is_empty() {
            self.root.clone()
        } else {
            self.root.join(relative)
        }
    }

    /// Logical path for a physical one, for messages.
    pub fn logical(&self, physical: &Path) -> String {
        let as_string = physical.to_string_lossy();
        if self.root == Path::new("/") {
            return as_string.into_owned();
        }
        let prefix = format!("{}/", self.root.to_string_lossy().trim_end_matches('/'));
        match as_string.strip_prefix(&prefix) {
            Some(rest) => format!("/{rest}"),
            None => as_string.into_owned(),
        }
    }

    /// Whether the path exists (broken symlinks count as absent).
    pub fn exists(&self, logical: &str) -> bool {
        self.abs(logical).exists()
    }

    /// Whether the path is a directory.
    pub fn is_dir(&self, logical: &str) -> bool {
        self.abs(logical).is_dir()
    }

    /// Whether the path is a regular file or a kernel pseudo-file.
    pub fn is_file(&self, logical: &str) -> bool {
        self.abs(logical).is_file()
    }

    /// Read a file as UTF-8, failing with the logical path in the message.
    pub fn read(&self, logical: &str) -> Result<String> {
        let path = self.abs(logical);
        fs::read_to_string(&path).map_err(|err| Error::new(Kind::Io, format!("{logical}: {err}")))
    }

    /// Read a file, returning `None` when it is missing or unreadable.
    pub fn read_optional(&self, logical: &str) -> Option<String> {
        fs::read_to_string(self.abs(logical)).ok()
    }

    /// Read a file with surrounding whitespace removed.
    pub fn read_trimmed(&self, logical: &str) -> Option<String> {
        self.read_optional(logical)
            .map(|text| text.trim().to_string())
    }

    /// Read a file into lines, dropping trailing empty lines.
    pub fn read_lines(&self, logical: &str) -> Vec<String> {
        match self.read_optional(logical) {
            Some(text) => text.lines().map(|line| line.to_string()).collect(),
            None => Vec::new(),
        }
    }

    /// Sorted directory entry names, empty when the directory is absent.
    pub fn list_dir(&self, logical: &str) -> Vec<String> {
        let mut names: Vec<String> = match fs::read_dir(self.abs(logical)) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names
    }

    /// Sorted subdirectory names of `logical`.
    pub fn list_dirs(&self, logical: &str) -> Vec<String> {
        let base = self.abs(logical);
        let mut names: Vec<String> = match fs::read_dir(&base) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().is_dir())
                .map(|entry| entry.file_name().to_string_lossy().into_owned())
                .collect(),
            Err(_) => Vec::new(),
        };
        names.sort();
        names
    }

    /// Write `value` to `logical`.
    ///
    /// Returns `None` when the content already matches, so callers can report
    /// "nothing to do" without special-casing. Sysfs and procfs are written in
    /// place because the kernel rejects a rename over a pseudo-file; regular
    /// files are replaced atomically, so a crash mid-write cannot leave a
    /// half-edited ``/etc/default/grub`` behind.
    pub fn write(&self, logical: &str, value: &str, reason: &str) -> Result<Option<WriteRecord>> {
        let path = self.abs(logical);
        let existed = path.exists();
        let previous = if existed {
            self.read_optional(logical)
        } else {
            None
        };
        let unchanged = previous.as_deref() == Some(value);
        let record = WriteRecord {
            path: logical.to_string(),
            value: value.to_string(),
            previous,
            reason: reason.to_string(),
            applied: false,
            created: !existed,
            reversible: is_reversible(logical),
            unchanged,
        };
        if unchanged {
            return Ok(None);
        }
        if self.dry_run {
            self.record(record);
            return Ok(self.journal.borrow().last().cloned());
        }

        self.perform_write(&path, logical, value)?;
        let mut applied = record;
        applied.applied = true;
        self.record(applied.clone());
        Ok(Some(applied))
    }

    /// Write a whole file from a string, ensuring a trailing newline.
    pub fn write_text(
        &self,
        logical: &str,
        content: &str,
        reason: &str,
    ) -> Result<Option<WriteRecord>> {
        let mut normalised = content.to_string();
        if !normalised.ends_with('\n') {
            normalised.push('\n');
        }
        self.write(logical, &normalised, reason)
    }

    /// Remove a file, recording the removal for rollback.
    pub fn remove(&self, logical: &str, reason: &str) -> Result<Option<WriteRecord>> {
        let path = self.abs(logical);
        if !path.exists() {
            return Ok(None);
        }
        let previous = self.read_optional(logical);
        let record = WriteRecord {
            path: logical.to_string(),
            value: String::new(),
            previous,
            reason: reason.to_string(),
            applied: false,
            created: false,
            reversible: is_reversible(logical),
            unchanged: false,
        };
        if self.dry_run {
            self.record(record);
            return Ok(self.journal.borrow().last().cloned());
        }
        fs::remove_file(&path)
            .map_err(|err| Error::new(Kind::Io, format!("{logical}: cannot remove: {err}")))?;
        let mut applied = record;
        applied.applied = true;
        self.record(applied.clone());
        Ok(Some(applied))
    }

    /// Create a directory and its parents.
    pub fn ensure_dir(&self, logical: &str) -> Result<()> {
        if self.dry_run {
            return Ok(());
        }
        let path = self.abs(logical);
        fs::create_dir_all(&path).map_err(|err| {
            Error::new(
                Kind::Io,
                format!("{logical}: cannot create directory: {err}"),
            )
        })
    }

    /// True when this view points at the real machine rather than a fixture root.
    pub fn is_live(&self) -> bool {
        self.root == Path::new("/")
    }

    fn perform_write(&self, path: &Path, logical: &str, value: &str) -> Result<()> {
        // Pseudo-files must be written in place: the kernel rejects a temp-file
        // plus rename over /proc and /sys. Under a prefixed root those same logical
        // paths are ordinary files, so they are replaced atomically like anything
        // else - which also truncates them, so a shorter value cannot leave a stale
        // tail behind (``0`` over ``0-3\n`` must not read back as ``0-3\n``).
        let pseudo = matches!(FileKind::of(logical), FileKind::Sysfs | FileKind::Procfs);
        if self.is_live() && pseudo {
            let mut handle = fs::OpenOptions::new()
                .write(true)
                .open(path)
                .map_err(|err| {
                    Error::new(
                        Kind::Io,
                        format!("{logical}: cannot open for writing: {err}"),
                    )
                })?;
            handle.write_all(value.as_bytes()).map_err(|err| {
                Error::new(
                    Kind::Io,
                    format!("{logical}: cannot write {value:?}: {err}"),
                )
            })?;
            handle
                .flush()
                .map_err(|err| Error::new(Kind::Io, format!("{logical}: flush failed: {err}")))
        } else {
            self.atomic_write(path, logical, value)
        }
    }

    fn atomic_write(&self, path: &Path, logical: &str, value: &str) -> Result<()> {
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent).map_err(|err| {
            Error::new(
                Kind::Io,
                format!("{logical}: cannot create {}: {err}", parent.display()),
            )
        })?;
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "isolera".to_string());
        let temp = parent.join(format!(".{name}.isolera.tmp"));
        {
            let mut handle = fs::File::create(&temp).map_err(|err| {
                Error::new(
                    Kind::Io,
                    format!("{logical}: cannot create temp file: {err}"),
                )
            })?;
            handle.write_all(value.as_bytes())?;
            handle.sync_all().map_err(|err| {
                Error::new(
                    Kind::Io,
                    format!("{logical}: cannot flush temp file: {err}"),
                )
            })?;
        }
        if let Ok(metadata) = fs::metadata(path) {
            let _ = fs::set_permissions(&temp, metadata.permissions());
        }
        fs::rename(&temp, path)
            .map_err(|err| Error::new(Kind::Io, format!("{logical}: cannot replace file: {err}")))
    }

    /// Append a record to the journal without performing the write.
    pub fn record(&self, record: WriteRecord) {
        self.journal.borrow_mut().push(record);
    }

    /// Snapshot of everything written (or planned) through this view.
    pub fn journal(&self) -> Vec<WriteRecord> {
        self.journal.borrow().clone()
    }

    /// Number of journalled records.
    pub fn journal_len(&self) -> usize {
        self.journal.borrow().len()
    }

    /// Drain the journal.
    pub fn take_journal(&self) -> Vec<WriteRecord> {
        std::mem::take(&mut *self.journal.borrow_mut())
    }

    /// Effective UID of this process, read from ``/proc/self/status``.
    ///
    /// Returns `None` on hosts without procfs (development laptops), which the
    /// privilege check treats as "unknown".
    pub fn effective_uid(&self) -> Option<u32> {
        let status = self.read_optional("/proc/self/status")?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("Uid:") {
                let fields: Vec<&str> = rest.split_whitespace().collect();
                if fields.len() >= 2 {
                    return fields[1].parse().ok();
                }
            }
        }
        None
    }

    /// Fail unless the effective UID is 0 (only enforced against the real root).
    pub fn require_root(&self) -> Result<()> {
        if self.root != Path::new("/") {
            return Ok(());
        }
        match self.effective_uid() {
            Some(0) => Ok(()),
            Some(uid) => Err(Error::new(
                Kind::NotRoot,
                format!("root is required to modify kernel state, but the effective uid is {uid}"),
            )),
            None => Err(Error::new(
                Kind::NotRoot,
                "cannot determine the effective uid on this host; run Isolera as root",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir::TempDir;

    fn fixture() -> (TempDir, SysRoot, SysRoot) {
        let dir = TempDir::new("isolera-sysroot").unwrap();
        let source = dir.path().join("src");
        let target = dir.path().join("target");
        fs::create_dir_all(source.join("proc/sys/kernel")).unwrap();
        fs::create_dir_all(source.join("sys/devices/system/cpu/cpu0")).unwrap();
        fs::write(source.join("proc/cmdline"), "quiet isolcpus=2-3\n").unwrap();
        fs::write(source.join("proc/sys/kernel/watchdog"), "1\n").unwrap();
        fs::write(source.join("sys/devices/system/cpu/cpu0/online"), "1\n").unwrap();
        crate::tempdir::copy_tree(&source, &target).unwrap();
        let live = SysRoot::new(target.clone(), false);
        let dry = SysRoot::new(target, true);
        (dir, live, dry)
    }

    #[test]
    fn paths_translate_between_root_and_logical() {
        let root = SysRoot::new("/fake/root", false);
        assert_eq!(
            root.abs("/proc/cmdline"),
            PathBuf::from("/fake/root/proc/cmdline")
        );
        assert_eq!(
            root.logical(Path::new("/fake/root/proc/cmdline")),
            "/proc/cmdline"
        );
        let host = SysRoot::new("/", false);
        assert_eq!(host.abs("/proc/cmdline"), PathBuf::from("/proc/cmdline"));
        assert_eq!(host.logical(Path::new("/proc/cmdline")), "/proc/cmdline");
    }

    #[test]
    fn reads_and_no_op_writes() {
        let (_dir, live, _dry) = fixture();
        assert_eq!(
            live.read_trimmed("/proc/cmdline").unwrap(),
            "quiet isolcpus=2-3"
        );
        assert!(live
            .write("/proc/sys/kernel/watchdog", "1\n", "noop")
            .unwrap()
            .is_none());
        assert_eq!(live.journal_len(), 0);
    }

    #[test]
    fn live_writes_land_on_disk_and_are_journalled() {
        let (_dir, live, _dry) = fixture();
        let record = live
            .write(
                "/proc/sys/kernel/watchdog",
                "0\n",
                "disable lockup detector",
            )
            .unwrap()
            .expect("change expected");
        assert!(record.applied);
        assert!(!record.created);
        assert_eq!(record.previous.as_deref(), Some("1\n"));
        assert_eq!(live.read_trimmed("/proc/sys/kernel/watchdog").unwrap(), "0");
        assert_eq!(live.journal_len(), 1);
        assert!(record
            .to_json()
            .contains("\"path\":\"/proc/sys/kernel/watchdog\""));
    }

    #[test]
    fn dry_run_journals_without_touching_the_tree() {
        let (_dir, _live, dry) = fixture();
        let record = dry
            .write("/proc/sys/kernel/watchdog", "0\n", "dry")
            .unwrap()
            .expect("change expected");
        assert!(!record.applied);
        assert_eq!(dry.read_trimmed("/proc/sys/kernel/watchdog").unwrap(), "1");
        assert_eq!(dry.journal_len(), 1);
    }

    #[test]
    fn creating_a_new_file_is_recorded_as_created() {
        let (_dir, live, _dry) = fixture();
        let record = live
            .write("/etc/default/grub", "GRUB_TIMEOUT=1\n", "seed grub")
            .unwrap()
            .unwrap();
        assert!(record.created);
        assert_eq!(record.previous, None);
        assert_eq!(
            live.read_trimmed("/etc/default/grub").unwrap(),
            "GRUB_TIMEOUT=1"
        );
    }

    #[test]
    fn atomic_regular_file_write_replaces_content_exactly() {
        let (_dir, live, _dry) = fixture();
        live.write_text("/etc/isolera/marker", "one\n", "seed")
            .unwrap();
        live.write_text("/etc/isolera/marker", "two", "update")
            .unwrap();
        assert_eq!(live.read("/etc/isolera/marker").unwrap(), "two\n");
        assert_eq!(live.list_dir("/etc/isolera"), vec!["marker".to_string()]);
    }

    #[test]
    fn removals_are_recorded_for_rollback() {
        let (_dir, live, _dry) = fixture();
        let record = live.remove("/proc/cmdline", "cleanup").unwrap().unwrap();
        assert_eq!(record.previous.as_deref(), Some("quiet isolcpus=2-3\n"));
        assert!(!live.exists("/proc/cmdline"));
        assert!(live.remove("/proc/cmdline", "cleanup").unwrap().is_none());
    }

    #[test]
    fn reversibility_classifies_pseudo_files() {
        assert!(!is_reversible("/proc/cmdline"));
        assert!(!is_reversible("/proc/interrupts"));
        assert!(is_reversible("/proc/sys/kernel/watchdog"));
        assert!(is_reversible("/proc/irq/42/smp_affinity_list"));
        assert!(is_reversible("/sys/devices/system/cpu/cpu1/online"));
        assert!(is_reversible("/etc/default/grub"));
    }

    #[test]
    fn file_kind_classification() {
        assert_eq!(
            FileKind::of("/sys/devices/system/cpu/cpu0/online"),
            FileKind::Sysfs
        );
        assert_eq!(FileKind::of("/proc/sys/kernel/watchdog"), FileKind::Procfs);
        assert_eq!(FileKind::of("/etc/default/grub"), FileKind::Regular);
    }

    #[test]
    fn json_escaping_is_safe() {
        assert_eq!(json_string("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(json_string("line\nbreak"), "\"line\\nbreak\"");
        assert_eq!(json_string("\u{1}"), "\"\\u0001\"");
    }

    #[test]
    fn require_root_is_skipped_for_fixture_roots() {
        let (_dir, live, _dry) = fixture();
        assert!(live.require_root().is_ok());
    }

    #[test]
    fn directory_listing_is_sorted() {
        let (_dir, live, _dry) = fixture();
        assert_eq!(
            live.list_dir("/proc/sys/kernel"),
            vec!["watchdog".to_string()]
        );
        assert_eq!(
            live.list_dirs("/sys/devices/system/cpu"),
            vec!["cpu0".to_string()]
        );
        assert!(live.list_dir("/does/not/exist").is_empty());
    }
}
