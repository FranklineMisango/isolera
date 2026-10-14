//! Phase 2, step 2: perform a plan.
//!
//! Ordering is deliberate. The governor and the SMT offlines come first because
//! they change what the hardware can do; interrupt steering comes next so the
//! kernel re-homes the IRQs that offlining displaced; the sysctls follow; the
//! grub file goes last, so a failure anywhere in the runtime steps leaves the boot
//! configuration untouched.
//!
//! `apply` is not transactional, but it is *auditable*: every write goes through
//! [`SysRoot`], lands in the journal with its previous value, and can be undone by
//! `rollback`. A step that fails stops the run where it is rather than pressing on.

use crate::config::Config;
use crate::cpulist;
use crate::error::Result;
use crate::grub::{self, GrubApply};
use crate::irq;
use crate::journal::Journal;
use crate::kernel;
use crate::log::Log;
use crate::plan::Plan;
use crate::sysroot::{SysRoot, WriteRecord};
use crate::topology;

/// What one apply run changed.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    /// Governor writes.
    pub governors: Vec<WriteRecord>,
    /// CPU offline writes.
    pub offline: Vec<WriteRecord>,
    /// Interrupt affinity writes and the IRQs that could not be written.
    pub irqs: irq::Applied,
    /// Sysctl writes.
    pub sysctls: Vec<WriteRecord>,
    /// The grub edit and its backup.
    pub grub: GrubApply,
    /// The journal itself.
    pub journal: Option<WriteRecord>,
    /// Steps the operator still has to take, in order.
    pub follow_up: Vec<String>,
}

impl Applied {
    /// Every record, in the order the writes happened.
    pub fn records(&self) -> Vec<&WriteRecord> {
        let mut records: Vec<&WriteRecord> = Vec::new();
        records.extend(self.governors.iter());
        records.extend(self.offline.iter());
        records.extend(self.irqs.records.iter());
        records.extend(self.sysctls.iter());
        records.extend(self.grub.records());
        records.extend(self.journal.iter());
        records
    }

    /// Number of writes performed (or journalled, in dry-run mode).
    pub fn writes(&self) -> usize {
        self.records().len()
    }

    /// IRQs the kernel refused to move.
    pub fn skipped(&self) -> &[(u32, String)] {
        &self.irqs.skipped
    }

    /// Governor changes as a count, for the report.
    pub fn governors(&self) -> usize {
        self.governors.len()
    }

    /// Render the applied changes as a report.
    pub fn render(&self) -> String {
        use crate::table::Table;
        let mut out = String::new();
        let records = self.records();
        if records.is_empty() {
            out.push_str("nothing to change: the node already matches the plan\n");
        } else {
            let mut table = Table::new(["path", "value", "reversible"]);
            for record in &records {
                table.push([
                    record.path.clone(),
                    crate::log::flatten(&record.value, 48),
                    if record.reversible { "yes" } else { "NO" }.to_string(),
                ]);
            }
            out.push_str(&format!("{} writes\n", records.len()));
            out.push_str(&table.render());
        }
        if !self.irqs.skipped.is_empty() {
            out.push_str("\nirqs the kernel refused\n");
            let mut table = Table::new(["irq", "reason"]);
            for (irq, reason) in &self.irqs.skipped {
                table.push([irq.to_string(), reason.clone()]);
            }
            out.push_str(&table.render());
        }
        if !self.follow_up.is_empty() {
            out.push_str("\nfollow-up\n");
            for step in &self.follow_up {
                out.push_str(&format!("  - {step}\n"));
            }
        }
        out
    }

    /// Serialise the run as a JSON document.
    pub fn to_json(&self) -> String {
        let records: Vec<String> = self
            .records()
            .iter()
            .map(|record| record.to_json())
            .collect();
        let skipped: Vec<String> = self
            .irqs
            .skipped
            .iter()
            .map(|(irq, reason)| {
                format!(
                    "{{\"irq\":{irq},\"reason\":{}}}",
                    crate::sysroot::json_string(reason)
                )
            })
            .collect();
        let follow_up: Vec<String> = self
            .follow_up
            .iter()
            .map(|step| crate::sysroot::json_string(step))
            .collect();
        format!(
            "{{\"writes\":{},\"records\":[{}],\"skipped\":[{}],\"journal\":{},\"follow_up\":[{}]}}",
            records.len(),
            records.join(","),
            skipped.join(","),
            match &self.journal {
                Some(record) => crate::sysroot::json_string(&record.path),
                None => "null".to_string(),
            },
            follow_up.join(","),
        )
    }
}

/// Apply a plan, journalling every write.
pub fn run(sysroot: &SysRoot, cfg: &Config, plan: &Plan, log: &Log) -> Result<Applied> {
    if !sysroot.dry_run() {
        sysroot.require_root()?;
    }
    let mut applied = Applied::default();
    let mut journal = Journal::new(&plan.node, &cfg.source);

    log.step("cpu frequency");
    for action in &plan.governors {
        let reason = format!(
            "isolera: pin the governor for the trading core to {}",
            action.target
        );
        if let Some(record) = topology::set_governor(sysroot, action.cpu, &action.target, &reason)?
        {
            log.detail(format!("cpu{} -> {}", action.cpu, action.target));
            applied.governors.push(record);
        }
    }

    log.step("smt siblings");
    for cpu in &plan.siblings_to_offline {
        let reason = "isolera: offline the sibling so the trading core owns its execution units";
        if let Some(record) = topology::set_cpu_online(sysroot, *cpu, false, reason)? {
            log.detail(format!("cpu{cpu} offline"));
            applied.offline.push(record);
        }
    }

    log.step("interrupts");
    applied.irqs = irq::apply(sysroot, &plan.irqs, plan.cpu_count)?;
    log.detail(format!("{} irq(s) rewritten", applied.irqs.records.len()));

    log.step("sysctls");
    applied.sysctls = kernel::apply_sysctls(sysroot, &plan.tunables)?;
    log.detail(format!("{} sysctl(s) written", applied.sysctls.len()));

    log.step("grub");
    applied.grub = grub::apply(sysroot, cfg, &plan.grub)?;

    journal.extend(&applied.governors);
    journal.extend(&applied.offline);
    journal.extend(&applied.irqs.records);
    journal.extend(&applied.sysctls);
    if let Some(record) = &applied.grub.backup {
        journal.push(record.clone());
    }
    if let Some(record) = &applied.grub.record {
        journal.push(record.clone());
    }
    if !journal.is_empty() {
        applied.journal = crate::journal::save(sysroot, cfg, &journal)?;
    }

    applied.follow_up = follow_up(cfg, plan, &applied);
    Ok(applied)
}

/// What the operator still has to do by hand.
fn follow_up(cfg: &Config, plan: &Plan, applied: &Applied) -> Vec<String> {
    let mut steps = Vec::new();
    if !applied.irqs.skipped.is_empty() {
        steps.push(format!(
            "{} interrupt(s) could not be steered; check /proc/irq for the missing files",
            applied.irqs.skipped.len()
        ));
    }
    if !plan.pins.is_empty() {
        steps.push(format!(
            "run `isolera pin --apply` to move {} kernel thread(s) off {}",
            plan.pins.len(),
            cpulist::format(&plan.isolated)
        ));
    }
    if plan.grub.changed {
        if let Some(command) = grub::regenerate_command(cfg) {
            steps.push(format!("run `{command}` to regenerate the bootloader menu"));
        }
    }
    if plan.cmdline_changed {
        steps.push(
            "reboot to boot the new kernel command line; isolation is only active after that"
                .to_string(),
        );
    }
    steps
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::journal as journal_io;
    use crate::plan;
    use crate::testing;
    use std::fs;

    fn applied() -> (testing::Node, Config, Applied) {
        let node = testing::node();
        let cfg = testing::config();
        let plan = plan::build(&node.sysroot, &cfg).unwrap();
        let applied = run(&node.sysroot, &cfg, &plan, &Log::quiet()).unwrap();
        (node, cfg, applied)
    }

    #[test]
    fn apply_changes_the_node_and_journals_every_write() {
        let (node, cfg, applied) = applied();
        assert_eq!(
            applied.writes(),
            8,
            "1 irq, 4 sysctls, grub, its backup and the journal"
        );
        assert_eq!(applied.governors(), 0, "the governors already match");

        assert_eq!(
            irq::affinity(&node.sysroot, 124),
            Some(cpulist::parse("2,3").unwrap())
        );
        assert_eq!(
            node.sysroot
                .read_trimmed("/proc/sys/kernel/watchdog")
                .as_deref(),
            Some("0")
        );
        assert_eq!(
            node.sysroot
                .read_trimmed("/proc/sys/kernel/sched_rt_runtime_us")
                .as_deref(),
            Some("-1")
        );
        let grub = node.sysroot.read_optional("/etc/default/grub").unwrap();
        assert!(grub.contains(
            "GRUB_CMDLINE_LINUX=\"quiet splash isolcpus=managed_irq,2,3 nohz_full=2,3 rcu_nocbs=2,3\""
        ));
        assert!(grub.contains("GRUB_TIMEOUT=\"1\""));

        // The journal is on disk, with the previous values rollback needs.
        let journal = journal_io::load(&node.sysroot, &cfg).unwrap().unwrap();
        assert_eq!(
            journal.records.len(),
            7,
            "the journal does not record itself"
        );
        assert_eq!(journal.node, "test-node");
        assert_eq!(journal.source, "fixture.toml");
        assert!(journal.records.iter().any(|record| {
            record.path == "/proc/irq/124/smp_affinity_list"
                && record.previous.as_deref() == Some("0-3\n")
        }));
        assert!(applied.journal.is_some());

        // Follow-up steps name the pinning script, the bootloader and the reboot.
        let steps = applied.follow_up.join("\n");
        assert!(steps.contains("isolera pin --apply"), "{steps}");
        assert!(steps.contains("update-grub"), "{steps}");
        assert!(steps.contains("reboot"), "{steps}");
    }

    #[test]
    fn a_second_apply_is_a_no_op() {
        let (node, cfg, _first) = applied();
        let plan = plan::build(&node.sysroot, &cfg).unwrap();
        let second = run(&node.sysroot, &cfg, &plan, &Log::quiet()).unwrap();
        assert_eq!(second.writes(), 0);
        assert!(second.journal.is_none(), "no writes means no new journal");
        assert!(second.render().starts_with("nothing to change"));
    }

    #[test]
    fn dry_run_journals_the_intent_without_touching_the_node() {
        let node = testing::node();
        let cfg = testing::config();
        let plan = plan::build(&node.sysroot, &cfg).unwrap();
        let applied = run(&node.dry, &cfg, &plan, &Log::quiet()).unwrap();

        assert_eq!(applied.writes(), 8);
        assert!(applied.records().iter().all(|record| !record.applied));
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
        let grub = node.sysroot.read_optional("/etc/default/grub").unwrap();
        assert!(
            grub.contains("GRUB_TIMEOUT=5"),
            "the grub file is untouched"
        );
        assert!(journal_io::load(&node.sysroot, &cfg).unwrap().is_none());
    }

    #[test]
    fn siblings_are_offlined_and_journalled() {
        let node = testing::node();
        let mut cfg = testing::config();
        cfg.isolation.cpus = "2".to_string();
        cfg.isolation.set = cpulist::parse("2").unwrap();
        let plan = plan::build(&node.sysroot, &cfg).unwrap();
        assert_eq!(plan.siblings_to_offline, cpulist::parse("3").unwrap());

        let applied = run(&node.sysroot, &cfg, &plan, &Log::quiet()).unwrap();
        assert_eq!(
            node.sysroot
                .read_trimmed("/sys/devices/system/cpu/cpu3/online")
                .as_deref(),
            Some("0")
        );
        assert!(applied
            .offline
            .iter()
            .any(|record| record.path == "/sys/devices/system/cpu/cpu3/online"));
        assert!(
            applied.render().contains("cpu3"),
            "the report names the offlined sibling"
        );
    }

    #[test]
    fn irqs_without_affinity_files_are_reported_not_ignored() {
        let node = testing::node();
        let cfg = testing::config();
        let plan = plan::build(&node.sysroot, &cfg).unwrap();
        fs::remove_dir_all(node.abs("/proc/irq/124")).unwrap();

        let applied = run(&node.sysroot, &cfg, &plan, &Log::quiet()).unwrap();
        assert_eq!(applied.skipped().len(), 1);
        assert_eq!(applied.skipped()[0].0, 124);
        assert!(applied.skipped()[0].1.contains("affinity"));
        assert!(applied.render().contains("irqs the kernel refused"));
        assert!(applied
            .follow_up
            .iter()
            .any(|step| step.contains("could not be steered")));
    }

    #[test]
    fn apply_render_and_json_describe_the_run() {
        let (_node, _cfg, applied) = applied();
        let rendered = applied.render();
        assert!(rendered.starts_with("8 writes\n"));
        assert!(rendered.contains("/proc/irq/124/smp_affinity_list"));
        assert!(rendered.contains("follow-up"));

        let json = applied.to_json();
        assert!(json.starts_with("{\"writes\":8"), "{json}");
        assert!(json.contains("\"journal\":\"/var/lib/isolera/state/journal.toml\""));
        assert!(json.contains("\"applied\":true"));
        assert_eq!(json.matches('{').count(), json.matches('}').count());
        assert_eq!(json.matches('[').count(), json.matches(']').count());
    }
}
