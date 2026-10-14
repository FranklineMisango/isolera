//! CPU topology: what the machine actually has, read from sysfs and procfs.
//!
//! Everything here is read-only except the three setters at the bottom
//! ([`set_cpu_online`], [`set_smt_control`], [`set_governor`]), which route
//! through [`SysRoot`] so they obey `--dry-run` and land in the write journal.
//!
//! Topology is *probed*, never assumed: SMT siblings come from
//! ``thread_siblings_list`` when the kernel exposes it, and fall back to the
//! ``siblings``/``core id`` pairs in ``/proc/cpuinfo`` only when sysfs is absent.

use std::collections::BTreeMap;

use crate::cpulist::{self, CpuSet};
use crate::error::Result;
use crate::sysroot::{SysRoot, WriteRecord};

/// One logical CPU as described by ``/proc/cpuinfo``.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuInfo {
    /// Logical CPU index.
    pub cpu: u32,
    /// Physical package (socket) id, when reported.
    pub package: Option<u32>,
    /// Core id within the package, when reported.
    pub core: Option<u32>,
    /// Model name, when reported.
    pub model: Option<String>,
}

/// One logical CPU with its sysfs state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CpuNode {
    /// Logical CPU index.
    pub cpu: u32,
    /// Physical package id, when known.
    pub package: Option<u32>,
    /// Core id within the package, when known.
    pub core: Option<u32>,
    /// CPUs that share this CPU's execution core (includes the CPU itself).
    pub siblings: CpuSet,
    /// Whether the CPU is currently online.
    pub online: bool,
    /// Current ``scaling_governor``, when cpufreq is present.
    pub governor: Option<String>,
}

impl CpuNode {
    /// True when the CPU shares its core with another logical CPU.
    pub fn is_hyperthread(&self) -> bool {
        self.siblings.len() > 1
    }
}

/// One NUMA node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumaNode {
    /// Node index as it appears in ``node<N>``.
    pub index: u32,
    /// CPUs attached to the node.
    pub cpus: CpuSet,
    /// Memory size in MiB when ``MemTotal`` could be parsed.
    pub memory_mb: Option<u64>,
}

/// A snapshot of the machine.
#[derive(Debug, Clone, Default)]
pub struct Topology {
    /// CPUs the kernel may bring online (``present``).
    pub present: CpuSet,
    /// CPUs currently online.
    pub online: CpuSet,
    /// CPUs the kernel was compiled for (``possible``).
    pub possible: CpuSet,
    /// Per-CPU details.
    pub cpus: BTreeMap<u32, CpuNode>,
    /// NUMA nodes, sorted by index.
    pub numa: Vec<NumaNode>,
    /// ``/proc/cpuinfo`` view, for reports.
    pub cpuinfo: Vec<CpuInfo>,
}

impl Topology {
    /// Number of logical CPUs the kernel may use.
    pub fn cpu_count(&self) -> usize {
        if !self.present.is_empty() {
            self.present.len()
        } else if !self.possible.is_empty() {
            self.possible.len()
        } else {
            self.cpus.len()
        }
    }

    /// Look up one CPU.
    pub fn get(&self, cpu: u32) -> Option<&CpuNode> {
        self.cpus.get(&cpu)
    }

    /// True when any CPU has a sibling, i.e. SMT is enabled in firmware.
    pub fn smt_enabled(&self) -> bool {
        self.cpus.values().any(|node| node.is_hyperthread())
    }

    /// Siblings of `cpu`, including `cpu` itself when known.
    pub fn siblings(&self, cpu: u32) -> CpuSet {
        self.cpus
            .get(&cpu)
            .map(|node| node.siblings.clone())
            .unwrap_or_else(|| {
                let mut single = CpuSet::new();
                single.insert(cpu);
                single
            })
    }

    /// Every sibling of every CPU in `set` that is not itself in `set`.
    ///
    /// This is the set that must be offlined for the isolated cores to have
    /// their execution units to themselves.
    pub fn siblings_not_in(&self, set: &CpuSet) -> CpuSet {
        let mut siblings = CpuSet::new();
        for cpu in set {
            for sibling in self.siblings(*cpu) {
                if !set.contains(&sibling) {
                    siblings.insert(sibling);
                }
            }
        }
        siblings
    }

    /// CPUs paired with another core's sibling - i.e. cores whose hyperthread
    /// shares a physical core with an isolated CPU.
    pub fn thread_pairs(&self) -> Vec<(u32, u32)> {
        let mut pairs = Vec::new();
        for cpu in &self.present {
            let siblings: Vec<u32> = self.siblings(*cpu).into_iter().collect();
            for (position, first) in siblings.iter().enumerate() {
                for second in siblings.iter().skip(position + 1) {
                    if first < second {
                        pairs.push((*first, *second));
                    }
                }
            }
        }
        pairs.sort_unstable();
        pairs.dedup();
        pairs
    }

    /// NUMA node holding `cpu`, when the machine is NUMA.
    pub fn numa_of(&self, cpu: u32) -> Option<u32> {
        self.numa
            .iter()
            .find(|node| node.cpus.contains(&cpu))
            .map(|node| node.index)
    }

    /// CPUs isolated according to the *running* kernel.
    pub fn isolated_now(&self, sysroot: &SysRoot) -> CpuSet {
        read_cpulist(sysroot, ISOLATED).unwrap_or_default()
    }

    /// CPUs with nohz_full active according to the running kernel.
    pub fn nohz_full_now(&self, sysroot: &SysRoot) -> CpuSet {
        read_cpulist(sysroot, NOHZ_FULL).unwrap_or_default()
    }
}

/// ``/sys`` paths used by the probe.
const CPU_ROOT: &str = "/sys/devices/system/cpu";
const ISOLATED: &str = "/sys/devices/system/cpu/isolated";
const NOHZ_FULL: &str = "/sys/devices/system/cpu/nohz_full";
const SMT_CONTROL: &str = "/sys/devices/system/cpu/smt/control";
const SMT_ACTIVE: &str = "/sys/devices/system/cpu/smt/active";

fn read_cpulist(sysroot: &SysRoot, path: &str) -> Option<CpuSet> {
    let text = sysroot.read_trimmed(path)?;
    cpulist::parse(&text).ok()
}

/// CPUs the kernel may bring online.
pub fn present_cpus(sysroot: &SysRoot) -> CpuSet {
    read_cpulist(sysroot, &format!("{CPU_ROOT}/present")).unwrap_or_default()
}

/// CPUs the kernel currently has online.
pub fn online_cpus(sysroot: &SysRoot) -> CpuSet {
    read_cpulist(sysroot, &format!("{CPU_ROOT}/online")).unwrap_or_else(|| present_cpus(sysroot))
}

/// Parse ``/proc/cpuinfo`` into per-CPU records.
///
/// Only the fields Isolera reasons about are extracted; unknown fields are
/// ignored so the parser tolerates differences between x86, arm64 and s390.
pub fn parse_cpuinfo(text: &str) -> Vec<CpuInfo> {
    let mut records: Vec<CpuInfo> = Vec::new();
    let mut current: Option<CpuInfo> = None;
    let mut next_index = 0u32;
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            if let Some(record) = current.take() {
                records.push(record);
            }
            continue;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let key = key.trim().to_ascii_lowercase();
        let value = value.trim();
        let record = current.get_or_insert(CpuInfo {
            cpu: next_index,
            package: None,
            core: None,
            model: None,
        });
        match key.as_str() {
            "processor" | "cpu" | "cpu number" => {
                if let Ok(index) = value.parse::<u32>() {
                    record.cpu = index;
                    next_index = index + 1;
                }
            }
            "physical id" => record.package = value.parse().ok(),
            "core id" => record.core = value.parse().ok(),
            "model name" | "cpu model" | "machine" => {
                if record.model.is_none() {
                    record.model = Some(value.to_string());
                }
            }
            _ => {}
        }
    }
    if let Some(record) = current.take() {
        records.push(record);
    }
    records.sort_by_key(|record| record.cpu);
    records.dedup_by_key(|record| record.cpu);
    records
}

/// Group ``/proc/cpuinfo`` records by physical core, for the sysfs fallback.
fn siblings_from_cpuinfo(records: &[CpuInfo]) -> BTreeMap<u32, CpuSet> {
    let mut groups: BTreeMap<(Option<u32>, Option<u32>), CpuSet> = BTreeMap::new();
    for record in records {
        if record.core.is_none() {
            continue;
        }
        groups
            .entry((record.package, record.core))
            .or_default()
            .insert(record.cpu);
    }
    let mut siblings: BTreeMap<u32, CpuSet> = BTreeMap::new();
    for group in groups.values() {
        for cpu in group {
            siblings.insert(*cpu, group.clone());
        }
    }
    siblings
}

/// Read NUMA nodes from ``/sys/devices/system/node``.
fn probe_numa(sysroot: &SysRoot) -> Vec<NumaNode> {
    let mut nodes = Vec::new();
    for entry in sysroot.list_dirs("/sys/devices/system/node") {
        let Some(index) = entry
            .strip_prefix("node")
            .and_then(|text| text.parse::<u32>().ok())
        else {
            continue;
        };
        let cpus = read_cpulist(
            sysroot,
            &format!("/sys/devices/system/node/{entry}/cpulist"),
        )
        .unwrap_or_default();
        let memory_mb = sysroot
            .read_lines(&format!("/sys/devices/system/node/{entry}/meminfo"))
            .iter()
            .find_map(|line| {
                let tokens: Vec<&str> = line.split_whitespace().collect();
                let position = tokens
                    .iter()
                    .position(|token| token.starts_with("MemTotal:"))?;
                let kilobytes: u64 = tokens.get(position + 1)?.parse().ok()?;
                Some(kilobytes / 1024)
            });
        nodes.push(NumaNode {
            index,
            cpus,
            memory_mb,
        });
    }
    nodes.sort_by_key(|node| node.index);
    nodes
}

/// Probe the running kernel (or a fixture sysroot) for its topology.
pub fn probe(sysroot: &SysRoot) -> Topology {
    let present = present_cpus(sysroot);
    let online = online_cpus(sysroot);
    let possible = read_cpulist(sysroot, &format!("{CPU_ROOT}/possible")).unwrap_or_default();
    let cpuinfo = sysroot
        .read_optional("/proc/cpuinfo")
        .map(|text| parse_cpuinfo(&text))
        .unwrap_or_default();
    let fallback_siblings = siblings_from_cpuinfo(&cpuinfo);
    let packages: BTreeMap<u32, Option<u32>> = cpuinfo
        .iter()
        .map(|record| (record.cpu, record.package))
        .collect();
    let cores: BTreeMap<u32, Option<u32>> = cpuinfo
        .iter()
        .map(|record| (record.cpu, record.core))
        .collect();

    let candidates: CpuSet = if present.is_empty() {
        cpuinfo.iter().map(|record| record.cpu).collect()
    } else {
        present.clone()
    };

    let mut cpus = BTreeMap::new();
    for cpu in candidates {
        let siblings = read_cpulist(
            sysroot,
            &format!("{CPU_ROOT}/cpu{cpu}/topology/thread_siblings_list"),
        )
        .or_else(|| fallback_siblings.get(&cpu).cloned())
        .unwrap_or_else(|| {
            let mut single = CpuSet::new();
            single.insert(cpu);
            single
        });
        let governor =
            sysroot.read_trimmed(&format!("{CPU_ROOT}/cpu{cpu}/cpufreq/scaling_governor"));
        cpus.insert(
            cpu,
            CpuNode {
                cpu,
                package: packages.get(&cpu).copied().flatten(),
                core: cores.get(&cpu).copied().flatten(),
                siblings,
                online: online.contains(&cpu),
                governor,
            },
        );
    }

    Topology {
        present,
        online,
        possible,
        cpus,
        numa: probe_numa(sysroot),
        cpuinfo,
    }
}

/// Current value of ``/sys/devices/system/cpu/smt/control``.
pub fn smt_control(sysroot: &SysRoot) -> Option<String> {
    sysroot.read_trimmed(SMT_CONTROL)
}

/// Whether the kernel believes SMT is active.
pub fn smt_active(sysroot: &SysRoot) -> Option<bool> {
    sysroot.read_trimmed(SMT_ACTIVE).map(|text| text == "1")
}

/// Take a CPU online or offline through sysfs.
///
/// Returns `None` when the CPU is already in the requested state.
pub fn set_cpu_online(
    sysroot: &SysRoot,
    cpu: u32,
    online: bool,
    reason: &str,
) -> Result<Option<WriteRecord>> {
    let path = format!("{CPU_ROOT}/cpu{cpu}/online");
    if !sysroot.exists(&path) {
        return Ok(None);
    }
    let value = if online { "1" } else { "0" };
    sysroot.write(&path, value, reason)
}

/// Disable or re-enable SMT through the kernel's own control file.
pub fn set_smt_control(
    sysroot: &SysRoot,
    control: &str,
    reason: &str,
) -> Result<Option<WriteRecord>> {
    if !sysroot.exists(SMT_CONTROL) {
        return Ok(None);
    }
    sysroot.write(SMT_CONTROL, control, reason)
}

/// Set the cpufreq governor for one CPU.
pub fn set_governor(
    sysroot: &SysRoot,
    cpu: u32,
    governor: &str,
    reason: &str,
) -> Result<Option<WriteRecord>> {
    let path = format!("{CPU_ROOT}/cpu{cpu}/cpufreq/scaling_governor");
    if !sysroot.exists(&path) {
        return Ok(None);
    }
    sysroot.write(&path, governor, reason)
}

/// Governors available to one CPU, when cpufreq exposes them.
pub fn available_governors(sysroot: &SysRoot, cpu: u32) -> Vec<String> {
    sysroot
        .read_trimmed(&format!(
            "{CPU_ROOT}/cpu{cpu}/cpufreq/scaling_available_governors"
        ))
        .map(|text| {
            text.split_whitespace()
                .map(|word| word.to_string())
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir::TempDir;
    use std::fs;

    const CPUINFO: &str = "\
processor\t: 0
vendor_id\t: GenuineIntel
model name\t: Intel(R) Xeon(R) Gold 6348 CPU @ 2.60GHz
physical id\t: 0
core id\t\t: 0

processor\t: 1
model name\t: Intel(R) Xeon(R) Gold 6348 CPU @ 2.60GHz
physical id\t: 0
core id\t\t: 1

processor\t: 2
model name\t: Intel(R) Xeon(R) Gold 6348 CPU @ 2.60GHz
physical id\t: 1
core id\t\t: 0

processor\t: 3
model name\t: Intel(R) Xeon(R) Gold 6348 CPU @ 2.60GHz
physical id\t: 1
core id\t\t: 1
";

    fn fixture() -> (TempDir, SysRoot) {
        let dir = TempDir::new("isolera-topology").unwrap();
        let root = dir.path().join("root");
        let cpu_root = root.join("sys/devices/system/cpu");
        fs::create_dir_all(&cpu_root).unwrap();
        fs::write(cpu_root.join("present"), "0-3\n").unwrap();
        fs::write(cpu_root.join("online"), "0-2\n").unwrap();
        fs::write(cpu_root.join("possible"), "0-7\n").unwrap();
        fs::write(cpu_root.join("isolated"), "3\n").unwrap();
        fs::write(cpu_root.join("nohz_full"), "3\n").unwrap();
        for (cpu, siblings) in [(0, "0,2"), (1, "1,3"), (2, "0,2"), (3, "1,3")] {
            let topology = cpu_root.join(format!("cpu{cpu}/topology"));
            fs::create_dir_all(&topology).unwrap();
            fs::write(
                topology.join("thread_siblings_list"),
                format!("{siblings}\n"),
            )
            .unwrap();
            let cpufreq = cpu_root.join(format!("cpu{cpu}/cpufreq"));
            fs::create_dir_all(&cpufreq).unwrap();
            fs::write(cpufreq.join("scaling_governor"), "powersave\n").unwrap();
            fs::write(
                cpufreq.join("scaling_available_governors"),
                "performance powersave\n",
            )
            .unwrap();
        }
        let smt = cpu_root.join("smt");
        fs::create_dir_all(&smt).unwrap();
        fs::write(smt.join("control"), "on\n").unwrap();
        fs::write(smt.join("active"), "1\n").unwrap();

        let node1 = root.join("sys/devices/system/node/node1");
        fs::create_dir_all(&node1).unwrap();
        fs::write(node1.join("cpulist"), "2-3\n").unwrap();
        fs::write(node1.join("meminfo"), "Node 1 MemTotal: 16384000 kB\n").unwrap();
        let node0 = root.join("sys/devices/system/node/node0");
        fs::create_dir_all(&node0).unwrap();
        fs::write(node0.join("cpulist"), "0-1\n").unwrap();
        fs::write(node0.join("meminfo"), "Node 0 MemTotal: 16384000 kB\n").unwrap();

        fs::create_dir_all(root.join("proc")).unwrap();
        fs::write(root.join("proc/cpuinfo"), CPUINFO).unwrap();
        (dir, SysRoot::new(root, false))
    }

    #[test]
    fn cpuinfo_parsing_extracts_index_package_and_core() {
        let records = parse_cpuinfo(CPUINFO);
        assert_eq!(records.len(), 4);
        assert_eq!(records[0].cpu, 0);
        assert_eq!(records[0].package, Some(0));
        assert_eq!(records[0].core, Some(0));
        assert_eq!(records[2].package, Some(1));
        assert_eq!(records[2].core, Some(0));
        assert!(records[0].model.as_deref().unwrap().contains("6348"));
    }

    #[test]
    fn cpuinfo_tolerates_odd_input() {
        assert!(parse_cpuinfo("").is_empty());
        assert!(parse_cpuinfo("nonsense without colon\n").is_empty());
        let single = parse_cpuinfo("processor : 7\ncore id : 3\n");
        assert_eq!(single.len(), 1);
        assert_eq!(single[0].cpu, 7);
        assert_eq!(single[0].core, Some(3));
    }

    #[test]
    fn probe_reads_the_whole_picture() {
        let (_dir, sysroot) = fixture();
        let topology = probe(&sysroot);
        assert_eq!(topology.present, cpulist::parse("0-3").unwrap());
        assert_eq!(topology.online, cpulist::parse("0-2").unwrap());
        assert_eq!(topology.possible, cpulist::parse("0-7").unwrap());
        assert_eq!(topology.cpu_count(), 4);
        assert!(topology.smt_enabled());
        assert!(
            !topology.get(3).unwrap().online,
            "cpu3 is offline in the fixture"
        );
        assert_eq!(
            topology.get(0).unwrap().siblings,
            cpulist::parse("0,2").unwrap()
        );
        assert_eq!(
            topology.get(0).unwrap().governor.as_deref(),
            Some("powersave")
        );
        assert_eq!(topology.siblings(1), cpulist::parse("1,3").unwrap());
        assert_eq!(topology.numa.len(), 2);
        assert_eq!(topology.numa_of(3), Some(1));
        assert_eq!(topology.numa[1].memory_mb, Some(16000));
        assert_eq!(
            topology.isolated_now(&sysroot),
            cpulist::parse("3").unwrap()
        );
        assert_eq!(
            topology.nohz_full_now(&sysroot),
            cpulist::parse("3").unwrap()
        );
    }

    #[test]
    fn siblings_not_in_is_the_offline_set() {
        let (_dir, sysroot) = fixture();
        let topology = probe(&sysroot);
        // Isolating CPU 3 must offline CPU 1 (its sibling) but not CPU 2.
        assert_eq!(
            topology.siblings_not_in(&cpulist::parse("3").unwrap()),
            cpulist::parse("1").unwrap()
        );
        assert!(topology
            .siblings_not_in(&cpulist::parse("0-3").unwrap())
            .is_empty());
        assert_eq!(topology.thread_pairs(), vec![(0, 2), (1, 3)]);
    }

    #[test]
    fn cpu_without_topology_data_is_its_own_sibling() {
        let (_dir, sysroot) = fixture();
        let mut topology = probe(&sysroot);
        topology.cpus.remove(&1);
        assert_eq!(topology.siblings(1), cpulist::parse("1").unwrap());
    }

    #[test]
    fn setters_write_through_the_sysroot() {
        let (_dir, sysroot) = fixture();
        assert!(set_cpu_online(&sysroot, 2, false, "test")
            .unwrap()
            .is_none());

        let record = set_smt_control(&sysroot, "off", "test").unwrap().unwrap();
        assert_eq!(record.previous.as_deref(), Some("on\n"));
        assert_eq!(smt_control(&sysroot).unwrap(), "off");
        assert_eq!(smt_active(&sysroot), Some(true));

        let record = set_governor(&sysroot, 0, "performance", "test")
            .unwrap()
            .unwrap();
        assert_eq!(record.previous.as_deref(), Some("powersave\n"));
        assert_eq!(
            probe(&sysroot).get(0).unwrap().governor.as_deref(),
            Some("performance")
        );
        assert_eq!(
            available_governors(&sysroot, 0),
            vec!["performance", "powersave"]
        );
    }

    #[test]
    fn cpu_online_write_targets_the_right_path() {
        let (dir, sysroot) = fixture();
        let path = dir.path().join("root/sys/devices/system/cpu/cpu2/online");
        std::fs::write(&path, "1\n").unwrap();
        let record = set_cpu_online(&sysroot, 2, false, "test").unwrap().unwrap();
        assert_eq!(record.path, "/sys/devices/system/cpu/cpu2/online");
        assert_eq!(
            sysroot
                .read_trimmed("/sys/devices/system/cpu/cpu2/online")
                .unwrap(),
            "0"
        );
    }

    #[test]
    fn empty_root_probe_is_harmless() {
        let dir = TempDir::new("isolera-empty").unwrap();
        let sysroot = SysRoot::new(dir.path().to_path_buf(), true);
        let topology = probe(&sysroot);
        assert_eq!(topology.cpu_count(), 0);
        assert!(!topology.smt_enabled());
        assert!(topology.numa.is_empty());
        assert_eq!(smt_control(&sysroot), None);
        assert!(available_governors(&sysroot, 0).is_empty());
    }
}
