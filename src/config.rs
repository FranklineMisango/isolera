//! The declarative plan: `isolera.toml` parsed into typed sections.
//!
//! The configuration is the single source of truth for every engine module. It
//! is read once, validated once, and then passed around immutably, which keeps
//! `plan`, `apply` and `verify` working from exactly the same intent.
//!
//! Unknown keys are rejected rather than ignored: a typo in an HFT node's
//! isolation config should fail loudly on the first run, not silently leave a
//! core shared with the kernel.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use crate::cpulist::{self, CpuSet};
use crate::error::{Error, Kind, Result};
use crate::toml::{self, Table, Value};

/// Shipped example configuration, also used by the test-suite.
pub const SAMPLE: &str = include_str!("../examples/isolera.toml");

/// Default location of the plan.
pub const DEFAULT_PATH: &str = "/etc/isolera.toml";

/// Which side wins when an IRQ matches both an exclusion and an override.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Precedence {
    /// Exact overrides beat broad exclusions (the default: operator intent wins).
    #[default]
    OverrideWins,
    /// Exclusions beat overrides.
    ExcludeWins,
}

impl Precedence {
    /// Parse the ``irq.precedence`` value.
    pub fn parse(text: &str) -> Result<Precedence> {
        match text {
            "override_wins" | "override" => Ok(Precedence::OverrideWins),
            "exclude_wins" | "exclude" => Ok(Precedence::ExcludeWins),
            other => Err(Error::new(
                Kind::Config,
                format!(
                    "irq.precedence must be \"override_wins\" or \"exclude_wins\", not {other:?}"
                ),
            )),
        }
    }

    /// Value as written in the configuration.
    pub fn as_str(self) -> &'static str {
        match self {
            Precedence::OverrideWins => "override_wins",
            Precedence::ExcludeWins => "exclude_wins",
        }
    }
}

/// Node identity, used in reports and the write journal.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeSection {
    /// Hostname or role label.
    pub name: Option<String>,
    /// Free-form role, e.g. ``matching-engine``.
    pub role: Option<String>,
    /// Arbitrary tags carried into reports.
    pub tags: Vec<String>,
}

/// Which CPUs are isolated, and how the kernel should treat them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IsolationSection {
    /// Raw specification exactly as written.
    pub cpus: String,
    /// Parsed set of isolated CPUs.
    pub set: CpuSet,
    /// When false, SMT siblings of isolated CPUs are offlined.
    pub smt: bool,
    /// CPU frequency governor enforced on the isolated cores.
    pub governor: String,
    /// Add ``nohz_full=<cpus>``.
    pub nohz_full: bool,
    /// Add ``rcu_nocbs=<cpus>``.
    pub rcu_nocbs: bool,
    /// Add the flag-only ``rcu_nocb_poll`` parameter (with ``rcu_nocbs``).
    pub rcu_nocb_poll: bool,
    /// Add the ``managed_irq`` flag to ``isolcpus``.
    pub managed_irq: bool,
    /// Add the ``domain`` flag to ``isolcpus`` (scheduler domain isolation).
    pub domain: bool,
    /// CPU idle driver request, e.g. ``poll``.
    pub idle: Option<String>,
    /// Keep the high-resolution tick on the isolated cores.
    pub high_res_timers: bool,
    /// Non-zero value for ``skew_tick``, which spreads timer ticks.
    pub skew_tick: Option<u32>,
    /// CPUs reserved for housekeeping, when stated explicitly.
    pub housekeeping: Option<String>,
}

impl Default for IsolationSection {
    fn default() -> Self {
        IsolationSection {
            cpus: String::new(),
            set: CpuSet::new(),
            smt: false,
            governor: "performance".to_string(),
            nohz_full: true,
            rcu_nocbs: true,
            rcu_nocb_poll: false,
            managed_irq: true,
            domain: false,
            idle: None,
            high_res_timers: true,
            skew_tick: None,
            housekeeping: None,
        }
    }
}

/// IRQ steering policy.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct IrqSection {
    /// Patterns whose IRQs must never be delivered to an isolated CPU.
    pub exclude: Vec<String>,
    /// Pattern -> CPU list, applied to matching IRQs.
    pub overrides: BTreeMap<String, String>,
    /// Precedence between the two lists.
    pub precedence: Precedence,
    /// Where non-overridden, non-excluded IRQs are sent when their current
    /// affinity points at an isolated CPU.
    pub housekeeping: Option<String>,
    /// Pattern -> leave alone; these IRQs are never rewritten.
    pub ignore: Vec<String>,
}

/// Kernel command line and sysctl intent.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KernelSection {
    /// Extra kernel parameters appended verbatim.
    pub cmdline_extra: Vec<String>,
    /// Sysctl path -> value.
    pub sysctls: BTreeMap<String, String>,
}

/// GRUB integration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GrubSection {
    /// Path of the grub defaults file.
    pub file: String,
    /// Variable that carries the kernel command line.
    pub cmdline_variable: String,
    /// Extra ``KEY=value`` variables to enforce.
    pub variables: BTreeMap<String, String>,
    /// Where to keep the pre-change copy.
    pub backup_dir: String,
    /// Command used to regenerate the bootloader configuration.
    pub regenerate: Option<String>,
}

impl Default for GrubSection {
    fn default() -> Self {
        GrubSection {
            file: "/etc/default/grub".to_string(),
            cmdline_variable: "GRUB_CMDLINE_LINUX".to_string(),
            variables: BTreeMap::new(),
            backup_dir: "/var/lib/isolera/backups".to_string(),
            regenerate: Some("update-grub".to_string()),
        }
    }
}

/// Real-time scheduling and kernel-thread pinning.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RtSection {
    /// SCHED_FIFO priority applied by the generated pinning commands.
    pub fifo_priority: Option<u32>,
    /// Pin movable kernel threads away from the isolated CPUs.
    pub pin_kernel_threads: bool,
    /// Thread-name patterns left where the kernel placed them.
    pub keep: Vec<String>,
    /// ``kernel.sched_rt_runtime_us`` value; -1 disables the RT throttle.
    pub rt_runtime_us: Option<i64>,
}

/// Verification thresholds.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifySection {
    /// Maximum tolerated ``cyclictest`` latency in microseconds.
    pub budget_us: u64,
    /// Measurement interval handed to ``cyclictest``.
    pub interval_us: u64,
    /// Measurement duration in seconds.
    pub duration_s: u64,
    /// Optional fixed cycle count; overrides the duration when set.
    pub cycles: Option<u64>,
    /// Fail when an IRQ still fires on an isolated CPU more often than this.
    pub irq_rate_per_second: f64,
}

impl Default for VerifySection {
    fn default() -> Self {
        VerifySection {
            budget_us: 0,
            interval_us: 200,
            duration_s: 60,
            cycles: None,
            irq_rate_per_second: 1.0,
        }
    }
}

/// Filesystem locations Isolera writes to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathsSection {
    /// Directory for pre-change copies of edited files.
    pub backup_dir: String,
    /// Directory for the write journal and reports.
    pub state_dir: String,
}

impl Default for PathsSection {
    fn default() -> Self {
        PathsSection {
            backup_dir: "/var/lib/isolera/backups".to_string(),
            state_dir: "/var/lib/isolera/state".to_string(),
        }
    }
}

/// Typed view of a whole plan file.
#[derive(Debug, Clone, PartialEq)]
pub struct Config {
    /// Where the configuration was read from (for reports and rollback).
    pub source: String,
    /// Node identity.
    pub node: NodeSection,
    /// Isolation intent.
    pub isolation: IsolationSection,
    /// IRQ steering policy.
    pub irq: IrqSection,
    /// Kernel command line and sysctls.
    pub kernel: KernelSection,
    /// Bootloader integration.
    pub grub: GrubSection,
    /// Real-time and kernel-thread handling.
    pub rt: RtSection,
    /// Verification thresholds.
    pub verify: VerifySection,
    /// Filesystem locations.
    pub paths: PathsSection,
}

/// Section names accepted at the top level.
const SECTIONS: [&str; 8] = [
    "node",
    "isolation",
    "irq",
    "kernel",
    "grub",
    "rt",
    "verify",
    "paths",
];

impl Config {
    /// Load, parse and validate a plan from disk.
    pub fn load(path: impl AsRef<Path>) -> Result<Config> {
        let display = path.as_ref().display().to_string();
        let text = std::fs::read_to_string(path.as_ref()).map_err(|err| {
            Error::new(
                Kind::Config,
                format!("cannot read configuration {display}: {err}"),
            )
        })?;
        let table = toml::parse_named(&text, &display)?;
        let mut config = Config::from_table(&table)?;
        config.source = display;
        config.validate()?;
        Ok(config)
    }

    /// Parse a plan from an in-memory string (used by tests and `--config -`).
    pub fn from_str_named(text: &str, origin: &str) -> Result<Config> {
        let table = toml::parse_named(text, origin)?;
        let mut config = Config::from_table(&table)?;
        config.source = origin.to_string();
        config.validate()?;
        Ok(config)
    }

    /// Build the typed config from a parsed TOML table.
    pub fn from_table(table: &Table) -> Result<Config> {
        for key in table.keys() {
            if !SECTIONS.contains(&key.as_str()) {
                return Err(Error::new(
                    Kind::Config,
                    format!(
                        "unknown configuration section {key:?}; expected one of {}",
                        SECTIONS.join(", ")
                    ),
                ));
            }
        }
        let node = match section(table, "node")? {
            Some(section) => node_section(section)?,
            None => NodeSection::default(),
        };
        let isolation = match section(table, "isolation")? {
            Some(section) => isolation_section(section)?,
            None => {
                return Err(Error::new(
                    Kind::Config,
                    "the [isolation] section is required",
                ))
            }
        };
        let irq = match section(table, "irq")? {
            Some(section) => irq_section(section)?,
            None => IrqSection::default(),
        };
        let kernel = match section(table, "kernel")? {
            Some(section) => kernel_section(section)?,
            None => KernelSection::default(),
        };
        let grub = match section(table, "grub")? {
            Some(section) => grub_section(section)?,
            None => GrubSection::default(),
        };
        let rt = match section(table, "rt")? {
            Some(section) => rt_section(section)?,
            None => RtSection::default(),
        };
        let verify = match section(table, "verify")? {
            Some(section) => verify_section(section)?,
            None => VerifySection::default(),
        };
        let paths = match section(table, "paths")? {
            Some(section) => paths_section(section)?,
            None => PathsSection::default(),
        };
        Ok(Config {
            source: DEFAULT_PATH.to_string(),
            node,
            isolation,
            irq,
            kernel,
            grub,
            rt,
            verify,
            paths,
        })
    }
}

fn section<'t>(table: &'t Table, name: &str) -> Result<Option<&'t Table>> {
    match table.get(name) {
        None => Ok(None),
        Some(Value::Table(section)) => Ok(Some(section)),
        Some(other) => Err(Error::new(
            Kind::Config,
            format!("[{name}] must be a table, found a {}", other.kind()),
        )),
    }
}

/// Typed accessor that also tracks which keys were consumed, so unknown keys can
/// be reported instead of silently ignored.
struct Reader<'t> {
    section: &'static str,
    table: &'t Table,
    remaining: BTreeSet<String>,
}

impl<'t> Reader<'t> {
    fn new(section: &'static str, table: &'t Table) -> Reader<'t> {
        Reader {
            section,
            table,
            remaining: table.keys().cloned().collect(),
        }
    }

    fn take(&mut self, key: &str) -> Option<&'t Value> {
        if self.remaining.remove(key) {
            self.table.get(key)
        } else {
            None
        }
    }

    fn type_error(&self, key: &str, expected: &str, found: &Value) -> Error {
        Error::new(
            Kind::Config,
            format!(
                "{}.{key} must be a {expected}, found a {}",
                self.section,
                found.kind()
            ),
        )
    }

    fn string(&mut self, key: &str) -> Result<Option<String>> {
        match self.take(key) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text.clone())),
            Some(other) => Err(self.type_error(key, "string", other)),
        }
    }

    fn required_string(&mut self, key: &str) -> Result<String> {
        self.string(key)?
            .ok_or_else(|| Error::new(Kind::Config, format!("{}.{key} is required", self.section)))
    }

    fn bool(&mut self, key: &str, default: bool) -> Result<bool> {
        match self.take(key) {
            None => Ok(default),
            Some(Value::Bool(flag)) => Ok(*flag),
            Some(other) => Err(self.type_error(key, "boolean", other)),
        }
    }

    fn u64(&mut self, key: &str) -> Result<Option<u64>> {
        match self.take(key) {
            None => Ok(None),
            Some(value) => match value.as_int() {
                Some(number) if number >= 0 => Ok(Some(number as u64)),
                Some(number) => Err(Error::new(
                    Kind::Config,
                    format!("{}.{key} must not be negative ({number})", self.section),
                )),
                None => Err(self.type_error(key, "integer", value)),
            },
        }
    }

    fn i64(&mut self, key: &str) -> Result<Option<i64>> {
        match self.take(key) {
            None => Ok(None),
            Some(value) => value
                .as_int()
                .map(Some)
                .ok_or_else(|| self.type_error(key, "integer", value)),
        }
    }

    fn f64(&mut self, key: &str) -> Result<Option<f64>> {
        match self.take(key) {
            None => Ok(None),
            Some(value) => value
                .as_float()
                .map(Some)
                .ok_or_else(|| self.type_error(key, "number", value)),
        }
    }

    fn string_list(&mut self, key: &str) -> Result<Vec<String>> {
        match self.take(key) {
            None => Ok(Vec::new()),
            Some(Value::Array(items)) => {
                let mut out = Vec::with_capacity(items.len());
                for item in items {
                    match item {
                        Value::String(text) => out.push(text.clone()),
                        other => {
                            return Err(Error::new(
                                Kind::Config,
                                format!(
                                    "{}.{key} must contain strings, found a {}",
                                    self.section,
                                    other.kind()
                                ),
                            ))
                        }
                    }
                }
                Ok(out)
            }
            Some(Value::String(text)) => Ok(vec![text.clone()]),
            Some(other) => Err(self.type_error(key, "array of strings", other)),
        }
    }

    fn string_map(&mut self, key: &str) -> Result<BTreeMap<String, String>> {
        match self.take(key) {
            None => Ok(BTreeMap::new()),
            Some(Value::Table(table)) => {
                let mut out = BTreeMap::new();
                for (name, value) in table {
                    match value {
                        Value::String(text) => {
                            out.insert(name.clone(), text.clone());
                        }
                        other => {
                            return Err(Error::new(
                                Kind::Config,
                                format!(
                                    "{}.{key}.{name} must be a string, found a {}",
                                    self.section,
                                    other.kind()
                                ),
                            ))
                        }
                    }
                }
                Ok(out)
            }
            Some(other) => Err(self.type_error(key, "table of strings", other)),
        }
    }

    fn finish(self) -> Result<()> {
        if self.remaining.is_empty() {
            return Ok(());
        }
        let unknown: Vec<&str> = self.remaining.iter().map(String::as_str).collect();
        Err(Error::new(
            Kind::Config,
            format!(
                "unknown key(s) in [{}]: {}",
                self.section,
                unknown.join(", ")
            ),
        ))
    }
}

fn node_section(table: &Table) -> Result<NodeSection> {
    let mut reader = Reader::new("node", table);
    let node = NodeSection {
        name: reader.string("name")?,
        role: reader.string("role")?,
        tags: reader.string_list("tags")?,
    };
    reader.finish()?;
    Ok(node)
}

fn isolation_section(table: &Table) -> Result<IsolationSection> {
    let mut reader = Reader::new("isolation", table);
    let cpus = reader.required_string("cpus")?;
    let set = cpulist::parse(&cpus)?;
    let skew_tick = match reader.u64("skew_tick")? {
        None => None,
        Some(0) => None,
        Some(value) if value <= u32::MAX as u64 => Some(value as u32),
        Some(value) => {
            return Err(Error::new(
                Kind::Config,
                format!("isolation.skew_tick is out of range: {value}"),
            ))
        }
    };
    let idle = reader.string("idle")?.filter(|text| !text.is_empty());
    let housekeeping = reader
        .string("housekeeping")?
        .filter(|text| !text.is_empty());
    let isolation = IsolationSection {
        cpus,
        set,
        smt: reader.bool("smt", false)?,
        governor: reader
            .string("governor")?
            .unwrap_or_else(|| "performance".to_string()),
        nohz_full: reader.bool("nohz_full", true)?,
        rcu_nocbs: reader.bool("rcu_nocbs", true)?,
        rcu_nocb_poll: reader.bool("rcu_nocb_poll", false)?,
        managed_irq: reader.bool("managed_irq", true)?,
        domain: reader.bool("domain", false)?,
        idle,
        high_res_timers: reader.bool("high_res_timers", true)?,
        skew_tick,
        housekeeping,
    };
    reader.finish()?;
    Ok(isolation)
}

fn irq_section(table: &Table) -> Result<IrqSection> {
    let mut reader = Reader::new("irq", table);
    let precedence = match reader.string("precedence")? {
        Some(text) => Precedence::parse(&text)?,
        None => Precedence::default(),
    };
    let housekeeping = reader
        .string("housekeeping")?
        .filter(|text| !text.is_empty());
    let section = IrqSection {
        exclude: reader.string_list("exclude")?,
        overrides: reader.string_map("overrides")?,
        precedence,
        housekeeping,
        ignore: reader.string_list("ignore")?,
    };
    reader.finish()?;
    Ok(section)
}

fn kernel_section(table: &Table) -> Result<KernelSection> {
    let mut reader = Reader::new("kernel", table);
    let section = KernelSection {
        cmdline_extra: reader.string_list("cmdline_extra")?,
        sysctls: reader.string_map("sysctls")?,
    };
    reader.finish()?;
    Ok(section)
}

fn grub_section(table: &Table) -> Result<GrubSection> {
    let mut reader = Reader::new("grub", table);
    let defaults = GrubSection::default();
    // An explicit empty string or "none" turns the follow-up command off; an
    // absent key keeps the distribution default (update-grub).
    let regenerate = match reader.string("regenerate")? {
        Some(text) => {
            let text = text.trim().to_string();
            if text.is_empty() || text.eq_ignore_ascii_case("none") {
                None
            } else {
                Some(text)
            }
        }
        None => defaults.regenerate,
    };
    let section = GrubSection {
        file: reader.string("file")?.unwrap_or(defaults.file),
        cmdline_variable: reader
            .string("cmdline_variable")?
            .unwrap_or(defaults.cmdline_variable),
        variables: reader.string_map("variables")?,
        backup_dir: reader.string("backup_dir")?.unwrap_or(defaults.backup_dir),
        regenerate,
    };
    reader.finish()?;
    Ok(section)
}

fn rt_section(table: &Table) -> Result<RtSection> {
    let mut reader = Reader::new("rt", table);
    let fifo_priority = match reader.u64("fifo_priority")? {
        None => None,
        Some(0) => None,
        Some(value) if value <= 99 => Some(value as u32),
        Some(value) => {
            return Err(Error::new(
                Kind::Config,
                format!("rt.fifo_priority must be between 1 and 99, not {value}"),
            ))
        }
    };
    let section = RtSection {
        fifo_priority,
        pin_kernel_threads: reader.bool("pin_kernel_threads", true)?,
        keep: reader.string_list("keep")?,
        rt_runtime_us: reader.i64("rt_runtime_us")?,
    };
    reader.finish()?;
    Ok(section)
}

fn verify_section(table: &Table) -> Result<VerifySection> {
    let mut reader = Reader::new("verify", table);
    let defaults = VerifySection::default();
    let section = VerifySection {
        budget_us: reader.u64("budget_us")?.unwrap_or(defaults.budget_us),
        interval_us: reader.u64("interval_us")?.unwrap_or(defaults.interval_us),
        duration_s: reader.u64("duration_s")?.unwrap_or(defaults.duration_s),
        cycles: reader.u64("cycles")?.filter(|cycles| *cycles > 0),
        irq_rate_per_second: reader
            .f64("irq_rate_per_second")?
            .unwrap_or(defaults.irq_rate_per_second),
    };
    reader.finish()?;
    Ok(section)
}

fn paths_section(table: &Table) -> Result<PathsSection> {
    let mut reader = Reader::new("paths", table);
    let defaults = PathsSection::default();
    let section = PathsSection {
        backup_dir: reader.string("backup_dir")?.unwrap_or(defaults.backup_dir),
        state_dir: reader.string("state_dir")?.unwrap_or(defaults.state_dir),
    };
    reader.finish()?;
    Ok(section)
}

fn invalid(message: impl Into<String>) -> Error {
    Error::new(Kind::Config, message)
}

fn is_shell_identifier(text: &str) -> bool {
    let mut characters = text.chars();
    match characters.next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => {}
        _ => return false,
    }
    characters.all(|character| character.is_ascii_alphanumeric() || character == '_')
}

impl Config {
    /// The isolated CPU set.
    pub fn isolated(&self) -> &CpuSet {
        &self.isolation.set
    }

    /// CPU set reserved for housekeeping, either stated explicitly or inferred
    /// as the complement of the isolated set.
    pub fn housekeeping(&self, cpu_count: usize) -> CpuSet {
        match &self.isolation.housekeeping {
            Some(spec) => cpulist::parse(spec).unwrap_or_default(),
            None => cpulist::complement(&self.isolation.set, cpu_count),
        }
    }

    /// IRQ overrides with their CPU lists parsed.
    pub fn irq_overrides(&self) -> Result<BTreeMap<String, CpuSet>> {
        let mut parsed = BTreeMap::new();
        for (pattern, spec) in &self.irq.overrides {
            let set = cpulist::parse(spec)
                .map_err(|err| invalid(format!("irq.overrides.{pattern}: {}", err.message())))?;
            parsed.insert(pattern.clone(), set);
        }
        Ok(parsed)
    }

    /// Fail on any inconsistency that would make the plan unsafe to apply.
    ///
    /// Everything checked here is a *hard* error: a plan that cannot be applied
    /// exactly as written must not partially apply.
    pub fn validate(&self) -> Result<()> {
        if self.isolation.set.is_empty() {
            return Err(invalid(
                "isolation.cpus must select at least one CPU (use e.g. \"2-5\")",
            ));
        }
        if self.isolation.governor.trim().is_empty() {
            return Err(invalid("isolation.governor must not be empty"));
        }
        if let Some(housekeeping) = &self.isolation.housekeeping {
            let set = cpulist::parse(housekeeping)
                .map_err(|err| invalid(format!("isolation.housekeeping: {}", err.message())))?;
            if cpulist::intersects(&set, &self.isolation.set) {
                return Err(invalid(format!(
                    "isolation.housekeeping ({}) overlaps isolation.cpus ({})",
                    cpulist::format(&set),
                    self.isolation.cpus
                )));
            }
        }

        for pattern in self.irq.exclude.iter().chain(self.irq.ignore.iter()) {
            if pattern.trim().is_empty() {
                return Err(invalid("irq patterns must not be empty strings"));
            }
        }
        let overrides = self.irq_overrides()?;
        for (pattern, set) in &overrides {
            if set.is_empty() {
                return Err(invalid(format!("irq.overrides.{pattern} selects no CPUs")));
            }
        }
        if let Some(housekeeping) = &self.irq.housekeeping {
            let set = cpulist::parse(housekeeping)
                .map_err(|err| invalid(format!("irq.housekeeping: {}", err.message())))?;
            if set.is_empty() {
                return Err(invalid("irq.housekeeping selects no CPUs"));
            }
        }

        for key in self.grub.variables.keys() {
            if !is_shell_identifier(key) {
                return Err(invalid(format!(
                    "grub.variables.{key} is not a valid shell variable name"
                )));
            }
        }
        if !is_shell_identifier(&self.grub.cmdline_variable) {
            return Err(invalid(format!(
                "grub.cmdline_variable {:?} is not a valid shell variable name",
                self.grub.cmdline_variable
            )));
        }

        for (label, path) in [
            ("paths.backup_dir", &self.paths.backup_dir),
            ("paths.state_dir", &self.paths.state_dir),
            ("grub.backup_dir", &self.grub.backup_dir),
            ("grub.file", &self.grub.file),
        ] {
            if !path.starts_with('/') {
                return Err(invalid(format!(
                    "{label} must be an absolute path, not {path:?}"
                )));
            }
        }

        if self.verify.interval_us == 0 || self.verify.interval_us > 1_000_000 {
            return Err(invalid(format!(
                "verify.interval_us must be between 1 and 1000000, not {}",
                self.verify.interval_us
            )));
        }
        if self.verify.duration_s == 0 && self.verify.cycles.is_none() {
            return Err(invalid(
                "verify.duration_s must be greater than zero (or set verify.cycles)",
            ));
        }
        if !self.verify.irq_rate_per_second.is_finite() || self.verify.irq_rate_per_second < 0.0 {
            return Err(invalid(
                "verify.irq_rate_per_second must be a finite, non-negative number",
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tempdir::TempDir;
    use std::fs;

    const MINIMAL: &str = r#"
[isolation]
cpus = "2-3"
"#;

    fn load_minimal(extra: &str) -> Result<Config> {
        Config::from_str_named(&format!("{MINIMAL}{extra}"), "test.toml")
    }

    #[test]
    fn shipped_sample_parses_and_validates() {
        let config = Config::from_str_named(SAMPLE, "examples/isolera.toml").unwrap();
        assert_eq!(config.isolation.cpus, "6-9");
        assert_eq!(config.isolated().len(), 4);
        assert_eq!(config.node.role.as_deref(), Some("matching-engine"));
        assert_eq!(config.housekeeping(10), cpulist::parse("0-5").unwrap());
        assert_eq!(config.irq.overrides.len(), 2);
        assert_eq!(
            config.irq_overrides().unwrap()["matching-engine-*"],
            cpulist::parse("6,7").unwrap()
        );
        assert_eq!(config.rt.rt_runtime_us, Some(-1));
        assert_eq!(config.verify.budget_us, 80);
        assert_eq!(config.grub.variables["GRUB_TIMEOUT"], "1");
    }

    #[test]
    fn sample_defaults_are_sensible() {
        let config = Config::from_str_named(SAMPLE, "sample").unwrap();
        assert!(
            !config.isolation.smt,
            "SMT siblings must be offlined by default"
        );
        assert!(config.isolation.nohz_full);
        assert!(config.isolation.rcu_nocbs);
        assert!(config.isolation.managed_irq);
        assert!(!config.isolation.domain);
        assert_eq!(config.isolation.governor, "performance");
        assert_eq!(config.isolation.skew_tick, Some(1));
        assert_eq!(config.irq.precedence, Precedence::OverrideWins);
        assert_eq!(config.rt.fifo_priority, Some(80));
    }

    #[test]
    fn minimal_config_is_accepted() {
        let config = load_minimal("").unwrap();
        assert_eq!(config.isolated(), &cpulist::parse("2,3").unwrap());
        assert_eq!(config.source, "test.toml");
        assert_eq!(config.grub.file, "/etc/default/grub");
        assert!(config.verify.duration_s > 0);
        assert!(config.kernel.sysctls.is_empty());
    }

    #[test]
    fn missing_isolation_section_is_an_error() {
        let err = Config::from_str_named("[node]\nname = \"x\"\n", "t").unwrap_err();
        assert_eq!(err.kind(), Kind::Config);
        assert!(err.message().contains("[isolation]"), "{}", err.message());
    }

    #[test]
    fn empty_cpu_set_is_rejected() {
        let err = Config::from_str_named("[isolation]\ncpus = \"\"\n", "t").unwrap_err();
        assert!(
            err.message().contains("at least one CPU"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn unknown_sections_and_keys_are_rejected() {
        let section_err = Config::from_str_named("[isolaton]\ncpus = \"1\"\n", "t").unwrap_err();
        assert!(section_err.message().contains("isolaton"));

        let key_err = load_minimal("\n[rt]\nfifo_prio = 80\n").unwrap_err();
        assert!(
            key_err.message().contains("unknown key"),
            "{}",
            key_err.message()
        );
        assert!(key_err.message().contains("fifo_prio"));
    }

    #[test]
    fn type_mismatches_name_the_offending_key() {
        let err =
            Config::from_str_named("[isolation]\ncpus = \"2-3\"\nsmt = \"no\"\n", "t").unwrap_err();
        assert!(err.message().contains("isolation.smt"), "{}", err.message());
        assert!(err.message().contains("boolean"), "{}", err.message());
    }

    #[test]
    fn overlapping_housekeeping_is_rejected() {
        let err =
            Config::from_str_named("[isolation]\ncpus = \"2-3\"\nhousekeeping = \"3-5\"\n", "t")
                .unwrap_err();
        assert!(err.message().contains("overlaps"), "{}", err.message());
    }

    #[test]
    fn invalid_irq_override_lists_are_rejected() {
        let err = Config::from_str_named(
            "[isolation]\ncpus = \"2\"\n[irq.overrides]\nnvme = \"x-y\"\n",
            "t",
        )
        .unwrap_err();
        assert!(
            err.message().contains("irq.overrides.nvme"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn precedence_parsing_is_strict() {
        assert_eq!(
            Precedence::parse("override_wins").unwrap(),
            Precedence::OverrideWins
        );
        assert_eq!(
            Precedence::parse("exclude_wins").unwrap(),
            Precedence::ExcludeWins
        );
        assert!(Precedence::parse("whatever").is_err());
        assert_eq!(Precedence::OverrideWins.as_str(), "override_wins");
    }

    #[test]
    fn rt_priority_range_is_enforced() {
        assert!(load_minimal("\n[rt]\nfifo_priority = 99\n").is_ok());
        assert!(load_minimal("\n[rt]\nfifo_priority = 100\n").is_err());
    }

    #[test]
    fn verify_thresholds_are_enforced() {
        assert!(load_minimal("\n[verify]\ninterval_us = 0\n").is_err());
        assert!(load_minimal("\n[verify]\nduration_s = 0\n").is_err());
        assert!(load_minimal("\n[verify]\nduration_s = 0\ncycles = 1000\n").is_ok());
        assert!(load_minimal("\n[verify]\nirq_rate_per_second = -1.0\n").is_err());
    }

    #[test]
    fn relative_paths_are_rejected() {
        let err = load_minimal("\n[paths]\nstate_dir = \"state\"\n").unwrap_err();
        assert!(err.message().contains("absolute"), "{}", err.message());
    }

    #[test]
    fn invalid_grub_variable_names_are_rejected() {
        let err = Config::from_str_named(
            "[isolation]\ncpus = \"2\"\n[grub.variables]\n\"bad-key\" = \"1\"\n",
            "t",
        )
        .unwrap_err();
        assert!(
            err.message().contains("shell variable"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn load_reads_from_disk_and_reports_missing_files() {
        let dir = TempDir::new("isolera-config").unwrap();
        let path = dir.path().join("isolera.toml");
        fs::write(&path, "[isolation]\ncpus = \"0-1\"\n").unwrap();
        let config = Config::load(&path).unwrap();
        assert_eq!(config.source, path.display().to_string());
        assert_eq!(config.isolated().len(), 2);

        let err = Config::load(dir.path().join("absent.toml")).unwrap_err();
        assert_eq!(err.kind(), Kind::Config);
        assert!(err.message().contains("cannot read configuration"));
    }

    #[test]
    fn inferred_housekeeping_is_the_complement() {
        let config = load_minimal("").unwrap();
        assert_eq!(
            config.housekeeping(4),
            cpulist::parse("0-1").unwrap(),
            "housekeeping defaults to every non-isolated CPU"
        );
    }

    #[test]
    fn sysctl_map_accepts_dotted_and_path_keys() {
        let config = Config::from_str_named(
            "[isolation]\ncpus = \"2\"\n[kernel.sysctls]\n\"kernel.watchdog\" = \"0\"\n\"/proc/sys/vm/stat_interval\" = \"10\"\n",
            "t",
        )
        .unwrap();
        assert_eq!(config.kernel.sysctls["kernel.watchdog"], "0");
        assert_eq!(config.kernel.sysctls["/proc/sys/vm/stat_interval"], "10");
    }

    #[test]
    fn config_equality_is_structural() {
        let first = load_minimal("").unwrap();
        let second = load_minimal("").unwrap();
        assert_eq!(first, second);
        assert_ne!(first, load_minimal("\n[isolation]\nsmt = true\n").unwrap());
    }
}
