//! Phase 1: the inventory of a trading node.
//!
//! The audit is read-only. It answers three questions an operator asks before
//! touching anything:
//!
//! * Is the machine laid out the way the plan assumes (CPU count, SMT siblings,
//!   NUMA, governors)?
//! * Is the running kernel already isolated, and does that match `isolcpus`?
//! * What is currently allowed to interrupt the trading cores?

use crate::config::Config;
use crate::cpulist::{self, CpuSet};
use crate::error::Result;
use crate::irq::{self, InterruptTable};
use crate::kernel::{self, Cmdline, Finding};
use crate::sysroot::SysRoot;
use crate::topology::{self, NumaNode, Topology};

/// How a CPU stands relative to the plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuRole {
    /// An isolated trading core.
    Isolated,
    /// A core kept available for the kernel, IRQs and housekeeping.
    Housekeeping,
    /// A spare core the plan does not use.
    Spare,
    /// The SMT sibling of an isolated core, which must be offlined.
    Sibling,
    /// Already offline.
    Offline,
}

impl CpuRole {
    /// Stable label used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            CpuRole::Isolated => "isolated",
            CpuRole::Housekeeping => "housekeeping",
            CpuRole::Spare => "spare",
            CpuRole::Sibling => "offline-sibling",
            CpuRole::Offline => "offline",
        }
    }
}

/// What one CPU looks like right now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuState {
    /// CPU number.
    pub cpu: u32,
    /// Whether it is online now.
    pub online: bool,
    /// Its current cpufreq governor, when cpufreq is built in.
    pub governor: Option<String>,
    /// True when the running kernel reports the CPU as isolated.
    pub isolated_now: bool,
    /// True when the running kernel has nohz_full on it.
    pub nohz_full_now: bool,
    /// Where the CPU fits in the plan.
    pub role: CpuRole,
    /// The other hyperthread on the same physical core, when known.
    pub sibling: Option<u32>,
}

/// One interrupt as inventoried, with its current steering.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptAudit {
    /// ``/proc/interrupts`` id.
    pub id: String,
    /// Device or pseudo-interrupt name.
    pub label: String,
    /// Current affinity mask (empty when the kernel does not expose one).
    pub affinity: CpuSet,
    /// Total count across every CPU.
    pub hits: u64,
    /// Count that landed on one of the plan's trading cores.
    ///
    /// Measured against the *planned* isolated set, not the running one: before
    /// provisioning those cores are ordinary CPUs, but the traffic they already
    /// carry is exactly what has to be moved away.
    pub hits_on_isolated: u64,
    /// True for kernel-internal interrupts (LOC, NMI, ...).
    pub pseudo: bool,
}

/// The whole inventory.
#[derive(Debug, Clone)]
pub struct Audit {
    /// Node label from the plan, when set.
    pub node: String,
    /// Role label from the plan, when set.
    pub role: Option<String>,
    /// ``/proc/version`` as found.
    pub kernel_version: String,
    /// Number of CPUs the plan reasons about.
    pub cpu_count: usize,
    /// CPUs with per-CPU state.
    pub states: Vec<CpuState>,
    /// CPUs the running kernel reports as isolated.
    pub isolated_now: CpuSet,
    /// CPUs the plan isolates.
    pub isolated_planned: CpuSet,
    /// CPUs kept for housekeeping.
    pub housekeeping: CpuSet,
    /// SMT siblings that must be offlined for the plan to hold.
    pub siblings_to_offline: CpuSet,
    /// NUMA nodes, when the machine exposes them.
    pub numa: Vec<NumaNode>,
    /// The running command line.
    pub cmdline: Cmdline,
    /// Findings about the running command line.
    pub findings: Vec<Finding>,
    /// Interrupts, highest traffic first.
    pub interrupts: Vec<InterruptAudit>,
    /// Movable kernel threads found on the node.
    pub kernel_threads: Vec<kernel::KernelThread>,
}

impl Audit {
    /// CPUs in a given role, in order.
    pub fn cpus_with_role(&self, role: CpuRole) -> CpuSet {
        self.states
            .iter()
            .filter(|state| state.role == role)
            .map(|state| state.cpu)
            .collect()
    }

    /// Interrupts that are currently pointed at an isolated core.
    pub fn interrupts_on_isolated(&self) -> Vec<&InterruptAudit> {
        self.interrupts
            .iter()
            .filter(|entry| {
                !entry.pseudo && cpulist::intersects(&entry.affinity, &self.isolated_planned)
            })
            .collect()
    }

    /// Steerable interrupts that have actually fired on an isolated core.
    ///
    /// Kernel-internal interrupts (``LOC``, ``NMI``, ...) are excluded: they are
    /// part of the kernel's own machinery and only the command line influences
    /// them, so listing them as "noise" would train the operator to ignore the
    /// report.
    pub fn noisy(&self, threshold: u64) -> Vec<&InterruptAudit> {
        self.interrupts
            .iter()
            .filter(|entry| !entry.pseudo && entry.hits_on_isolated >= threshold)
            .collect()
    }

    /// Headline numbers for a report.
    pub fn summary(&self) -> Vec<(&'static str, String)> {
        vec![
            (
                "cpus",
                format!(
                    "{} total, {} online",
                    self.cpu_count,
                    self.states.iter().filter(|s| s.online).count()
                ),
            ),
            ("isolated (planned)", or_none(&self.isolated_planned)),
            ("isolated (running)", or_none(&self.isolated_now)),
            ("housekeeping", or_none(&self.housekeeping)),
            (
                "smt siblings to offline",
                or_none(&self.siblings_to_offline),
            ),
            ("interrupts", format!("{} listed", self.interrupts.len())),
            (
                "movable kernel threads",
                self.kernel_threads.len().to_string(),
            ),
        ]
    }

    /// Serialise the summary as a single-line JSON object.
    pub fn summary_json(&self) -> String {
        let items: Vec<String> = self
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
        format!("{{{}}}", items.join(","))
    }
}

/// ``<list>`` or ``none``.
fn or_none(set: &CpuSet) -> String {
    if set.is_empty() {
        "none".to_string()
    } else {
        cpulist::format(set)
    }
}

/// Inventory a node without changing it.
pub fn run(sysroot: &SysRoot, cfg: &Config) -> Result<Audit> {
    let topology = topology::probe(sysroot);
    let cpu_count = topology.cpu_count().max(1);
    let isolated_planned = cfg.isolated().clone();
    let housekeeping = cfg.housekeeping(cpu_count);
    let cmdline = Cmdline::read(sysroot);

    // The kernel's own view wins: /sys/devices/system/cpu/isolated is what the
    // scheduler is actually doing, while the command line is what was requested.
    let isolated_now = {
        let reported = topology.isolated_now(sysroot);
        if reported.is_empty() {
            kernel::active_isolated(&cmdline)
        } else {
            reported
        }
    };
    let nohz_now = {
        let reported = topology.nohz_full_now(sysroot);
        if reported.is_empty() {
            kernel::active_nohz_full(&cmdline)
        } else {
            reported
        }
    };

    let siblings_to_offline = if cfg.isolation.smt {
        CpuSet::new()
    } else {
        topology.siblings_not_in(&isolated_planned)
    };

    let findings = kernel::inspect_cmdline(&cmdline, cfg, &isolated_planned, &housekeeping);
    let states = cpu_states(
        &topology,
        &isolated_planned,
        &housekeeping,
        &siblings_to_offline,
        &isolated_now,
        &nohz_now,
    );
    let table = irq::read_interrupts(sysroot);
    let interrupts = interrupt_inventory(sysroot, &table, &isolated_planned);

    Ok(Audit {
        node: cfg
            .node
            .name
            .clone()
            .unwrap_or_else(|| "unnamed".to_string()),
        role: cfg.node.role.clone(),
        kernel_version: sysroot.read_trimmed("/proc/version").unwrap_or_default(),
        cpu_count,
        states,
        isolated_now,
        isolated_planned,
        housekeeping,
        siblings_to_offline,
        numa: topology.numa.clone(),
        cmdline,
        findings,
        interrupts,
        kernel_threads: kernel::kernel_threads(sysroot),
    })
}

/// Per-CPU state, one entry per CPU the machine or the plan mentions.
fn cpu_states(
    topology: &Topology,
    isolated: &CpuSet,
    housekeeping: &CpuSet,
    siblings: &CpuSet,
    isolated_now: &CpuSet,
    nohz_now: &CpuSet,
) -> Vec<CpuState> {
    let mut candidates = topology.present.clone();
    for cpu in topology
        .online
        .iter()
        .chain(isolated)
        .chain(housekeeping)
        .chain(siblings)
    {
        candidates.insert(*cpu);
    }
    candidates
        .into_iter()
        .map(|cpu| {
            let node = topology.get(cpu);
            let online = node.map(|node| node.online).unwrap_or(true);
            let role = if !online {
                CpuRole::Offline
            } else if isolated.contains(&cpu) {
                CpuRole::Isolated
            } else if siblings.contains(&cpu) {
                CpuRole::Sibling
            } else if housekeeping.contains(&cpu) {
                CpuRole::Housekeeping
            } else {
                CpuRole::Spare
            };
            CpuState {
                cpu,
                online,
                governor: node.and_then(|node| node.governor.clone()),
                isolated_now: isolated_now.contains(&cpu),
                nohz_full_now: nohz_now.contains(&cpu),
                role,
                sibling: topology
                    .siblings(cpu)
                    .into_iter()
                    .find(|other| *other != cpu),
            }
        })
        .collect()
}

/// Interrupts with their steering and traffic, noisiest isolated traffic first.
fn interrupt_inventory(
    sysroot: &SysRoot,
    table: &InterruptTable,
    isolated: &CpuSet,
) -> Vec<InterruptAudit> {
    let mut entries: Vec<InterruptAudit> = table
        .entries
        .iter()
        .map(|entry| InterruptAudit {
            id: entry.id.clone(),
            label: entry.label(),
            affinity: entry
                .number
                .and_then(|irq| irq::affinity(sysroot, irq))
                .unwrap_or_default(),
            hits: entry.total(),
            hits_on_isolated: entry
                .cpus
                .iter()
                .zip(entry.counts.iter())
                .filter(|(cpu, _)| isolated.contains(cpu))
                .map(|(_, count)| *count)
                .sum(),
            pseudo: entry.is_pseudo(),
        })
        .collect();
    entries.sort_by(|left, right| {
        right
            .hits_on_isolated
            .cmp(&left.hits_on_isolated)
            .then_with(|| right.hits.cmp(&left.hits))
            .then_with(|| left.label.cmp(&right.label))
    });
    entries
}

/// ``yes``/``no`` for report columns.
fn yes_no(value: bool) -> String {
    if value {
        "yes".to_string()
    } else {
        "no".to_string()
    }
}

/// ``"value"`` or ``null``.
fn optional_json(value: Option<&str>) -> String {
    match value {
        Some(text) => crate::sysroot::json_string(text),
        None => "null".to_string(),
    }
}

/// Render the audit as a stack of tables.
pub fn render(audit: &Audit) -> String {
    use crate::table::Table;
    let mut out = String::new();
    out.push_str(&format!("node      {}\n", audit.node));
    if let Some(role) = &audit.role {
        out.push_str(&format!("role      {role}\n"));
    }
    if !audit.kernel_version.is_empty() {
        out.push_str(&format!(
            "kernel    {}\n",
            crate::log::truncate(&audit.kernel_version, 72)
        ));
    }

    let mut summary = Table::new(["item", "value"]);
    for (key, value) in audit.summary() {
        summary.push([key, value.as_str()]);
    }
    out.push('\n');
    out.push_str(&summary.render());

    out.push_str("\nkernel command line\n");
    let mut findings = Table::new(["level", "finding", "detail"]);
    for finding in &audit.findings {
        findings.push([
            finding.level.as_str().to_string(),
            finding.title.clone(),
            crate::log::truncate(&finding.detail, 68),
        ]);
    }
    out.push_str(&findings.render());

    out.push_str("\ncpu layout\n");
    let mut cpus = Table::new([
        "cpu",
        "role",
        "online",
        "isolated",
        "nohz_full",
        "governor",
        "sibling",
    ]);
    for state in &audit.states {
        cpus.push([
            state.cpu.to_string(),
            state.role.as_str().to_string(),
            yes_no(state.online),
            yes_no(state.isolated_now),
            yes_no(state.nohz_full_now),
            state.governor.clone().unwrap_or_else(|| "-".to_string()),
            state
                .sibling
                .map(|cpu| cpu.to_string())
                .unwrap_or_else(|| "-".to_string()),
        ]);
    }
    out.push_str(&cpus.render());

    out.push_str("\ninterrupts on the isolated cores\n");
    let noisy = audit.noisy(1);
    if noisy.is_empty() {
        out.push_str("none\n");
    } else {
        let mut table = Table::new(["irq", "name", "affinity", "hits", "on isolated"]);
        for entry in noisy.iter().take(25) {
            table.push([
                entry.id.clone(),
                crate::log::truncate(&entry.label, 32),
                cpulist::format(&entry.affinity),
                entry.hits.to_string(),
                entry.hits_on_isolated.to_string(),
            ]);
        }
        out.push_str(&table.render());
        if noisy.len() > 25 {
            out.push_str(&format!("... and {} more\n", noisy.len() - 25));
        }
    }

    if !audit.numa.is_empty() {
        out.push_str("\nnuma\n");
        let mut table = Table::new(["node", "cpus", "memory_mb"]);
        for node in &audit.numa {
            table.push([
                node.index.to_string(),
                cpulist::format(&node.cpus),
                node.memory_mb
                    .map(|mb| mb.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            ]);
        }
        out.push_str(&table.render());
    }

    out
}

/// Serialise the audit as a JSON document.
pub fn to_json(audit: &Audit) -> String {
    let states: Vec<String> = audit
        .states
        .iter()
        .map(|state| {
            format!(
                "{{\"cpu\":{},\"role\":{},\"online\":{},\"isolated\":{},\"nohz_full\":{},\"governor\":{},\"sibling\":{}}}",
                state.cpu,
                crate::sysroot::json_string(state.role.as_str()),
                state.online,
                state.isolated_now,
                state.nohz_full_now,
                optional_json(state.governor.as_deref()),
                optional_json(state.sibling.map(|cpu| cpu.to_string()).as_deref()),
            )
        })
        .collect();
    let interrupts: Vec<String> = audit
        .interrupts
        .iter()
        .map(|entry| {
            format!(
                "{{\"irq\":{},\"name\":{},\"affinity\":{},\"hits\":{},\"hits_on_isolated\":{},\"pseudo\":{}}}",
                crate::sysroot::json_string(&entry.id),
                crate::sysroot::json_string(&entry.label),
                crate::sysroot::json_string(&cpulist::format(&entry.affinity)),
                entry.hits,
                entry.hits_on_isolated,
                entry.pseudo,
            )
        })
        .collect();
    let findings: Vec<String> = audit.findings.iter().map(Finding::to_json).collect();
    format!(
        "{{\"node\":{},\"role\":{},\"kernel\":{},\"summary\":{},\"cpu_count\":{},\"isolated_planned\":{},\"isolated_running\":{},\"housekeeping\":{},\"siblings_to_offline\":{},\"findings\":[{}],\"cpus\":[{}],\"interrupts\":[{}]}}",
        crate::sysroot::json_string(&audit.node),
        optional_json(audit.role.as_deref()),
        crate::sysroot::json_string(&audit.kernel_version),
        audit.summary_json(),
        audit.cpu_count,
        crate::sysroot::json_string(&cpulist::format(&audit.isolated_planned)),
        crate::sysroot::json_string(&cpulist::format(&audit.isolated_now)),
        crate::sysroot::json_string(&cpulist::format(&audit.housekeeping)),
        crate::sysroot::json_string(&cpulist::format(&audit.siblings_to_offline)),
        findings.join(","),
        states.join(","),
        interrupts.join(","),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::kernel::FindingLevel;
    use crate::testing;
    use std::fs;

    fn audit() -> Audit {
        run(&testing::node().sysroot, &testing::config()).unwrap()
    }

    fn state(audit: &Audit, cpu: u32) -> &CpuState {
        audit
            .states
            .iter()
            .find(|state| state.cpu == cpu)
            .unwrap_or_else(|| panic!("no state for cpu {cpu}"))
    }

    #[test]
    fn audit_inventories_the_node() {
        let audit = audit();
        assert_eq!(audit.node, "test-node");
        assert_eq!(audit.role.as_deref(), Some("matching-engine"));
        assert_eq!(audit.cpu_count, 4);
        assert_eq!(audit.isolated_planned, cpulist::parse("2,3").unwrap());
        assert_eq!(audit.housekeeping, cpulist::parse("0,1").unwrap());
        assert!(
            audit.isolated_now.is_empty(),
            "the node is not isolated yet"
        );
        assert!(
            audit.siblings_to_offline.is_empty(),
            "both hyperthreads of the core are isolated, so nothing is left over"
        );
        assert!(audit.kernel_version.contains("6.8.0"));
        assert_eq!(audit.kernel_threads.len(), 3, "kthreadd plus two kworkers");
        assert_eq!(audit.cmdline.value_of("root"), Some("/dev/nvme0n1p2"));
    }

    #[test]
    fn audit_assigns_every_cpu_a_role() {
        let audit = audit();
        assert_eq!(state(&audit, 2).role, CpuRole::Isolated);
        assert_eq!(state(&audit, 0).role, CpuRole::Housekeeping);
        assert_eq!(
            audit.cpus_with_role(CpuRole::Isolated),
            cpulist::parse("2,3").unwrap()
        );
        assert_eq!(audit.cpus_with_role(CpuRole::Sibling), CpuSet::new());
        assert_eq!(state(&audit, 2).sibling, Some(3));
        assert_eq!(state(&audit, 0).sibling, Some(1));
        assert_eq!(state(&audit, 0).governor.as_deref(), Some("performance"));
        assert!(!state(&audit, 3).isolated_now);
        assert_eq!(state(&audit, 3).role.as_str(), "isolated");
    }

    #[test]
    fn a_half_isolated_pair_leaves_a_sibling_to_offline() {
        let node = testing::node();
        let mut cfg = testing::config();
        cfg.isolation.cpus = "2".to_string();
        cfg.isolation.set = cpulist::parse("2").unwrap();
        let audit = run(&node.sysroot, &cfg).unwrap();
        assert_eq!(audit.siblings_to_offline, cpulist::parse("3").unwrap());
        assert_eq!(state(&audit, 3).role, CpuRole::Sibling);
        assert_eq!(
            audit.cpus_with_role(CpuRole::Sibling),
            cpulist::parse("3").unwrap()
        );
    }

    #[test]
    fn offline_cpus_are_marked_offline() {
        let node = testing::node();
        fs::write(node.abs("/sys/devices/system/cpu/online"), "0,2,3\n").unwrap();
        let audit = run(&node.sysroot, &testing::config()).unwrap();
        assert_eq!(state(&audit, 1).role, CpuRole::Offline);
        assert!(!state(&audit, 1).online);
        let summary = audit.summary();
        assert!(
            summary
                .iter()
                .any(|(key, value)| *key == "cpus" && value == "4 total, 3 online"),
            "{summary:?}"
        );
    }

    #[test]
    fn audit_reports_findings_and_real_noise_only() {
        let audit = audit();
        assert!(audit
            .findings
            .iter()
            .any(|finding| finding.level == FindingLevel::Error
                && finding.title.contains("not active")));

        let noisy = audit.noisy(1);
        let labels: Vec<&str> = noisy.iter().map(|entry| entry.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["matching-engine-0"],
            "the engine IRQ is the only steerable traffic on the trading cores"
        );
        assert_eq!(noisy[0].hits_on_isolated, 1200);
        assert_eq!(noisy[0].hits, 1200);
        assert_eq!(noisy[0].affinity, cpulist::parse("0-3").unwrap());
        assert_eq!(audit.interrupts_on_isolated().len(), 1);
        assert!(
            audit.interrupts.iter().any(|entry| entry.pseudo),
            "kernel-internal interrupts are inventoried too"
        );
    }

    #[test]
    fn audit_render_and_json_agree() {
        let audit = audit();
        let rendered = render(&audit);
        assert!(
            rendered.starts_with("node      test-node\nrole      matching-engine\n"),
            "{rendered}"
        );
        assert!(rendered.contains("cpu layout"));
        assert!(rendered.contains("offline-sibling") || rendered.contains("isolated"));
        assert!(rendered.contains("interrupts on the isolated cores"));
        assert!(rendered.contains("matching-engine-0"));
        assert!(rendered.contains("kernel command line"));

        let json = to_json(&audit);
        assert!(json.starts_with("{\"node\":\"test-node\""), "{json}");
        assert!(json.contains("\"isolated_running\":\"\""));
        assert!(json.contains("\"cpus\":[{\"cpu\":0"));
        assert!(json.ends_with('}'));
        assert_eq!(json.matches('{').count(), json.matches('}').count());
        assert_eq!(json.matches('[').count(), json.matches(']').count());
    }
}
