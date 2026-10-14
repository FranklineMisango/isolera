//! A synthetic Linux node for the test-suite.
//!
//! Every module's tests need the same thing: a sysroot that looks enough like a
//! four-CPU SMT machine (`0,1` a hyperthread pair, `2,3` the trading cores) to
//! exercise cmdline merging, IRQ steering, sysctls and topology walking. Building
//! it here keeps the fixtures consistent between modules, and makes it obvious
//! that the whole suite runs without privileges.

use std::fs;
use std::path::Path;

use crate::config::Config;
use crate::error::Result;
use crate::sysroot::SysRoot;
use crate::tempdir::TempDir;

/// The interrupt table a fixture node reports.
pub const INTERRUPTS: &str = "\
            CPU0       CPU1       CPU2       CPU3
  0:         18          0          0          0   IO-APIC   2-edge      timer
 24:          0          0          0          0   IR-PCI-MSI 327680-edge      nvme0q0
 25:          0          0          0          0   IR-PCI-MSI 327681-edge      nvme0q1
122:     400000     380000          0          0   IR-PCI-MSI 524288-edge      eth0-rx-0
124:          0          0       1200          0   IR-PCI-MSI 1048576-edge      matching-engine-0
NMI:          5          5          5          5   Non-maskable interrupts
LOC:      12000      11000      20000      19000   Local timer interrupts
";

/// ``/proc/cmdline`` for a node that is not isolated yet.
pub const CMDLINE: &str = "BOOT_IMAGE=/vmlinuz-6.8.0 root=/dev/nvme0n1p2 ro quiet splash";

/// The fixture plan, exposed so tests can copy and edit it.
pub const PLAN: &str = "\
[node]
name = \"test-node\"
role = \"matching-engine\"

[isolation]
cpus = \"2,3\"
housekeeping = \"0,1\"
governor = \"performance\"
nohz_full = true
rcu_nocbs = true
managed_irq = true

[irq]
exclude = [\"nvme*\"]
ignore = [\"eth*\"]
overrides = { \"matching-engine-*\" = \"2,3\" }

[kernel.sysctls]
\"kernel.watchdog\" = \"0\"
\"kernel.nmi_watchdog\" = \"0\"

[grub]
file = \"/etc/default/grub\"
cmdline_variable = \"GRUB_CMDLINE_LINUX\"

[grub.variables]
GRUB_TIMEOUT = \"1\"

[rt]
pin_kernel_threads = true
keep = [\"kthreadd\"]
rt_runtime_us = -1

[verify]
budget_us = 80
interval_us = 200
duration_s = 1
";

/// A node plus the temp dir that owns it.
#[derive(Debug)]
pub struct Node {
    /// Keeps the tree alive for the duration of the test.
    pub dir: TempDir,
    /// Root view used for reads and writes.
    pub sysroot: SysRoot,
    /// The same tree in dry-run mode, for plan-only assertions.
    pub dry: SysRoot,
}

impl Node {
    /// Path of a file inside the fixture, for assertions that bypass the sysroot.
    pub fn abs(&self, logical: &str) -> std::path::PathBuf {
        self.sysroot.abs(logical)
    }
}

/// Build a four-CPU SMT node with two hyperthread pairs.
pub fn node() -> Node {
    build(true)
}

/// Build the same node with SMT disabled (siblings already offline).
pub fn node_without_smt() -> Node {
    build(false)
}

/// A plan for the fixture node: cores 2,3 isolated, 0,1 for housekeeping.
pub fn config() -> Config {
    Config::from_str_named(PLAN, "fixture.toml").expect("fixture config")
}

fn build(smt: bool) -> Node {
    let dir = TempDir::new("isolera-node").expect("temp dir");
    let root = dir.path().join("root");
    write_tree(&root, smt).expect("fixture tree");
    let sysroot = SysRoot::new(&root, false);
    let dry = SysRoot::new(&root, true);
    Node { dir, sysroot, dry }
}

/// ``/proc/cpuinfo`` for the four-CPU fixture.
const CPUINFO: &str = "\
processor\t: 0
model name\t: Xeon Test
physical id\t: 0
core id\t\t: 0
cpu cores\t: 2

processor\t: 1
model name\t: Xeon Test
physical id\t: 0
core id\t\t: 0
cpu cores\t: 2

processor\t: 2
model name\t: Xeon Test
physical id\t: 0
core id\t\t: 1
cpu cores\t: 2

processor\t: 3
model name\t: Xeon Test
physical id\t: 0
core id\t\t: 1
cpu cores\t: 2
";

fn write_tree(root: &Path, smt: bool) -> Result<()> {
    for cpu in 0..4 {
        let base = root.join(format!("sys/devices/system/cpu/cpu{cpu}"));
        fs::create_dir_all(base.join("topology"))?;
        fs::create_dir_all(base.join("cpufreq"))?;
        fs::write(base.join("online"), "1\n")?;
        fs::write(base.join("cpufreq/scaling_governor"), "performance\n")?;
        let siblings = if cpu < 2 { "0,1" } else { "2,3" };
        fs::write(
            base.join("topology/thread_siblings_list"),
            format!("{siblings}\n"),
        )?;
        fs::write(base.join("topology/core_id"), format!("{}\n", cpu % 2))?;
    }
    fs::write(root.join("sys/devices/system/cpu/present"), "0-3\n")?;
    fs::write(root.join("sys/devices/system/cpu/online"), "0-3\n")?;
    fs::create_dir_all(root.join("sys/devices/system/cpu/smt"))?;
    fs::write(root.join("sys/devices/system/cpu/smt/control"), "on\n")?;
    fs::write(
        root.join("sys/devices/system/cpu/smt/active"),
        if smt { "1\n" } else { "0\n" },
    )?;
    fs::write(root.join("sys/devices/system/cpu/isolated"), "\n")?;

    for (irq, affinity) in [("24", "0"), ("25", "0"), ("122", "0,1"), ("124", "0-3")] {
        let base = root.join(format!("proc/irq/{irq}"));
        fs::create_dir_all(&base)?;
        fs::write(base.join("smp_affinity_list"), format!("{affinity}\n"))?;
    }
    fs::write(root.join("proc/interrupts"), INTERRUPTS)?;
    fs::write(root.join("proc/cmdline"), CMDLINE)?;
    fs::write(root.join("proc/cpuinfo"), CPUINFO)?;
    fs::write(
        root.join("proc/version"),
        "Linux version 6.8.0-isolera (gcc 13.2.0) #1 SMP PREEMPT_RT\n",
    )?;

    for (relative, value) in [
        ("kernel/sched_rt_runtime_us", "950000"),
        ("kernel/timer_migration", "1"),
        ("kernel/watchdog", "1"),
        ("kernel/nmi_watchdog", "1"),
    ] {
        let path = root.join("proc/sys").join(relative);
        fs::create_dir_all(path.parent().expect("parent"))?;
        fs::write(path, format!("{value}\n"))?;
    }

    for (pid, name, cpus) in [
        ("2", "kthreadd", "0-3"),
        ("15", "kworker/2:1", "2-3"),
        ("16", "kworker/0:2", "0-1"),
    ] {
        let base = root.join(format!("proc/{pid}"));
        fs::create_dir_all(&base)?;
        fs::write(base.join("comm"), format!("{name}\n"))?;
        fs::write(base.join("cmdline"), "")?;
        fs::write(
            base.join("status"),
            format!("Name:\t{name}\nCpus_allowed_list:\t{cpus}\n"),
        )?;
    }

    fs::create_dir_all(root.join("etc/default"))?;
    fs::write(
        root.join("etc/default/grub"),
        "# fixture\nGRUB_DEFAULT=0\nGRUB_TIMEOUT=5\n\nGRUB_CMDLINE_LINUX=\"quiet splash\"\n",
    )?;
    Ok(())
}
