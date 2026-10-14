//! Phase 2, step 1: everything Isolera *would* change, before it changes it.
//!
//! [`Plan::build`] is read-only by construction: it walks the same functions the
//! apply step uses, but through a [`SysRoot`] that is only ever read. That is what
//! makes `isolera plan` trustworthy - the printed plan and the executed plan come
//! from one code path, so they cannot drift apart.
//!
//! The plan covers every subsystem the suite owns:
//!
//! * the kernel command line and the grub file that carries it,
//! * SMT siblings that must be offlined,
//! * the cpufreq governor on the trading cores,
//! * interrupt steering, sysctls and movable kernel threads.

use std::collections::BTreeMap;

use crate::config::Config;
use crate::cpulist::{self, CpuSet};
use crate::error::{Error, Kind, Result};
use crate::grub::{self, GrubPlan};
use crate::irq::{self, Decision, Policy};
use crate::kernel::{self, PinCommand, Tunable};
use crate::sysroot::SysRoot;
use crate::topology::{self, Topology};

/// One CPU whose governor the plan enforces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GovernorAction {
    /// CPU number.
    pub cpu: u32,
    /// Governor found on the node, when cpufreq is available.
    pub current: Option<String>,
    /// Governor the plan wants.
    pub target: String,
}

/// Everything Isolera would do to a node.
#[derive(Debug, Clone)]
pub struct Plan {
    /// Configuration file the plan came from.
    pub source: String,
    /// Node label.
    pub node: String,
    /// CPUs the machine has.
    pub cpu_count: usize,
    /// CPUs reserved for the strategy.
    pub isolated: CpuSet,
    /// CPUs left for the kernel, IRQs and housekeeping.
    pub housekeeping: CpuSet,
    /// SMT siblings the plan takes offline.
    pub siblings_to_offline: CpuSet,
    /// Governor changes.
    pub governors: Vec<GovernorAction>,
    /// Running command line.
    pub current_cmdline: String,
    /// Command line the plan wants.
    pub cmdline: String,
    /// True when the two differ.
    pub cmdline_changed: bool,
    /// The grub file edit that carries the command line.
    pub grub: GrubPlan,
    /// Interrupt decisions, including the ones that change nothing.
    pub irqs: Vec<Decision>,
    /// Sysctls the plan wants written.
    pub tunables: Vec<Tunable>,
    /// Movable kernel threads that still sit on a trading core.
    pub pins: Vec<PinCommand>,
    /// ``SCHED_FIFO`` priority the plan uses for the pinning script, when set.
    pub fifo_priority: Option<u32>,
    /// Findings that do not stop the plan.
    pub warnings: Vec<String>,
}

impl Plan {
    /// Interrupt decisions that change an affinity mask.
    pub fn irq_changes(&self) -> Vec<&Decision> {
        self.irqs
            .iter()
            .filter(|decision| decision.changes())
            .collect()
    }

    /// Number of writes the plan would perform.
    pub fn write_count(&self) -> usize {
        let sysctls = self
            .tunables
            .iter()
            .filter(|tunable| !tunable.already_set())
            .count();
        let governor = self.governors.len();
        let offline = self.siblings_to_offline.len();
        let grub_file = usize::from(self.grub.changed);
        let backup = usize::from(self.grub.changed && !self.grub.before.is_empty());
        self.irq_changes().len() + sysctls + governor + offline + grub_file + backup
    }

    /// True when the node already matches the plan.
    pub fn is_up_to_date(&self) -> bool {
        self.write_count() == 0 && self.pins.is_empty()
    }

    /// Headline numbers, for reports and the CLI's closing summary.
    pub fn summary(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "command line",
                if self.cmdline_changed {
                    "update"
                } else {
                    "already set"
                }
                .to_string(),
            ),
            ("irq changes", self.irq_changes().len().to_string()),
            (
                "sysctls",
                self.tunables
                    .iter()
                    .filter(|tunable| !tunable.already_set())
                    .count()
                    .to_string(),
            ),
            ("cpu offline", self.siblings_to_offline.len().to_string()),
            ("governors", self.governors.len().to_string()),
            ("kernel threads", self.pins.len().to_string()),
            (
                "grub",
                if self.grub.changed {
                    "update"
                } else {
                    "already set"
                }
                .to_string(),
            ),
        ]
    }

    /// Interrupt decisions grouped by action.
    pub fn irq_summary(&self) -> BTreeMap<&'static str, usize> {
        irq::summarize(&self.irqs)
    }
}

/// Read the node and work out the complete plan.
pub fn build(sysroot: &SysRoot, cfg: &Config) -> Result<Plan> {
    cfg.validate()?;
    let probe = topology::probe(sysroot);
    let cpu_count = probe.cpu_count().max(1);
    let isolated = cfg.isolated().clone();
    let housekeeping = cfg.housekeeping(cpu_count);
    if housekeeping.is_empty() {
        return Err(Error::new(
            Kind::Config,
            "no housekeeping CPUs: isolation.cpus covers every CPU on this node".to_string(),
        ));
    }

    let params = kernel::isolation_params(cfg, &isolated)?;
    let current_cmdline = kernel::Cmdline::read(sysroot);
    let current = current_cmdline.render();
    let cmdline = kernel::merge_managed(&current, &params);

    let siblings_to_offline = if cfg.isolation.smt {
        CpuSet::new()
    } else {
        probe.siblings_not_in(&isolated)
    };

    let policy = Policy::from_config(cfg, cpu_count)?;
    let table = irq::read_interrupts(sysroot);
    let irqs = irq::plan(sysroot, &table, &policy);
    let mut warnings = irq_warnings(&policy, &table);

    if cfg.isolation.smt && probe.smt_enabled() {
        warnings.push(
            "isolation.smt is true, so hyperthread siblings stay online and share execution units \
             with the trading cores"
                .to_string(),
        );
    }
    // A CPU the kernel cannot see at all is a different problem from one that is
    // merely offline: "offline" sends an operator looking for a BIOS setting, when
    // the real cause is usually a typo in `isolation.cpus` or a config copied from a
    // bigger machine. Only claim absence when the node actually reported a
    // `present` set.
    let knows_present = !probe.present.is_empty();
    for cpu in &isolated {
        if knows_present && !probe.present.contains(cpu) {
            warnings.push(format!(
                "isolated CPU {cpu} is not present on this node ({} CPUs); \
                 check isolation.cpus for a typo",
                probe.cpu_count()
            ));
        } else if !probe.online.contains(cpu) {
            warnings.push(format!(
                "isolated CPU {cpu} is offline; isolation takes effect after it is brought back \
                 online"
            ));
        }
    }

    Ok(Plan {
        source: cfg.source.clone(),
        node: cfg
            .node
            .name
            .clone()
            .unwrap_or_else(|| "unnamed".to_string()),
        cpu_count,
        isolated,
        housekeeping,
        siblings_to_offline,
        governors: governor_actions(sysroot, cfg, &probe),
        cmdline_changed: cmdline != current,
        current_cmdline: current,
        cmdline,
        grub: grub::plan(sysroot, cfg)?,
        irqs,
        tunables: kernel::sysctl_plan(sysroot, cfg)?,
        pins: kernel::pinning_plan(sysroot, cfg, &cfg.housekeeping(cpu_count)),
        fifo_priority: cfg.rt.fifo_priority,
        warnings,
    })
}

/// Patterns that matched nothing, so a typo becomes visible instead of silent.
fn irq_warnings(policy: &Policy, table: &irq::InterruptTable) -> Vec<String> {
    policy
        .unused_patterns(table)
        .into_iter()
        .map(|pattern| format!("irq pattern {pattern:?} matched no interrupt on this node (typo?)"))
        .collect()
}

/// Governor changes for every CPU that shares a core with a trading core.
fn governor_actions(sysroot: &SysRoot, cfg: &Config, probe: &Topology) -> Vec<GovernorAction> {
    let target = cfg.isolation.governor.trim();
    if target.is_empty() {
        return Vec::new();
    }
    let mut cpus = cfg.isolated().clone();
    cpus.extend(probe.siblings_not_in(cfg.isolated()));
    cpus.into_iter()
        .filter_map(|cpu| {
            let current = probe.get(cpu).and_then(|node| node.governor.clone());
            // A CPU without cpufreq (or a fixture without the file) cannot be tuned.
            if current.is_none() && !sysroot.exists(&governor_path(cpu)) {
                return None;
            }
            if current.as_deref() == Some(target) {
                return None;
            }
            Some(GovernorAction {
                cpu,
                current,
                target: target.to_string(),
            })
        })
        .collect()
}

/// ``/sys`` path of a CPU's governor.
pub fn governor_path(cpu: u32) -> String {
    format!("/sys/devices/system/cpu/cpu{cpu}/cpufreq/scaling_governor")
}

/// CPUs whose governor will be set, in order.
pub fn governor_cpus(plan: &Plan) -> CpuSet {
    plan.governors.iter().map(|action| action.cpu).collect()
}

/// ``<list>`` or ``none``.
fn or_none(set: &CpuSet) -> String {
    if set.is_empty() {
        "none".to_string()
    } else {
        cpulist::format(set)
    }
}

/// Indent a rendered block so it sits under its section heading.
fn indent(text: &str) -> String {
    text.lines().map(|line| format!("  {line}\n")).collect()
}

/// Render the plan as a sequence of sections an operator can review.
pub fn render(plan: &Plan) -> String {
    use crate::table::Table;
    let mut out = String::new();
    out.push_str(&format!(
        "node      {}\nconfig    {}\ncpus      {} isolated, {} housekeeping, {} total\n",
        plan.node,
        plan.source,
        or_none(&plan.isolated),
        or_none(&plan.housekeeping),
        plan.cpu_count
    ));

    let mut summary = Table::new(["item", "state"]);
    for (key, value) in plan.summary() {
        summary.push([key, value.as_str()]);
    }
    out.push('\n');
    out.push_str(&summary.render());

    out.push_str("\nkernel command line\n");
    if plan.cmdline_changed {
        out.push_str(&format!(
            "  current  {}\n  planned  {}\n",
            crate::log::truncate(&plan.current_cmdline, 88),
            crate::log::truncate(&plan.cmdline, 88)
        ));
    } else {
        out.push_str("  already set\n");
    }

    out.push_str("\ngrub\n");
    out.push_str(&indent(&grub::render_plan(&plan.grub)));

    out.push_str("\ncpu frequency\n");
    if plan.governors.is_empty() {
        out.push_str("  already set\n");
    } else {
        let mut table = Table::new(["cpu", "current", "target"]);
        for action in &plan.governors {
            table.push([
                action.cpu.to_string(),
                action.current.clone().unwrap_or_else(|| "-".to_string()),
                action.target.clone(),
            ]);
        }
        out.push_str(&indent(&table.render()));
    }

    out.push_str("\ncpu offline\n");
    if plan.siblings_to_offline.is_empty() {
        out.push_str("  none\n");
    } else {
        out.push_str(&format!(
            "  {}\n",
            cpulist::format(&plan.siblings_to_offline)
        ));
    }

    out.push_str("\ninterrupts\n");
    out.push_str(&indent(&irq::render_decisions(&plan.irqs, true)));

    out.push_str("\nsysctls\n");
    out.push_str(&indent(&kernel::render_tunables(&plan.tunables)));

    out.push_str("\nkernel threads\n");
    out.push_str(&indent(&kernel::render_pin_commands(
        &plan.pins,
        plan.fifo_priority,
    )));

    out.push_str("\nwarnings\n");
    if plan.warnings.is_empty() {
        out.push_str("  none\n");
    } else {
        for warning in &plan.warnings {
            out.push_str(&format!("  - {warning}\n"));
        }
    }
    out
}

/// Serialise the plan as a JSON document.
pub fn to_json(plan: &Plan) -> String {
    let irqs: Vec<String> = plan
        .irq_changes()
        .iter()
        .map(|decision| {
            format!(
                "{{\"irq\":{},\"name\":{},\"action\":{},\"from\":{},\"to\":{},\"reason\":{}}}",
                decision.irq,
                crate::sysroot::json_string(&decision.label),
                crate::sysroot::json_string(decision.action.as_str()),
                crate::sysroot::json_string(&cpulist::format(&decision.current)),
                crate::sysroot::json_string(&cpulist::format(&decision.target)),
                crate::sysroot::json_string(&decision.reason),
            )
        })
        .collect();
    let tunables: Vec<String> = plan.tunables.iter().map(Tunable::to_json).collect();
    let governors: Vec<String> = plan.governors.iter().map(governor_json).collect();
    let warnings: Vec<String> = plan
        .warnings
        .iter()
        .map(|warning| crate::sysroot::json_string(warning))
        .collect();
    let summary: Vec<String> = plan
        .summary()
        .into_iter()
        .map(|(key, value)| {
            format!(
                "{}:{}",
                crate::sysroot::json_string(key),
                crate::sysroot::json_string(&value)
            )
        })
        .collect();
    let irq_summary: Vec<String> = plan
        .irq_summary()
        .into_iter()
        .map(|(key, value)| format!("{}:{value}", crate::sysroot::json_string(key)))
        .collect();
    let grub_variables: Vec<String> = plan
        .grub
        .changes()
        .iter()
        .map(|change| {
            format!(
                "{{\"key\":{},\"from\":{},\"to\":{}}}",
                crate::sysroot::json_string(&change.key),
                match &change.before {
                    Some(text) => crate::sysroot::json_string(text),
                    None => "null".to_string(),
                },
                crate::sysroot::json_string(&change.after),
            )
        })
        .collect();

    format!(
        "{{\"node\":{},\"source\":{},\"cpu_count\":{},\"isolated\":{},\"housekeeping\":{},\"siblings_to_offline\":{},\"cmdline\":{{\"current\":{},\"planned\":{},\"changed\":{}}},\"writes\":{},\"summary\":{{{}}},\"irq_summary\":{{{}}},\"grub\":{{\"path\":{},\"changed\":{},\"variables\":[{}]}},\"irqs\":[{}],\"sysctls\":[{}],\"governors\":[{}],\"pins\":{},\"warnings\":[{}]}}",
        crate::sysroot::json_string(&plan.node),
        crate::sysroot::json_string(&plan.source),
        plan.cpu_count,
        crate::sysroot::json_string(&cpulist::format(&plan.isolated)),
        crate::sysroot::json_string(&cpulist::format(&plan.housekeeping)),
        crate::sysroot::json_string(&cpulist::format(&plan.siblings_to_offline)),
        crate::sysroot::json_string(&plan.current_cmdline),
        crate::sysroot::json_string(&plan.cmdline),
        plan.cmdline_changed,
        plan.write_count(),
        summary.join(","),
        irq_summary.join(","),
        crate::sysroot::json_string(&plan.grub.path),
        plan.grub.changed,
        grub_variables.join(","),
        irqs.join(","),
        tunables.join(","),
        governors.join(","),
        plan.pins.len(),
        warnings.join(","),
    )
}

/// One governor action as JSON.
fn governor_json(action: &GovernorAction) -> String {
    format!(
        "{{\"cpu\":{},\"from\":{},\"to\":{}}}",
        action.cpu,
        match &action.current {
            Some(text) => crate::sysroot::json_string(text),
            None => "null".to_string(),
        },
        crate::sysroot::json_string(&action.target),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::irq::Action;
    use crate::testing;
    use std::fs;

    fn plan_for(cfg: &Config) -> (testing::Node, Plan) {
        let node = testing::node();
        let plan = build(&node.sysroot, cfg).unwrap();
        (node, plan)
    }

    #[test]
    fn building_a_plan_never_writes_to_the_node() {
        let node = testing::node();
        let _ = build(&node.sysroot, &testing::config()).unwrap();
        assert!(node.sysroot.journal().is_empty(), "plan must be read-only");
    }

    #[test]
    fn plan_collects_every_subsystem() {
        let (node, plan) = plan_for(&testing::config());
        assert_eq!(plan.node, "test-node");
        assert_eq!(plan.source, "fixture.toml");
        assert_eq!(plan.cpu_count, 4);
        assert_eq!(plan.isolated, cpulist::parse("2,3").unwrap());
        assert_eq!(plan.housekeeping, cpulist::parse("0,1").unwrap());

        // The command line gains the isolation parameters, in place.
        assert!(plan.cmdline_changed);
        assert!(plan
            .cmdline
            .starts_with("BOOT_IMAGE=/vmlinuz-6.8.0 root=/dev/nvme0n1p2 ro quiet splash"));
        assert!(plan.cmdline.contains("isolcpus=managed_irq,2,3"));
        assert!(plan.cmdline.ends_with("nohz_full=2,3 rcu_nocbs=2,3"));

        // The engine IRQ is the one that has to move.
        let moved = plan.irq_changes();
        assert_eq!(moved.len(), 1);
        assert_eq!(moved[0].irq, 124);
        assert_eq!(moved[0].action, Action::Override);
        assert_eq!(moved[0].current, cpulist::parse("0-3").unwrap());
        assert_eq!(moved[0].target, cpulist::parse("2,3").unwrap());
        assert_eq!(
            plan.irq_summary().get("ignore"),
            Some(&1),
            "eth0-rx-0 is ignored"
        );
        assert_eq!(
            plan.irq_summary().get("exclude"),
            Some(&2),
            "nvme queues are excluded"
        );

        // Sysctls, governors, kernel threads and the grub file.
        assert!(plan
            .tunables
            .iter()
            .any(|tunable| tunable.name == "kernel.watchdog"));
        assert_eq!(plan.pins.len(), 1);
        assert_eq!(plan.pins[0].name, "kworker/2:1");
        assert_eq!(plan.fifo_priority, None);
        assert!(plan.grub.changed);
        assert_eq!(
            plan.write_count(),
            7,
            "1 irq, 4 sysctls, the grub file and its backup"
        );
        assert!(node.sysroot.journal().is_empty());
    }

    #[test]
    fn a_matching_governor_is_not_planned_again() {
        let node = testing::node();
        let cfg = testing::config();
        assert_eq!(cfg.isolation.governor, "performance");
        let plan = build(&node.sysroot, &cfg).unwrap();
        assert!(
            plan.governors.is_empty(),
            "every CPU already runs performance"
        );

        let mut slow = testing::config();
        slow.isolation.governor = "powersave".to_string();
        let plan = build(&node.sysroot, &slow).unwrap();
        assert_eq!(governor_cpus(&plan), cpulist::parse("2,3").unwrap());
        assert!(plan
            .governors
            .iter()
            .all(|action| action.current.as_deref() == Some("performance")));
        assert_eq!(
            governor_path(2),
            "/sys/devices/system/cpu/cpu2/cpufreq/scaling_governor"
        );
    }

    #[test]
    fn a_typo_in_an_irq_pattern_becomes_a_warning() {
        let node = testing::node();
        let mut cfg = testing::config();
        cfg.irq.exclude.push("nvem*".to_string());
        let plan = build(&node.sysroot, &cfg).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("nvem*")),
            "{:?}",
            plan.warnings
        );
        assert!(
            !plan
                .warnings
                .iter()
                .any(|warning| warning.contains("\"nvme*\"")),
            "used patterns stay silent"
        );
    }

    #[test]
    fn smt_warnings_and_offline_cpus_are_reported() {
        let node = testing::node();
        let mut cfg = testing::config();
        cfg.isolation.smt = true;
        let plan = build(&node.sysroot, &cfg).unwrap();
        assert!(plan.siblings_to_offline.is_empty(), "smt stays online");
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("hyperthread siblings")));

        let node = testing::node();
        fs::write(node.abs("/sys/devices/system/cpu/online"), "0,1\n").unwrap();
        let plan = build(&node.sysroot, &testing::config()).unwrap();
        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("CPU 2 is offline")),
            "{:?}",
            plan.warnings
        );
    }

    #[test]
    fn a_cpu_the_node_does_not_have_is_not_described_as_offline() {
        // A config copied from a bigger machine (or a typo) must not read as if the
        // hardware were merely asleep: CPU 2 is present and online here, CPU 6 does
        // not exist on a four-CPU node.
        let node = testing::node();
        let mut cfg = testing::config();
        cfg.isolation.cpus = "2,6".to_string();
        cfg.isolation.set = cpulist::parse("2,6").unwrap();
        let plan = build(&node.sysroot, &cfg).unwrap();

        assert!(
            plan.warnings
                .iter()
                .any(|warning| warning.contains("CPU 6 is not present on this node (4 CPUs)")),
            "{:?}",
            plan.warnings
        );
        assert!(
            !plan
                .warnings
                .iter()
                .any(|warning| warning.contains("CPU 6 is offline")),
            "a missing CPU is not an offline CPU: {:?}",
            plan.warnings
        );
        assert!(
            !plan
                .warnings
                .iter()
                .any(|warning| warning.contains("CPU 2 is")),
            "cpu2 is present and online: {:?}",
            plan.warnings
        );
    }

    #[test]
    fn absence_is_not_claimed_when_the_node_reports_no_present_set() {
        // Without `/sys/.../present` the suite cannot tell "absent" from "offline",
        // and guessing would produce a warning on every isolated CPU of an old node.
        let node = testing::node();
        fs::remove_file(node.abs("/sys/devices/system/cpu/present")).unwrap();
        fs::write(node.abs("/sys/devices/system/cpu/online"), "0,1\n").unwrap();
        let plan = build(&node.sysroot, &testing::config()).unwrap();
        assert!(
            !plan
                .warnings
                .iter()
                .any(|warning| warning.contains("is not present on this node")),
            "{:?}",
            plan.warnings
        );
        assert!(plan
            .warnings
            .iter()
            .any(|warning| warning.contains("CPU 2 is offline")));
    }

    #[test]
    fn plan_render_has_every_section() {
        let (_node, plan) = plan_for(&testing::config());
        let rendered = render(&plan);
        for section in [
            "kernel command line",
            "grub",
            "cpu frequency",
            "cpu offline",
            "interrupts",
            "sysctls",
            "kernel threads",
            "warnings",
        ] {
            assert!(
                rendered.contains(section),
                "missing {section} in:\n{rendered}"
            );
        }
        assert!(rendered.starts_with("node      test-node\nconfig    fixture.toml\n"));
        assert!(rendered.contains("matching-engine-0"));
        assert!(rendered.contains("kworker/2:1"));
        assert!(rendered.contains("isolcpus=managed_irq,2,3"));
    }

    #[test]
    fn plan_json_describes_the_same_work() {
        let (_node, plan) = plan_for(&testing::config());
        let json = to_json(&plan);
        assert!(json.contains("\"node\":\"test-node\""), "{json}");
        assert!(json.contains("\"cmdline\":{\"current\":\"BOOT_IMAGE"));
        assert!(json.contains("\"changed\":true"));
        assert!(json.contains("\"irq\":124"));
        assert!(json.contains("\"path\":\"/etc/default/grub\""));
        assert!(json.contains("\"pins\":1"));
        assert!(json.contains("\"writes\":7"));
        assert_eq!(json.matches('{').count(), json.matches('}').count());
        assert_eq!(json.matches('[').count(), json.matches(']').count());

        let expected = plan.irq_changes().len()
            + plan
                .tunables
                .iter()
                .filter(|tunable| !tunable.already_set())
                .count()
            + plan.governors.len()
            + plan.siblings_to_offline.len()
            + usize::from(plan.grub.changed)
            + usize::from(plan.grub.changed && !plan.grub.before.is_empty());
        assert_eq!(plan.write_count(), expected);
    }

    #[test]
    fn an_up_to_date_plan_is_recognised() {
        let node = testing::node();
        let cfg = testing::config();
        let plan = build(&node.sysroot, &cfg).unwrap();
        assert!(!plan.is_up_to_date());

        // Apply the file edits and sysctls by hand, then plan again.
        node.sysroot
            .write_text("/etc/default/grub", &plan.grub.after, "test")
            .unwrap();
        for tunable in &plan.tunables {
            node.sysroot
                .write(&tunable.path, &tunable.value, "test")
                .unwrap();
        }
        let change = plan.irq_changes()[0];
        irq::set_affinity(&node.sysroot, change.irq, &change.target, 4, "test").unwrap();

        let again = build(&node.sysroot, &cfg).unwrap();
        assert!(!again.grub.changed, "the file now carries the plan");
        assert!(
            again.cmdline_changed,
            "but the running kernel still boots the old command line, so a reboot is still owed"
        );
        assert!(again.tunables.iter().all(Tunable::already_set));
        assert!(again.irq_changes().is_empty());
        assert_eq!(again.write_count(), 0, "no file writes left to do");
        assert_eq!(again.pins.len(), 1, "the kernel thread still needs taskset");
        assert!(
            !again.is_up_to_date(),
            "a work list item and a reboot are outstanding"
        );
    }

    #[test]
    fn a_plan_for_every_cpu_is_rejected() {
        let node = testing::node();
        let mut cfg = testing::config();
        cfg.isolation.cpus = "0-3".to_string();
        cfg.isolation.set = cpulist::parse("0-3").unwrap();
        cfg.isolation.housekeeping = None;
        // With no explicit housekeeping set, the complement of "all CPUs" is empty
        // and the kernel would have no core left for RCU and device work.
        let err = build(&node.sysroot, &cfg).unwrap_err();
        assert_eq!(err.kind(), Kind::Config);
        assert!(err.message().contains("housekeeping"), "{}", err.message());
    }
}
