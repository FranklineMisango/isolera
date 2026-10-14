//! Moving movable kernel threads off the trading cores.
//!
//! The kernel decides where ``kworker``, ``ksoftirqd`` and the RCU offload threads
//! run, and it will happily run them on an isolated core: ``isolcpus`` keeps the
//! *scheduler* from placing user tasks there, not the kernel's own work. Isolera
//! therefore renders ``taskset`` commands (``kernel::pinning_plan``) and, when
//! asked, runs them.
//!
//! Affinity is a userspace syscall, so applying it means either unsafe code (which
//! this crate forbids) or the ``taskset`` binary from util-linux - the same tool an
//! operator would use by hand. Isolera runs it, reads the thread's affinity back,
//! and reports what actually happened.

use crate::config::Config;
use crate::cpulist::{self, CpuSet};
use crate::error::{Error, Kind, Result};
use crate::kernel::{self, KernelThread, PinCommand};
use crate::log::Log;
use crate::sysroot::SysRoot;

/// What happened to one kernel thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// The move that was attempted.
    pub command: PinCommand,
    /// Result of the attempt.
    pub status: Status,
    /// Affinity read back after the move, when it could be re-read.
    pub affinity: Option<CpuSet>,
}

impl Outcome {
    /// True when the thread now runs where the plan wanted it.
    pub fn moved(&self) -> bool {
        matches!(self.status, Status::Applied | Status::DryRun)
    }
}

/// Result of applying one move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Status {
    /// `taskset` ran and the thread moved.
    Applied,
    /// Dry run: the command was rendered but not executed.
    DryRun,
    /// The thread is gone (it exited or the pid was recycled).
    Vanished(String),
    /// `taskset` refused the move.
    Failed(String),
}

impl Status {
    /// Stable label used in reports.
    pub fn as_str(&self) -> &'static str {
        match self {
            Status::Applied => "applied",
            Status::DryRun => "dry-run",
            Status::Vanished(_) => "vanished",
            Status::Failed(_) => "failed",
        }
    }

    /// Detail line, when there is one.
    pub fn detail(&self) -> Option<&str> {
        match self {
            Status::Vanished(text) | Status::Failed(text) => Some(text.as_str()),
            _ => None,
        }
    }
}

/// The threads that still need moving, as a work list.
pub fn list(sysroot: &SysRoot, cfg: &Config) -> Vec<PinCommand> {
    let cpu_count = crate::topology::probe(sysroot).cpu_count().max(1);
    kernel::pinning_plan(sysroot, cfg, &cfg.housekeeping(cpu_count))
}

/// Every kernel thread on the node, moved or not.
pub fn inventory(sysroot: &SysRoot) -> Vec<KernelThread> {
    kernel::kernel_threads(sysroot)
}

/// Apply the moves with `taskset`.
pub fn apply(sysroot: &SysRoot, commands: &[PinCommand], log: &Log) -> Result<Vec<Outcome>> {
    apply_with(sysroot, commands, "taskset", log)
}

/// Apply the moves with a named binary; the binary is a parameter so the failure
/// path (no `taskset` installed) is testable.
pub fn apply_with(
    sysroot: &SysRoot,
    commands: &[PinCommand],
    binary: &str,
    log: &Log,
) -> Result<Vec<Outcome>> {
    let mut outcomes = Vec::new();
    for command in commands {
        if sysroot.dry_run() {
            log.detail(command.command());
            outcomes.push(Outcome {
                command: command.clone(),
                status: Status::DryRun,
                affinity: command.from.clone(),
            });
            continue;
        }
        let list = cpulist::format(&command.target);
        let output = std::process::Command::new(binary)
            .arg("-pc")
            .arg(&list)
            .arg(command.pid.to_string())
            .output()
            .map_err(|err| match err.kind() {
                std::io::ErrorKind::NotFound => Error::new(
                    Kind::Unsupported,
                    format!(
                        "{binary} is not installed; install util-linux or run the rendered pin script"
                    ),
                ),
                _ => Error::new(Kind::Io, format!("cannot run {binary}: {err}")),
            })?;
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        let mut status = if output.status.success() {
            Status::Applied
        } else if stderr.contains("No such process") || stderr.contains("failed to get") {
            Status::Vanished(stderr)
        } else {
            Status::Failed(stderr)
        };
        let affinity = thread_affinity(sysroot, command.pid);
        // Trust but verify: read the mask back and fail the move if the kernel did
        // not end up where the plan asked.
        if status == Status::Applied {
            match &affinity {
                Some(set) if !cpulist::is_subset(set, &command.target) => {
                    status = Status::Failed(format!(
                        "affinity is {} after the move, expected a subset of {}",
                        cpulist::format(set),
                        cpulist::format(&command.target)
                    ));
                }
                _ => {}
            }
        }
        if status == Status::Applied {
            log.detail(format!("pid {} -> {list}", command.pid));
        } else {
            log.warn(format!("pid {}: {}", command.pid, status.as_str()));
        }
        outcomes.push(Outcome {
            command: command.clone(),
            status,
            affinity,
        });
    }
    Ok(outcomes)
}

/// Current affinity of a pid, read back through the sysroot.
pub fn thread_affinity(sysroot: &SysRoot, pid: u32) -> Option<CpuSet> {
    sysroot
        .read_optional(&format!("/proc/{pid}/status"))
        .and_then(|text| kernel::status_cpus(&text))
}

/// Render the outcomes as a report.
pub fn render(outcomes: &[Outcome]) -> String {
    use crate::table::Table;
    if outcomes.is_empty() {
        return "no kernel threads need moving\n".to_string();
    }
    let mut table = Table::new(["pid", "thread", "from", "target", "result"]);
    for outcome in outcomes {
        table.push([
            outcome.command.pid.to_string(),
            outcome.command.name.clone(),
            outcome
                .command
                .from
                .as_ref()
                .map(cpulist::format)
                .unwrap_or_else(|| "-".to_string()),
            cpulist::format(&outcome.command.target),
            match outcome.status.detail() {
                Some(detail) => format!("{}: {}", outcome.status.as_str(), detail),
                None => outcome.status.as_str().to_string(),
            },
        ]);
    }
    table.render()
}

/// Serialise the outcomes as a JSON array.
pub fn to_json(outcomes: &[Outcome]) -> String {
    let rows: Vec<String> = outcomes
        .iter()
        .map(|outcome| {
            format!(
                "{{\"pid\":{},\"thread\":{},\"target\":{},\"status\":{},\"detail\":{},\"affinity\":{}}}",
                outcome.command.pid,
                crate::sysroot::json_string(&outcome.command.name),
                crate::sysroot::json_string(&cpulist::format(&outcome.command.target)),
                crate::sysroot::json_string(outcome.status.as_str()),
                match outcome.status.detail() {
                    Some(detail) => crate::sysroot::json_string(detail),
                    None => "null".to_string(),
                },
                match &outcome.affinity {
                    Some(set) => crate::sysroot::json_string(&cpulist::format(set)),
                    None => "null".to_string(),
                },
            )
        })
        .collect();
    format!("{{\"outcomes\":[{}]}}", rows.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    fn commands(node: &testing::Node, cfg: &Config) -> Vec<PinCommand> {
        let listed = list(&node.sysroot, cfg);
        assert_eq!(
            listed.len(),
            1,
            "the fixture has one movable thread on the trading cores"
        );
        listed
    }

    #[test]
    fn the_work_list_holds_only_threads_on_trading_cores() {
        let node = testing::node();
        let cfg = testing::config();
        let listed = commands(&node, &cfg);
        assert_eq!(listed[0].name, "kworker/2:1");
        assert_eq!(listed[0].pid, 15);
        assert_eq!(listed[0].target, cpulist::parse("0,1").unwrap());
        assert_eq!(
            listed[0].command(),
            "taskset -pc 0,1 15       # kworker/2:1",
            "the rendered command names the thread"
        );

        // kthreadd is kept by the plan and kworker/0:2 is already housekeeping.
        let threads = inventory(&node.sysroot);
        let names: Vec<&str> = threads.iter().map(|thread| thread.name.as_str()).collect();
        assert!(names.contains(&"kworker/0:2"));
        assert!(names.contains(&"kthreadd"));
    }

    #[test]
    fn a_dry_run_renders_without_running_taskset() {
        let node = testing::node();
        let cfg = testing::config();
        let listed = commands(&node, &cfg);
        let outcomes = apply(&node.dry, &listed, &Log::quiet()).unwrap();
        assert_eq!(outcomes.len(), 1);
        assert!(outcomes[0].moved());
        assert_eq!(outcomes[0].status, Status::DryRun);
        assert_eq!(outcomes[0].status.as_str(), "dry-run");
        assert!(outcomes[0].status.detail().is_none());
    }

    #[test]
    fn a_missing_taskset_is_reported_as_unsupported() {
        let node = testing::node();
        let cfg = testing::config();
        let listed = commands(&node, &cfg);
        let err = apply_with(
            &node.sysroot,
            &listed,
            "/nonexistent/taskset",
            &Log::quiet(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), Kind::Unsupported);
        assert!(err.message().contains("not installed"), "{}", err.message());
    }

    #[test]
    fn statuses_carry_their_detail() {
        assert_eq!(Status::Applied.as_str(), "applied");
        assert_eq!(Status::Failed("denied".into()).detail(), Some("denied"));
        assert_eq!(Status::Vanished("gone".into()).detail(), Some("gone"));
        assert_eq!(Status::Vanished("gone".into()).as_str(), "vanished");
        assert!(Status::Applied.detail().is_none());
    }

    #[test]
    fn render_and_json_describe_the_outcomes() {
        let node = testing::node();
        let cfg = testing::config();
        let listed = commands(&node, &cfg);
        let outcomes = apply(&node.dry, &listed, &Log::quiet()).unwrap();

        let rendered = render(&outcomes);
        assert!(rendered.starts_with("pid  thread"), "{rendered}");
        assert!(rendered.contains("kworker/2:1"));
        assert!(rendered.contains("dry-run"));
        assert_eq!(render(&[]), "no kernel threads need moving\n");

        let json = to_json(&outcomes);
        assert!(json.starts_with("{\"outcomes\":[{\"pid\":15"));
        assert!(json.contains("\"status\":\"dry-run\""));
        assert_eq!(json.matches('{').count(), json.matches('}').count());
        assert_eq!(json.matches('[').count(), json.matches(']').count());
    }

    #[test]
    fn affinity_is_read_back_from_proc() {
        let node = testing::node();
        assert_eq!(
            thread_affinity(&node.sysroot, 15),
            Some(cpulist::parse("2-3").unwrap())
        );
        assert_eq!(thread_affinity(&node.sysroot, 9999), None);
    }
}
