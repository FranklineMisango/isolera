//! End-to-end tests that drive the built `isolera` binary.
//!
//! The library's own tests call the modules directly; these run the real process
//! against the on-disk fixture sysroot, which is what catches argument-parsing,
//! exit-code and path mistakes that unit tests cannot see.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use isolera::tempdir::{copy_tree, TempDir};

/// The binary under test, built by cargo.
fn binary() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_isolera"))
}

/// The fixture sysroot that ships with the repository.
fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/sysroot")
}

/// The plan that goes with it.
fn plan() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/isolera.toml")
}

/// A writable copy of the fixture, so a test can apply changes.
fn writable() -> (TempDir, PathBuf) {
    let dir = TempDir::new("isolera-cli-e2e").expect("temp dir");
    let root = dir.path().join("root");
    copy_tree(&fixture(), &root).expect("copy fixture");
    (dir, root)
}

/// Run the binary with `--config`/`--root` pointed at a fixture.
fn run(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(binary());
    command.arg("--config").arg(plan()).arg("--root").arg(root);
    command.args(args);
    command.output().expect("run isolera")
}

/// stdout of a run, as text.
fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

/// stderr of a run, as text.
fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// Exit code of a run.
fn code(output: &Output) -> i32 {
    output.status.code().unwrap_or(-1)
}

/// Read a file inside the fixture root.
fn read(root: &Path, logical: &str) -> String {
    std::fs::read_to_string(root.join(logical.trim_start_matches('/')))
        .unwrap_or_else(|err| panic!("cannot read {logical}: {err}"))
}

#[test]
fn help_and_version_do_not_need_a_node() {
    let help = Command::new(binary()).arg("--help").output().unwrap();
    assert_eq!(code(&help), 0);
    assert!(stdout(&help).contains("USAGE"), "{}", stdout(&help));
    assert!(stdout(&help).contains("rollback"));

    let version = Command::new(binary()).arg("version").output().unwrap();
    assert_eq!(code(&version), 0);
    assert_eq!(stdout(&version), format!("isolera {}\n", isolera::VERSION));

    // An unknown option is a usage error (exit code 2), never a silent no-op.
    let bad = Command::new(binary()).arg("--nope").output().unwrap();
    assert_eq!(code(&bad), 2);
    assert!(stdout(&bad).contains("unknown option"), "{}", stdout(&bad));
}

#[test]
fn audit_and_plan_read_the_fixture() {
    let audit = run(&fixture(), &["audit"]);
    assert_eq!(code(&audit), 0, "{}", stderr(&audit));
    let text = stdout(&audit);
    assert!(text.contains("node      fixture-node"), "{text}");
    assert!(
        text.contains("isolated (planned)"),
        "the summary names the planned set:\n{text}"
    );
    assert!(text.contains("2,3"), "{text}");
    assert!(text.contains("matching-engine-0"), "{text}");
    assert!(text.contains("movable kernel threads"), "{text}");

    let planned = run(&fixture(), &["plan"]);
    assert_eq!(code(&planned), 0);
    let text = stdout(&planned);
    assert!(text.contains("isolcpus=managed_irq,2,3"), "{text}");
    assert!(text.contains("nohz_full=2,3"), "{text}");
    assert!(text.contains("/etc/default/grub"), "{text}");
    assert!(text.contains("taskset -pc 0,1 15"), "{text}");
}

#[test]
fn json_output_is_wrapped_in_a_versioned_envelope() {
    let planned = run(&fixture(), &["plan", "--json"]);
    assert_eq!(code(&planned), 0);
    let text = stdout(&planned);
    assert!(
        text.starts_with("{\"schema\":1,\"kind\":\"plan\",\"node\":\"fixture-node\""),
        "{text}"
    );
    assert!(text.contains(&format!("\"version\":\"{}\"", isolera::VERSION)));
    assert!(text.ends_with("}\n"));
    assert_eq!(text.matches('{').count(), text.matches('}').count());

    assert!(stdout(&run(&fixture(), &["audit", "--json"])).contains("\"kind\":\"audit\""));
    assert!(
        stdout(&run(&fixture(), &["verify", "--sample", "0", "--json"]))
            .contains("\"kind\":\"verify\"")
    );
}

#[test]
fn apply_then_rollback_restores_the_fixture() {
    let (_dir, root) = writable();
    let grub_before = read(&root, "/etc/default/grub");

    // Without confirmation, nothing happens.
    let refused = run(&root, &["apply"]);
    assert_eq!(code(&refused), 2);
    assert!(stdout(&refused).contains("--yes"), "{}", stdout(&refused));
    assert_eq!(read(&root, "/etc/default/grub"), grub_before);

    let applied = run(&root, &["apply", "--yes"]);
    assert_eq!(code(&applied), 0, "{}", stderr(&applied));
    let text = stdout(&applied);
    assert!(text.contains("8 writes"), "{text}");
    assert!(text.contains("follow-up"), "{text}");

    // The node really changed: IRQ, sysctls, grub file and journal.
    assert_eq!(read(&root, "/proc/irq/124/smp_affinity_list"), "2,3");
    assert_eq!(read(&root, "/proc/sys/kernel/watchdog"), "0");
    let grub_after = read(&root, "/etc/default/grub");
    assert!(
        grub_after.contains(
            "GRUB_CMDLINE_LINUX=\"quiet splash isolcpus=managed_irq,2,3 nohz_full=2,3 rcu_nocbs=2,3\""
        ),
        "{grub_after}"
    );
    assert!(grub_after.contains("GRUB_TIMEOUT=\"1\""));
    assert!(root.join("var/lib/isolera/state/journal.toml").is_file());

    // A second apply is a no-op.
    let again = run(&root, &["apply", "--yes"]);
    assert_eq!(code(&again), 0);
    assert!(
        stdout(&again).contains("nothing to change"),
        "{}",
        stdout(&again)
    );

    let rolled = run(&root, &["rollback", "--yes"]);
    assert_eq!(code(&rolled), 0, "{}", stderr(&rolled));
    assert!(
        stdout(&rolled).starts_with("6 restored, 1 removed\n"),
        "{}",
        stdout(&rolled)
    );
    assert_eq!(read(&root, "/etc/default/grub"), grub_before);
    assert_eq!(read(&root, "/proc/irq/124/smp_affinity_list"), "0-3\n");
    assert_eq!(read(&root, "/proc/sys/kernel/watchdog"), "1\n");
}

#[test]
fn verify_fails_until_the_reboot_state_is_in_place() {
    let (_dir, root) = writable();
    let failing = run(&root, &["verify", "--sample", "0"]);
    assert_eq!(code(&failing), 8, "a failing verdict is its own exit code");
    assert!(
        stdout(&failing).contains("verdict   NOT isolated"),
        "{}",
        stdout(&failing)
    );

    assert_eq!(code(&run(&root, &["apply", "--yes"])), 0);
    assert!(
        read(&root, "/proc/cmdline").contains("splash"),
        "the fixture still boots the old command line"
    );

    // What a reboot into the new command line produces: the running cmdline, the
    // kernel's own isolation view, and the tuned parameters the plan recommended.
    let recommended = "nmi_watchdog=0 nowatchdog skew_tick=1 audit=0 \
                       intel_idle.max_cstate=0 processor.max_cstate=0 tsc=reliable";
    std::fs::write(
        root.join("proc/cmdline"),
        format!(
            "BOOT_IMAGE=/vmlinuz-6.8.0 root=/dev/nvme0n1p2 ro quiet splash \
             isolcpus=managed_irq,2,3 nohz_full=2,3 rcu_nocbs=2,3 {recommended}\n"
        ),
    )
    .unwrap();
    std::fs::write(root.join("sys/devices/system/cpu/isolated"), "2,3\n").unwrap();
    std::fs::write(root.join("sys/devices/system/cpu/nohz_full"), "2,3\n").unwrap();

    let passing = run(&root, &["verify", "--sample", "0"]);
    assert_eq!(code(&passing), 0, "{}", stdout(&passing));
    assert!(
        stdout(&passing).contains("verdict   isolated"),
        "{}",
        stdout(&passing)
    );
}

#[test]
fn bench_judges_a_capture_and_files_a_report() {
    let (_dir, root) = writable();

    let script = run(&root, &["bench", "--script"]);
    assert_eq!(code(&script), 0);
    assert!(stdout(&script).contains("cyclictest -m -q -p 80 -t2 -a2,3 -i200 -D 1s"));

    let capture = root.join("cyclictest.txt");
    std::fs::write(
        &capture,
        "T: 2 ( 1) P:80 I:200 C: 1000 Min: 1 Avg: 2 Max: 350\n",
    )
    .unwrap();
    let judged = run(&root, &["bench", "--input", &capture.to_string_lossy()]);
    assert_eq!(code(&judged), 8);
    assert!(
        stdout(&judged).contains("worst 350us on cpu2"),
        "{}",
        stdout(&judged)
    );

    let filed = run(
        &root,
        &["report", "--out", "--bench", &capture.to_string_lossy()],
    );
    assert_eq!(code(&filed), 0);
    let path = stdout(&filed).trim().to_string();
    assert!(path.starts_with("/var/lib/isolera/state/report-"), "{path}");
    let body = read(&root, &path);
    assert!(body.contains("verdict   needs attention"), "{body}");
    assert!(body.contains("worst 350us on cpu2"), "{body}");
    assert!(body.contains("inventory"));
}

#[test]
fn pin_lists_the_work_without_touching_anything() {
    let (_dir, root) = writable();
    let listed = run(&root, &["pin"]);
    assert_eq!(code(&listed), 0);
    let text = stdout(&listed);
    assert!(text.contains("kworker/2:1"), "{text}");
    assert!(text.contains("taskset -pc 0,1 15"), "{text}");

    // kthreadd is kept by the plan; kworker/0:2 is already on housekeeping.
    assert!(!text.contains("# kthreadd"), "{text}");
    assert!(!text.contains("kworker/0:2"), "{text}");
}
