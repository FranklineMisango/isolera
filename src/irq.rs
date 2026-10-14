//! Interrupt parsing, classification and steering.
//!
//! Three facts drive this module, and all three are easy to get wrong:
//!
//! 1. ``/proc/interrupts`` counts are **monotonic since boot**. Deciding whether
//!    an IRQ is *noisy* therefore needs two samples and a [`Interrupt::delta`],
//!    never a single reading.
//! 2. Most interrupts whose affinity matters (``LOC``, ``NMI``, ``RES``, ``CAL``)
//!    are not user-steerable at all. They can only be influenced through the
//!    kernel command line, so they are classified and reported rather than pinned.
//! 3. A steering decision needs a *policy*, not a patch: an exclusion list plus
//!    exact overrides, with an explicit precedence rule between them.

use crate::config::Precedence;
use crate::cpulist::{self, CpuSet};
use crate::error::{Error, Kind, Result};
use crate::sysroot::{SysRoot, WriteRecord};
use std::collections::BTreeMap;

/// ``/proc`` paths for the interrupt surfaces.
const PROC_INTERRUPTS: &str = "/proc/interrupts";
const PROC_IRQ_DIR: &str = "/proc/irq";
const PROC_DEFAULT_AFFINITY: &str = "/proc/irq/default_smp_affinity";

/// True for pseudo-interrupts the user cannot steer.
///
/// These names appear in the left-hand column of ``/proc/interrupts`` and are
/// reported by the kernel without a device behind them.
pub fn is_pseudo(name: &str) -> bool {
    const PSEUDO: [&str; 24] = [
        "NMI", "LOC", "SPU", "PMI", "IWI", "RTR", "RES", "CAL", "TLB", "TRM", "THR", "DFR", "MCE",
        "HYP", "ERR", "MIS", "PIN", "NPI", "PIW", "SYS", "CLK", "LVT", "CNT", "PLT",
    ];
    PSEUDO.contains(&name)
}

/// One row of ``/proc/interrupts``.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interrupt {
    /// Identifier as printed: ``"24"`` for a device IRQ, ``"NMI"`` for a pseudo one.
    pub id: String,
    /// Numeric IRQ, absent for pseudo-interrupts.
    pub number: Option<u32>,
    /// Per-CPU counts, in the order the header listed the CPUs.
    pub counts: Vec<u64>,
    /// CPU indices matching `counts`.
    pub cpus: Vec<u32>,
    /// Interrupt controller or trigger description, when present.
    pub controller: Option<String>,
    /// Device or driver name, when present.
    pub name: String,
}

impl Interrupt {
    /// Total hits since boot.
    pub fn total(&self) -> u64 {
        self.counts.iter().sum()
    }

    /// True when the interrupt has a device behind it and can be steered.
    pub fn is_steerable(&self) -> bool {
        self.number.is_some()
    }

    /// True when this is a kernel pseudo-interrupt.
    pub fn is_pseudo(&self) -> bool {
        self.number.is_none()
    }

    /// CPUs with a non-zero count.
    pub fn active_cpus(&self) -> CpuSet {
        self.cpus
            .iter()
            .zip(self.counts.iter())
            .filter(|(_, count)| **count > 0)
            .map(|(cpu, _)| *cpu)
            .collect()
    }

    /// Hits attributable to `cpu`, if that CPU was present in the sample.
    pub fn hits_on(&self, cpu: u32) -> u64 {
        self.cpus
            .iter()
            .position(|candidate| *candidate == cpu)
            .and_then(|position| self.counts.get(position).copied())
            .unwrap_or(0)
    }

    /// Per-CPU difference against an earlier sample of the same interrupt.
    ///
    /// Returns `None` when the two samples cover different CPUs, which happens
    /// when CPUs are onlined or offlined between reads.
    pub fn delta(&self, earlier: &Interrupt) -> Option<Vec<u64>> {
        if earlier.cpus != self.cpus {
            return None;
        }
        Some(
            self.counts
                .iter()
                .zip(earlier.counts.iter())
                .map(|(now, before)| now.saturating_sub(*before))
                .collect(),
        )
    }

    /// CPUs that saw activity between an earlier sample and this one.
    pub fn delta_cpus(&self, earlier: &Interrupt) -> Option<CpuSet> {
        let deltas = self.delta(earlier)?;
        Some(
            self.cpus
                .iter()
                .zip(deltas.iter())
                .filter(|(_, delta)| **delta > 0)
                .map(|(cpu, _)| *cpu)
                .collect(),
        )
    }

    /// Best-effort display name: the device name, or the controller kind.
    pub fn label(&self) -> String {
        if !self.name.is_empty() {
            self.name.clone()
        } else if let Some(controller) = &self.controller {
            controller.clone()
        } else {
            self.id.clone()
        }
    }
}

/// A snapshot of ``/proc/interrupts``.
#[derive(Debug, Clone, Default)]
pub struct InterruptTable {
    /// CPUs in the order the header listed them.
    pub cpus: Vec<u32>,
    /// One entry per row.
    pub entries: Vec<Interrupt>,
}

impl InterruptTable {
    /// Look up an interrupt by its printed id (``"24"`` or ``"NMI"``).
    pub fn get(&self, id: &str) -> Option<&Interrupt> {
        self.entries.iter().find(|entry| entry.id == id)
    }

    /// Look up an interrupt by number.
    pub fn by_number(&self, number: u32) -> Option<&Interrupt> {
        self.entries
            .iter()
            .find(|entry| entry.number == Some(number))
    }

    /// Entries that can be steered through sysfs.
    pub fn steerable(&self) -> impl Iterator<Item = &Interrupt> {
        self.entries.iter().filter(|entry| entry.is_steerable())
    }

    /// Total hits recorded on `cpu` across every interrupt.
    pub fn total_on(&self, cpu: u32) -> u64 {
        self.entries.iter().map(|entry| entry.hits_on(cpu)).sum()
    }

    /// Per-entry deltas against an earlier sample of the same table.
    pub fn deltas(&self, earlier: &InterruptTable) -> Vec<(String, Vec<u64>)> {
        self.entries
            .iter()
            .filter_map(|entry| {
                let previous = earlier.get(&entry.id)?;
                let delta = entry.delta(previous)?;
                if delta.iter().all(|value| *value == 0) {
                    None
                } else {
                    Some((entry.id.clone(), delta))
                }
            })
            .collect()
    }
}

/// True when a token looks like a controller name (``IR-PCI-MSI``, ``IO-APIC``).
fn is_chip_token(token: &str) -> bool {
    token.len() > 1
        && token.chars().all(|character| {
            character.is_ascii_uppercase() || character.is_ascii_digit() || character == '-'
        })
        && token
            .chars()
            .any(|character| character.is_ascii_uppercase())
}

/// True when a token is a flow/trigger descriptor (``524288-edge``, ``16-fasteoi``).
fn is_flow_token(token: &str) -> bool {
    if token.is_empty() {
        return false;
    }
    if token.contains("edge") || token.contains("level") {
        return true;
    }
    if token.ends_with("eoi") || token.ends_with("EOI") {
        return true;
    }
    let leading_digits = token
        .chars()
        .take_while(|character| character.is_ascii_digit())
        .count();
    leading_digits > 0 && token.chars().nth(leading_digits) == Some('-')
}

/// Parse the text of ``/proc/interrupts``.
pub fn parse_interrupts(text: &str) -> InterruptTable {
    let mut cpus: Vec<u32> = Vec::new();
    let mut entries: Vec<Interrupt> = Vec::new();

    for line in text.lines() {
        let trimmed = line.trim_end();
        if trimmed.trim().is_empty() {
            continue;
        }
        let tokens: Vec<&str> = trimmed.split_whitespace().collect();
        if tokens.is_empty() {
            continue;
        }
        if cpus.is_empty() && tokens[0].starts_with("CPU") {
            for token in &tokens {
                if let Some(index) = token.strip_prefix("CPU") {
                    if let Ok(cpu) = index.parse::<u32>() {
                        cpus.push(cpu);
                    }
                }
            }
            continue;
        }
        let Some(label) = tokens[0].strip_suffix(':') else {
            continue;
        };
        let number = label.parse::<u32>().ok();
        let id = label.to_string();

        let mut counts: Vec<u64> = Vec::new();
        let mut rest: Vec<&str> = Vec::new();
        let mut reading_counts = true;
        for token in tokens.iter().skip(1) {
            if reading_counts {
                if let Ok(value) = token.parse::<u64>() {
                    counts.push(value);
                    continue;
                }
                reading_counts = false;
            }
            rest.push(token);
        }
        let cpus_for_row = if cpus.is_empty() {
            (0..counts.len() as u32).collect()
        } else {
            cpus.clone()
        };

        let mut controller_parts: Vec<&str> = Vec::new();
        if let Some(first) = rest.first() {
            if is_chip_token(first) {
                controller_parts.push(first);
                rest.remove(0);
            }
        }
        if let Some(first) = rest.first() {
            if is_flow_token(first) {
                controller_parts.push(first);
                rest.remove(0);
            }
        }
        let controller = if controller_parts.is_empty() {
            None
        } else {
            Some(controller_parts.join(" "))
        };
        let name = rest.join(" ");

        entries.push(Interrupt {
            id,
            number,
            counts,
            cpus: cpus_for_row,
            controller,
            name,
        });
    }

    if cpus.is_empty() {
        let width = entries
            .iter()
            .map(|entry| entry.counts.len())
            .max()
            .unwrap_or(0);
        cpus = (0..width as u32).collect();
        for entry in &mut entries {
            entry.cpus = cpus.clone();
        }
    }
    InterruptTable { cpus, entries }
}

/// Read ``/proc/interrupts`` through the sysroot.
pub fn read_interrupts(sysroot: &SysRoot) -> InterruptTable {
    match sysroot.read_optional(PROC_INTERRUPTS) {
        Some(text) => parse_interrupts(&text),
        None => InterruptTable::default(),
    }
}

/// Shell-style pattern match supporting `*`, `?` and `[abc]`/`[!abc]` sets.
///
/// This is fnmatch semantics, so a pattern without wildcards is an exact match.
/// `report` warns when a configured pattern matched nothing, which turns a typo
/// into a visible finding instead of a silent no-op.
pub fn matches(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    matches_from(&pattern, 0, &text, 0)
}

fn matches_from(pattern: &[char], mut p: usize, text: &[char], mut t: usize) -> bool {
    loop {
        match pattern.get(p) {
            None => return t == text.len(),
            Some('*') => {
                p += 1;
                if p == pattern.len() {
                    return true;
                }
                while t <= text.len() {
                    if matches_from(pattern, p, text, t) {
                        return true;
                    }
                    t += 1;
                }
                return false;
            }
            Some('?') => {
                if t >= text.len() {
                    return false;
                }
                p += 1;
                t += 1;
            }
            Some('[') => {
                if t >= text.len() {
                    return false;
                }
                let (negated, end) = match pattern.get(p + 1) {
                    Some('!') | Some('^') => (true, match_set_end(pattern, p + 2)),
                    _ => (false, match_set_end(pattern, p + 1)),
                };
                let Some(end) = end else {
                    // Unterminated set: treat '[' literally.
                    if text.get(t) != Some(&'[') {
                        return false;
                    }
                    p += 1;
                    t += 1;
                    continue;
                };
                let member = set_contains(&pattern[p + 1..end], text[t], negated);
                if !member {
                    return false;
                }
                p = end + 1;
                t += 1;
            }
            Some(character) => {
                if text.get(t) != Some(character) {
                    return false;
                }
                p += 1;
                t += 1;
            }
        }
    }
}

fn match_set_end(pattern: &[char], from: usize) -> Option<usize> {
    let mut index = from;
    while let Some(character) = pattern.get(index) {
        if *character == ']' {
            return Some(index);
        }
        index += 1;
    }
    None
}

fn set_contains(set: &[char], candidate: char, negated: bool) -> bool {
    let mut found = false;
    let mut index = 0;
    while index < set.len() {
        let start = set[index];
        if set.get(index + 1) == Some(&'-') && set.get(index + 2).is_some() && set[index + 2] != ']'
        {
            let end = set[index + 2];
            if candidate >= start && candidate <= end {
                found = true;
            }
            index += 3;
        } else {
            if candidate == start {
                found = true;
            }
            index += 1;
        }
    }
    found != negated
}

/// Current affinity mask for an IRQ, preferring the list form.
pub fn affinity(sysroot: &SysRoot, irq: u32) -> Option<CpuSet> {
    let list = format!("{PROC_IRQ_DIR}/{irq}/smp_affinity_list");
    if let Some(text) = sysroot.read_trimmed(&list) {
        if let Ok(set) = cpulist::parse(&text) {
            return Some(set);
        }
    }
    let mask = format!("{PROC_IRQ_DIR}/{irq}/smp_affinity");
    let text = sysroot.read_trimmed(&mask)?;
    cpulist::from_hex_mask(&text).ok()
}

/// Affinity mask the kernel is actually using, when it differs from the request.
pub fn effective_affinity(sysroot: &SysRoot, irq: u32) -> Option<CpuSet> {
    let text = sysroot.read_trimmed(&format!("{PROC_IRQ_DIR}/{irq}/effective_affinity_list"))?;
    cpulist::parse(&text).ok()
}

/// Default affinity applied to IRQs that do not override it.
pub fn default_affinity(sysroot: &SysRoot) -> Option<CpuSet> {
    let text = sysroot.read_trimmed(PROC_DEFAULT_AFFINITY)?;
    cpulist::from_hex_mask(&text).ok()
}

/// True when the running kernel will accept an affinity change for `irq`.
pub fn is_writable(sysroot: &SysRoot, irq: u32) -> bool {
    let list = format!("{PROC_IRQ_DIR}/{irq}/smp_affinity_list");
    sysroot.exists(&list) || sysroot.exists(&format!("{PROC_IRQ_DIR}/{irq}/smp_affinity"))
}

/// Write the affinity of one IRQ, using the list form when available.
pub fn set_affinity(
    sysroot: &SysRoot,
    irq: u32,
    target: &CpuSet,
    cpu_count: usize,
    reason: &str,
) -> Result<Option<WriteRecord>> {
    if target.is_empty() {
        return Err(Error::new(
            Kind::Config,
            format!("refusing to set an empty affinity mask for IRQ {irq}"),
        ));
    }
    let list = format!("{PROC_IRQ_DIR}/{irq}/smp_affinity_list");
    if sysroot.exists(&list) {
        return sysroot.write(&list, &cpulist::format(target), reason);
    }
    let mask = format!("{PROC_IRQ_DIR}/{irq}/smp_affinity");
    if sysroot.exists(&mask) {
        return sysroot.write(&mask, &cpulist::to_hex_mask(target, cpu_count), reason);
    }
    Ok(None)
}

/// Write ``/proc/irq/default_smp_affinity``.
pub fn set_default_affinity(
    sysroot: &SysRoot,
    target: &CpuSet,
    cpu_count: usize,
    reason: &str,
) -> Result<Option<WriteRecord>> {
    if !sysroot.exists(PROC_DEFAULT_AFFINITY) {
        return Ok(None);
    }
    sysroot.write(
        PROC_DEFAULT_AFFINITY,
        &cpulist::to_hex_mask(target, cpu_count),
        reason,
    )
}

/// What the plan does with one IRQ.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Already where the policy wants it.
    Keep,
    /// Affinity includes isolated CPUs and is moved back to housekeeping.
    Pin,
    /// Matched an exclusion rule.
    Exclude,
    /// Matched an explicit override.
    Override,
    /// Explicitly left alone.
    Ignore,
    /// Kernel-internal interrupt; not steerable from sysfs.
    Unsteerable,
}

impl Action {
    /// Short label used in reports.
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Keep => "keep",
            Action::Pin => "pin",
            Action::Exclude => "exclude",
            Action::Override => "override",
            Action::Ignore => "ignore",
            Action::Unsteerable => "unsteerable",
        }
    }

    /// True when the action implies a write.
    pub fn writes(self) -> bool {
        matches!(self, Action::Pin | Action::Exclude | Action::Override)
    }
}

/// One IRQ's fate under the policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    /// IRQ number (0 for pseudo-interrupts).
    pub irq: u32,
    /// Device or pseudo-interrupt name.
    pub label: String,
    /// Affinity found on the node.
    pub current: CpuSet,
    /// Affinity the policy wants.
    pub target: CpuSet,
    /// What the plan does.
    pub action: Action,
    /// Why, in one line.
    pub reason: String,
}

impl Decision {
    /// True when applying this decision changes the node.
    pub fn changes(&self) -> bool {
        self.action.writes() && self.target != self.current
    }
}

/// The steering policy distilled from the configuration.
#[derive(Debug, Clone)]
pub struct Policy {
    /// CPUs reserved for the strategy.
    pub isolated: CpuSet,
    /// CPUs that may take interrupts.
    pub housekeeping: CpuSet,
    /// Patterns that must not reach an isolated CPU.
    pub exclude: Vec<String>,
    /// Pattern -> CPUs, checked before exclusions.
    pub overrides: Vec<(String, CpuSet)>,
    /// Patterns to leave untouched.
    pub ignore: Vec<String>,
    /// Which of exclude/override wins on a double match.
    pub precedence: Precedence,
}

impl Policy {
    /// Build the policy from a validated config.
    pub fn from_config(config: &crate::config::Config, cpu_count: usize) -> Result<Policy> {
        let housekeeping = config.housekeeping(cpu_count);
        if housekeeping.is_empty() {
            return Err(Error::new(
                Kind::Config,
                "no housekeeping CPUs: isolation.cpus covers every CPU on this node".to_string(),
            ));
        }
        Ok(Policy {
            isolated: config.isolated().clone(),
            housekeeping,
            exclude: config.irq.exclude.clone(),
            overrides: config.irq_overrides()?.into_iter().collect(),
            ignore: config.irq.ignore.clone(),
            precedence: config.irq.precedence,
        })
    }

    fn override_for(&self, label: &str) -> Option<(&str, &CpuSet)> {
        self.overrides
            .iter()
            .find(|(pattern, _)| matches(pattern, label))
            .map(|(pattern, set)| (pattern.as_str(), set))
    }

    fn is_excluded(&self, label: &str) -> Option<&str> {
        self.exclude
            .iter()
            .find(|pattern| matches(pattern, label))
            .map(String::as_str)
    }

    fn is_ignored(&self, label: &str) -> Option<&str> {
        self.ignore
            .iter()
            .find(|pattern| matches(pattern, label))
            .map(String::as_str)
    }

    /// Which configured patterns matched nothing at all.
    pub fn unused_patterns(&self, table: &InterruptTable) -> Vec<String> {
        let labels: Vec<String> = table.entries.iter().map(|entry| entry.label()).collect();
        let mut unused = Vec::new();
        for pattern in self.exclude.iter().chain(self.ignore.iter()) {
            if !labels.iter().any(|label| matches(pattern, label)) {
                unused.push(pattern.clone());
            }
        }
        for (pattern, _) in &self.overrides {
            if !labels.iter().any(|label| matches(pattern, label)) {
                unused.push(pattern.clone());
            }
        }
        unused.sort();
        unused.dedup();
        unused
    }
}

/// Decide what to do with every interrupt on the node.
///
/// Pseudo-interrupts are included so the output shows the complete picture; they
/// always come back as [`Action::Unsteerable`].
pub fn plan(sysroot: &SysRoot, table: &InterruptTable, policy: &Policy) -> Vec<Decision> {
    let mut decisions = Vec::new();
    for entry in &table.entries {
        let label = entry.label();
        let Some(irq) = entry.number else {
            decisions.push(Decision {
                irq: 0,
                label,
                current: CpuSet::new(),
                target: CpuSet::new(),
                action: Action::Unsteerable,
                reason: format!(
                    "kernel-internal interrupt ({}); only the command line can influence it",
                    entry.id
                ),
            });
            continue;
        };
        let current = affinity(sysroot, irq).unwrap_or_default();

        if let Some(pattern) = policy.is_ignored(&label) {
            decisions.push(Decision {
                irq,
                label,
                target: current.clone(),
                current,
                action: Action::Ignore,
                reason: format!("matches irq.ignore pattern {pattern:?}"),
            });
            continue;
        }

        let override_match = policy
            .override_for(&label)
            .map(|(pattern, set)| (pattern.to_string(), set.clone()));
        let excluded = policy.is_excluded(&label).map(str::to_string);
        let take_override = match (&override_match, &excluded) {
            (Some(_), Some(_)) => policy.precedence == Precedence::OverrideWins,
            (Some(_), None) => true,
            _ => false,
        };

        let (action, target, reason) = if take_override {
            let (pattern, target) = override_match.unwrap_or_default();
            (
                Action::Override,
                target.clone(),
                format!(
                    "irq.overrides pattern {pattern:?} pins this IRQ to {}",
                    cpulist::format(&target)
                ),
            )
        } else if let Some(pattern) = excluded {
            if cpulist::intersects(&current, &policy.isolated) {
                (
                    Action::Exclude,
                    policy.housekeeping.clone(),
                    format!(
                        "irq.exclude pattern {pattern:?} moves this IRQ off isolated CPU(s) {}",
                        cpulist::format(&cpulist::intersection(&current, &policy.isolated))
                    ),
                )
            } else {
                // The mask already avoids every isolated CPU. Widening it to the
                // whole housekeeping set would let the device spread onto cores it
                // does not use today, so the exclusion records the rule and stops.
                (
                    Action::Exclude,
                    current.clone(),
                    format!(
                        "irq.exclude pattern {pattern:?}; affinity {} already avoids the isolated cores",
                        cpulist::format(&current)
                    ),
                )
            }
        } else if cpulist::intersects(&current, &policy.isolated) {
            (
                Action::Pin,
                policy.housekeeping.clone(),
                format!(
                    "current affinity {} includes isolated CPU(s) {}",
                    cpulist::format(&current),
                    cpulist::format(&cpulist::intersection(&current, &policy.isolated))
                ),
            )
        } else {
            (
                Action::Keep,
                current.clone(),
                format!(
                    "affinity {} already avoids the isolated cores",
                    cpulist::format(&current)
                ),
            )
        };

        decisions.push(Decision {
            irq,
            label,
            current,
            target,
            action,
            reason,
        });
    }
    decisions
}

/// Outcome of applying a plan.
#[derive(Debug, Clone, Default)]
pub struct Applied {
    /// Writes that were performed or journalled.
    pub records: Vec<WriteRecord>,
    /// IRQs whose affinity file is missing or read-only.
    pub skipped: Vec<(u32, String)>,
}

/// Execute the steering decisions through the sysroot.
pub fn apply(sysroot: &SysRoot, decisions: &[Decision], cpu_count: usize) -> Result<Applied> {
    let mut outcome = Applied::default();
    for decision in decisions {
        if !decision.changes() {
            continue;
        }
        let reason = format!("isolera: {}", decision.reason);
        if !is_writable(sysroot, decision.irq) {
            outcome
                .skipped
                .push((decision.irq, "no writable affinity file".to_string()));
            continue;
        }
        match set_affinity(sysroot, decision.irq, &decision.target, cpu_count, &reason)? {
            Some(record) => outcome.records.push(record),
            None => outcome.skipped.push((
                decision.irq,
                "already at the requested affinity".to_string(),
            )),
        }
    }
    Ok(outcome)
}

/// Render decisions as an aligned table.
pub fn render_decisions(decisions: &[Decision], changes_only: bool) -> String {
    use crate::table::Table;
    let mut table = Table::new(["irq", "name", "current", "target", "action", "reason"]);
    let mut shown = 0usize;
    for decision in decisions {
        if changes_only && !decision.changes() {
            continue;
        }
        shown += 1;
        table.push([
            decision.irq.to_string(),
            crate::log::truncate(&decision.label, 32),
            cpulist::format(&decision.current),
            cpulist::format(&decision.target),
            decision.action.as_str().to_string(),
            crate::log::truncate(&decision.reason, 64),
        ]);
    }
    if shown == 0 {
        return "no interrupt changes required\n".to_string();
    }
    table.render()
}

/// Count the decisions by action.
pub fn summarize(decisions: &[Decision]) -> BTreeMap<&'static str, usize> {
    let mut summary: BTreeMap<&'static str, usize> = BTreeMap::new();
    for decision in decisions {
        *summary.entry(decision.action.as_str()).or_insert(0) += 1;
    }
    summary
}

/// Cumulative hits on the isolated CPUs, per interrupt, highest first.
pub fn isolated_hits(table: &InterruptTable, isolated: &CpuSet) -> Vec<(String, String, u64)> {
    let mut hits: Vec<(String, String, u64)> = table
        .entries
        .iter()
        .filter_map(|entry| {
            let total: u64 = entry
                .cpus
                .iter()
                .zip(entry.counts.iter())
                .filter(|(cpu, _)| isolated.contains(cpu))
                .map(|(_, count)| *count)
                .sum();
            if total == 0 {
                None
            } else {
                Some((entry.id.clone(), entry.label(), total))
            }
        })
        .collect();
    hits.sort_by(|left, right| right.2.cmp(&left.2));
    hits
}

/// Hits on the isolated CPUs between two samples, as `(id, label, hits)`.
pub fn isolated_deltas(
    before: &InterruptTable,
    after: &InterruptTable,
    isolated: &CpuSet,
) -> Vec<(String, String, u64)> {
    let mut hits: Vec<(String, String, u64)> = after
        .entries
        .iter()
        .filter_map(|entry| {
            let previous = before.get(&entry.id)?;
            let deltas = entry.delta(previous)?;
            let total: u64 = entry
                .cpus
                .iter()
                .zip(deltas.iter())
                .filter(|(cpu, _)| isolated.contains(cpu))
                .map(|(_, delta)| *delta)
                .sum();
            if total == 0 {
                None
            } else {
                Some((entry.id.clone(), entry.label(), total))
            }
        })
        .collect();
    hits.sort_by(|left, right| right.2.cmp(&left.2));
    hits
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir::TempDir;
    use std::fs;

    const INTERRUPTS: &str = "\
           CPU0       CPU1       CPU2       CPU3
  0:         12          2          0          1   IO-APIC   2-edge      timer
 24:     100000          0          0          0   IR-PCI-MSI 524288-edge      nvme0q0
 25:     200000          0          0          0   IR-PCI-MSI 524289-edge      nvme0q1
122:          0       1512          0          0   IR-PCI-MSI 327680-edge      eth0-rx-0
123:          0          0      98741          0   IR-PCI-MSI 327681-edge      eth0-tx-0
124:        400          0          0        512   IR-PCI-MSI 327682-edge      matching-engine-0
NMI:          1          2          3          4   Non-maskable interrupts
LOC:    9988776    9988776    7654321    7654321   Local timer interrupts
RES:       1000       1001       1002       1003   Rescheduling interrupts
CAL:       5000       5001       5002       5003   Function call interrupts
TLB:       2000       2001       2002       2003   TLB shootdowns
";

    fn fixture() -> (TempDir, SysRoot) {
        let dir = TempDir::new("isolera-irq").unwrap();
        let root = dir.path().join("root");
        fs::create_dir_all(root.join("proc/irq/24")).unwrap();
        fs::create_dir_all(root.join("proc/irq/124")).unwrap();
        fs::write(root.join("proc/interrupts"), INTERRUPTS).unwrap();
        fs::write(root.join("proc/irq/24/smp_affinity_list"), "0\n").unwrap();
        fs::write(root.join("proc/irq/124/smp_affinity_list"), "0-3\n").unwrap();
        fs::write(root.join("proc/irq/124/smp_affinity"), "0000000f\n").unwrap();
        fs::write(root.join("proc/irq/default_smp_affinity"), "000000ff\n").unwrap();
        (dir, SysRoot::new(root, false))
    }

    #[test]
    fn parses_every_row_of_proc_interrupts() {
        let table = parse_interrupts(INTERRUPTS);
        assert_eq!(table.cpus, vec![0, 1, 2, 3]);
        assert_eq!(table.entries.len(), 11, "every non-header line is a row");

        let nvme = table.by_number(24).unwrap();
        assert_eq!(nvme.counts, vec![100_000, 0, 0, 0]);
        assert_eq!(nvme.controller.as_deref(), Some("IR-PCI-MSI 524288-edge"));
        assert_eq!(nvme.name, "nvme0q0");
        assert!(nvme.is_steerable());
        assert_eq!(nvme.active_cpus(), cpulist::parse("0").unwrap());
        assert_eq!(nvme.total(), 100_000);
        assert_eq!(nvme.hits_on(3), 0);

        assert_eq!(table.by_number(122).unwrap().hits_on(1), 1512);

        let nmi = table.get("NMI").unwrap();
        assert!(nmi.is_pseudo());
        assert!(!nmi.is_steerable());
        assert_eq!(nmi.name, "Non-maskable interrupts");

        assert_eq!(table.get("LOC").unwrap().hits_on(3), 7_654_321);
    }

    #[test]
    fn parsing_survives_malformed_rows() {
        // A row whose counters cannot be parsed keeps its raw text as the name
        // instead of being dropped, so a kernel quirk is visible in the report.
        let table = parse_interrupts("CPU0\n\n   \nnot a row\n 17: nope IO-APIC 1-edge x\n");
        assert_eq!(table.entries.len(), 1);
        let entry = table.get("17").unwrap();
        assert!(entry.counts.is_empty());
        assert_eq!(entry.name, "nope IO-APIC 1-edge x");

        // Rows are attributed to the header's CPUs, not to guesswork.
        let table = parse_interrupts("CPU0 CPU1\n 42: garbage\n");
        let entry = table.get("42").unwrap();
        assert!(entry.counts.is_empty());
        assert_eq!(entry.cpus, vec![0, 1]);
        assert_eq!(entry.name, "garbage");
    }

    #[test]
    fn deltas_use_differences_not_absolute_counts() {
        let before = parse_interrupts(INTERRUPTS);
        let after = parse_interrupts(&INTERRUPTS.replace("     100000", "     100050"));
        let nvme_before = before.by_number(24).unwrap();
        let nvme_after = after.by_number(24).unwrap();
        assert_eq!(nvme_after.delta(nvme_before).unwrap(), vec![50, 0, 0, 0]);
        assert_eq!(
            nvme_after.delta_cpus(nvme_before).unwrap(),
            cpulist::parse("0").unwrap()
        );

        let deltas = after.deltas(&before);
        assert_eq!(deltas.len(), 1, "only the changed interrupt is reported");
        assert_eq!(deltas[0].0, "24");
    }

    #[test]
    fn delta_refuses_mismatched_cpu_sets() {
        let before = parse_interrupts(INTERRUPTS);
        let after = parse_interrupts("CPU0 CPU1\n 24: 5 5 IR-PCI-MSI 1-edge nvme0q0\n");
        assert!(after
            .by_number(24)
            .unwrap()
            .delta(before.by_number(24).unwrap())
            .is_none());
    }

    #[test]
    fn pseudo_interrupts_are_classified() {
        for name in ["NMI", "LOC", "RES", "CAL", "TLB", "SPU", "MCE"] {
            assert!(is_pseudo(name), "{name} must be classified as pseudo");
        }
        for name in ["nvme0q0", "eth0-rx-0", "24"] {
            assert!(!is_pseudo(name));
        }
    }

    #[test]
    fn glob_matching_follows_fnmatch_rules() {
        assert!(matches("nvme*", "nvme0q0"));
        assert!(matches("eth*-rx-?", "eth0-rx-0"));
        assert!(matches("eth0-*", "eth0-"));
        assert!(
            !matches("nvme", "nvme0q0"),
            "a bare pattern is an exact match"
        );
        assert!(
            !matches("nvme.*", "nvme0q0"),
            "dots are literal, as in fnmatch"
        );
        assert!(matches("nvme.*", "nvme.0"));
        assert!(matches("*", "anything"));
        assert!(matches("[abc]x", "bx"));
        assert!(!matches("[abc]x", "dx"));
        assert!(!matches("[!abc]x", "bx"));
        assert!(matches("[0-9]*-edge", "122-edge"));
        assert!(!matches("[0-9]*-edge", "x122-edge"));
        assert!(matches("matching-engine-*", "matching-engine-0"));
        assert!(!matches("matching-engine-*", "MATCHING-ENGINE-0"));
        assert!(!matches("[unterminated", "unterminated"));
    }

    #[test]
    fn affinity_prefers_the_list_form_and_falls_back_to_hex() {
        let (dir, sysroot) = fixture();
        assert_eq!(
            affinity(&sysroot, 124),
            Some(cpulist::parse("0-3").unwrap())
        );
        fs::remove_file(dir.path().join("root/proc/irq/124/smp_affinity_list")).unwrap();
        assert_eq!(
            affinity(&sysroot, 124),
            Some(cpulist::parse("0-3").unwrap())
        );
        assert_eq!(affinity(&sysroot, 999), None);
        assert_eq!(
            default_affinity(&sysroot),
            Some(cpulist::parse("0-7").unwrap())
        );
    }

    #[test]
    fn writing_affinity_updates_the_list_file() {
        let (_dir, sysroot) = fixture();
        let record = set_affinity(&sysroot, 124, &cpulist::parse("0,1").unwrap(), 8, "test")
            .unwrap()
            .unwrap();
        assert_eq!(record.path, "/proc/irq/124/smp_affinity_list");
        assert_eq!(record.previous.as_deref(), Some("0-3\n"));
        assert_eq!(
            affinity(&sysroot, 124),
            Some(cpulist::parse("0,1").unwrap())
        );
        assert!(set_affinity(&sysroot, 124, &CpuSet::new(), 8, "test").is_err());
        assert!(
            set_affinity(&sysroot, 777, &cpulist::parse("0").unwrap(), 8, "test")
                .unwrap()
                .is_none()
        );
        assert!(is_writable(&sysroot, 124));
        assert!(!is_writable(&sysroot, 777));
    }

    #[test]
    fn default_affinity_write_uses_the_hex_mask() {
        let (_dir, sysroot) = fixture();
        let record = set_default_affinity(&sysroot, &cpulist::parse("0-3").unwrap(), 8, "test")
            .unwrap()
            .unwrap();
        assert_eq!(record.value, "0000000f");
        assert_eq!(
            default_affinity(&sysroot),
            Some(cpulist::parse("0-3").unwrap())
        );
    }

    fn policy(precedence: Precedence) -> Policy {
        Policy {
            isolated: cpulist::parse("2,3").unwrap(),
            housekeeping: cpulist::parse("0,1").unwrap(),
            exclude: vec!["nvme*".to_string()],
            overrides: vec![(
                "matching-engine-*".to_string(),
                cpulist::parse("0").unwrap(),
            )],
            ignore: vec!["eth*".to_string()],
            precedence,
        }
    }

    #[test]
    fn plan_applies_exclusions_pins_and_overrides() {
        let (_dir, sysroot) = fixture();
        let table = parse_interrupts(INTERRUPTS);
        let decisions = plan(&sysroot, &table, &policy(Precedence::OverrideWins));

        // The exclusion matches, but this NVMe queue already sits on CPU0, so the
        // rule is recorded without widening its mask to the whole housekeeping set.
        let nvme = decisions
            .iter()
            .find(|decision| decision.irq == 24)
            .unwrap();
        assert_eq!(nvme.action, Action::Exclude);
        assert_eq!(nvme.current, cpulist::parse("0").unwrap());
        assert_eq!(nvme.target, cpulist::parse("0").unwrap());
        assert!(!nvme.changes());
        assert!(nvme.reason.contains("already avoids"));

        // The override narrows 0-3 down to CPU0.
        let engine = decisions
            .iter()
            .find(|decision| decision.irq == 124)
            .unwrap();
        assert_eq!(engine.action, Action::Override);
        assert_eq!(engine.target, cpulist::parse("0").unwrap());
        assert!(engine.changes(), "0-3 must be narrowed to 0");
        assert!(engine.reason.contains("matching-engine-*"));

        // IRQs the config says nothing about but that are ignored stay untouched.
        assert_eq!(
            decisions
                .iter()
                .find(|decision| decision.irq == 122)
                .unwrap()
                .action,
            Action::Ignore
        );

        let nmi = decisions
            .iter()
            .find(|decision| decision.label == "Non-maskable interrupts")
            .unwrap();
        assert_eq!(nmi.action, Action::Unsteerable);
        assert!(nmi.reason.contains("command line"));
    }

    #[test]
    fn precedence_decides_double_matches() {
        let (_dir, sysroot) = fixture();
        let table = parse_interrupts(INTERRUPTS);
        let mut conflicting = policy(Precedence::OverrideWins);
        conflicting.exclude = vec!["matching-engine-*".to_string()];
        let decisions = plan(&sysroot, &table, &conflicting);
        assert_eq!(
            decisions
                .iter()
                .find(|decision| decision.irq == 124)
                .unwrap()
                .action,
            Action::Override
        );

        let mut exclusion_wins = policy(Precedence::ExcludeWins);
        exclusion_wins.exclude = vec!["matching-engine-*".to_string()];
        let decisions = plan(&sysroot, &table, &exclusion_wins);
        assert_eq!(
            decisions
                .iter()
                .find(|decision| decision.irq == 124)
                .unwrap()
                .action,
            Action::Exclude
        );
    }

    #[test]
    fn ignore_patterns_leave_irqs_untouched() {
        let (_dir, sysroot) = fixture();
        let table = parse_interrupts(INTERRUPTS);
        let mut policy = policy(Precedence::OverrideWins);
        policy.exclude = vec!["matching-engine-*".to_string()];
        policy.overrides.clear();
        policy.ignore = vec!["matching-engine-*".to_string()];
        let decisions = plan(&sysroot, &table, &policy);
        let engine = decisions
            .iter()
            .find(|decision| decision.irq == 124)
            .unwrap();
        assert_eq!(engine.action, Action::Ignore);
        assert!(!engine.changes());
    }

    #[test]
    fn default_policy_moves_irqs_off_isolated_cores() {
        let (_dir, sysroot) = fixture();
        let table = parse_interrupts(INTERRUPTS);
        let bare = Policy {
            isolated: cpulist::parse("2,3").unwrap(),
            housekeeping: cpulist::parse("0,1").unwrap(),
            exclude: Vec::new(),
            overrides: Vec::new(),
            ignore: Vec::new(),
            precedence: Precedence::OverrideWins,
        };
        let decisions = plan(&sysroot, &table, &bare);
        let engine = decisions
            .iter()
            .find(|decision| decision.irq == 124)
            .unwrap();
        assert_eq!(engine.action, Action::Pin);
        assert_eq!(engine.target, cpulist::parse("0,1").unwrap());
        assert!(engine.reason.contains("isolated CPU(s) 2,3"));
        assert_eq!(
            decisions
                .iter()
                .find(|decision| decision.irq == 0)
                .unwrap()
                .action,
            Action::Keep
        );
    }

    #[test]
    fn apply_writes_only_the_decisions_that_change() {
        let (_dir, sysroot) = fixture();
        let table = parse_interrupts(INTERRUPTS);
        let decisions = plan(&sysroot, &table, &policy(Precedence::OverrideWins));
        let applied = apply(&sysroot, &decisions, 4).unwrap();
        assert_eq!(applied.records.len(), 1, "only the override narrows a mask");
        assert_eq!(applied.records[0].path, "/proc/irq/124/smp_affinity_list");
        assert_eq!(applied.records[0].previous.as_deref(), Some("0-3\n"));
        assert!(applied.records[0].applied);
        assert!(applied.skipped.is_empty());
        assert_eq!(affinity(&sysroot, 124), Some(cpulist::parse("0").unwrap()));

        // Running the same plan again is a no-op: the suite must be idempotent.
        let again = plan(&sysroot, &table, &policy(Precedence::OverrideWins));
        assert!(apply(&sysroot, &again, 4).unwrap().records.is_empty());
    }

    #[test]
    fn apply_reports_irqs_whose_affinity_cannot_be_written() {
        let (_dir, sysroot) = fixture();
        let orphan = Decision {
            irq: 777,
            label: "orphan-0".to_string(),
            current: cpulist::parse("0-3").unwrap(),
            target: cpulist::parse("0,1").unwrap(),
            action: Action::Pin,
            reason: "device disappeared between plan and apply".to_string(),
        };
        let applied = apply(&sysroot, &[orphan], 4).unwrap();
        assert!(applied.records.is_empty());
        assert_eq!(applied.skipped.len(), 1);
        assert_eq!(applied.skipped[0].0, 777);
        assert!(applied.skipped[0].1.contains("no writable affinity file"));
    }

    #[test]
    fn dry_run_apply_records_without_writing() {
        let (dir, _live) = fixture();
        let root = dir.path().join("root");
        let dry = SysRoot::new(root.clone(), true);
        let table = parse_interrupts(INTERRUPTS);
        let decisions = plan(&dry, &table, &policy(Precedence::OverrideWins));
        let applied = apply(&dry, &decisions, 4).unwrap();
        assert_eq!(applied.records.len(), 1);
        assert!(!applied.records[0].applied);
        assert_eq!(
            fs::read_to_string(root.join("proc/irq/124/smp_affinity_list")).unwrap(),
            "0-3\n"
        );
    }

    #[test]
    fn rendering_and_summaries() {
        let (_dir, sysroot) = fixture();
        let table = parse_interrupts(INTERRUPTS);
        let decisions = plan(&sysroot, &table, &policy(Precedence::OverrideWins));
        let rendered = render_decisions(&decisions, true);
        assert!(rendered.contains("matching-engine-0"));
        assert!(rendered.contains("override"));
        assert!(!rendered.contains("nvme0q0"), "unchanged IRQs are hidden");

        let all = render_decisions(&decisions, false);
        assert!(all.contains("nvme0q0"));
        assert!(all.contains("unsteerable"));

        let summary = summarize(&decisions);
        assert_eq!(summary.get("unsteerable"), Some(&5));
        assert_eq!(summary["override"], 1);
        assert!(render_decisions(&[], true).contains("no interrupt changes"));
    }

    #[test]
    fn unused_patterns_are_reported() {
        let table = parse_interrupts(INTERRUPTS);
        let mut policy = policy(Precedence::OverrideWins);
        policy.exclude.push("typo-*".to_string());
        assert_eq!(policy.unused_patterns(&table), vec!["typo-*".to_string()]);
    }

    #[test]
    fn isolated_hits_and_deltas_focus_on_the_isolated_cpus() {
        let before = parse_interrupts(INTERRUPTS);
        let after = parse_interrupts(
            &INTERRUPTS
                .replace(
                    "        512   IR-PCI-MSI 327682-edge",
                    "        900   IR-PCI-MSI 327682-edge",
                )
                .replace(
                    "   7654321    7654321   Local",
                    "   7654400    7654400   Local",
                ),
        );
        let isolated = cpulist::parse("2,3").unwrap();
        assert!(isolated_hits(&before, &isolated)
            .iter()
            .any(|(id, _, _)| id == "LOC"));

        let deltas = isolated_deltas(&before, &after, &isolated);
        assert!(deltas
            .iter()
            .any(|(id, _, hits)| id == "124" && *hits == 388));
        assert!(deltas.iter().any(|(id, _, _)| id == "LOC"));
        assert!(
            !deltas.iter().any(|(id, _, _)| id == "24"),
            "cpu0 hits are not isolated"
        );
    }
}
