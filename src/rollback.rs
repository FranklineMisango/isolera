//! Undo the last `apply` run.
//!
//! Rollback replays the journal newest-first and asks one question per record:
//!
//! * did this write *create* the file? then remove it;
//! * did the write change an existing file? then put the previous value back;
//! * does the kernel refuse to restore this path (``/proc/cmdline`` and other live
//!   statistics)? then say so instead of pretending.
//!
//! Newest-first is what makes two writes to the same path unwind correctly: the
//! later write is undone before the earlier one, so the file ends up carrying the
//! oldest recorded value - the one the node had before Isolera touched it.

use crate::config::Config;
use crate::error::Result;
use crate::journal::{self, Journal};
use crate::log::Log;
use crate::sysroot::{SysRoot, WriteRecord};

/// What rollback restored, removed and refused.
#[derive(Debug, Clone, Default)]
pub struct Rolled {
    /// Files restored to their previous value.
    pub restored: Vec<WriteRecord>,
    /// Files removed because the apply run created them.
    pub removed: Vec<WriteRecord>,
    /// Paths left alone, with the reason.
    pub skipped: Vec<(String, String)>,
    /// The journal rewrite that marks the run as rolled back.
    pub journal: Option<WriteRecord>,
}

impl Rolled {
    /// Every record, in the order the undo happened.
    pub fn records(&self) -> Vec<&WriteRecord> {
        let mut records: Vec<&WriteRecord> = Vec::new();
        records.extend(self.restored.iter());
        records.extend(self.removed.iter());
        records.extend(self.journal.iter());
        records
    }

    /// Number of paths touched.
    pub fn changes(&self) -> usize {
        self.restored.len() + self.removed.len()
    }

    /// True when nothing needed undoing.
    pub fn is_empty(&self) -> bool {
        self.restored.is_empty() && self.removed.is_empty()
    }

    /// Render the outcome as a report.
    pub fn render(&self) -> String {
        use crate::table::Table;
        let mut out = String::new();
        if self.is_empty() && self.skipped.is_empty() {
            out.push_str("nothing to roll back\n");
        } else {
            out.push_str(&format!(
                "{} restored, {} removed\n",
                self.restored.len(),
                self.removed.len()
            ));
            let mut table = Table::new(["path", "action", "value"]);
            for record in &self.restored {
                table.push([
                    record.path.clone(),
                    "restore".to_string(),
                    crate::log::flatten(record.previous.as_deref().unwrap_or(""), 40),
                ]);
            }
            for record in &self.removed {
                table.push([record.path.clone(), "remove".to_string(), "-".to_string()]);
            }
            out.push_str(&table.render());
        }
        if !self.skipped.is_empty() {
            out.push_str("\nleft alone\n");
            let mut table = Table::new(["path", "reason"]);
            for (path, reason) in &self.skipped {
                table.push([path.clone(), reason.clone()]);
            }
            out.push_str(&table.render());
        }
        out
    }

    /// Serialise the run as a JSON document.
    pub fn to_json(&self) -> String {
        let restored: Vec<String> = self.restored.iter().map(WriteRecord::to_json).collect();
        let removed: Vec<String> = self.removed.iter().map(WriteRecord::to_json).collect();
        let skipped: Vec<String> = self
            .skipped
            .iter()
            .map(|(path, reason)| {
                format!(
                    "{{\"path\":{},\"reason\":{}}}",
                    crate::sysroot::json_string(path),
                    crate::sysroot::json_string(reason)
                )
            })
            .collect();
        format!(
            "{{\"restored\":[{}],\"removed\":[{}],\"skipped\":[{}],\"journal\":{}}}",
            restored.join(","),
            removed.join(","),
            skipped.join(","),
            match &self.journal {
                Some(record) => crate::sysroot::json_string(&record.path),
                None => "null".to_string(),
            },
        )
    }
}

/// Restore the node to the state the last apply run found it in.
pub fn run(sysroot: &SysRoot, cfg: &Config, log: &Log) -> Result<Rolled> {
    if !sysroot.dry_run() {
        sysroot.require_root()?;
    }
    let mut rolled = Rolled::default();
    let Some(mut loaded) = journal::load(sysroot, cfg)? else {
        log.detail("no journal on this node");
        return Ok(rolled);
    };
    if loaded.rolled_back() {
        rolled.skipped.push((
            Journal::path(cfg),
            "this run was already rolled back; nothing to do".to_string(),
        ));
        return Ok(rolled);
    }
    if loaded.is_empty() {
        log.detail("the journal has no records");
        return Ok(rolled);
    }

    for record in loaded.restorable() {
        let path = record.path.clone();
        if journal::is_journal_path(cfg, &path) {
            continue;
        }
        if !record.reversible {
            rolled.skipped.push((
                path,
                "the kernel rejects a restored value for this path".to_string(),
            ));
            continue;
        }
        let reason = format!("isolera: roll back {path} to the value from before the change");
        if record.created {
            if !sysroot.exists(&path) {
                rolled
                    .skipped
                    .push((path, "already gone; nothing to remove".to_string()));
                continue;
            }
            if let Some(removed) = sysroot.remove(&path, &reason)? {
                log.detail(format!("removed {path}"));
                rolled.removed.push(removed);
            }
            continue;
        }
        let Some(previous) = record.previous.clone() else {
            rolled.skipped.push((
                path,
                "no previous value was recorded, so the original content is unknown".to_string(),
            ));
            continue;
        };
        let mut restored = record.clone();
        restored.previous = Some(previous.clone());
        restored.value = previous.clone();
        if sysroot.write(&restored.path, &previous, &reason)?.is_some() {
            log.detail(format!("restored {}", restored.path));
            restored.applied = !sysroot.dry_run();
            rolled.restored.push(restored);
        }
    }

    loaded.rolled_back_at = Some(crate::grub::now_seconds());
    loaded.records.clear();
    rolled.journal = journal::save(sysroot, cfg, &loaded)?;
    Ok(rolled)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply;
    use crate::cpulist;
    use crate::irq;
    use crate::plan;
    use crate::testing;
    use std::fs;

    /// Apply the fixture plan, then hand back the node.
    fn applied_node() -> (testing::Node, Config) {
        let node = testing::node();
        let cfg = testing::config();
        let built = plan::build(&node.sysroot, &cfg).unwrap();
        apply::run(&node.sysroot, &cfg, &built, &Log::quiet()).unwrap();
        (node, cfg)
    }

    /// A record as the journal would hold it.
    fn record(path: &str, value: &str, previous: &str) -> WriteRecord {
        WriteRecord {
            path: path.to_string(),
            value: value.to_string(),
            previous: Some(previous.to_string()),
            reason: "isolera: test".to_string(),
            applied: true,
            created: false,
            reversible: true,
            unchanged: false,
        }
    }

    #[test]
    fn rollback_restores_every_path_apply_touched() {
        let (node, cfg) = applied_node();
        let rolled = run(&node.sysroot, &cfg, &Log::quiet()).unwrap();

        assert_eq!(
            irq::affinity(&node.sysroot, 124),
            Some(cpulist::parse("0-3").unwrap())
        );
        assert_eq!(
            node.sysroot
                .read_trimmed("/proc/sys/kernel/watchdog")
                .as_deref(),
            Some("1")
        );
        assert_eq!(
            node.sysroot
                .read_trimmed("/proc/sys/kernel/sched_rt_runtime_us")
                .as_deref(),
            Some("950000")
        );
        assert_eq!(
            node.sysroot.read_optional("/etc/default/grub").unwrap(),
            testing::node()
                .sysroot
                .read_optional("/etc/default/grub")
                .unwrap(),
            "the grub file is back to its original content"
        );

        // 1 irq + 4 sysctls + the grub file, and the backup is removed again.
        assert_eq!(rolled.restored.len(), 6);
        assert_eq!(rolled.removed.len(), 1);
        assert!(rolled.removed[0]
            .path
            .contains("/var/lib/isolera/backups/grub."));
        assert!(rolled.journal.is_some());

        // The journal records that it has been rolled back, so a second run is a
        // deliberate no-op rather than a silent re-restore.
        let journal = journal::load(&node.sysroot, &cfg).unwrap().unwrap();
        assert!(journal.rolled_back());
        assert!(journal.is_empty());
    }

    #[test]
    fn a_second_rollback_says_so_instead_of_guessing() {
        let (node, cfg) = applied_node();
        run(&node.sysroot, &cfg, &Log::quiet()).unwrap();
        let second = run(&node.sysroot, &cfg, &Log::quiet()).unwrap();
        assert!(second.is_empty());
        assert_eq!(second.skipped.len(), 1);
        assert!(second.skipped[0].1.contains("already rolled back"));
        assert!(second.journal.is_none(), "nothing changed, so no rewrite");
    }

    #[test]
    fn irreversible_paths_are_reported_not_restored() {
        let node = testing::node();
        let cfg = testing::config();
        let mut journal = Journal::new("test-node", &cfg.source);
        let mut cmdline = record("/proc/cmdline", "isolera boot", "quiet");
        cmdline.reversible = false;
        journal.push(cmdline);
        journal.push(record("/proc/sys/kernel/watchdog", "0", "1\n"));
        node.sysroot
            .write("/proc/sys/kernel/watchdog", "0", "test")
            .unwrap();
        journal::save(&node.sysroot, &cfg, &journal).unwrap();

        let rolled = run(&node.sysroot, &cfg, &Log::quiet()).unwrap();
        assert_eq!(rolled.restored.len(), 1);
        assert_eq!(rolled.skipped.len(), 1);
        assert_eq!(rolled.skipped[0].0, "/proc/cmdline");
        assert!(rolled.render().contains("left alone"));
        assert_eq!(
            node.sysroot
                .read_trimmed("/proc/sys/kernel/watchdog")
                .as_deref(),
            Some("1")
        );
    }

    #[test]
    fn two_writes_to_one_path_unwind_in_reverse_order() {
        let node = testing::node();
        let cfg = testing::config();
        let path = "/proc/irq/124/smp_affinity_list";
        let mut journal = Journal::new("test-node", &cfg.source);
        journal.push(record(path, "2", "0-3\n"));
        journal.push(record(path, "2,3", "2\n"));
        node.sysroot.write(path, "2,3", "test").unwrap();
        journal::save(&node.sysroot, &cfg, &journal).unwrap();

        run(&node.sysroot, &cfg, &Log::quiet()).unwrap();
        assert_eq!(
            node.sysroot.read_trimmed(path).as_deref(),
            Some("0-3"),
            "the oldest recorded value wins"
        );
    }

    #[test]
    fn a_node_without_a_journal_is_left_alone() {
        let node = testing::node();
        let cfg = testing::config();
        let rolled = run(&node.sysroot, &cfg, &Log::quiet()).unwrap();
        assert!(rolled.is_empty());
        assert!(rolled.skipped.is_empty());
        assert!(rolled.render().starts_with("nothing to roll back"));

        let empty = Journal::new("test-node", &cfg.source);
        journal::save(&node.sysroot, &cfg, &empty).unwrap();
        assert!(run(&node.sysroot, &cfg, &Log::quiet()).unwrap().is_empty());
    }

    #[test]
    fn dry_run_rollback_changes_nothing_and_writes_no_journal() {
        let (node, cfg) = applied_node();
        let before = node
            .sysroot
            .read_optional("/proc/sys/kernel/watchdog")
            .unwrap();
        let rolled = run(&node.dry, &cfg, &Log::quiet()).unwrap();
        assert!(rolled.restored.len() >= 6);
        assert!(rolled.records().iter().all(|record| !record.applied));
        assert_eq!(
            node.sysroot
                .read_optional("/proc/sys/kernel/watchdog")
                .unwrap(),
            before
        );
        assert!(!journal::load(&node.sysroot, &cfg)
            .unwrap()
            .unwrap()
            .rolled_back());
    }

    #[test]
    fn rollback_render_and_json_describe_the_run() {
        let (node, cfg) = applied_node();
        let rolled = run(&node.sysroot, &cfg, &Log::quiet()).unwrap();
        let rendered = rolled.render();
        assert!(
            rendered.starts_with("6 restored, 1 removed\n"),
            "{rendered}"
        );
        assert!(rendered.contains("/proc/irq/124/smp_affinity_list"));

        let json = rolled.to_json();
        assert!(json.starts_with("{\"restored\":["), "{json}");
        assert!(json.contains("\"/var/lib/isolera/state/journal.toml\""));
        assert_eq!(json.matches('{').count(), json.matches('}').count());
        assert_eq!(json.matches('[').count(), json.matches(']').count());
    }

    #[test]
    fn rollback_removes_files_that_apply_created() {
        let node = testing::node();
        let cfg = testing::config();
        // A node whose grub file does not exist yet: applying creates it.
        fs::remove_dir_all(node.abs("/etc/default")).unwrap();
        let built = plan::build(&node.sysroot, &cfg).unwrap();
        apply::run(&node.sysroot, &cfg, &built, &Log::quiet()).unwrap();
        assert!(node.abs("/etc/default/grub").is_file());

        let rolled = run(&node.sysroot, &cfg, &Log::quiet()).unwrap();
        assert!(rolled
            .removed
            .iter()
            .any(|record| record.path == "/etc/default/grub"));
        assert!(
            !node.abs("/etc/default/grub").exists(),
            "the created file is gone again"
        );
    }
}
