//! Minimal temporary-directory helper (no external crates).
//!
//! Used by the test-suite to copy a fixture sysroot somewhere writable, and by
//! the CLI when it needs a scratch space for backups or benchmark output.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use crate::error::{Error, Kind, Result};

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// A directory that removes itself when dropped.
#[derive(Debug)]
pub struct TempDir {
    path: PathBuf,
    keep: bool,
}

impl TempDir {
    /// Create a uniquely named directory under the system temp dir.
    pub fn new(prefix: &str) -> Result<TempDir> {
        let base = std::env::temp_dir();
        for _ in 0..64 {
            let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
            let candidate = base.join(format!("{prefix}-{}-{serial}", std::process::id()));
            match fs::create_dir_all(&candidate) {
                Ok(()) => {
                    return Ok(TempDir {
                        path: candidate,
                        keep: false,
                    })
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(Error::new(
                        Kind::Io,
                        format!(
                            "cannot create temp directory {}: {err}",
                            candidate.display()
                        ),
                    ))
                }
            }
        }
        Err(Error::new(
            Kind::Io,
            "cannot create a unique temporary directory after 64 attempts",
        ))
    }

    /// Path of the directory.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Path as a string.
    pub fn as_str(&self) -> String {
        self.path.to_string_lossy().into_owned()
    }

    /// Leave the directory behind (for debugging a failing test).
    pub fn keep(mut self) -> PathBuf {
        self.keep = true;
        self.path.clone()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        if !self.keep {
            let _ = fs::remove_dir_all(&self.path);
        }
    }
}

/// Recursively copy `source` into `destination`, creating parents as needed.
pub fn copy_tree(source: &Path, destination: &Path) -> Result<()> {
    fs::create_dir_all(destination).map_err(|err| {
        Error::new(
            Kind::Io,
            format!("{}: cannot create: {err}", destination.display()),
        )
    })?;
    let entries = fs::read_dir(source).map_err(|err| {
        Error::new(
            Kind::Io,
            format!("{}: cannot read: {err}", source.display()),
        )
    })?;
    for entry in entries {
        let entry = entry.map_err(|err| Error::new(Kind::Io, err.to_string()))?;
        let target = destination.join(entry.file_name());
        let file_type = entry
            .file_type()
            .map_err(|err| Error::new(Kind::Io, err.to_string()))?;
        if file_type.is_dir() {
            copy_tree(&entry.path(), &target)?;
        } else if file_type.is_symlink() {
            // Fixtures avoid symlinks; treat them as plain files to stay portable.
            if let Ok(content) = fs::read(entry.path()) {
                fs::write(&target, content).map_err(|err| {
                    Error::new(
                        Kind::Io,
                        format!("{}: cannot write: {err}", target.display()),
                    )
                })?;
            }
        } else {
            fs::copy(entry.path(), &target).map_err(|err| {
                Error::new(
                    Kind::Io,
                    format!("{}: cannot copy: {err}", target.display()),
                )
            })?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tempdir_is_created_and_removed() {
        let root;
        {
            let dir = TempDir::new("isolera-test").unwrap();
            assert!(dir.path().is_dir());
            root = dir.path().to_path_buf();
        }
        assert!(!root.exists(), "TempDir must clean up on drop");
    }

    #[test]
    fn tempdirs_are_unique() {
        let first = TempDir::new("isolera-uniq").unwrap();
        let second = TempDir::new("isolera-uniq").unwrap();
        assert_ne!(first.path(), second.path());
    }

    #[test]
    fn copy_tree_reproduces_nested_layout() {
        let source = TempDir::new("isolera-src").unwrap();
        fs::create_dir_all(source.path().join("proc/sys/kernel")).unwrap();
        fs::write(source.path().join("proc/cmdline"), "quiet\n").unwrap();
        fs::write(source.path().join("proc/sys/kernel/watchdog"), "1\n").unwrap();

        let destination = TempDir::new("isolera-dst").unwrap();
        copy_tree(source.path(), &destination.path().join("root")).unwrap();
        let copied = destination.path().join("root");
        assert_eq!(
            fs::read_to_string(copied.join("proc/cmdline")).unwrap(),
            "quiet\n"
        );
        assert!(copied.join("proc/sys/kernel/watchdog").is_file());
    }
}
