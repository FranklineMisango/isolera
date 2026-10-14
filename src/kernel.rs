//! Kernel command line assembly, cmdline inspection and runtime sysctls.
//!
//! Isolation is decided at boot, so this module owns the part of a plan that a
//! running process cannot express. It answers three questions:
//!
//! 1. *What should the boot command line say?* [`isolation_params`] synthesises
//!    the authoritative parameter set from a plan, and [`merge_managed`] folds it
//!    into the existing cmdline without ever appending a second ``isolcpus=`` on
//!    top of an old one - the classic way a hand-edited ``/etc/default/grub``
//!    silently ends up with two conflicting sets.
//! 2. *Is the running node actually isolated?* [`inspect_cmdline`] compares
//!    ``/proc/cmdline`` against the plan and returns findings.
//! 3. *Which runtime knobs complement the boot parameters?* [`sysctl_plan`]
//!    produces ``/proc/sys`` writes, [`restore_sysctls`] puts a stock tunable set
//!    back, and [`pinning_plan`] lists the kernel threads to move off the trading
//!    cores.

use crate::config::Config;
use crate::cpulist::{self, CpuSet};
use crate::error::{Error, Kind, Result};
use crate::irq;
use crate::sysroot::{SysRoot, WriteRecord};

/// Live kernel command line.
pub const CMDLINE: &str = "/proc/cmdline";

/// Root of the writable sysctl tree.
pub const SYSCTL_ROOT: &str = "/proc/sys";

/// Boot parameters Isolera rewrites. Everything else on the cmdline is preserved
/// exactly as the operator or the distribution left it.
pub const MANAGED_PARAMS: [&str; 4] = ["isolcpus", "nohz_full", "rcu_nocbs", "rcu_nocb_poll"];

/// Parameters that reduce timer/ISR jitter but are *not* owned by Isolera: they
/// are reported as recommendations because they carry workload-specific
/// trade-offs.
pub const RECOMMENDED_PARAMS: [(&str, &str); 7] = [
    ("skew_tick=1", "de-phase per-CPU scheduler ticks"),
    (
        "nmi_watchdog=0",
        "disable the NMI watchdog on trading cores",
    ),
    ("nowatchdog", "stop the softlockup/hardlockup detectors"),
    ("audit=0", "disable syscall auditing"),
    (
        "intel_idle.max_cstate=0",
        "prevent deep C-state entry latency",
    ),
    (
        "processor.max_cstate=0",
        "prevent deep C-state entry latency",
    ),
    (
        "tsc=reliable",
        "trust the invariant TSC for latency measurement",
    ),
];

/// An ordered ``key``/``value`` view of a kernel command line.
///
/// Order is preserved because the whole point of [`merge_managed`] is a
/// reviewable diff: a managed parameter keeps the slot of the first occurrence it
/// replaced. A parameter written without ``=`` is a flag and maps to
/// ``Some(None)``.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cmdline {
    entries: Vec<(String, Option<String>)>,
}

impl Cmdline {
    /// An empty command line.
    pub fn new() -> Cmdline {
        Cmdline {
            entries: Vec::new(),
        }
    }

    /// Parse kernel-cmdline text, tolerating repeated whitespace.
    pub fn parse(text: &str) -> Cmdline {
        let mut cmdline = Cmdline::new();
        for token in text.split_whitespace() {
            let (key, value) = split_token(token);
            if key.is_empty() {
                continue;
            }
            cmdline.set(key, value);
        }
        cmdline
    }

    /// Read ``/proc/cmdline`` through the sysroot.
    pub fn read(sysroot: &SysRoot) -> Cmdline {
        match sysroot.read_optional(CMDLINE) {
            Some(text) => Cmdline::parse(&text),
            None => Cmdline::new(),
        }
    }

    /// Number of parameters.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// True when nothing was parsed.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Parameters in cmdline order.
    pub fn iter(&self) -> impl Iterator<Item = (&str, Option<&str>)> {
        self.entries
            .iter()
            .map(|(key, value)| (key.as_str(), value.as_deref()))
    }

    /// ``None`` when the parameter is absent, ``Some(None)`` for a bare flag.
    pub fn get(&self, key: &str) -> Option<Option<&str>> {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_deref())
    }

    /// True when the parameter is present, with or without a value.
    pub fn contains(&self, key: &str) -> bool {
        self.get(key).is_some()
    }

    /// True when the parameter is present without a value.
    pub fn is_flag(&self, key: &str) -> bool {
        matches!(self.get(key), Some(None))
    }

    /// The value of ``key``, or ``None`` when it is absent or a flag.
    pub fn value_of(&self, key: &str) -> Option<&str> {
        self.get(key).flatten()
    }

    /// Insert or replace a parameter, keeping the position of the first match.
    pub fn set(&mut self, key: &str, value: Option<&str>) {
        let mut replaced = false;
        self.entries.retain_mut(|(name, existing)| {
            if name != key {
                return true;
            }
            if replaced {
                return false;
            }
            replaced = true;
            *existing = value.map(str::to_string);
            true
        });
        if !replaced {
            self.entries
                .push((key.to_string(), value.map(str::to_string)));
        }
    }

    /// Drop a parameter entirely.
    pub fn remove(&mut self, key: &str) {
        self.entries.retain(|(name, _)| name != key);
    }

    /// Render back into a single cmdline string.
    pub fn render(&self) -> String {
        self.entries
            .iter()
            .map(|(key, value)| match value {
                Some(value) => format!("{key}={value}"),
                None => key.clone(),
            })
            .collect::<Vec<String>>()
            .join(" ")
    }
}

impl std::fmt::Display for Cmdline {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.render())
    }
}

/// Split ``key=value`` / ``key`` without allocating.
fn split_token(token: &str) -> (&str, Option<&str>) {
    match token.split_once('=') {
        Some((key, value)) => (key.trim(), Some(value)),
        None => (token.trim(), None),
    }
}

/// Parse ``/proc/cmdline`` text into an ordered parameter map.
pub fn parse_cmdline(text: &str) -> Cmdline {
    Cmdline::parse(text)
}

/// Render a parameter map back into a single cmdline string.
pub fn format_params(params: &Cmdline) -> String {
    params.render()
}

/// Fold ``params`` into an existing cmdline, replacing managed duplicates.
///
/// Order is preserved: untouched parameters keep their position, and a managed
/// parameter keeps the slot of the first occurrence it replaced, so the result
/// stays reviewable in a diff and stable across repeated runs.
pub fn merge_managed(current: &str, params: &Cmdline) -> String {
    let existing = Cmdline::parse(current);
    let mut result = Cmdline::new();
    let mut emitted: Vec<String> = Vec::new();
    for (key, value) in existing.iter() {
        match params.get(key) {
            Some(replacement) => {
                if emitted.iter().any(|seen| seen == key) {
                    continue;
                }
                result.set(key, replacement);
                emitted.push(key.to_string());
            }
            None => result.set(key, value),
        }
    }
    for (key, value) in params.iter() {
        if emitted.iter().any(|seen| seen == key) {
            continue;
        }
        result.set(key, value);
    }
    result.render()
}

/// Parse a CPU list from a cmdline value, treating garbage as "none".
fn cpus_or_empty(value: Option<&str>) -> CpuSet {
    value
        .and_then(|text| cpulist::parse(text).ok())
        .unwrap_or_default()
}

/// Extract the isolated CPU set, handling both ``isolcpus`` dialects.
///
/// ``isolcpus=2-5`` (legacy) and ``isolcpus=domain,managed_irq,2-5`` (modern) both
/// appear in the wild, and mis-parsing either one would make Isolera believe a
/// node is isolated when it is not. Every leading flag token is skipped, not just
/// the first: the modern form carries two of them.
pub fn active_isolated(params: &Cmdline) -> CpuSet {
    let raw = params.value_of("isolcpus").map(str::trim).unwrap_or("");
    if raw.is_empty() {
        return CpuSet::new();
    }
    let tokens: Vec<&str> = raw.split(',').collect();
    let start = tokens.iter().position(|token| {
        token
            .chars()
            .next()
            .map(|first| first.is_ascii_digit())
            .unwrap_or(false)
    });
    match start {
        Some(index) => cpus_or_empty(Some(&tokens[index..].join(","))),
        None => CpuSet::new(),
    }
}

/// CPUs covered by ``nohz_full=``.
pub fn active_nohz_full(params: &Cmdline) -> CpuSet {
    cpus_or_empty(params.value_of("nohz_full"))
}

/// CPUs covered by ``rcu_nocbs=``.
pub fn active_rcu_nocbs(params: &Cmdline) -> CpuSet {
    cpus_or_empty(params.value_of("rcu_nocbs"))
}

/// True when a token looks like a kernel ``key`` or ``key=value`` parameter.
fn is_valid_param(token: &str) -> bool {
    let (key, value) = match token.split_once('=') {
        Some((key, value)) => (key, Some(value)),
        None => (token, None),
    };
    if key.is_empty() {
        return false;
    }
    let key_ok = key
        .chars()
        .all(|character| character.is_ascii_alphanumeric() || "_.+-".contains(character));
    let value_ok = value
        .map(|text| !text.contains(char::is_whitespace))
        .unwrap_or(true);
    key_ok && value_ok
}

/// The authoritative parameter set Isolera wants on the kernel cmdline.
pub fn isolation_params(cfg: &Config, isolated: &CpuSet) -> Result<Cmdline> {
    let mut params = Cmdline::new();
    let cpus = cpulist::format(isolated);

    params.set("isolcpus", Some(&isolcpus_value(cfg, isolated)));
    if cfg.isolation.nohz_full {
        params.set("nohz_full", Some(&cpus));
    }
    if cfg.isolation.rcu_nocbs {
        params.set("rcu_nocbs", Some(&cpus));
        if cfg.isolation.rcu_nocb_poll {
            params.set("rcu_nocb_poll", None);
        }
    }
    if let Some(idle) = cfg.isolation.idle.as_deref() {
        params.set("idle", Some(idle));
    }
    if let Some(skew) = cfg.isolation.skew_tick {
        params.set("skew_tick", Some(&skew.to_string()));
    }
    for extra in &cfg.kernel.cmdline_extra {
        let token = extra.trim();
        if token.is_empty() {
            continue;
        }
        if !is_valid_param(token) {
            return Err(Error::new(
                Kind::Config,
                format!("malformed extra kernel parameter: {token:?}"),
            ));
        }
        let (key, value) = split_token(token);
        params.set(key, value);
    }
    Ok(params)
}

/// Shell-quote a cmdline for a ``GRUB_CMDLINE_LINUX_DEFAULT="..."`` value.
///
/// Mirrors ``shlex.quote``: a string made only of shell-safe characters is
/// returned untouched, anything else is single-quoted.
pub fn quote_for_grub(cmdline: &str) -> String {
    let safe = !cmdline.is_empty()
        && cmdline
            .chars()
            .all(|character| character.is_alphanumeric() || "@%+=:,./-_".contains(character));
    if safe {
        return cmdline.to_string();
    }
    format!("'{}'", cmdline.replace('\'', "'\\''"))
}

/// Severity of a cmdline inspection finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum FindingLevel {
    /// The node matches the plan.
    Ok,
    /// Works, but not what the plan asked for.
    Warn,
    /// The node is not isolated as configured.
    Error,
}

impl FindingLevel {
    /// ``ok``/``warn``/``error``, for reports.
    pub fn as_str(self) -> &'static str {
        match self {
            FindingLevel::Ok => "ok",
            FindingLevel::Warn => "warn",
            FindingLevel::Error => "error",
        }
    }
}

/// One observation about the running node.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Finding {
    /// Severity.
    pub level: FindingLevel,
    /// One-line summary.
    pub title: String,
    /// What was seen, and what was expected.
    pub detail: String,
}

impl Finding {
    /// Build a finding.
    pub fn new(
        level: FindingLevel,
        title: impl Into<String>,
        detail: impl Into<String>,
    ) -> Finding {
        Finding {
            level,
            title: title.into(),
            detail: detail.into(),
        }
    }

    /// True for informational findings.
    pub fn ok(&self) -> bool {
        self.level == FindingLevel::Ok
    }

    /// Serialise as a single-line JSON object.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"level\":{},\"title\":{},\"detail\":{}}}",
            crate::sysroot::json_string(self.level.as_str()),
            crate::sysroot::json_string(&self.title),
            crate::sysroot::json_string(&self.detail),
        )
    }
}

/// Compare the *running* cmdline against the intended isolation state.
pub fn inspect_cmdline(
    params: &Cmdline,
    cfg: &Config,
    isolated: &CpuSet,
    housekeeping: &CpuSet,
) -> Vec<Finding> {
    let mut findings: Vec<Finding> = Vec::new();
    if isolated.is_empty() {
        return findings;
    }

    let active = active_isolated(params);
    let nohz = active_nohz_full(params);
    let rcu = active_rcu_nocbs(params);
    let raw = params.value_of("isolcpus").unwrap_or("").trim();

    if active.is_empty() {
        findings.push(Finding::new(
            FindingLevel::Error,
            "isolcpus is not active",
            format!(
                "the scheduler still load balances onto {}; provision and reboot before trusting latencies",
                cpulist::format(isolated)
            ),
        ));
    } else if &active != isolated {
        let mut detail: Vec<String> = Vec::new();
        let missing = cpulist::difference(isolated, &active);
        let extra = cpulist::difference(&active, isolated);
        if !missing.is_empty() {
            detail.push(format!("not isolated: {}", cpulist::format(&missing)));
        }
        if !extra.is_empty() {
            detail.push(format!(
                "unexpectedly isolated: {}",
                cpulist::format(&extra)
            ));
        }
        findings.push(Finding::new(
            FindingLevel::Warn,
            "isolcpus does not match the configuration",
            detail.join("; "),
        ));
    } else {
        findings.push(Finding::new(
            FindingLevel::Ok,
            "isolcpus matches the configuration",
            format!("{} removed from the scheduler", cpulist::format(&active)),
        ));
    }

    if !raw.is_empty() {
        let head = raw.split(',').next().unwrap_or("");
        if head.chars().any(|character| character.is_ascii_digit()) {
            findings.push(Finding::new(
                FindingLevel::Warn,
                "isolcpus uses the legacy bare form",
                "prefer isolcpus=domain,managed_irq,<list> so load balancing and managed IRQs are both handled explicitly",
            ));
        } else {
            findings.push(Finding::new(
                FindingLevel::Ok,
                "isolcpus uses the explicit flag form",
                format!("isolcpus={raw}"),
            ));
        }
    }

    if cfg.isolation.nohz_full {
        if &nohz == isolated {
            findings.push(Finding::new(
                FindingLevel::Ok,
                "nohz_full covers the trading cores",
                cpulist::format(&nohz),
            ));
        } else {
            findings.push(Finding::new(
                FindingLevel::Warn,
                "nohz_full does not cover the trading cores",
                format!(
                    "active: {}; expected: {}",
                    or_none(&nohz),
                    cpulist::format(isolated)
                ),
            ));
        }
    }

    if cfg.isolation.rcu_nocbs {
        if &rcu == isolated {
            findings.push(Finding::new(
                FindingLevel::Ok,
                "rcu_nocbs covers the trading cores",
                cpulist::format(&rcu),
            ));
        } else {
            findings.push(Finding::new(
                FindingLevel::Warn,
                "rcu_nocbs does not cover the trading cores",
                format!(
                    "active: {}; expected: {}",
                    or_none(&rcu),
                    cpulist::format(isolated)
                ),
            ));
        }
    }

    let overlap = cpulist::intersection(&active, housekeeping);
    if !overlap.is_empty() {
        findings.push(Finding::new(
            FindingLevel::Error,
            "housekeeping and isolated sets overlap",
            format!(
                "{} are both isolated and housekeeping",
                cpulist::format(&overlap)
            ),
        ));
    }

    if housekeeping.is_empty() {
        findings.push(Finding::new(
            FindingLevel::Error,
            "no housekeeping CPUs remain",
            "isolating every core breaks RCU, khugepaged and device work; always leave at least one (ideally two) cores unisolated",
        ));
    }

    for (token, why) in RECOMMENDED_PARAMS {
        let key = token.split('=').next().unwrap_or(token);
        if !params.contains(key) {
            findings.push(Finding::new(
                FindingLevel::Warn,
                format!("recommended parameter missing: {token}"),
                why,
            ));
        }
    }

    findings
}

/// ``<list>`` or ``none``, so a warning never renders an empty field.
fn or_none(set: &CpuSet) -> String {
    if set.is_empty() {
        "none".to_string()
    } else {
        cpulist::format(set)
    }
}

/// Warn on tokens Isolera manages that a plan would rewrite anyway.
pub fn managed_drift(params: &Cmdline, cfg: &Config, isolated: &CpuSet) -> Vec<Finding> {
    let mut findings = Vec::new();
    let wanted = match isolation_params(cfg, isolated) {
        Ok(wanted) => wanted,
        Err(_) => return findings,
    };
    for (key, value) in wanted.iter() {
        if !MANAGED_PARAMS.contains(&key) {
            continue;
        }
        match (params.get(key), value) {
            (Some(current), Some(target)) if current != Some(target) => {
                findings.push(Finding::new(
                    FindingLevel::Warn,
                    format!("{key} differs from the plan"),
                    format!("running: {current:?}; plan: {target:?}"),
                ))
            }
            (None, Some(target)) => findings.push(Finding::new(
                FindingLevel::Warn,
                format!("{key} is missing from the command line"),
                format!("plan: {target}"),
            )),
            _ => {}
        }
    }
    findings
}

/// ``(relative path, tuned value, stock value)``.
///
/// The stock value is what a distribution kernel ships, so [`restore_sysctls`]
/// can undo a tuning run without a reboot.
pub const SYSCTL_RULES: [(&str, &str, &str); 6] = [
    ("kernel/sched_rt_runtime_us", "-1", "950000"),
    ("kernel/timer_migration", "0", "1"),
    ("kernel/watchdog", "0", "1"),
    ("kernel/nmi_watchdog", "0", "1"),
    ("kernel/numa_balancing", "0", "1"),
    ("kernel/sched_autogroup_enabled", "0", "1"),
];

/// Normalise a dotted sysctl name or a ``/proc/sys`` path to a logical path.
///
/// ``kernel.watchdog`` and ``/proc/sys/kernel/watchdog`` both describe the same
/// node, and an operator will use both spellings.
pub fn sysctl_path(key: &str) -> Result<String> {
    let trimmed = key.trim();
    if trimmed.is_empty() {
        return Err(Error::new(Kind::Config, "sysctl name must not be empty"));
    }
    let allowed = |character: char| character.is_ascii_alphanumeric() || "_-".contains(character);
    if let Some(rest) = trimmed.strip_prefix(SYSCTL_ROOT) {
        let rest = rest.trim_start_matches('/');
        if rest.is_empty()
            || !rest
                .chars()
                .all(|character| allowed(character) || character == '/')
            || rest.contains("//")
        {
            return Err(Error::new(
                Kind::Config,
                format!("malformed sysctl path: {key:?}"),
            ));
        }
        return Ok(format!("{SYSCTL_ROOT}/{rest}"));
    }
    let mut path = String::from(SYSCTL_ROOT);
    for part in trimmed.split('.') {
        if part.is_empty() || !part.chars().all(allowed) {
            return Err(Error::new(
                Kind::Config,
                format!("malformed sysctl name: {key:?}"),
            ));
        }
        path.push('/');
        path.push_str(part);
    }
    Ok(path)
}

/// ``kernel.watchdog`` for ``/proc/sys/kernel/watchdog``.
pub fn sysctl_name(path: &str) -> String {
    match path.strip_prefix(SYSCTL_ROOT) {
        Some(rest) => rest.trim_start_matches('/').replace('/', "."),
        None => path.to_string(),
    }
}

/// One ``/proc/sys`` value Isolera intends to write.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tunable {
    /// Absolute logical path, e.g. ``/proc/sys/kernel/watchdog``.
    pub path: String,
    /// Dotted name, e.g. ``kernel.watchdog``.
    pub name: String,
    /// Value Isolera wants.
    pub value: String,
    /// Value found on the node, when the node exposes the knob.
    pub current: Option<String>,
    /// Why the value is wanted.
    pub reason: String,
}

impl Tunable {
    /// True when the node already carries the wanted value.
    pub fn already_set(&self) -> bool {
        self.current.as_deref().map(str::trim) == Some(self.value.as_str())
    }

    /// Serialise as a single-line JSON object.
    pub fn to_json(&self) -> String {
        let current = match &self.current {
            Some(text) => crate::sysroot::json_string(text),
            None => "null".to_string(),
        };
        format!(
            "{{\"path\":{},\"name\":{},\"value\":{},\"current\":{},\"changed\":{},\"reason\":{}}}",
            crate::sysroot::json_string(&self.path),
            crate::sysroot::json_string(&self.name),
            crate::sysroot::json_string(&self.value),
            current,
            !self.already_set(),
            crate::sysroot::json_string(&self.reason),
        )
    }
}

/// Tunables implied by the plan rather than spelled out in ``[kernel.sysctls]``.
fn implicit_sysctls(cfg: &Config) -> Vec<(&'static str, String, &'static str)> {
    let mut implied = Vec::new();
    if let Some(value) = cfg.rt.rt_runtime_us {
        let reason = if value < 0 {
            "unthrottled SCHED_FIFO: a negative budget removes the RT bandwidth cap"
        } else {
            "RT bandwidth budget from [rt].rt_runtime_us"
        };
        implied.push(("kernel/sched_rt_runtime_us", value.to_string(), reason));
    }
    if cfg.isolation.high_res_timers {
        implied.push((
            "kernel/timer_migration",
            "0".to_string(),
            "keep timers on the CPU that armed them",
        ));
    }
    implied
}

/// Pending ``/proc/sys`` writes, skipping nodes this kernel does not expose.
///
/// Values from ``[kernel.sysctls]`` win over the ones Isolera implies from other
/// sections, so an operator can always override the default reasoning.
pub fn sysctl_plan(sysroot: &SysRoot, cfg: &Config) -> Result<Vec<Tunable>> {
    let mut wanted: Vec<(String, String, String)> = Vec::new();
    let mut covered: Vec<String> = Vec::new();
    for (key, value) in &cfg.kernel.sysctls {
        let path = sysctl_path(key)?;
        covered.push(path.clone());
        wanted.push((
            path,
            value.clone(),
            "configured in [kernel.sysctls]".to_string(),
        ));
    }
    for (relative, value, reason) in implicit_sysctls(cfg) {
        let path = format!("{SYSCTL_ROOT}/{relative}");
        if covered.contains(&path) {
            continue;
        }
        wanted.push((path, value, reason.to_string()));
    }

    let mut plan = Vec::new();
    for (path, value, reason) in wanted {
        if !sysroot.exists(&path) {
            continue;
        }
        let current = sysroot.read_trimmed(&path);
        plan.push(Tunable {
            name: sysctl_name(&path),
            path,
            value,
            current,
            reason,
        });
    }
    plan.sort_by(|left, right| left.path.cmp(&right.path));
    Ok(plan)
}

/// Write the pending sysctls; unchanged knobs are skipped, so this is idempotent.
pub fn apply_sysctls(sysroot: &SysRoot, tunables: &[Tunable]) -> Result<Vec<WriteRecord>> {
    let mut records = Vec::new();
    for tunable in tunables {
        if tunable.already_set() || !sysroot.exists(&tunable.path) {
            continue;
        }
        let reason = format!("isolera: {}", tunable.reason);
        if let Some(record) = sysroot.write(&tunable.path, &tunable.value, &reason)? {
            records.push(record);
        }
    }
    Ok(records)
}

/// Reset Isolera's sysctls to their stock values.
pub fn restore_sysctls(sysroot: &SysRoot) -> Result<Vec<WriteRecord>> {
    let mut records = Vec::new();
    for (relative, _tuned, stock) in SYSCTL_RULES {
        let path = format!("{SYSCTL_ROOT}/{relative}");
        if !sysroot.exists(&path) {
            continue;
        }
        if sysroot.read_trimmed(&path).as_deref() == Some(stock) {
            continue;
        }
        let reason = format!("isolera: restore {relative} to the stock value");
        if let Some(record) = sysroot.write(&path, stock, &reason)? {
            records.push(record);
        }
    }
    Ok(records)
}

/// Render the pending sysctls as an aligned table.
pub fn render_tunables(tunables: &[Tunable]) -> String {
    use crate::table::Table;
    if tunables.is_empty() {
        return "no sysctl changes required\n".to_string();
    }
    let mut table = Table::new(["sysctl", "current", "target", "pending", "reason"]);
    for tunable in tunables {
        table.push([
            tunable.name.clone(),
            tunable.current.clone().unwrap_or_else(|| "-".to_string()),
            tunable.value.clone(),
            if tunable.already_set() { "no" } else { "yes" }.to_string(),
            tunable.reason.clone(),
        ]);
    }
    table.render()
}

/// Movable kernel threads: work the kernel can run anywhere, so Isolera keeps it
/// off the trading cores.
pub const KERNEL_THREAD_PATTERNS: [&str; 7] = [
    "kworker/*",
    "ksoftirqd/*",
    "migration/*",
    "rcuos/*",
    "rcuog/*",
    "kcompactd*",
    "khugepaged",
];

/// A kernel thread found under ``/proc``.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelThread {
    /// Thread id.
    pub pid: u32,
    /// ``comm`` name, e.g. ``kworker/2:1``.
    pub name: String,
    /// Allowed CPU set, when ``/proc/<pid>/status`` could be read.
    pub affinity: Option<CpuSet>,
}

impl KernelThread {
    /// Serialise as a single-line JSON object.
    pub fn to_json(&self) -> String {
        let affinity = match &self.affinity {
            Some(set) => crate::sysroot::json_string(&cpulist::format(set)),
            None => "null".to_string(),
        };
        format!(
            "{{\"pid\":{},\"name\":{},\"affinity\":{}}}",
            self.pid,
            crate::sysroot::json_string(&self.name),
            affinity,
        )
    }
}

/// ``Cpus_allowed_list:`` from ``/proc/<pid>/status``.
pub fn status_cpus(status: &str) -> Option<CpuSet> {
    for line in status.lines() {
        if let Some(rest) = line.trim().strip_prefix("Cpus_allowed_list:") {
            return cpulist::parse(rest.trim()).ok();
        }
    }
    None
}

/// Kernel threads on the node, identified by an empty ``/proc/<pid>/cmdline``.
///
/// A pid whose ``cmdline`` cannot be read is skipped rather than guessed at: a
/// false positive here would `taskset` somebody's trading process.
pub fn kernel_threads(sysroot: &SysRoot) -> Vec<KernelThread> {
    let mut threads = Vec::new();
    for entry in sysroot.list_dirs("/proc") {
        let Ok(pid) = entry.parse::<u32>() else {
            continue;
        };
        let Some(comm) = sysroot.read_trimmed(&format!("/proc/{pid}/comm")) else {
            continue;
        };
        if comm.is_empty() {
            continue;
        }
        let Some(cmdline) = sysroot.read_optional(&format!("/proc/{pid}/cmdline")) else {
            continue;
        };
        if !cmdline.trim_matches('\0').trim().is_empty() {
            continue;
        }
        let affinity = sysroot
            .read_optional(&format!("/proc/{pid}/status"))
            .and_then(|text| status_cpus(&text));
        threads.push(KernelThread {
            pid,
            name: comm,
            affinity,
        });
    }
    threads.sort_by_key(|thread| thread.pid);
    threads
}

/// Whether a thread name matches one of the configured keep patterns.
pub fn should_keep(name: &str, keep: &[String]) -> bool {
    keep.iter()
        .any(|pattern| irq::matches(pattern.trim(), name))
}

/// One ``taskset`` invocation that moves a kernel thread off the trading cores.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PinCommand {
    /// Thread id.
    pub pid: u32,
    /// Thread name, used as the trailing comment.
    pub name: String,
    /// Where the thread is allowed to run today.
    pub from: Option<CpuSet>,
    /// CPU set the thread is moved to.
    pub target: CpuSet,
    /// Why this thread is being moved.
    pub reason: String,
}

impl PinCommand {
    /// ``taskset -pc <list> <pid>``, with the thread name as a trailing comment.
    pub fn command(&self) -> String {
        format!(
            "taskset -pc {} {:<8} # {}",
            cpulist::format(&self.target),
            self.pid,
            self.name
        )
    }

    /// The same move, after promoting the thread to ``SCHED_FIFO``.
    pub fn priority_command(&self, priority: u32) -> String {
        format!("chrt -f -p {priority} {:<8} # {}", self.pid, self.name)
    }

    /// Serialise as a single-line JSON object.
    pub fn to_json(&self) -> String {
        let from = match &self.from {
            Some(set) => crate::sysroot::json_string(&cpulist::format(set)),
            None => "null".to_string(),
        };
        format!(
            "{{\"pid\":{},\"name\":{},\"from\":{},\"target\":{},\"reason\":{}}}",
            self.pid,
            crate::sysroot::json_string(&self.name),
            from,
            crate::sysroot::json_string(&cpulist::format(&self.target)),
            crate::sysroot::json_string(&self.reason),
        )
    }
}

/// Build the ``isolcpus`` value, honouring the configured scheduler flags.
///
/// Modern kernels expect ``isolcpus=<flags>,<cpulist>``; ``domain`` removes the
/// cores from load balancing and ``managed_irq`` stops managed interrupts from
/// being aimed at them.
pub fn isolcpus_value(cfg: &Config, isolated: &CpuSet) -> String {
    let cpus = cpulist::format(isolated);
    let mut flags: Vec<&str> = Vec::new();
    if cfg.isolation.domain {
        flags.push("domain");
    }
    if cfg.isolation.managed_irq {
        flags.push("managed_irq");
    }
    if flags.is_empty() {
        cpus
    } else {
        format!("{},{cpus}", flags.join(","))
    }
}

/// Kernel threads that still sit on an isolated CPU, with the move that fixes it.
///
/// Threads the kernel has already parked elsewhere are omitted, so the result is
/// a work list rather than an inventory; [`kernel_threads`] is the inventory.
pub fn pinning_plan(sysroot: &SysRoot, cfg: &Config, housekeeping: &CpuSet) -> Vec<PinCommand> {
    let mut commands = Vec::new();
    if !cfg.rt.pin_kernel_threads || housekeeping.is_empty() {
        return commands;
    }
    let isolated = cfg.isolated();
    for thread in kernel_threads(sysroot) {
        if should_keep(&thread.name, &cfg.rt.keep) {
            continue;
        }
        if !KERNEL_THREAD_PATTERNS
            .iter()
            .any(|pattern| irq::matches(pattern, &thread.name))
        {
            continue;
        }
        let reason = match &thread.affinity {
            Some(set) if set == housekeeping => continue,
            Some(set) if cpulist::intersects(set, isolated) => format!(
                "bound to isolated CPU(s) {}",
                cpulist::format(&cpulist::intersection(set, isolated))
            ),
            Some(_) => continue,
            None => "affinity unknown; pinned because it matches a movable pattern".to_string(),
        };
        commands.push(PinCommand {
            pid: thread.pid,
            name: thread.name,
            from: thread.affinity,
            target: housekeeping.clone(),
            reason,
        });
    }
    commands
}

/// Render the pinning commands as a runnable ``sh`` block.
pub fn render_pin_commands(commands: &[PinCommand], priority: Option<u32>) -> String {
    if commands.is_empty() {
        return "no kernel threads need moving\n".to_string();
    }
    let mut out = String::from("#!/bin/sh\n# Move movable kernel threads off the trading cores.\n");
    out.push_str("set -e\n");
    for command in commands {
        out.push_str(&format!("# {}\n", command.reason));
        if let Some(priority) = priority {
            out.push_str(&command.priority_command(priority));
            out.push('\n');
        }
        out.push_str(&command.command());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir::TempDir;
    use std::fs;

    /// A fixture root carrying the cmdline and sysctls a plan touches.
    fn fixture() -> (TempDir, SysRoot) {
        let dir = TempDir::new("isolera-kernel").unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("proc/sys/kernel")).unwrap();
        fs::write(
            root.join("proc/cmdline"),
            "BOOT_IMAGE=/vmlinuz root=/dev/sda1 isolcpus=2,3 rcu_nocbs=2,3",
        )
        .unwrap();
        fs::write(root.join("proc/sys/kernel/watchdog"), "1\n").unwrap();
        fs::write(root.join("proc/sys/kernel/timer_migration"), "1\n").unwrap();
        fs::write(root.join("proc/sys/kernel/sched_rt_runtime_us"), "950000\n").unwrap();
        let sysroot = SysRoot::new(root, false);
        (dir, sysroot)
    }

    /// A plan with the isolation section plus whatever the test needs.
    fn config(extra: &str) -> Config {
        Config::from_str_named(
            &format!("[isolation]\ncpus = \"2,3\"\nhousekeeping = \"0,1\"\n{extra}"),
            "test.toml",
        )
        .unwrap()
    }

    #[test]
    fn cmdline_round_trips_flags_and_values() {
        let parsed = Cmdline::parse("quiet isolcpus=domain,managed_irq,2-3  rcu_nocb_poll ro");
        assert_eq!(parsed.value_of("isolcpus"), Some("domain,managed_irq,2-3"));
        assert!(parsed.is_flag("rcu_nocb_poll"));
        assert!(parsed.contains("quiet"));
        assert_eq!(parsed.value_of("quiet"), None, "a flag has no value");
        assert_eq!(parsed.get("absent"), None);
        assert_eq!(parsed.len(), 4);
        assert_eq!(
            parsed.render(),
            "quiet isolcpus=domain,managed_irq,2-3 rcu_nocb_poll ro"
        );
        assert_eq!(format!("{parsed}"), parsed.render());

        // A repeated key keeps its first slot and the last value.
        assert_eq!(Cmdline::parse("a=1 b=2 a=3").render(), "a=3 b=2");
        let mut edited = Cmdline::parse("a=1 b");
        edited.set("a", Some("2"));
        edited.remove("b");
        assert_eq!(edited.render(), "a=2");
    }

    #[test]
    fn cmdline_is_read_through_the_sysroot() {
        let (_dir, sysroot) = fixture();
        let params = Cmdline::read(&sysroot);
        assert_eq!(active_isolated(&params), cpulist::parse("2,3").unwrap());
        let empty = SysRoot::new(sysroot.root().join("missing"), false);
        assert!(Cmdline::read(&empty).is_empty());
    }

    #[test]
    fn merge_managed_replaces_in_place_and_is_idempotent() {
        let mut wanted = Cmdline::new();
        wanted.set("isolcpus", Some("domain,managed_irq,6,7"));
        wanted.set("nohz_full", Some("6,7"));
        let merged = merge_managed("quiet root=/dev/sda1 isolcpus=2-3 audit=0", &wanted);
        assert_eq!(
            merged, "quiet root=/dev/sda1 isolcpus=domain,managed_irq,6,7 audit=0 nohz_full=6,7",
            "the managed parameter keeps its slot, new ones are appended"
        );
        assert_eq!(
            merge_managed(&merged, &wanted),
            merged,
            "second pass changes nothing"
        );
    }

    #[test]
    fn merge_managed_drops_duplicate_managed_tokens() {
        let mut wanted = Cmdline::new();
        wanted.set("isolcpus", Some("4"));
        assert_eq!(
            merge_managed("isolcpus=1 keep=me isolcpus=2", &wanted),
            "isolcpus=4 keep=me"
        );
    }

    #[test]
    fn active_sets_are_read_from_the_running_cmdline() {
        let params = Cmdline::parse("isolcpus=domain,managed_irq,6,7 nohz_full=6-7 rcu_nocbs=6,7");
        assert_eq!(active_isolated(&params), cpulist::parse("6,7").unwrap());
        assert_eq!(active_nohz_full(&params), cpulist::parse("6,7").unwrap());
        assert_eq!(active_rcu_nocbs(&params), cpulist::parse("6,7").unwrap());

        let legacy = Cmdline::parse("isolcpus=2-5");
        assert_eq!(active_isolated(&legacy), cpulist::parse("2-5").unwrap());

        // Flags without a CPU list must not be mistaken for isolation.
        assert!(active_isolated(&Cmdline::parse("isolcpus=domain,managed_irq")).is_empty());
        assert!(active_isolated(&Cmdline::new()).is_empty());
    }

    #[test]
    fn isolation_params_follows_the_plan() {
        let cfg = config(
            "nohz_full = true\nrcu_nocbs = true\nrcu_nocb_poll = true\ndomain = true\nidle = \"poll\"\nskew_tick = 1\n[kernel]\ncmdline_extra = [\"audit=0\", \"processor.max_cstate=0\"]\n",
        );
        let params = isolation_params(&cfg, cfg.isolated()).unwrap();
        assert_eq!(params.value_of("isolcpus"), Some("domain,managed_irq,2,3"));
        assert_eq!(params.value_of("nohz_full"), Some("2,3"));
        assert_eq!(params.value_of("rcu_nocbs"), Some("2,3"));
        assert!(params.is_flag("rcu_nocb_poll"));
        assert_eq!(params.value_of("idle"), Some("poll"));
        assert_eq!(params.value_of("skew_tick"), Some("1"));
        assert_eq!(params.value_of("audit"), Some("0"));
        assert_eq!(params.value_of("processor.max_cstate"), Some("0"));

        // The same set is what the plan asks a provisioner to write.
        let merged = merge_managed("quiet isolcpus=9", &params);
        assert!(
            merged.starts_with("quiet isolcpus=domain,managed_irq,2,3"),
            "{merged}"
        );
    }

    #[test]
    fn isolation_params_omits_disabled_parameters() {
        let cfg = config("nohz_full = false\nrcu_nocbs = false\nmanaged_irq = false\n");
        let params = isolation_params(&cfg, cfg.isolated()).unwrap();
        assert_eq!(params.value_of("isolcpus"), Some("2,3"), "no flags left");
        assert!(!params.contains("nohz_full"));
        assert!(!params.contains("rcu_nocbs"));
        assert!(
            !params.contains("rcu_nocb_poll"),
            "only meaningful with rcu_nocbs"
        );
    }

    #[test]
    fn malformed_extra_kernel_parameters_are_rejected() {
        let cfg = config("[kernel]\ncmdline_extra = [\"has space\"]\n");
        let err = isolation_params(&cfg, cfg.isolated()).unwrap_err();
        assert_eq!(err.kind(), Kind::Config);
        assert!(err.message().contains("malformed"), "{}", err.message());
    }

    #[test]
    fn grub_quoting_only_quotes_when_needed() {
        // Mirrors shlex.quote: anything outside the shell-safe set is quoted, and
        // a space is not in that set.
        assert_eq!(quote_for_grub("quiet"), "quiet");
        assert_eq!(quote_for_grub("isolcpus=2-3"), "isolcpus=2-3");
        assert_eq!(quote_for_grub("quiet isolcpus=2-3"), "'quiet isolcpus=2-3'");
        assert_eq!(quote_for_grub("semi;colon"), "'semi;colon'");
        assert_eq!(quote_for_grub("it's"), r"'it'\''s'");
        assert_eq!(quote_for_grub(""), "''");
    }

    #[test]
    fn inspect_cmdline_reports_a_matching_node() {
        let cfg = config("");
        let params = Cmdline::parse(
            "quiet isolcpus=domain,managed_irq,2,3 nohz_full=2,3 rcu_nocbs=2,3 nmi_watchdog=0 \
             nowatchdog skew_tick=1 audit=0 intel_idle.max_cstate=0 processor.max_cstate=0 tsc=reliable",
        );
        let findings = inspect_cmdline(&params, &cfg, cfg.isolated(), &cfg.housekeeping(4));
        assert!(findings.iter().all(Finding::ok), "{findings:?}");
        assert!(findings
            .iter()
            .any(|f| f.title.contains("isolcpus matches")));
        assert!(findings
            .iter()
            .any(|f| f.title.contains("rcu_nocbs covers")));
    }

    #[test]
    fn inspect_cmdline_flags_a_node_that_is_not_isolated() {
        let cfg = config("");
        let bare = inspect_cmdline(
            &Cmdline::parse("quiet"),
            &cfg,
            cfg.isolated(),
            &cfg.housekeeping(4),
        );
        assert!(bare
            .iter()
            .any(|f| f.level == FindingLevel::Error && f.title.contains("not active")));

        let legacy = inspect_cmdline(
            &Cmdline::parse("isolcpus=3"),
            &cfg,
            cfg.isolated(),
            &cfg.housekeeping(4),
        );
        assert!(legacy
            .iter()
            .any(|f| f.level == FindingLevel::Warn && f.title.contains("does not match")));
        assert!(
            legacy.iter().any(|f| f.title.contains("legacy")),
            "{legacy:?}"
        );
    }

    #[test]
    fn inspect_cmdline_errors_when_sets_overlap_or_housekeeping_is_empty() {
        let cfg = config("");
        let overlap = inspect_cmdline(
            &Cmdline::parse("isolcpus=2,3"),
            &cfg,
            cfg.isolated(),
            &cpulist::parse("3,4").unwrap(),
        );
        assert!(overlap
            .iter()
            .any(|f| f.level == FindingLevel::Error && f.title.contains("overlap")));

        let everything = inspect_cmdline(
            &Cmdline::parse("isolcpus=0-3"),
            &cfg,
            cfg.isolated(),
            &CpuSet::new(),
        );
        assert!(everything
            .iter()
            .any(|f| f.title.contains("no housekeeping CPUs remain")));
    }

    #[test]
    fn managed_drift_points_at_parameters_that_differ() {
        let cfg = config("");
        let findings = managed_drift(&Cmdline::parse("isolcpus=2-3"), &cfg, cfg.isolated());
        assert_eq!(
            findings.len(),
            3,
            "isolcpus differs, nohz_full and rcu_nocbs are missing"
        );
        assert!(findings.iter().all(|f| f.level == FindingLevel::Warn));
        assert!(findings.iter().any(|f| f.title.contains("nohz_full")));

        let clean = Cmdline::parse("isolcpus=managed_irq,2,3 nohz_full=2,3 rcu_nocbs=2,3");
        assert!(managed_drift(&clean, &cfg, cfg.isolated()).is_empty());
    }

    #[test]
    fn sysctl_paths_accept_both_spellings() {
        assert_eq!(
            sysctl_path("kernel.watchdog").unwrap(),
            "/proc/sys/kernel/watchdog"
        );
        assert_eq!(
            sysctl_path("/proc/sys/vm/stat_interval").unwrap(),
            "/proc/sys/vm/stat_interval"
        );
        assert_eq!(
            sysctl_name("/proc/sys/vm/stat_interval"),
            "vm.stat_interval"
        );
        assert_eq!(
            sysctl_name("/proc/sys/kernel/sched_rt_runtime_us"),
            "kernel.sched_rt_runtime_us"
        );
        assert!(sysctl_path("").is_err());
        assert!(sysctl_path("kernel..watchdog").is_err());
        assert!(sysctl_path("kernel/../etc/shadow").is_err());
        assert!(sysctl_path("/proc/sys/").is_err());
    }

    #[test]
    fn sysctl_plan_prefers_explicit_values_and_skips_missing_knobs() {
        let (_dir, sysroot) = fixture();
        let cfg = config(
            "[rt]\nrt_runtime_us = -1\n[kernel.sysctls]\n\"kernel.watchdog\" = \"0\"\n\"vm.stat_interval\" = \"10\"\n",
        );
        let plan = sysctl_plan(&sysroot, &cfg).unwrap();
        let names: Vec<&str> = plan.iter().map(|tunable| tunable.name.as_str()).collect();
        assert!(
            !names.contains(&"vm.stat_interval"),
            "the node does not expose it: {names:?}"
        );
        assert!(names.contains(&"kernel.watchdog"));
        assert!(
            names.contains(&"kernel.timer_migration"),
            "high_res_timers implies it"
        );
        assert!(names.contains(&"kernel.sched_rt_runtime_us"));

        let watchdog = plan.iter().find(|t| t.name == "kernel.watchdog").unwrap();
        assert_eq!(watchdog.path, "/proc/sys/kernel/watchdog");
        assert_eq!(watchdog.value, "0");
        assert_eq!(watchdog.current.as_deref(), Some("1"));
        assert!(!watchdog.already_set());
        assert_eq!(watchdog.reason, "configured in [kernel.sysctls]");

        let rt = plan
            .iter()
            .find(|t| t.name == "kernel.sched_rt_runtime_us")
            .unwrap();
        assert_eq!(rt.value, "-1");
        assert!(rt.reason.contains("negative budget"), "{}", rt.reason);
    }

    #[test]
    fn explicit_sysctls_override_implied_ones() {
        let (_dir, sysroot) = fixture();
        let cfg = config(
            "[rt]\nrt_runtime_us = -1\n[kernel.sysctls]\n\"kernel.sched_rt_runtime_us\" = \"800000\"\n",
        );
        let plan = sysctl_plan(&sysroot, &cfg).unwrap();
        let matches: Vec<&Tunable> = plan
            .iter()
            .filter(|tunable| tunable.name == "kernel.sched_rt_runtime_us")
            .collect();
        assert_eq!(matches.len(), 1, "one entry, not two");
        assert_eq!(matches[0].value, "800000");
        assert_eq!(matches[0].reason, "configured in [kernel.sysctls]");
    }

    #[test]
    fn apply_sysctls_writes_once_and_is_idempotent() {
        let (_dir, sysroot) = fixture();
        let cfg = config("[kernel.sysctls]\n\"kernel.watchdog\" = \"0\"\n");
        let plan = sysctl_plan(&sysroot, &cfg).unwrap();
        let records = apply_sysctls(&sysroot, &plan).unwrap();
        assert!(records
            .iter()
            .any(|record| record.path == "/proc/sys/kernel/watchdog"));
        assert_eq!(
            sysroot.read_trimmed("/proc/sys/kernel/watchdog").as_deref(),
            Some("0")
        );

        let second = sysctl_plan(&sysroot, &cfg).unwrap();
        assert!(second
            .iter()
            .find(|t| t.name == "kernel.watchdog")
            .unwrap()
            .already_set());
        assert!(apply_sysctls(&sysroot, &second).unwrap().is_empty());
    }

    #[test]
    fn dry_run_journals_sysctls_without_writing() {
        let dir = TempDir::new("isolera-kernel-dry").unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("proc/sys/kernel")).unwrap();
        fs::write(root.join("proc/sys/kernel/watchdog"), "1\n").unwrap();
        let sysroot = SysRoot::new(&root, true);
        let cfg = config("[kernel.sysctls]\n\"kernel.watchdog\" = \"0\"\n");
        let plan = sysctl_plan(&sysroot, &cfg).unwrap();
        let records = apply_sysctls(&sysroot, &plan).unwrap();
        assert!(!records.is_empty());
        assert!(records.iter().all(|record| !record.applied));
        assert_eq!(
            fs::read_to_string(root.join("proc/sys/kernel/watchdog")).unwrap(),
            "1\n"
        );
    }

    #[test]
    fn restore_sysctls_resets_only_non_stock_values() {
        let (_dir, sysroot) = fixture();
        sysroot
            .write("/proc/sys/kernel/watchdog", "0", "test")
            .unwrap();
        let records = restore_sysctls(&sysroot).unwrap();
        assert!(records
            .iter()
            .any(|record| record.path == "/proc/sys/kernel/watchdog"));
        assert!(
            !records
                .iter()
                .any(|record| record.path == "/proc/sys/kernel/timer_migration"),
            "a knob that is already stock is left alone"
        );
        assert_eq!(
            sysroot.read_trimmed("/proc/sys/kernel/watchdog").as_deref(),
            Some("1")
        );
        assert!(
            restore_sysctls(&sysroot).unwrap().is_empty(),
            "restore is idempotent"
        );
    }

    #[test]
    fn render_tunables_shows_pending_state() {
        let tunables = vec![Tunable {
            path: "/proc/sys/kernel/watchdog".to_string(),
            name: "kernel.watchdog".to_string(),
            value: "0".to_string(),
            current: Some("1".to_string()),
            reason: "configured in [kernel.sysctls]".to_string(),
        }];
        let rendered = render_tunables(&tunables);
        assert!(rendered.starts_with("sysctl"), "{rendered}");
        assert!(rendered.contains("kernel.watchdog"));
        assert!(rendered.contains("yes"));
        assert_eq!(render_tunables(&[]), "no sysctl changes required\n");
    }

    /// A fixture ``/proc`` with kernel threads and one user process.
    fn process_fixture() -> (TempDir, SysRoot) {
        let dir = TempDir::new("isolera-threads").unwrap();
        let root = dir.path().join("root");
        let rows: [(&str, &str, &str, &str); 5] = [
            ("2", "kthreadd", "", "0-3"),
            ("15", "kworker/2:1", "", "2-3"),
            ("16", "kworker/0:2", "", "0-1"),
            ("17", "migration/3", "", "0-3"),
            ("4242", "matching-engine", "/opt/engine\0--run\0", "2,3"),
        ];
        for (pid, name, cmdline, cpus) in rows {
            let base = root.join(format!("proc/{pid}"));
            fs::create_dir_all(&base).unwrap();
            fs::write(base.join("comm"), format!("{name}\n")).unwrap();
            fs::write(base.join("cmdline"), cmdline).unwrap();
            fs::write(
                base.join("status"),
                format!("Name:\t{name}\nCpus_allowed_list:\t{cpus}\n"),
            )
            .unwrap();
        }
        let sysroot = SysRoot::new(root, false);
        (dir, sysroot)
    }

    #[test]
    fn kernel_threads_ignores_user_processes() {
        let (_dir, sysroot) = process_fixture();
        let threads = kernel_threads(&sysroot);
        let names: Vec<&str> = threads.iter().map(|thread| thread.name.as_str()).collect();
        assert_eq!(
            names,
            vec!["kthreadd", "kworker/2:1", "kworker/0:2", "migration/3"]
        );
        let kworker = threads
            .iter()
            .find(|thread| thread.name == "kworker/2:1")
            .unwrap();
        assert_eq!(kworker.pid, 15);
        assert_eq!(kworker.affinity, Some(cpulist::parse("2-3").unwrap()));
        assert!(threads
            .iter()
            .all(|thread| thread.name != "matching-engine"));
    }

    #[test]
    fn status_cpus_finds_the_allowed_list() {
        let status = "Name:\tkworker/2:1\nState:\tS\nCpus_allowed_list:\t0-3\n";
        assert_eq!(status_cpus(status), Some(cpulist::parse("0-3").unwrap()));
        assert_eq!(status_cpus("Name:\tx\n"), None);
        assert_eq!(status_cpus("Cpus_allowed_list:\tgarbage\n"), None);
    }

    #[test]
    fn pinning_plan_selects_threads_that_sit_on_isolated_cores() {
        let (_dir, sysroot) = process_fixture();
        let housekeeping = cpulist::parse("0,1").unwrap();
        let cfg = config("[rt]\npin_kernel_threads = true\nkeep = [\"kworker/0:2\"]\n");
        let commands = pinning_plan(&sysroot, &cfg, &housekeeping);
        let names: Vec<&str> = commands
            .iter()
            .map(|command| command.name.as_str())
            .collect();
        assert_eq!(names, vec!["kworker/2:1", "migration/3"]);
        assert_eq!(commands[0].target, housekeeping);
        assert!(
            commands[0].reason.contains("isolated CPU(s) 2,3"),
            "{}",
            commands[0].reason
        );
        assert!(
            commands.iter().all(|command| command.name != "kthreadd"),
            "not movable"
        );
        assert!(
            commands.iter().all(|command| command.name != "kworker/0:2"),
            "kept by config"
        );
    }

    #[test]
    fn pinning_is_off_by_default_and_needs_housekeeping_cores() {
        let (_dir, sysroot) = process_fixture();
        let housekeeping = cpulist::parse("0,1").unwrap();
        let disabled = config("[rt]\npin_kernel_threads = false\n");
        assert!(pinning_plan(&sysroot, &disabled, &housekeeping).is_empty());
        let enabled = config("[rt]\npin_kernel_threads = true\n");
        assert!(pinning_plan(&sysroot, &enabled, &CpuSet::new()).is_empty());
    }

    #[test]
    fn render_pin_commands_emits_a_runnable_shell_block() {
        let commands = vec![PinCommand {
            pid: 15,
            name: "kworker/2:1".to_string(),
            from: Some(cpulist::parse("2-3").unwrap()),
            target: cpulist::parse("0,1").unwrap(),
            reason: "bound to isolated CPU(s) 2,3".to_string(),
        }];
        let rendered = render_pin_commands(&commands, Some(80));
        assert!(rendered.starts_with("#!/bin/sh\n"), "{rendered}");
        assert!(rendered.contains("# bound to isolated CPU(s) 2,3\n"));
        assert!(rendered.contains("chrt -f -p 80 15"));
        assert!(rendered.contains("taskset -pc 0,1 15"));
        assert!(rendered.ends_with("kworker/2:1\n"));
        assert_eq!(
            render_pin_commands(&[], None),
            "no kernel threads need moving\n"
        );
    }

    #[test]
    fn json_serialisers_escape_and_represent_absent_values() {
        let finding = Finding::new(FindingLevel::Warn, "a \"quoted\" title", "detail");
        assert!(finding.to_json().contains("\\\"quoted\\\""));
        assert!(finding.to_json().contains("\"level\":\"warn\""));

        let tunable = Tunable {
            path: "/proc/sys/kernel/watchdog".to_string(),
            name: "kernel.watchdog".to_string(),
            value: "0".to_string(),
            current: None,
            reason: "x".to_string(),
        };
        assert!(tunable.to_json().contains("\"current\":null"));

        let command = PinCommand {
            pid: 15,
            name: "kworker/2:1".to_string(),
            from: None,
            target: cpulist::parse("0").unwrap(),
            reason: "r".to_string(),
        };
        assert!(command.to_json().contains("\"from\":null"));
        let thread = KernelThread {
            pid: 15,
            name: "kworker/2:1".to_string(),
            affinity: None,
        };
        assert!(thread.to_json().contains("\"affinity\":null"));
    }
}
