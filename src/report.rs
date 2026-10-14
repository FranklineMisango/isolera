//! The `isolera report` command: one document to attach to a change ticket.
//!
//! A report is a snapshot, not a measurement: it inventories the node, says what
//! the plan would change, runs every *static* verification check (command line,
//! sysfs, governor, sysctls, interrupt steering) and, when the operator hands over
//! a `cyclictest` capture, judges that too. The two checks that need time - the
//! interrupt-traffic sample and the measurement itself - are deliberately left to
//! `isolera verify` and `isolera bench`, and the report says so.

use crate::audit::{self, Audit};
use crate::bench::{self, Run};
use crate::config::Config;
use crate::error::Result;
use crate::grub;
use crate::log::Log;
use crate::plan::{self, Plan};
use crate::render::Format;
use crate::sysroot::SysRoot;
use crate::verify::{self, Check};

/// Everything a report carries.
#[derive(Debug, Clone)]
pub struct Report {
    /// When the report was generated, in Unix seconds.
    pub generated_at: u64,
    /// Configuration file the report used.
    pub config: String,
    /// Node label.
    pub node: String,
    /// The inventory.
    pub audit: Audit,
    /// What the plan would change.
    pub plan: Plan,
    /// Static verification checks.
    pub checks: Vec<Check>,
    /// Latency measurement, when one was supplied.
    pub bench: Option<Run>,
    /// Latency budget the plan asks for, in microseconds.
    pub budget_us: u64,
}

impl Report {
    /// True when no check failed.
    pub fn passed(&self) -> bool {
        let checks_ok = self
            .checks
            .iter()
            .all(|check| check.level != crate::kernel::FindingLevel::Error);
        let bench_ok = self
            .bench
            .as_ref()
            .map(|run| run.passed(self.budget_us))
            .unwrap_or(true);
        checks_ok && bench_ok
    }

    /// One-word verdict.
    pub fn verdict(&self) -> &'static str {
        if self.passed() {
            "isolated"
        } else {
            "needs attention"
        }
    }

    /// Checks that failed.
    pub fn errors(&self) -> Vec<&Check> {
        self.checks
            .iter()
            .filter(|check| check.level == crate::kernel::FindingLevel::Error)
            .collect()
    }
}

/// Gather a report from a node.
pub fn collect(
    sysroot: &SysRoot,
    cfg: &Config,
    bench_input: Option<&str>,
    log: &Log,
) -> Result<Report> {
    log.step("inventory");
    let audit = audit::run(sysroot, cfg)?;
    log.step("plan");
    let plan = plan::build(sysroot, cfg)?;

    log.step("checks");
    // A zero-length sample window keeps the report instant; the traffic check is
    // dropped because a snapshot cannot speak for a time window.
    let options = verify::Options {
        sample_seconds: 0,
        irq_rate_per_second: cfg.verify.irq_rate_per_second,
    };
    let checks: Vec<Check> = verify::run(sysroot, cfg, &options, log)?
        .checks
        .into_iter()
        .filter(|check| check.name != "irq-traffic")
        .collect();

    let bench = match bench_input {
        Some(path) => {
            log.step("latency capture");
            let text = std::fs::read_to_string(path).map_err(|err| {
                crate::error::Error::new(
                    crate::error::Kind::NotFound,
                    format!("cannot read {path}: {err}"),
                )
            })?;
            Some(bench::parse_output(&text))
        }
        None => None,
    };

    Ok(Report {
        generated_at: grub::now_seconds(),
        config: cfg.source.clone(),
        node: cfg
            .node
            .name
            .clone()
            .unwrap_or_else(|| "unnamed".to_string()),
        audit,
        plan,
        checks,
        bench,
        budget_us: cfg.verify.budget_us,
    })
}

/// Render the report as a text document.
pub fn render(report: &Report) -> String {
    use crate::table::Table;
    let mut out = String::new();
    out.push_str(&format!(
        "node      {}\nconfig    {}\ngenerated {}\nverdict   {}\n",
        report.node,
        report.config,
        grub::utc_stamp(report.generated_at),
        report.verdict()
    ));

    out.push_str("\ninventory\n");
    out.push_str(&indent(&audit::render(&report.audit)));

    out.push_str("\nplan\n");
    let mut summary = Table::new(["item", "state"]);
    for (key, value) in report.plan.summary() {
        summary.push([key, value.as_str()]);
    }
    out.push_str(&indent(&summary.render()));
    if !report.plan.warnings.is_empty() {
        out.push_str("  warnings\n");
        for warning in &report.plan.warnings {
            out.push_str(&format!("    - {warning}\n"));
        }
    }

    out.push_str("\nchecks\n");
    let mut checks = Table::new(["check", "result", "detail"]);
    for check in &report.checks {
        checks.push([
            check.name.clone(),
            check.level.as_str().to_string(),
            crate::log::truncate(&check.detail, 68),
        ]);
    }
    out.push_str(&indent(&checks.render()));

    out.push_str("\nlatency\n");
    match &report.bench {
        Some(run) => out.push_str(&indent(&run.render(report.budget_us))),
        None => out.push_str("  no capture supplied; run `isolera bench` to measure\n"),
    }

    out.push_str(
        "\nnotes\n  - interrupt traffic needs a sample window: run `isolera verify`\n  \
         - a reboot is required before a command-line change takes effect\n",
    );
    out
}

/// Serialise the report as a JSON document.
pub fn to_json(report: &Report) -> String {
    format!(
        "{{\"generated_at\":{},\"node\":{},\"config\":{},\"passed\":{},\"budget_us\":{},\"audit\":{},\"plan\":{},\"checks\":[{}],\"bench\":{}}}",
        report.generated_at,
        crate::sysroot::json_string(&report.node),
        crate::sysroot::json_string(&report.config),
        report.passed(),
        report.budget_us,
        audit::to_json(&report.audit),
        plan::to_json(&report.plan),
        report
            .checks
            .iter()
            .map(Check::to_json)
            .collect::<Vec<String>>()
            .join(","),
        match &report.bench {
            Some(run) => run.to_json(report.budget_us),
            None => "null".to_string(),
        },
    )
}

/// Where a report is filed on the node.
pub fn path(cfg: &Config, generated_at: u64, format: Format) -> String {
    let stamp = grub::utc_stamp(generated_at).replace(':', "");
    let extension = match format {
        Format::Json => "json",
        Format::Text => "txt",
    };
    format!(
        "{}/report-{stamp}.{extension}",
        cfg.paths.state_dir.trim_end_matches('/')
    )
}

/// Write a report next to the journal and return its logical path.
pub fn write(sysroot: &SysRoot, cfg: &Config, report: &Report, format: Format) -> Result<String> {
    let target = path(cfg, report.generated_at, format);
    let body = match format {
        Format::Json => to_json(report),
        Format::Text => render(report),
    };
    let reason = format!("isolera: report for {}", report.node);
    sysroot.write_text(&target, &body, &reason)?;
    Ok(target)
}

/// Indent a rendered block under its heading.
fn indent(text: &str) -> String {
    text.lines().map(|line| format!("  {line}\n")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;

    #[test]
    fn a_report_covers_every_phase() {
        let node = testing::node();
        let cfg = testing::config();
        let report = collect(&node.sysroot, &cfg, None, &Log::quiet()).unwrap();

        assert_eq!(report.node, "test-node");
        assert_eq!(report.config, "fixture.toml");
        assert_eq!(report.budget_us, 80);
        assert!(!report.passed());
        assert_eq!(report.verdict(), "needs attention");
        assert!(report
            .audit
            .interrupts
            .iter()
            .any(|entry| entry.label == "matching-engine-0"));
        assert!(report.plan.cmdline_changed);
        assert!(report.errors().iter().any(|check| check.name == "cmdline"));
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.name != "irq-traffic"),
            "a snapshot cannot speak for a time window"
        );
        assert!(report.bench.is_none());

        let rendered = render(&report);
        for heading in ["inventory", "plan", "checks", "latency", "notes"] {
            assert!(rendered.contains(heading), "missing {heading}");
        }
        assert!(rendered.starts_with("node      test-node\nconfig    fixture.toml\ngenerated "));
        assert!(rendered.contains("no capture supplied"));
        assert!(rendered.contains("isolera verify"));
    }

    #[test]
    fn a_supplied_capture_is_judged_and_decides_the_verdict() {
        let node = testing::node();
        let cfg = testing::config();
        let capture = node.dir.path().join("cyclictest.txt");
        std::fs::write(
            &capture,
            "T: 2 ( 1) P:80 I:200 C: 1000 Min: 1 Act: 2 Avg: 2 Max: 45\n\
             T: 3 ( 2) P:80 I:200 C: 1000 Min: 1 Act: 2 Avg: 2 Max: 380\n",
        )
        .unwrap();

        let report = collect(
            &node.sysroot,
            &cfg,
            Some(&capture.to_string_lossy()),
            &Log::quiet(),
        )
        .unwrap();
        let run = report.bench.as_ref().unwrap();
        assert_eq!(run.latencies.len(), 2);
        assert_eq!(run.worst().unwrap().cpu, 3);
        assert!(!report.passed(), "380us is over the 80us budget");
        let rendered = render(&report);
        assert!(rendered.contains("FAIL"));
        assert!(rendered.contains("worst 380us on cpu3"));

        // A capture that fits the budget does not rescue the failing checks.
        let good = node.dir.path().join("good.txt");
        std::fs::write(
            &good,
            "T: 2 ( 1) P:80 I:200 C: 1000 Min: 1 Avg: 1 Max: 12\n",
        )
        .unwrap();
        let report = collect(
            &node.sysroot,
            &cfg,
            Some(&good.to_string_lossy()),
            &Log::quiet(),
        )
        .unwrap();
        assert!(report.bench.as_ref().unwrap().passed(report.budget_us));
        assert!(
            !report.passed(),
            "the cmdline checks still fail on an unprovisioned node"
        );
        assert!(report.errors().iter().any(|check| check.name == "cmdline"));
    }

    #[test]
    fn a_missing_capture_is_a_not_found_error() {
        let node = testing::node();
        let err = collect(
            &node.sysroot,
            &testing::config(),
            Some("/nonexistent/c.txt"),
            &Log::quiet(),
        )
        .unwrap_err();
        assert_eq!(err.kind(), crate::error::Kind::NotFound);
        assert!(err.message().contains("/nonexistent/c.txt"));
    }

    #[test]
    fn reports_are_filed_under_the_state_directory() {
        let node = testing::node();
        let cfg = testing::config();
        let report = collect(&node.sysroot, &cfg, None, &Log::quiet()).unwrap();

        assert_eq!(
            path(&cfg, 1_709_000_000, Format::Json),
            "/var/lib/isolera/state/report-2024-02-27T021320Z.json"
        );
        let written = write(&node.sysroot, &cfg, &report, Format::Json).unwrap();
        assert!(written.starts_with("/var/lib/isolera/state/report-"));
        let filed = node.sysroot.read_optional(&written).unwrap();
        assert_eq!(filed.trim_end(), to_json(&report));

        let text = write(&node.sysroot, &cfg, &report, Format::Text).unwrap();
        assert!(text.ends_with(".txt"));
        assert!(node
            .sysroot
            .read_optional(&text)
            .unwrap()
            .contains("verdict   needs attention"));
    }

    #[test]
    fn report_json_is_balanced_and_versioned_by_its_caller() {
        let node = testing::node();
        let report = collect(&node.sysroot, &testing::config(), None, &Log::quiet()).unwrap();
        let json = to_json(&report);
        assert!(json.starts_with("{\"generated_at\":"));
        assert!(json.contains("\"passed\":false"));
        assert!(json.contains("\"bench\":null"));
        assert!(json.contains("\"budget_us\":80"));
        assert_eq!(json.matches('{').count(), json.matches('}').count());
        assert_eq!(json.matches('[').count(), json.matches(']').count());

        let wrapped = crate::render::envelope("report", &report.node, &json);
        assert!(wrapped.contains("\"kind\":\"report\""));
        assert_eq!(wrapped.matches('{').count(), wrapped.matches('}').count());
    }
}
