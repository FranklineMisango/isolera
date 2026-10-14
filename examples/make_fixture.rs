//! Build `tests/fixtures/sysroot`: a four-CPU SMT node, two hyperthread pairs.
//!
//! The layout mirrors `src/testing.rs` so the in-memory fixtures and the on-disk
//! fixture describe the same machine. Cores 2,3 are the trading cores; 0,1 are
//! housekeeping.
//!
//! This is a *fixture builder*, not part of the tool, and it deliberately does not
//! go through `SysRoot`: a bug in the write path must not be able to redefine the
//! data the tests are written against. Plain `std::fs` and the standard library are
//! the whole program.
//!
//! ```sh
//! cargo run --example make_fixture             # writes tests/fixtures/sysroot
//! cargo run --example make_fixture -- /tmp/out # writes somewhere else
//! ```

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

/// `/proc/interrupts` on the fixture node: seven rows, including a kernel-internal
/// `LOC`/`NMI` pair, two idle NVMe queues, a busy `eth0-rx-0` on housekeeping, and
/// `matching-engine-0` firing on a trading core.
const INTERRUPTS: &str = concat!(
    "            CPU0       CPU1       CPU2       CPU3\n",
    "  0:         18          0          0          0   IO-APIC   2-edge      timer\n",
    " 24:          0          0          0          0   IR-PCI-MSI 327680-edge      nvme0q0\n",
    " 25:          0          0          0          0   IR-PCI-MSI 327681-edge      nvme0q1\n",
    "122:     400000     380000          0          0   IR-PCI-MSI 524288-edge      eth0-rx-0\n",
    "124:          0          0       1200          0   IR-PCI-MSI 1048576-edge      matching-engine-0\n",
    "NMI:          5          5          5          5   Non-maskable interrupts\n",
    "LOC:      12000      11000      20000      19000   Local timer interrupts\n",
);

/// Four processors, two per physical core, with real tabs - the same shape
/// `linux::parse_cpuinfo` has to survive on a live node.
const CPUINFO: &str = concat!(
    "processor\t: 0\n",
    "model name\t: Xeon Test\n",
    "physical id\t: 0\n",
    "core id\t\t: 0\n",
    "cpu cores\t: 2\n",
    "\n",
    "processor\t: 1\n",
    "model name\t: Xeon Test\n",
    "physical id\t: 0\n",
    "core id\t\t: 0\n",
    "cpu cores\t: 2\n",
    "\n",
    "processor\t: 2\n",
    "model name\t: Xeon Test\n",
    "physical id\t: 0\n",
    "core id\t\t: 1\n",
    "cpu cores\t: 2\n",
    "\n",
    "processor\t: 3\n",
    "model name\t: Xeon Test\n",
    "physical id\t: 0\n",
    "core id\t\t: 1\n",
    "cpu cores\t: 2\n",
);

/// Comments, a blank line and a pre-existing `GRUB_CMDLINE_LINUX`, so cmdline
/// merging is exercised rather than assumed.
const GRUB: &str = concat!(
    "# Written by Isolera's fixture generator; see tests/fixtures/README.md.\n",
    "GRUB_DEFAULT=0\n",
    "GRUB_TIMEOUT=5\n",
    "\n",
    "GRUB_CMDLINE_LINUX=\"quiet splash\"\n",
);

/// The fixture, as one table of logical path -> content.
///
/// Logical paths are absolute, exactly as `SysRoot` sees them, so the tree can be
/// dropped under any root.
#[derive(Debug)]
struct Fixture {
    files: Vec<(String, String)>,
}

impl Fixture {
    /// An empty fixture.
    fn new() -> Fixture {
        Fixture { files: Vec::new() }
    }

    /// Add one file.
    fn add(&mut self, logical: &str, content: &str) {
        self.files.push((logical.to_string(), content.to_string()));
    }

    /// Number of files; the generator prints it so a shrinking fixture is visible.
    fn len(&self) -> usize {
        self.files.len()
    }

    /// Write the whole tree under `root`, creating directories as needed.
    fn write(&self, root: &Path) -> std::io::Result<()> {
        for (logical, content) in &self.files {
            // `/proc/x` becomes `<root>/proc/x`: the root prefix replaces the
            // leading slash.
            let path = root.join(logical.trim_start_matches('/'));
            if let Some(parent) = path.parent() {
                fs::create_dir_all(parent)?;
            }
            fs::write(&path, content)?;
        }
        Ok(())
    }
}

/// Every file the fixture needs.
///
/// `proc/irq/*/smp_affinity_list` exists only for the four IRQs above, so steering
/// is exercised without a driver; `proc/{2,15,16}` gives the plan a `kthreadd` to
/// keep and a `kworker/2:1` sitting on a trading core to move with `taskset`.
fn fixture() -> Fixture {
    let mut fixture = Fixture::new();
    fixture.add(
        "/proc/cmdline",
        "BOOT_IMAGE=/vmlinuz-6.8.0 root=/dev/nvme0n1p2 ro quiet splash\n",
    );
    fixture.add("/proc/interrupts", INTERRUPTS);
    fixture.add("/proc/cpuinfo", CPUINFO);
    fixture.add(
        "/proc/version",
        "Linux version 6.8.0-isolera (gcc 13.2.0) #1 SMP PREEMPT_RT\n",
    );
    fixture.add("/proc/irq/24/smp_affinity_list", "0\n");
    fixture.add("/proc/irq/25/smp_affinity_list", "0\n");
    fixture.add("/proc/irq/122/smp_affinity_list", "0,1\n");
    fixture.add("/proc/irq/124/smp_affinity_list", "0-3\n");
    fixture.add("/proc/sys/kernel/watchdog", "1\n");
    fixture.add("/proc/sys/kernel/nmi_watchdog", "1\n");
    fixture.add("/proc/sys/kernel/timer_migration", "1\n");
    fixture.add("/proc/sys/kernel/sched_rt_runtime_us", "950000\n");
    fixture.add("/sys/devices/system/cpu/present", "0-3\n");
    fixture.add("/sys/devices/system/cpu/online", "0-3\n");
    fixture.add("/sys/devices/system/cpu/possible", "0-3\n");
    // Empty on purpose: this is what an unprovisioned node looks like.
    fixture.add("/sys/devices/system/cpu/isolated", "\n");
    fixture.add("/sys/devices/system/cpu/nohz_full", "\n");
    fixture.add("/sys/devices/system/cpu/smt/control", "on\n");
    fixture.add("/sys/devices/system/cpu/smt/active", "1\n");
    fixture.add("/etc/default/grub", GRUB);

    // Two hyperthread pairs: 0,1 are one physical core, 2,3 are the other. Both
    // trading cores are siblings of each other, so this plan needs no offlining;
    // isolate 2 alone and cpu3 becomes a sibling that must be offlined.
    for cpu in 0..4 {
        let siblings = if cpu < 2 { "0,1" } else { "2,3" };
        let base = format!("/sys/devices/system/cpu/cpu{cpu}");
        fixture.add(&format!("{base}/online"), "1\n");
        fixture.add(&format!("{base}/cpufreq/scaling_governor"), "performance\n");
        fixture.add(
            &format!("{base}/cpufreq/scaling_available_governors"),
            "performance powersave\n",
        );
        fixture.add(
            &format!("{base}/topology/thread_siblings_list"),
            &format!("{siblings}\n"),
        );
        fixture.add(
            &format!("{base}/topology/core_id"),
            &format!("{}\n", cpu % 2),
        );
    }

    for (pid, name, cpus) in [
        ("2", "kthreadd", "0-3"),
        ("15", "kworker/2:1", "2-3"),
        ("16", "kworker/0:2", "0-1"),
    ] {
        fixture.add(&format!("/proc/{pid}/comm"), &format!("{name}\n"));
        // Kernel threads have an empty command line; identity comes from `comm`.
        fixture.add(&format!("/proc/{pid}/cmdline"), "");
        fixture.add(
            &format!("/proc/{pid}/status"),
            &format!("Name:\t{name}\nCpus_allowed_list:\t{cpus}\n"),
        );
    }

    fixture
}

/// Write the fixture, defaulting to `tests/fixtures/sysroot` in the repository.
fn main() -> ExitCode {
    let argument = env::args().nth(1);
    if matches!(argument.as_deref(), Some("-h" | "--help")) {
        println!("usage: cargo run --example make_fixture [-- <dir>]");
        println!("builds the four-CPU SMT fixture (default: tests/fixtures/sysroot)");
        return ExitCode::SUCCESS;
    }
    let root = match argument {
        Some(dir) => PathBuf::from(dir),
        // Anchored to the manifest, so it does not matter where it is run from.
        None => Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sysroot"),
    };

    let fixture = fixture();
    match fixture.write(&root) {
        Ok(()) => {
            println!("wrote {} files under {}", fixture.len(), root.display());
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("isolera: cannot write {}: {error}", root.display());
            ExitCode::FAILURE
        }
    }
}
