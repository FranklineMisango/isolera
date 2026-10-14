//! The `cyclictest` harness: render the command, parse the output, judge it.
//!
//! Isolera does not pretend to measure jitter itself. `cyclictest` is the tool the
//! latency community trusts, so Isolera builds the exact invocation for a plan
//! (affinity, priority, interval, duration), parses what `cyclictest` printed, and
//! compares the worst latency per core against `verify.budget_us`.
//!
//! Parsing is deliberately forgiving about spacing, because `cyclictest` has
//! printed ``Min:``/``Max:`` as both ``Min:      1`` and ``Min:1`` across the
//! versions that are deployed on production distributions.

use crate::config::Config;
use crate::cpulist::{self, CpuSet};

/// Latency statistics for one core.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Latency {
    /// CPU the thread was pinned to.
    pub cpu: u32,
    /// ``SCHED_FIFO`` priority used.
    pub priority: Option<u32>,
    /// Measurement interval in microseconds.
    pub interval_us: Option<u64>,
    /// Cycles completed.
    pub cycles: Option<u64>,
    /// Minimum wake-up latency.
    pub min_us: u64,
    /// Average wake-up latency.
    pub avg_us: Option<u64>,
    /// Worst wake-up latency.
    pub max_us: u64,
}

impl Latency {
    /// True when the worst case fits the budget.
    pub fn within(&self, budget_us: u64) -> bool {
        self.max_us <= budget_us
    }

    /// Serialise as a single-line JSON object.
    pub fn to_json(&self) -> String {
        format!(
            "{{\"cpu\":{},\"priority\":{},\"interval_us\":{},\"cycles\":{},\"min_us\":{},\"avg_us\":{},\"max_us\":{}}}",
            self.cpu,
            option_json(self.priority),
            option_json(self.interval_us),
            option_json(self.cycles),
            self.min_us,
            option_json(self.avg_us),
            self.max_us,
        )
    }
}

/// ``7`` or ``null``.
fn option_json<T: std::fmt::Display>(value: Option<T>) -> String {
    match value {
        Some(value) => value.to_string(),
        None => "null".to_string(),
    }
}

/// One parsed `cyclictest` run.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Run {
    /// Per-core statistics, in the order they were printed.
    pub latencies: Vec<Latency>,
    /// True when `cyclictest` reported that it could not keep up.
    pub overflow: bool,
}

impl Run {
    /// The core with the worst latency.
    pub fn worst(&self) -> Option<&Latency> {
        self.latencies.iter().max_by_key(|latency| latency.max_us)
    }

    /// Cores whose worst latency exceeds the budget.
    pub fn offenders(&self, budget_us: u64) -> Vec<&Latency> {
        self.latencies
            .iter()
            .filter(|latency| !latency.within(budget_us))
            .collect()
    }

    /// True when every core fits the budget and `cyclictest` kept up.
    pub fn passed(&self, budget_us: u64) -> bool {
        !self.overflow && !self.latencies.is_empty() && self.offenders(budget_us).is_empty()
    }

    /// Render the run as a table with the budget as the pass/fail line.
    pub fn render(&self, budget_us: u64) -> String {
        use crate::table::Table;
        if self.latencies.is_empty() {
            return "cyclictest produced no latency rows\n".to_string();
        }
        let mut out = String::new();
        let mut table = Table::new(["cpu", "min_us", "avg_us", "max_us", "budget", "verdict"]);
        for latency in &self.latencies {
            table.push([
                latency.cpu.to_string(),
                latency.min_us.to_string(),
                latency
                    .avg_us
                    .map(|value| value.to_string())
                    .unwrap_or_else(|| "-".to_string()),
                latency.max_us.to_string(),
                budget_us.to_string(),
                if latency.within(budget_us) {
                    "ok"
                } else {
                    "FAIL"
                }
                .to_string(),
            ]);
        }
        out.push_str(&table.render());
        if self.overflow {
            out.push_str("cyclictest reported overruns: the run is not trustworthy\n");
        }
        if let Some(worst) = self.worst() {
            out.push_str(&format!(
                "worst {}us on cpu{} (budget {budget_us}us)\n",
                worst.max_us, worst.cpu
            ));
        }
        out
    }

    /// Serialise the run as a JSON document.
    pub fn to_json(&self, budget_us: u64) -> String {
        let rows: Vec<String> = self.latencies.iter().map(Latency::to_json).collect();
        format!(
            "{{\"budget_us\":{budget_us},\"passed\":{},\"overflow\":{},\"worst_us\":{},\"latencies\":[{}]}}",
            self.passed(budget_us),
            self.overflow,
            match self.worst() {
                Some(latency) => latency.max_us.to_string(),
                None => "null".to_string(),
            },
            rows.join(","),
        )
    }
}

/// Parse `cyclictest` output.
pub fn parse_output(text: &str) -> Run {
    let mut run = Run::default();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let lowered = line.to_ascii_lowercase();
        if lowered.contains("overrun") || lowered.contains("could not keep up") {
            run.overflow = true;
        }
        if !line.starts_with("T:") {
            continue;
        }
        if let Some(latency) = parse_line(line) {
            run.latencies.push(latency);
        }
    }
    run
}

/// Parse one ``T: ...`` line.
fn parse_line(line: &str) -> Option<Latency> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    let mut parsed = Parsed::default();

    let mut index = 0usize;
    while index < tokens.len() {
        let token = tokens[index];
        // ``Min:1`` fits on one token; ``Min:  1`` needs the next one.
        if let Some((key, value)) = token.split_once(':') {
            if let Ok(value) = value.parse::<u64>() {
                parsed.assign(key, value);
                index += 1;
                continue;
            }
            if let Some(next) = tokens.get(index + 1) {
                if let Ok(value) = next.parse::<u64>() {
                    parsed.assign(key, value);
                    index += 2;
                    continue;
                }
            }
        }
        index += 1;
    }

    Some(Latency {
        cpu: parsed.cpu?,
        priority: parsed.priority,
        interval_us: parsed.interval,
        cycles: parsed.cycles,
        min_us: parsed.min.unwrap_or(0),
        avg_us: parsed.avg,
        max_us: parsed.max?,
    })
}

/// The ``KEY:value`` pairs of one `cyclictest` line.
#[derive(Debug, Default)]
struct Parsed {
    cpu: Option<u32>,
    priority: Option<u32>,
    interval: Option<u64>,
    cycles: Option<u64>,
    min: Option<u64>,
    avg: Option<u64>,
    max: Option<u64>,
}

impl Parsed {
    fn assign(&mut self, key: &str, value: u64) {
        match key.trim() {
            "T" => self.cpu = Some(value as u32),
            "P" => self.priority = Some(value as u32),
            "I" => self.interval = Some(value),
            "C" => self.cycles = Some(value),
            "Min" => self.min = Some(value),
            "Avg" => self.avg = Some(value),
            "Max" => self.max = Some(value),
            _ => {}
        }
    }
}

/// Default ``SCHED_FIFO`` priority when the plan does not state one.
pub const DEFAULT_PRIORITY: u32 = 80;

/// The `cyclictest` invocation for a plan.
pub fn command(cfg: &Config, isolated: &CpuSet) -> String {
    let cpus = cpulist::format(isolated);
    let threads = isolated.len().max(1);
    let priority = cfg.rt.fifo_priority.unwrap_or(DEFAULT_PRIORITY);
    let mut command = format!(
        "cyclictest -m -q -p {priority} -t{threads} -a{cpus} -i{}",
        cfg.verify.interval_us.max(1)
    );
    match cfg.verify.cycles {
        Some(cycles) if cycles > 0 => command.push_str(&format!(" -l {cycles}")),
        _ => command.push_str(&format!(" -D {}s", cfg.verify.duration_s.max(1))),
    }
    command
}

/// The measurement as a runnable shell block, with a note about its cost.
pub fn render_script(cfg: &Config, isolated: &CpuSet) -> String {
    let budget = cfg.verify.budget_us;
    let mut out = String::from("#!/bin/sh\n");
    out.push_str(&format!(
        "# Latency measurement for {}.\n",
        cpulist::format(isolated)
    ));
    out.push_str("# Run as root: cyclictest needs SCHED_FIFO and mlockall.\n");
    out.push_str("# Quiesce the strategy first - this measures the kernel, not the load.\n");
    out.push_str("set -e\n");
    out.push_str(&format!(
        "{} | tee /tmp/isolera-cyclictest.txt\n",
        command(cfg, isolated)
    ));
    out.push_str(&format!(
        "# Judge the result: isolera bench --input /tmp/isolera-cyclictest.txt (budget {budget}us)\n"
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    /// Real `cyclictest` output, both spacing styles.
    const OUTPUT: &str = "\
# /dev/cpu_dma_latency set to 0us
policy: fifo: loadavg: 0.12 0.15 0.09 1/523 4242

T: 0 ( 4243) P:80 I:200 C:  10000 Min:      1 Act:    2 Avg:    1 Max:      18
T: 1 ( 4244) P:80 I:200 C:  10000 Min:2 Act:3 Avg:2 Max:34
T: 2 ( 4245) P:80 I:200 C:  10000 Min:      3 Act:    4 Avg:    4 Max:      212
T: 3 ( 4246) P:80 I:200 C:  10000 Min:      2 Act:    2 Avg:    2 Max:      41
";

    #[test]
    fn parses_both_cyclictest_spacing_styles() {
        let run = parse_output(OUTPUT);
        assert_eq!(run.latencies.len(), 4);
        assert!(!run.overflow);

        let first = &run.latencies[0];
        assert_eq!(first.cpu, 0);
        assert_eq!(first.priority, Some(80));
        assert_eq!(first.interval_us, Some(200));
        assert_eq!(first.cycles, Some(10_000));
        assert_eq!(first.min_us, 1);
        assert_eq!(first.avg_us, Some(1));
        assert_eq!(first.max_us, 18);

        let second = &run.latencies[1];
        assert_eq!(second.cpu, 1);
        assert_eq!(second.max_us, 34);

        // Lines that carry no latency information are ignored, not guessed at.
        let run = parse_output("policy: fifo: loadavg: 1.0\nT: 9 ( 1) P:80 I:200\n");
        assert!(run.latencies.is_empty());
    }

    #[test]
    fn overruns_make_the_run_untrustworthy() {
        let run = parse_output("T: 0 ( 1) P:80 I:200 C: 10 Min: 1 Max: 5\ncyclictest: overrun!\n");
        assert!(run.overflow);
        assert!(!run.passed(80), "an overrun invalidates the measurement");
        assert!(run.render(80).contains("not trustworthy"));
        assert!(!parse_output(OUTPUT).overflow);
    }

    #[test]
    fn the_worst_core_decides() {
        let run = parse_output(OUTPUT);
        assert_eq!(run.worst().unwrap().cpu, 2);
        assert_eq!(run.worst().unwrap().max_us, 212);

        assert!(!run.passed(80));
        let offenders = run.offenders(80);
        assert_eq!(offenders.len(), 1);
        assert_eq!(offenders[0].cpu, 2);
        assert!(run.passed(250));
        assert!(run.offenders(250).is_empty());
        // An empty run never passes: silence is not evidence.
        assert!(!Run::default().passed(80));
    }

    #[test]
    fn render_and_json_report_the_budget() {
        let run = parse_output(OUTPUT);
        let rendered = run.render(80);
        assert!(rendered.starts_with("cpu  min_us"), "{rendered}");
        assert!(rendered.contains("FAIL"));
        assert!(rendered.contains("worst 212us on cpu2 (budget 80us)"));
        assert_eq!(
            Run::default().render(80),
            "cyclictest produced no latency rows\n"
        );

        let json = run.to_json(80);
        assert!(json.starts_with("{\"budget_us\":80,\"passed\":false"));
        assert!(json.contains("\"worst_us\":212"));
        assert!(json.contains("\"cpu\":3"));
        assert_eq!(json.matches('{').count(), json.matches('}').count());
        assert_eq!(json.matches('[').count(), json.matches(']').count());
    }

    #[test]
    fn the_command_matches_the_plan() {
        let cfg = testing::config();
        let isolated = cpulist::parse("2,3").unwrap();
        let rendered = command(&cfg, &isolated);
        assert_eq!(rendered, "cyclictest -m -q -p 80 -t2 -a2,3 -i200 -D 1s");

        let script = render_script(&cfg, &isolated);
        assert!(script.starts_with("#!/bin/sh\n"));
        assert!(script.contains(
            "cyclictest -m -q -p 80 -t2 -a2,3 -i200 -D 1s | tee /tmp/isolera-cyclictest.txt"
        ));
        assert!(script.contains("budget 80us"));
        assert!(script.contains("# Run as root"));

        // A fixed cycle count replaces the duration, and the plan's priority wins.
        let mut counted = testing::config();
        counted.verify.cycles = Some(50_000);
        counted.rt.fifo_priority = Some(99);
        assert_eq!(
            command(&counted, &isolated),
            "cyclictest -m -q -p 99 -t2 -a2,3 -i200 -l 50000"
        );
    }
}
