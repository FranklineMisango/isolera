//! Phase 3: prove the node really is isolated.
//!
//! Verification is the part that keeps an operator honest. It compares what the
//! plan *asked* for with what the running kernel is *doing*, in five places:
//!
//! 1. the command line (via [`kernel::inspect_cmdline`]),
//! 2. the kernel's own isolated/``nohz_full`` view under ``/sys``,
//! 3. SMT siblings that should be offline,
//! 4. the cpufreq governor on the trading cores,
//! 5. interrupt steering and, through two samples of ``/proc/interrupts``, actual
//!    interrupt traffic on the trading cores.
//!
//! Counters are monotonic, so traffic is judged from the *delta* between two
//! samples: a cumulative total says nothing about the last minute.

use crate::config::{Config, VerifySection};
use crate::cpulist::{self, CpuSet};
use crate::error::Result;
use crate::irq::{self, InterruptTable};
use crate::kernel::{self, FindingLevel};
use crate::log::Log;
use crate::sysroot::SysRoot;
use crate::topology;

/// One verification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Check {
    /// Stable check name, e.g. ``cmdline``.
    pub name: String,
    /// Severity.
    pub level: FindingLevel,
    /// What was found.
    pub detail: String,
}

impl Check {
    /// Build a check result.
    pub fn new(name: &str, level: FindingLevel, detail: impl Into<String>) -> Check {
        Check {
            name: name.to_string(),
            level,
            detail: detail.into(),
        }
    }

    /// Serialise as a single-line JSON object.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"check\":{},\"level\":{},\"detail\":{}}}",
            crate::sysroot::json_string(&self.name),
            crate::sysroot::json_string(self.level.as_str()),
            crate::sysroot::json_string(&self.detail),
        )
    }
}

/// How long and how strictly to verify.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    /// Seconds between the two ``/proc/interrupts`` samples.
    pub sample_seconds: u64,
    /// Interrupts per second on a trading core above which a check fails.
    pub irq_rate_per_second: f64,
}

impl Options {
    /// Derive the sampling window from the plan.
    ///
    /// The verify section's duration is meant for `cyclictest`, so it is clamped:
    /// a verification run should answer in seconds, not minutes.
    pub fn from_config(verify: &VerifySection) -> Options {
        Options {
            sample_seconds: verify.duration_s.clamp(1, 30),
            irq_rate_per_second: verify.irq_rate_per_second,
        }
    }
}

impl Default for Options {
    fn default() -> Self {
        Options {
            sample_seconds: 5,
            irq_rate_per_second: 1.0,
        }
    }
}

/// The result of a verification run.
#[derive(Debug, Clone)]
pub struct Verification {
    /// Node label.
    pub node: String,
    /// Every check, in the order it ran.
    pub checks: Vec<Check>,
    /// Interrupts that fired on a trading core during the sample window.
    pub irq_noise: Vec<(String, String, u64)>,
    /// Length of the sample window in seconds.
    pub sample_seconds: u64,
}

impl Verification {
    /// True when nothing failed.
    pub fn passed(&self) -> bool {
        self.checks
            .iter()
            .all(|check| check.level != FindingLevel::Error)
    }

    /// Checks that failed.
    pub fn errors(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|check| check.level == FindingLevel::Error)
            .collect()
    }

    /// Checks that warn.
    pub fn warnings(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|check| check.level == FindingLevel::Warn)
            .collect()
    }

    /// True when a named check passed, for scripts.
    pub fn ok(&self, name: &str) -> bool {
        self.checks
            .iter()
            .filter(|check| check.name == name)
            .all(|check| check.level == FindingLevel::Ok)
    }

    /// Render the verification as a report.
    pub fn render(&self) -> String {
        use crate::table::Table;
        let mut out = format!("node      {}\n", self.node);
        out.push_str(&format!(
            "verdict   {}\n",
            if self.passed() {
                "isolated"
            } else {
                "NOT isolated"
            }
        ));
        let mut table = Table::new(["check", "result", "detail"]);
        for check in &self.checks {
            table.push([
                check.name.clone(),
                check.level.as_str().to_string(),
                crate::log::truncate(&check.detail, 72),
            ]);
        }
        out.push('\n');
        out.push_str(&table.render());

        out.push_str(&format!(
            "\ninterrupt traffic over {}s\n",
            self.sample_seconds
        ));
        if self.irq_noise.is_empty() {
            out.push_str("no steerable interrupt fired on a trading core\n");
        } else {
            let mut table = Table::new(["irq", "name", "hits"]);
            for (id, label, hits) in self.irq_noise.iter().take(25) {
                table.push([
                    id.clone(),
                    crate::log::truncate(label, 32),
                    hits.to_string(),
                ]);
            }
            out.push_str(&table.render());
        }
        out
    }

    /// Serialise the verification as a JSON document.
    pub fn to_json(&self) -> String {
        let checks: Vec<String> = self.checks.iter().map(Check::to_json).collect();
        let noise: Vec<String> = self
            .irq_noise
            .iter()
            .map(|(id, label, hits)| {
                format!(
                    "{{\"irq\":{},\"name\":{},\"hits\":{hits}}}",
                    crate::sysroot::json_string(id),
                    crate::sysroot::json_string(label),
                )
            })
            .collect();
        format!(
            "{{\"node\":{},\"passed\":{},\"sample_seconds\":{},\"checks\":[{}],\"irq_noise\":[{}]}}",
            crate::sysroot::json_string(&self.node),
            self.passed(),
            self.sample_seconds,
            checks.join(","),
            noise.join(","),
        )
    }
}

/// Verify a node against the plan.
pub fn run(sysroot: &SysRoot, cfg: &Config, options: &Options, log: &Log) -> Result<Verification> {
    let probe = topology::probe(sysroot);
    let cpu_count = probe.cpu_count().max(1);
    let isolated = cfg.isolated().clone();
    let housekeeping = cfg.housekeeping(cpu_count);
    let mut checks: Vec<Check> = Vec::new();

    log.step("kernel cmdline");
    let cmdline = kernel::Cmdline::read(sysroot);
    for finding in kernel::inspect_cmdline(&cmdline, cfg, &isolated, &housekeeping) {
        checks.push(Check::new("cmdline", finding.level, finding.title));
    }
    for finding in kernel::managed_drift(&cmdline, cfg, &isolated) {
        checks.push(Check::new("cmdline", finding.level, finding.title));
    }

    log.step("kernel isolation state");
    checks.extend(isolation_checks(sysroot, cfg, &probe, &isolated));

    log.step("cpu governor");
    checks.push(governor_check(&probe, cfg, &isolated));

    log.step("sysctls");
    checks.push(sysctl_check(sysroot, cfg)?);

    log.step("interrupt steering");
    let before = irq::read_interrupts(sysroot);
    let policy = irq::Policy::from_config(cfg, cpu_count)?;
    checks.push(steering_check(sysroot, &before, &policy, &isolated));

    log.step(format!(
        "sampling interrupt traffic for {}s",
        options.sample_seconds
    ));
    std::thread::sleep(std::time::Duration::from_secs(options.sample_seconds));
    let after = irq::read_interrupts(sysroot);
    let noise = irq::isolated_deltas(&before, &after, &isolated);
    checks.push(noise_check(&noise_offenders(&noise, options), options));

    Ok(Verification {
        node: cfg
            .node
            .name
            .clone()
            .unwrap_or_else(|| "unnamed".to_string()),
        checks,
        irq_noise: noise,
        sample_seconds: options.sample_seconds,
    })
}

/// Check the kernel's own view of isolation and SMT.
fn isolation_checks(
    sysroot: &SysRoot,
    cfg: &Config,
    probe: &topology::Topology,
    isolated: &CpuSet,
) -> Vec<Check> {
    let mut checks = Vec::new();
    let reported = probe.isolated_now(sysroot);
    if reported.is_empty() {
        checks.push(Check::new(
            "isolated-sysfs",
            FindingLevel::Warn,
            "the kernel does not report any isolated CPU under /sys/devices/system/cpu/isolated",
        ));
    } else if reported == *isolated {
        checks.push(Check::new(
            "isolated-sysfs",
            FindingLevel::Ok,
            format!(
                "the kernel reports {} as isolated",
                cpulist::format(&reported)
            ),
        ));
    } else {
        checks.push(Check::new(
            "isolated-sysfs",
            FindingLevel::Error,
            format!(
                "the kernel reports {} as isolated, the plan asks for {}",
                cpulist::format(&reported),
                cpulist::format(isolated)
            ),
        ));
    }

    if cfg.isolation.nohz_full {
        let nohz = probe.nohz_full_now(sysroot);
        if nohz.is_empty() {
            checks.push(Check::new(
                "nohz-full",
                FindingLevel::Warn,
                "the kernel reports no nohz_full CPUs yet",
            ));
        } else if nohz == *isolated {
            checks.push(Check::new(
                "nohz-full",
                FindingLevel::Ok,
                format!("nohz_full covers {}", cpulist::format(&nohz)),
            ));
        } else {
            checks.push(Check::new(
                "nohz-full",
                FindingLevel::Warn,
                format!(
                    "nohz_full covers {} instead of {}",
                    cpulist::format(&nohz),
                    cpulist::format(isolated)
                ),
            ));
        }
    }

    let siblings = if cfg.isolation.smt {
        CpuSet::new()
    } else {
        probe.siblings_not_in(isolated)
    };
    let online: CpuSet = siblings
        .iter()
        .filter(|cpu| probe.online.contains(cpu))
        .copied()
        .collect();
    if online.is_empty() {
        checks.push(Check::new(
            "smt-siblings",
            FindingLevel::Ok,
            if siblings.is_empty() {
                "every trading core has both hyperthreads isolated".to_string()
            } else {
                format!("{} offline as planned", cpulist::format(&siblings))
            },
        ));
    } else {
        checks.push(Check::new(
            "smt-siblings",
            FindingLevel::Error,
            format!(
                "{} share a physical core with a trading core and are still online",
                cpulist::format(&online)
            ),
        ));
    }
    checks
}

/// Check the governor on every trading core.
fn governor_check(probe: &topology::Topology, cfg: &Config, isolated: &CpuSet) -> Check {
    let target = cfg.isolation.governor.trim();
    let wrong: Vec<String> = isolated
        .iter()
        .filter_map(|cpu| probe.get(*cpu).map(|node| (*cpu, node.governor.clone())))
        .filter(|(_, governor)| governor.as_deref() != Some(target))
        .map(|(cpu, governor)| {
            format!(
                "cpu{cpu}={}",
                governor.unwrap_or_else(|| "unknown".to_string())
            )
        })
        .collect();
    if wrong.is_empty() {
        Check::new(
            "governor",
            FindingLevel::Ok,
            format!("every trading core runs {target}"),
        )
    } else {
        Check::new(
            "governor",
            FindingLevel::Warn,
            format!("{target} expected, found {}", wrong.join(", ")),
        )
    }
}

/// Check that the tuned sysctls are in place.
fn sysctl_check(sysroot: &SysRoot, cfg: &Config) -> Result<Check> {
    let plan = kernel::sysctl_plan(sysroot, cfg)?;
    let pending: Vec<String> = plan
        .iter()
        .filter(|tunable| !tunable.already_set())
        .map(|tunable| tunable.name.clone())
        .collect();
    if pending.is_empty() {
        Ok(Check::new(
            "sysctls",
            FindingLevel::Ok,
            format!("{} tuned value(s) in place", plan.len()),
        ))
    } else {
        Ok(Check::new(
            "sysctls",
            FindingLevel::Warn,
            format!("not applied: {}", pending.join(", ")),
        ))
    }
}

/// Check that nothing but explicitly allowed interrupts reaches a trading core.
///
/// An `irq.overrides` entry is the operator saying "this device belongs on the
/// trading cores" - a feed handler or the matching engine itself. Those are not
/// noise; anything *else* aimed at those cores is.
fn steering_check(
    sysroot: &SysRoot,
    table: &InterruptTable,
    policy: &irq::Policy,
    isolated: &CpuSet,
) -> Check {
    let decisions = irq::plan(sysroot, table, policy);
    let mut offenders: Vec<String> = Vec::new();
    let mut allowed: Vec<String> = Vec::new();
    for decision in &decisions {
        if !cpulist::intersects(&decision.current, isolated) {
            continue;
        }
        match decision.action {
            irq::Action::Override => allowed.push(decision.label.clone()),
            irq::Action::Ignore => {}
            _ => offenders.push(format!("{} ({})", decision.label, decision.action.as_str())),
        }
    }
    if offenders.is_empty() {
        let detail = if allowed.is_empty() {
            "no interrupt is bound to a trading core".to_string()
        } else {
            format!(
                "the only interrupts on {} are explicitly allowed: {}",
                cpulist::format(isolated),
                crate::log::truncate(&allowed.join(", "), 48)
            )
        };
        Check::new("irq-affinity", FindingLevel::Ok, detail)
    } else {
        Check::new(
            "irq-affinity",
            FindingLevel::Error,
            format!(
                "{} interrupt(s) bound to {} without an override: {}",
                offenders.len(),
                cpulist::format(isolated),
                crate::log::truncate(&offenders.join(", "), 48)
            ),
        )
    }
}

/// Interrupts whose sampled traffic exceeds the configured budget.
///
/// `hits` are counts over the sample window, so the budget is the per-second limit
/// scaled by that window.
pub fn noise_offenders(
    noise: &[(String, String, u64)],
    options: &Options,
) -> Vec<(String, String, u64)> {
    let budget = options.irq_rate_per_second * options.sample_seconds.max(1) as f64;
    noise
        .iter()
        .filter(|(_, _, hits)| (*hits as f64) > budget)
        .cloned()
        .collect()
}

/// Turn the sampled traffic into a verdict.
fn noise_check(offenders: &[(String, String, u64)], options: &Options) -> Check {
    if offenders.is_empty() {
        return Check::new(
            "irq-traffic",
            FindingLevel::Ok,
            format!(
                "no interrupt exceeded {:.1}/s on a trading core",
                options.irq_rate_per_second
            ),
        );
    }
    let names: Vec<String> = offenders
        .iter()
        .map(|(_, label, hits)| format!("{label} ({hits})"))
        .collect();
    Check::new(
        "irq-traffic",
        FindingLevel::Error,
        format!(
            "{} interrupt(s) fired on a trading core: {}",
            offenders.len(),
            crate::log::truncate(&names.join(", "), 56)
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::apply;
    use crate::plan;
    use crate::testing;
    use std::fs;

    /// Verifying without waiting: the sample window is one second of real time.
    fn quick() -> Options {
        Options {
            sample_seconds: 1,
            irq_rate_per_second: 1.0,
        }
    }

    #[test]
    fn an_unprovisioned_node_fails_verification() {
        let node = testing::node();
        let cfg = testing::config();
        let report = run(&node.sysroot, &cfg, &quick(), &Log::quiet()).unwrap();

        assert!(!report.passed());
        assert!(report.errors().iter().any(|check| check.name == "cmdline"));
        assert!(
            report
                .errors()
                .iter()
                .any(|check| check.detail.contains("not active")),
            "{:?}",
            report.errors()
        );
        assert!(report
            .warnings()
            .iter()
            .any(|check| check.name == "sysctls"));
        assert_eq!(report.node, "test-node");
        assert!(
            report.ok("irq-affinity"),
            "the engine's own IRQ is allowed on the trading cores by irq.overrides: {:?}",
            report.checks
        );
    }

    #[test]
    fn an_interrupt_without_an_override_on_a_trading_core_fails() {
        let node = testing::node();
        // The NVMe queue is excluded by the plan, but the kernel has it on CPU2:
        // exactly the drift an operator has to be told about.
        fs::write(node.abs("/proc/irq/24/smp_affinity_list"), "2,3\n").unwrap();
        let report = run(&node.sysroot, &testing::config(), &quick(), &Log::quiet()).unwrap();
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "irq-affinity")
            .unwrap();
        assert_eq!(check.level, FindingLevel::Error);
        assert!(check.detail.contains("nvme0q0"), "{}", check.detail);
    }

    #[test]
    fn a_provisioned_node_passes_verification() {
        let node = testing::node();
        let cfg = testing::config();
        let built = plan::build(&node.sysroot, &cfg).unwrap();
        apply::run(&node.sysroot, &cfg, &built, &Log::quiet()).unwrap();

        // The fixture kernel never re-reads its command line, so the post-reboot
        // state is written by hand: what /proc/cmdline and sysfs look like after a
        // reboot into the command line the plan asked for.
        fs::write(node.abs("/sys/devices/system/cpu/isolated"), "2,3\n").unwrap();
        fs::write(node.abs("/sys/devices/system/cpu/nohz_full"), "2,3\n").unwrap();
        let recommended: Vec<&str> = kernel::RECOMMENDED_PARAMS
            .iter()
            .map(|(token, _)| *token)
            .collect();
        fs::write(
            node.abs("/proc/cmdline"),
            format!("{} {}\n", built.cmdline, recommended.join(" ")),
        )
        .unwrap();

        let report = run(&node.sysroot, &cfg, &quick(), &Log::quiet()).unwrap();
        assert!(report.passed(), "{:?}", report.errors());
        assert!(report.ok("cmdline"), "{:?}", report.checks);
        assert!(report.ok("irq-affinity"));
        assert!(report.ok("sysctls"));
        assert!(report.ok("smt-siblings"));
        assert!(report.ok("governor"));
        assert!(report.ok("irq-traffic"), "{:?}", report.checks);
    }

    #[test]
    fn online_siblings_are_an_error() {
        let node = testing::node();
        let mut cfg = testing::config();
        cfg.isolation.cpus = "2".to_string();
        cfg.isolation.set = cpulist::parse("2").unwrap();
        let report = run(&node.sysroot, &cfg, &quick(), &Log::quiet()).unwrap();
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "smt-siblings")
            .unwrap();
        assert_eq!(check.level, FindingLevel::Error);
        assert!(check.detail.contains("3"), "{}", check.detail);
    }

    #[test]
    fn traffic_is_judged_from_deltas_not_totals() {
        // The fixture node already shows 1200 hits on CPU2, but a sample window in
        // which nothing fires must still pass: only the delta counts.
        let before = irq::read_interrupts(&testing::node().sysroot);
        let mut after = before.clone();
        for entry in &mut after.entries {
            if entry.id == "124" {
                entry.counts[2] += 300;
            }
            if entry.id == "122" {
                entry.counts[0] += 5000;
            }
        }
        let noise = irq::isolated_deltas(&before, &after, &cpulist::parse("2,3").unwrap());
        assert_eq!(noise.len(), 1);
        assert_eq!(noise[0].1, "matching-engine-0");
        assert_eq!(noise[0].2, 300, "eth0 traffic on CPU0 is irrelevant");

        let strict = Options {
            sample_seconds: 1,
            irq_rate_per_second: 1.0,
        };
        assert_eq!(noise_offenders(&noise, &strict).len(), 1);
        let relaxed = Options {
            sample_seconds: 1,
            irq_rate_per_second: 1000.0,
        };
        assert!(noise_offenders(&noise, &relaxed).is_empty());
        let long_window = Options {
            sample_seconds: 10,
            irq_rate_per_second: 100.0,
        };
        assert!(
            noise_offenders(&noise, &long_window).is_empty(),
            "100/s over 10s is 1000 hits"
        );
    }

    #[test]
    fn options_are_clamped_to_a_sane_window() {
        let mut verify = crate::config::VerifySection {
            duration_s: 300,
            irq_rate_per_second: 0.5,
            ..Default::default()
        };
        let options = Options::from_config(&verify);
        assert_eq!(
            options.sample_seconds, 30,
            "a verification run answers in seconds"
        );
        assert_eq!(options.irq_rate_per_second, 0.5);

        verify.duration_s = 0;
        assert_eq!(Options::from_config(&verify).sample_seconds, 1);
    }

    #[test]
    fn verify_render_and_json_agree() {
        let node = testing::node();
        let report = run(&node.sysroot, &testing::config(), &quick(), &Log::quiet()).unwrap();
        let rendered = report.render();
        assert!(
            rendered.starts_with("node      test-node\nverdict   NOT isolated\n"),
            "{rendered}"
        );
        assert!(rendered.contains("irq-affinity"));
        assert!(rendered.contains("interrupt traffic over 1s"));

        let json = report.to_json();
        assert!(json.starts_with("{\"node\":\"test-node\",\"passed\":false"));
        assert!(json.contains("\"irq_noise\":["));
        assert_eq!(json.matches('{').count(), json.matches('}').count());
        assert_eq!(json.matches('[').count(), json.matches(']').count());

        assert!(!report.ok("cmdline"));
        assert!(report
            .errors()
            .iter()
            .all(|check| check.level == FindingLevel::Error));
    }
}
