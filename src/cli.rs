//! The Isolera command line.
//!
//! Parsing is hand-rolled for the same reason the rest of the suite has no
//! dependencies, and for one more: an argument parser is where a root-privileged
//! tool must be *predictable*. Every flag is checked, an unknown one is an error
//! (never silently ignored), and `apply` refuses to touch a node unless the
//! operator said `--yes` or asked for `--dry-run`.
//!
//! [`run`] returns an [`Outcome`] - the exit code plus everything written to
//! stdout - so the whole command line is exercisable from the test-suite without
//! spawning a process.

use crate::apply;
use crate::audit;
use crate::bench;
use crate::config::Config;
use crate::error::{Error, Kind, Result};
use crate::log::{Level, Log};
use crate::pin;
use crate::plan;
use crate::render::{self, Format};
use crate::report;
use crate::rollback;
use crate::sysroot::SysRoot;
use crate::verify;

/// Usage text, printed by `--help` and on a usage error.
pub const USAGE: &str = "\
isolera - deterministic CPU-core isolation for low-latency trading nodes

USAGE
  isolera <command> [options]

COMMANDS
  audit      inventory the node: topology, isolation state, IRQ traffic
  plan       show every change the configuration asks for (read-only)
  apply      perform the plan (requires --yes or --dry-run)
  rollback   undo the last apply (requires --yes or --dry-run)
  verify     check a provisioned node, sampling interrupt traffic
  bench      render the cyclictest harness and judge a capture
  report     one document combining audit, plan, checks and latency
  pin        move movable kernel threads off the trading cores
  version    print the suite version

OPTIONS
  -c, --config <path>   configuration file (\"-\" reads stdin, default /etc/isolera.toml)
      --root <dir>      treat <dir> as / (fixtures, containers)
  -n, --dry-run         record intended writes instead of performing them
  -y, --yes             confirm a command that changes the node
      --json            emit JSON instead of tables
      --output <fmt>    text (default) or json
  -q, --quiet           only warnings and errors
  -v, --verbose         add debug detail (-v, or --trace for everything)

COMMAND OPTIONS
  verify --sample <s>   interrupt sampling window in seconds
  bench  --input <f>    judge a cyclictest capture
  bench  --script       render a runnable measurement script
  report --bench <f>    include a cyclictest capture
  report --out          file the report under paths.state_dir
  pin    --apply        run taskset instead of printing the work list
";

/// Default configuration path.
pub const DEFAULT_CONFIG: &str = "/etc/isolera.toml";

/// What the operator asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Inventory the node.
    Audit,
    /// Print the plan.
    Plan,
    /// Perform the plan.
    Apply,
    /// Undo the last apply.
    Rollback,
    /// Verify a provisioned node.
    Verify {
        /// Sampling window override.
        sample_seconds: Option<u64>,
    },
    /// Render the measurement harness, or judge a capture.
    Bench {
        /// Capture to judge.
        input: Option<String>,
        /// Render the script instead of running anything.
        script: bool,
    },
    /// Produce a report.
    Report {
        /// Capture to include.
        bench: Option<String>,
        /// File the report on the node.
        out: bool,
    },
    /// Move movable kernel threads.
    Pin {
        /// Run `taskset` instead of printing the work list.
        apply: bool,
    },
    /// Print the version.
    Version,
    /// Print the usage text.
    Help,
}

impl Command {
    /// The command's name, as typed.
    pub fn as_str(&self) -> &'static str {
        match self {
            Command::Audit => "audit",
            Command::Plan => "plan",
            Command::Apply => "apply",
            Command::Rollback => "rollback",
            Command::Verify { .. } => "verify",
            Command::Bench { .. } => "bench",
            Command::Report { .. } => "report",
            Command::Pin { .. } => "pin",
            Command::Version => "version",
            Command::Help => "help",
        }
    }

    /// True when the command writes to the node.
    pub fn changes_the_node(&self) -> bool {
        matches!(
            self,
            Command::Apply | Command::Rollback | Command::Pin { apply: true }
        )
    }
}

/// Exit code and stdout of one command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    /// Process exit code.
    pub code: i32,
    /// Everything the command printed.
    pub stdout: String,
}

impl Outcome {
    /// A successful run.
    pub fn ok(stdout: impl Into<String>) -> Outcome {
        Outcome {
            code: 0,
            stdout: stdout.into(),
        }
    }

    /// A completed run whose verdict was negative.
    pub fn failed(code: i32, stdout: impl Into<String>) -> Outcome {
        Outcome {
            code,
            stdout: stdout.into(),
        }
    }
}

/// Global options plus the command.
#[derive(Debug, Clone)]
pub struct Options {
    /// Command to run.
    pub command: Command,
    /// Configuration file, or ``-`` for stdin.
    pub config: String,
    /// Root prefix for every path.
    pub root: String,
    /// Record instead of performing writes.
    pub dry_run: bool,
    /// The operator confirmed a changing command.
    pub assume_yes: bool,
    /// Output format.
    pub format: Format,
    /// Verbosity threshold.
    pub log_level: Level,
    /// Suppress everything but warnings and errors.
    pub quiet: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            command: Command::Help,
            config: DEFAULT_CONFIG.to_string(),
            root: "/".to_string(),
            dry_run: false,
            assume_yes: false,
            format: Format::Text,
            log_level: Level::Info,
            quiet: false,
        }
    }
}

/// Parse the command line.
pub fn parse(args: &[String]) -> Result<Options> {
    let mut options = Options::default();
    // Command-specific flags are collected first and bound to the command at the
    // end, so `isolera verify --sample 3` and `isolera --sample 3 verify` parse the
    // same way.
    let mut command_name: Option<String> = None;
    let mut sample: Option<u64> = None;
    let mut input: Option<String> = None;
    let mut bench_input: Option<String> = None;
    let mut script = false;
    let mut out = false;
    let mut pin_apply = false;
    let mut index = 0usize;

    while index < args.len() {
        let arg = args[index].clone();
        let (name, inline) = split_flag(&arg);
        match name.as_str() {
            "-c" | "--config" => {
                options.config = take_value(args, &mut index, inline, &name)?;
            }
            "--root" => {
                options.root = take_value(args, &mut index, inline, &name)?;
            }
            "-n" | "--dry-run" => {
                options.dry_run = true;
                index += 1;
            }
            "-y" | "--yes" => {
                options.assume_yes = true;
                index += 1;
            }
            "--json" => {
                options.format = Format::Json;
                index += 1;
            }
            "--output" => {
                let value = take_value(args, &mut index, inline, &name)?;
                options.format = Format::parse(&value)?;
            }
            "-q" | "--quiet" => {
                options.quiet = true;
                index += 1;
            }
            "-v" | "--verbose" => {
                options.log_level = Level::Debug;
                index += 1;
            }
            "--trace" => {
                options.log_level = Level::Trace;
                index += 1;
            }
            "--sample" => {
                let value = take_value(args, &mut index, inline, &name)?;
                sample = Some(value.parse::<u64>().map_err(|_| {
                    Error::new(
                        Kind::Usage,
                        format!("--sample expects seconds, not {value:?}"),
                    )
                })?);
            }
            "--input" => {
                input = Some(take_value(args, &mut index, inline, &name)?);
            }
            "--bench" => {
                bench_input = Some(take_value(args, &mut index, inline, &name)?);
            }
            "--script" => {
                script = true;
                index += 1;
            }
            "--out" => {
                out = true;
                index += 1;
            }
            "--apply" => {
                pin_apply = true;
                index += 1;
            }
            "-h" | "--help" => {
                command_name = Some("help".to_string());
                index += 1;
            }
            "-V" | "--version" => {
                command_name = Some("version".to_string());
                index += 1;
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(Error::new(
                    Kind::Usage,
                    format!("unknown option {other:?}; run `isolera --help`"),
                ));
            }
            name => {
                if command_name.is_some() {
                    return Err(Error::new(
                        Kind::Usage,
                        format!("unexpected argument {name:?}; one command at a time, please"),
                    ));
                }
                command_name = Some(name.to_string());
                index += 1;
            }
        }
    }

    let Some(name) = command_name else {
        // Nothing to do is not an error: print the usage text and succeed.
        return Ok(Options {
            command: Command::Help,
            ..options
        });
    };
    let command = match name.as_str() {
        "audit" => Command::Audit,
        "plan" => Command::Plan,
        "apply" => Command::Apply,
        "rollback" => Command::Rollback,
        "verify" => Command::Verify {
            sample_seconds: sample,
        },
        "bench" => Command::Bench {
            input: input.clone(),
            script,
        },
        "report" => Command::Report {
            bench: bench_input.clone(),
            out,
        },
        "pin" => Command::Pin { apply: pin_apply },
        "version" => Command::Version,
        "help" => Command::Help,
        other => {
            return Err(Error::new(
                Kind::Usage,
                format!("unknown command {other:?}; run `isolera --help`"),
            ))
        }
    };
    reject_irrelevant_flags(
        &command,
        sample,
        &input,
        &bench_input,
        script,
        out,
        pin_apply,
    )?;
    options.command = command;
    Ok(options)
}

/// ``--flag`` and ``--flag=value`` in one place.
fn split_flag(arg: &str) -> (String, Option<String>) {
    if arg.starts_with("--") {
        if let Some((name, value)) = arg.split_once('=') {
            return (name.to_string(), Some(value.to_string()));
        }
    }
    (arg.to_string(), None)
}

/// Read a flag's value, whether inline or as the next argument.
fn take_value(
    args: &[String],
    index: &mut usize,
    inline: Option<String>,
    name: &str,
) -> Result<String> {
    if let Some(value) = inline {
        *index += 1;
        return Ok(value);
    }
    match args.get(*index + 1) {
        Some(value) if !value.starts_with("--") => {
            *index += 2;
            Ok(value.clone())
        }
        _ => Err(Error::new(
            Kind::Usage,
            format!("{name} needs a value, e.g. `{name} /etc/isolera.toml`"),
        )),
    }
}

/// Refuse flags that belong to another command, so a typo cannot silently do
/// nothing.
fn reject_irrelevant_flags(
    command: &Command,
    sample: Option<u64>,
    input: &Option<String>,
    bench_input: &Option<String>,
    script: bool,
    out: bool,
    pin_apply: bool,
) -> Result<()> {
    let mut stray: Vec<&str> = Vec::new();
    if sample.is_some() && !matches!(command, Command::Verify { .. }) {
        stray.push("--sample");
    }
    if (input.is_some() || script) && !matches!(command, Command::Bench { .. }) {
        stray.push("--input/--script");
    }
    if bench_input.is_some() && !matches!(command, Command::Report { .. }) {
        stray.push("--bench");
    }
    if out && !matches!(command, Command::Report { .. }) {
        stray.push("--out");
    }
    if pin_apply && !matches!(command, Command::Pin { .. }) {
        stray.push("--apply");
    }
    if stray.is_empty() {
        return Ok(());
    }
    Err(Error::new(
        Kind::Usage,
        format!(
            "{} does not take {}; run `isolera --help`",
            command.as_str(),
            stray.join(", ")
        ),
    ))
}

/// Run the command line; errors are returned for the caller to map to a code.
pub fn run(args: &[String]) -> Result<Outcome> {
    let options = parse(args)?;
    dispatch(&options)
}

/// Parse and run, turning any error into an exit code and a message.
pub fn run_to_outcome(args: &[String]) -> Outcome {
    match run(args) {
        Ok(outcome) => outcome,
        Err(err) => Outcome::failed(err.kind().exit_code(), format!("isolera: {err}\n")),
    }
}

/// Entry point for the binary.
pub fn main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let outcome = run_to_outcome(&args);
    print!("{}", outcome.stdout);
    outcome.code
}

fn dispatch(options: &Options) -> Result<Outcome> {
    match &options.command {
        Command::Help => Ok(Outcome::ok(USAGE.to_string())),
        Command::Version => Ok(Outcome::ok(if options.format.is_json() {
            format!(
                "{}\n",
                render::envelope(
                    "version",
                    "-",
                    &format!(
                        "{{\"version\":{}}}",
                        crate::sysroot::json_string(crate::VERSION)
                    )
                )
            )
        } else {
            format!("isolera {}\n", crate::VERSION)
        })),
        command => {
            confirm(command, options)?;
            let cfg = load_config(&options.config)?;
            let sysroot = SysRoot::new(&options.root, options.dry_run);
            let log = Log::new(options.log_level, options.quiet);
            match command {
                Command::Audit => {
                    let audit = audit::run(&sysroot, &cfg)?;
                    Ok(Outcome::ok(emit(
                        "audit",
                        &audit.node,
                        options,
                        audit::render(&audit),
                        audit::to_json(&audit),
                    )))
                }
                Command::Plan => {
                    let built = plan::build(&sysroot, &cfg)?;
                    Ok(Outcome::ok(emit(
                        "plan",
                        &built.node,
                        options,
                        plan::render(&built),
                        plan::to_json(&built),
                    )))
                }
                Command::Apply => {
                    let built = plan::build(&sysroot, &cfg)?;
                    let applied = apply::run(&sysroot, &cfg, &built, &log)?;
                    Ok(Outcome::ok(emit(
                        "apply",
                        &built.node,
                        options,
                        applied.render(),
                        applied.to_json(),
                    )))
                }
                Command::Rollback => {
                    let rolled = rollback::run(&sysroot, &cfg, &log)?;
                    Ok(Outcome::ok(emit(
                        "rollback",
                        &report_node(&cfg),
                        options,
                        rolled.render(),
                        rolled.to_json(),
                    )))
                }
                Command::Verify { sample_seconds } => {
                    let mut verifier = verify::Options::from_config(&cfg.verify);
                    if let Some(seconds) = sample_seconds {
                        verifier.sample_seconds = *seconds;
                    }
                    let checked = verify::run(&sysroot, &cfg, &verifier, &log)?;
                    let body = emit(
                        "verify",
                        &checked.node,
                        options,
                        checked.render(),
                        checked.to_json(),
                    );
                    if checked.passed() {
                        Ok(Outcome::ok(body))
                    } else {
                        Ok(Outcome::failed(Kind::Verify.exit_code(), body))
                    }
                }
                Command::Bench { input, script } => {
                    bench_command(input.as_deref(), *script, options, &cfg)
                }
                Command::Report { bench, out } => {
                    report_command(bench.as_deref(), *out, options, &cfg, &sysroot, &log)
                }
                Command::Pin { apply } => pin_command(*apply, options, &cfg, &sysroot, &log),
                Command::Help | Command::Version => unreachable!("handled above"),
            }
        }
    }
}

/// A node label for commands whose result carries none.
fn report_node(cfg: &Config) -> String {
    cfg.node
        .name
        .clone()
        .unwrap_or_else(|| "unnamed".to_string())
}

/// Choose text or JSON and make sure the output ends with a newline.
fn emit(kind: &str, node: &str, options: &Options, text: String, json: String) -> String {
    let mut body = render::render(options.format, text, render::envelope(kind, node, &json));
    if !body.ends_with('\n') {
        body.push('\n');
    }
    body
}

/// Changing commands need an explicit confirmation.
fn confirm(command: &Command, options: &Options) -> Result<()> {
    if !command.changes_the_node() || options.assume_yes || options.dry_run {
        return Ok(());
    }
    Err(Error::new(
        Kind::Usage,
        format!(
            "{} changes this node: pass --yes to confirm, or --dry-run to preview it",
            command.as_str()
        ),
    ))
}

/// Load the plan, from a file or from stdin.
fn load_config(path: &str) -> Result<Config> {
    if path != "-" {
        return Config::load(path);
    }
    use std::io::Read as _;
    let mut text = String::new();
    std::io::stdin()
        .read_to_string(&mut text)
        .map_err(|err| Error::new(Kind::Io, format!("cannot read the plan from stdin: {err}")))?;
    Config::from_str_named(&text, "<stdin>")
}

/// `isolera bench`: render the harness, or judge a capture.
fn bench_command(
    input: Option<&str>,
    script: bool,
    options: &Options,
    cfg: &Config,
) -> Result<Outcome> {
    let isolated = cfg.isolated();
    match input {
        None => {
            let text = if script {
                bench::render_script(cfg, isolated)
            } else {
                format!(
                    "{}\n\nRun it as root on the node, then judge the capture:\n  \
                     isolera bench --input /tmp/isolera-cyclictest.txt\n",
                    bench::render_script(cfg, isolated)
                )
            };
            let json = format!(
                "{{\"command\":{},\"script\":{},\"budget_us\":{}}}",
                crate::sysroot::json_string(&bench::command(cfg, isolated)),
                crate::sysroot::json_string(&bench::render_script(cfg, isolated)),
                cfg.verify.budget_us,
            );
            Ok(Outcome::ok(emit(
                "bench",
                &report_node(cfg),
                options,
                text,
                json,
            )))
        }
        Some(path) => {
            let text = std::fs::read_to_string(path)
                .map_err(|err| Error::new(Kind::NotFound, format!("cannot read {path}: {err}")))?;
            let run = bench::parse_output(&text);
            let body = emit(
                "bench",
                &report_node(cfg),
                options,
                run.render(cfg.verify.budget_us),
                run.to_json(cfg.verify.budget_us),
            );
            if run.passed(cfg.verify.budget_us) {
                Ok(Outcome::ok(body))
            } else {
                Ok(Outcome::failed(Kind::Verify.exit_code(), body))
            }
        }
    }
}

/// `isolera report`: one document, optionally filed on the node.
fn report_command(
    bench_input: Option<&str>,
    out: bool,
    options: &Options,
    cfg: &Config,
    sysroot: &SysRoot,
    log: &Log,
) -> Result<Outcome> {
    let collected = report::collect(sysroot, cfg, bench_input, log)?;
    if out {
        let path = report::write(sysroot, cfg, &collected, options.format)?;
        return Ok(Outcome::ok(format!("{path}\n")));
    }
    let body = emit(
        "report",
        &collected.node,
        options,
        report::render(&collected),
        report::to_json(&collected),
    );
    if collected.passed() {
        Ok(Outcome::ok(body))
    } else {
        Ok(Outcome::failed(Kind::Verify.exit_code(), body))
    }
}

/// `isolera pin`: the work list, or the moves themselves.
fn pin_command(
    apply_moves: bool,
    options: &Options,
    cfg: &Config,
    sysroot: &SysRoot,
    log: &Log,
) -> Result<Outcome> {
    let listed = pin::list(sysroot, cfg);
    if !apply_moves {
        let outcomes: Vec<pin::Outcome> = listed
            .iter()
            .map(|command| pin::Outcome {
                command: command.clone(),
                status: pin::Status::DryRun,
                affinity: command.from.clone(),
            })
            .collect();
        let text = format!(
            "{}script to run:\n\n{}",
            pin::render(&outcomes),
            crate::kernel::render_pin_commands(&listed, cfg.rt.fifo_priority)
        );
        return Ok(Outcome::ok(emit(
            "pin",
            &report_node(cfg),
            options,
            text,
            pin::to_json(&outcomes),
        )));
    }
    let outcomes = pin::apply(sysroot, &listed, log)?;
    Ok(Outcome::ok(emit(
        "pin",
        &report_node(cfg),
        options,
        pin::render(&outcomes),
        pin::to_json(&outcomes),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing;
    use std::path::{Path, PathBuf};

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    /// A fixture node plus a plan file on disk, ready for the CLI.
    fn node() -> (testing::Node, PathBuf) {
        let node = testing::node();
        let plan_path = node.dir.path().join("isolera.toml");
        std::fs::write(&plan_path, testing::PLAN).unwrap();
        (node, plan_path)
    }

    /// The global flags that point the CLI at the fixture.
    fn target(node: &testing::Node, plan: &Path) -> Vec<String> {
        args(&[
            "--config",
            &plan.to_string_lossy(),
            "--root",
            &node.sysroot.root().to_string_lossy(),
            "--quiet",
        ])
    }

    /// Run a command against the fixture.
    fn cli(node: &testing::Node, plan: &Path, tail: &[&str]) -> Outcome {
        let mut argv = args(tail);
        argv.extend(target(node, plan));
        run_to_outcome(&argv)
    }

    #[test]
    fn parsing_covers_globals_and_commands() {
        let options = parse(&args(&[
            "apply",
            "-c",
            "/tmp/x.toml",
            "--root",
            "/tmp/root",
            "-n",
            "-y",
            "--json",
            "-v",
        ]))
        .unwrap();
        assert_eq!(options.command, Command::Apply);
        assert_eq!(options.config, "/tmp/x.toml");
        assert_eq!(options.root, "/tmp/root");
        assert!(options.dry_run);
        assert!(options.assume_yes);
        assert!(options.format.is_json());
        assert_eq!(options.log_level, Level::Debug);

        let options = parse(&args(&["verify", "--sample", "3"])).unwrap();
        assert_eq!(
            options.command,
            Command::Verify {
                sample_seconds: Some(3)
            }
        );
        let options = parse(&args(&["bench", "--input=/tmp/c.txt"])).unwrap();
        assert_eq!(
            options.command,
            Command::Bench {
                input: Some("/tmp/c.txt".to_string()),
                script: false
            }
        );
        let options = parse(&args(&["report", "--out", "--bench", "/tmp/c.txt"])).unwrap();
        assert_eq!(
            options.command,
            Command::Report {
                bench: Some("/tmp/c.txt".to_string()),
                out: true
            }
        );
        let options = parse(&args(&["pin", "--apply"])).unwrap();
        assert_eq!(options.command, Command::Pin { apply: true });

        // No arguments at all prints the usage text, and does not fail.
        let options = parse(&[]).unwrap();
        assert_eq!(options.command, Command::Help);
        assert_eq!(options.config, DEFAULT_CONFIG);
        assert_eq!(options.root, "/");
        assert!(!options.command.changes_the_node());
        assert!(Command::Apply.changes_the_node());
    }

    #[test]
    fn bad_arguments_are_usage_errors() {
        let err = parse(&args(&["--nope"])).unwrap_err();
        assert_eq!(err.kind(), Kind::Usage);
        assert!(err.message().contains("--nope"));

        let err = parse(&args(&["fly"])).unwrap_err();
        assert!(err.message().contains("unknown command \"fly\""));

        let err = parse(&args(&["plan", "apply"])).unwrap_err();
        assert!(err.message().contains("one command at a time"));

        let err = parse(&args(&["--config"])).unwrap_err();
        assert!(err.message().contains("needs a value"), "{}", err.message());

        let err = parse(&args(&["verify", "--sample", "soon"])).unwrap_err();
        assert!(
            err.message().contains("--sample expects seconds"),
            "{}",
            err.message()
        );

        // A flag from another command is refused instead of ignored.
        let err = parse(&args(&["plan", "--sample", "3"])).unwrap_err();
        assert!(
            err.message().contains("plan does not take --sample"),
            "{}",
            err.message()
        );

        let err = parse(&args(&["audit", "--output", "yaml"])).unwrap_err();
        assert!(
            err.message().contains("unknown output format"),
            "{}",
            err.message()
        );
    }

    #[test]
    fn version_and_help_need_no_configuration() {
        let outcome = run_to_outcome(&args(&["version"]));
        assert_eq!(outcome.code, 0);
        assert_eq!(outcome.stdout, format!("isolera {}\n", crate::VERSION));

        let json = run_to_outcome(&args(&["version", "--json"]));
        assert!(json
            .stdout
            .starts_with("{\"schema\":1,\"kind\":\"version\""));
        assert!(json
            .stdout
            .contains(&format!("\"version\":\"{}\"", crate::VERSION)));

        let help = run_to_outcome(&args(&["--help"]));
        assert_eq!(help.code, 0);
        assert!(help.stdout.contains("USAGE"));
        assert!(help.stdout.contains("rollback"));
        assert_eq!(run_to_outcome(&args(&[])).stdout, help.stdout);
    }

    #[test]
    fn changing_commands_need_confirmation() {
        let (node, plan) = node();
        let refused = cli(&node, &plan, &["apply"]);
        assert_eq!(refused.code, Kind::Usage.exit_code());
        assert!(refused.stdout.contains("--yes"), "{}", refused.stdout);
        assert!(node.sysroot.journal().is_empty(), "nothing was written");

        let preview = cli(&node, &plan, &["apply", "--dry-run"]);
        assert_eq!(preview.code, 0);
        assert!(preview.stdout.contains("8 writes"));
        assert!(
            node.sysroot.journal().is_empty(),
            "a dry run writes nothing"
        );

        let refused = cli(&node, &plan, &["rollback"]);
        assert!(refused.stdout.contains("--yes"));
        assert!(cli(&node, &plan, &["rollback", "--dry-run"])
            .stdout
            .contains("nothing to roll back"));
    }

    #[test]
    fn audit_plan_apply_and_rollback_run_end_to_end() {
        let (node, plan) = node();

        let audit = cli(&node, &plan, &["audit"]);
        assert_eq!(audit.code, 0);
        assert!(audit.stdout.starts_with("node      test-node\n"));
        assert!(audit.stdout.contains("cpu layout"));
        assert!(audit.stdout.contains("matching-engine-0"));

        let planned = cli(&node, &plan, &["plan"]);
        assert_eq!(planned.code, 0);
        assert!(planned.stdout.contains("isolcpus=managed_irq,2,3"));
        assert!(planned.stdout.contains("cpu offline"));
        assert!(
            cli(&node, &plan, &["plan", "--json"])
                .stdout
                .contains("\"kind\":\"plan\""),
            "JSON output is wrapped in the envelope"
        );

        let applied = cli(&node, &plan, &["apply", "--yes"]);
        assert_eq!(applied.code, 0);
        assert!(applied.stdout.starts_with("8 writes\n"));
        assert!(applied.stdout.contains("follow-up"));
        assert_eq!(
            node.sysroot
                .read_trimmed("/proc/sys/kernel/watchdog")
                .as_deref(),
            Some("0")
        );

        // The applied state is journalled, so rollback can undo it.
        let rolled = cli(&node, &plan, &["rollback", "--yes"]);
        assert_eq!(rolled.code, 0);
        assert!(rolled.stdout.starts_with("6 restored, 1 removed\n"));
        assert_eq!(
            node.sysroot
                .read_trimmed("/proc/sys/kernel/watchdog")
                .as_deref(),
            Some("1")
        );
    }

    #[test]
    fn verify_and_bench_exit_nonzero_when_the_node_or_capture_fails() {
        let (node, plan) = node();
        let verified = cli(&node, &plan, &["verify", "--sample", "0"]);
        assert_eq!(verified.code, Kind::Verify.exit_code());
        assert!(verified.stdout.contains("verdict   NOT isolated"));
        assert!(verified.stdout.contains("interrupt traffic over 0s"));

        let capture = node.dir.path().join("cyclictest.txt");
        std::fs::write(
            &capture,
            "T: 2 ( 1) P:80 I:200 C: 100 Min: 1 Avg: 1 Max: 900\n",
        )
        .unwrap();
        let judged = cli(
            &node,
            &plan,
            &["bench", "--input", &capture.to_string_lossy()],
        );
        assert_eq!(judged.code, Kind::Verify.exit_code());
        assert!(judged.stdout.contains("FAIL"));
        assert!(judged.stdout.contains("worst 900us on cpu2"));

        let good = node.dir.path().join("good.txt");
        std::fs::write(&good, "T: 2 ( 1) P:80 I:200 C: 100 Min: 1 Avg: 1 Max: 12\n").unwrap();
        assert_eq!(
            cli(&node, &plan, &["bench", "--input", &good.to_string_lossy()]).code,
            0
        );

        let script = cli(&node, &plan, &["bench", "--script"]);
        assert_eq!(script.code, 0);
        assert!(script
            .stdout
            .contains("cyclictest -m -q -p 80 -t2 -a2,3 -i200 -D 1s"));
        assert!(cli(&node, &plan, &["bench"])
            .stdout
            .contains("isolera bench --input"));
        assert!(cli(&node, &plan, &["bench", "--json"])
            .stdout
            .contains("\"kind\":\"bench\""));
    }

    #[test]
    fn report_and_pin_render_their_documents() {
        let (node, plan) = node();
        let reported = cli(&node, &plan, &["report"]);
        assert_eq!(
            reported.code,
            Kind::Verify.exit_code(),
            "an unprovisioned node needs attention"
        );
        assert!(reported.stdout.contains("inventory"));
        assert!(reported.stdout.contains("no capture supplied"));

        let filed = cli(&node, &plan, &["report", "--out"]);
        assert_eq!(filed.code, 0);
        let path = filed.stdout.trim().to_string();
        assert!(path.starts_with("/var/lib/isolera/state/report-"), "{path}");
        assert!(node
            .sysroot
            .read_optional(&path)
            .unwrap()
            .contains("verdict"));

        let json = cli(&node, &plan, &["report", "--json"]);
        assert!(json.stdout.contains("\"kind\":\"report\""));

        let pinned = cli(&node, &plan, &["pin"]);
        assert_eq!(pinned.code, 0);
        assert!(pinned.stdout.contains("kworker/2:1"));
        assert!(pinned.stdout.contains("taskset -pc 0,1 15"));
        assert!(pinned.stdout.contains("script to run:"));
        assert!(cli(&node, &plan, &["pin", "--json"])
            .stdout
            .contains("\"status\":\"dry-run\""));

        // Applying pins needs confirmation, and the dry run still lists the move.
        assert!(cli(&node, &plan, &["pin", "--apply"])
            .stdout
            .contains("--yes"));
        assert!(cli(&node, &plan, &["pin", "--apply", "--dry-run"])
            .stdout
            .contains("dry-run"));
    }

    #[test]
    fn a_broken_configuration_stops_the_run_with_the_right_code() {
        let node = testing::node();
        let broken = node.dir.path().join("broken.toml");
        std::fs::write(&broken, "[isolation]\ncpus = \"2\"\nnope = true\n").unwrap();
        let outcome = cli(&node, &broken, &["plan"]);
        assert_eq!(outcome.code, Kind::Config.exit_code());
        assert!(outcome.stdout.contains("isolera:"), "{}", outcome.stdout);
        assert!(outcome.stdout.contains("nope"), "{}", outcome.stdout);

        let missing = cli(&node, &node.dir.path().join("absent.toml"), &["plan"]);
        assert_eq!(missing.code, Kind::Config.exit_code());
        assert!(missing.stdout.contains("cannot read configuration"));
    }
}
