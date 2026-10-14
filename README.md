# Isolera

**Deterministic Linux CPU-core isolation for low-latency trading nodes.**

Isolera takes a declarative plan (`isolera.toml`) and turns it into the kernel
command line, an interrupt-routing table, an SMT and CPU-governor state, a set of
tuning sysctls and a kernel-thread pinning recipe - then proves the result with
counters and `cyclictest`. It is written in Rust with **no third-party crates**,
because it runs as root on a production trading node and the standard library is
the entire supply chain.

```sh
cargo build --release
sudo target/release/isolera --config /etc/isolera.toml audit
sudo target/release/isolera --config /etc/isolera.toml plan
sudo target/release/isolera --config /etc/isolera.toml apply --yes
sudo reboot
sudo target/release/isolera --config /etc/isolera.toml verify
```

## The problem it solves

A trading strategy pinned to a core is still interrupted: the scheduler's tick
fires there, RCU callbacks run there, `kworker` threads get scheduled there, a
device queue delivers its interrupt there, and a neighbour hyperthread shares the
physical core's execution units. Each of those contributes microseconds of
jitter, and each of them is controllable - with `isolcpus`, `nohz_full`,
`rcu_nocbs`, IRQ affinity masks, sysfs offlining and a handful of sysctls.

Getting all of that right by hand is a dozen chances to typo a CPU list. Isolera
makes it one reviewed plan, one auditable list of actions, and one rollback.

## Design rules

| Rule | Why |
| --- | --- |
| No third-party crates | The dependency tree of a root-privileged tool is attack surface. TOML, glob matching, table rendering and JSON all live here. |
| Every host interaction goes through `SysRoot` | Paths are logical (`/proc/cmdline`); a root prefix is applied on access. The whole suite runs against `tests/fixtures/sysroot`, unprivileged, and `--root` makes containers and chroots first-class. |
| Plan before apply | `plan` and `apply` share one code path, so the printed plan cannot drift from the executed one. `--dry-run` records intended writes instead of performing them. |
| Everything is reversible or declared irreversible | Each write is journalled with its previous value; `/proc/cmdline` and live statistics are marked irreversible and reported as such instead of being silently skipped. |
| Unknown is an error | An unknown configuration key, an unknown command-line flag, or a command-line variable that is not a valid shell name fails loudly. Nothing is guessed. |
| `#![forbid(unsafe_code)]` | Affinity is applied with `taskset` rather than a raw syscall, so the crate stays free of `unsafe`. |

## Commands

| Command | What it does | Exit codes |
| --- | --- | --- |
| `audit` | Inventories the node: topology, SMT pairs, the kernel's isolation view, cmdline findings, per-IRQ affinity and traffic, kernel threads. Read-only. | 0 |
| `plan` | Prints every change the configuration asks for, grouped by subsystem. Read-only. | 0, 3 |
| `apply` | Performs the plan in a fixed order, backs up the grub file and journals every change. Needs `--yes` or `--dry-run`. | 0, 3, 4, 5 |
| `rollback` | Replays the journal newest-first and puts every path back. Needs `--yes` or `--dry-run`. | 0, 4, 5 |
| `verify` | Checks a provisioned node against the plan, sampling `/proc/interrupts` for a window to judge real traffic. | 0, 8 |
| `bench` | Renders the `cyclictest` invocation for the plan, or judges a capture against `verify.budget_us`. | 0, 8 |
| `report` | One document combining inventory, plan, static checks and an optional capture; `--out` files it under `paths.state_dir`. | 0, 8 |
| `pin` | Lists (or, with `--apply`, moves) the kernel threads still sitting on the trading cores. | 0, 7 |

Global options: `--config <path>` (`-` reads stdin), `--root <dir>`, `--dry-run`,
`--yes`, `--json`, `--output text|json`, `--quiet`, `--verbose`, `--trace`.

## The plan file

```toml
[node]
name = "matching-engine-01"
role = "matching-engine"

[isolation]
cpus = "6-9"            # the trading cores
smt = false             # offline the hyperthread sibling of every trading core
governor = "performance"
nohz_full = true        # no scheduler tick on those cores
rcu_nocbs = true        # RCU callbacks run elsewhere
managed_irq = true      # managed IRQs stay away from them
housekeeping = "0-5"    # what is left for the kernel and the OS

[irq]
exclude = ["nvme*", "eth0-rx-*"]       # never on a trading core
ignore  = ["IR-PCI-MSI.*", "spurious"] # never rewritten at all

[irq.overrides]                        # exact targets, these win by default
"matching-engine-*" = "6,7"
"feed-handler-*"    = "8,9"

[kernel.sysctls]
"kernel.watchdog" = "0"

[grub]
file = "/etc/default/grub"
cmdline_variable = "GRUB_CMDLINE_LINUX"

[grub.variables]
GRUB_TIMEOUT = "1"

[rt]
pin_kernel_threads = true
keep = ["kthreadd", "migration/*"]
rt_runtime_us = -1

[verify]
budget_us = 80          # fail verification above 80 us of jitter
interval_us = 200
duration_s = 300
```

`examples/isolera.toml` is the annotated, complete version. It describes a real
16-core node, so `plan` against the four-CPU fixture reports the CPUs it cannot find
and the IRQ patterns that match nothing - which is the point of the warnings, and why
the fixture has its own smaller plan in `tests/fixtures/isolera.toml`.

## What it changes on a node

| Surface | Change |
| --- | --- |
| `/etc/default/grub` (carries `/proc/cmdline`) | `isolcpus`, `nohz_full`, `rcu_nocbs`, `rcu_nocb_poll` rewritten in place; every other parameter, comment and blank line preserved. A pre-change copy is filed under `grub.backup_dir`. Takes effect on reboot. |
| `/proc/irq/<n>/smp_affinity_list` | Exclusions moved to housekeeping, overrides pinned to their listed CPUs, everything else left alone. |
| `/sys/devices/system/cpu/cpu<n>/online` | Hyperthread siblings of trading cores taken offline (unless `isolation.smt`). |
| `/sys/devices/system/cpu/cpu<n>/cpufreq/scaling_governor` | Set to `isolation.governor` on every CPU sharing a core with a trading core. |
| `/proc/sys/...` | The plan's sysctls, plus `kernel.sched_rt_runtime_us` and `kernel.timer_migration` where the plan implies them. Knobs a kernel does not expose are skipped. |
| kernel threads | `kworker/*`, `ksoftirqd/*`, the RCU offload threads and friends are listed as `taskset` commands; `pin --apply` runs them. |
| `/var/lib/isolera/state/journal.toml` | Every write with its previous value, so `rollback` can undo the run. |

## Report and output

`--json` wraps every document in a versioned envelope, so a provisioning script
can identify it and refuse a version it does not understand:

```json
{"schema":1,"kind":"plan","node":"matching-engine-01","version":"1.0.0","data":{...}}
```

## Exit codes

`0` success · `1` internal error · `2` usage · `3` configuration · `4` I/O ·
`5` root required · `6` not found · `7` unsupported · `8` verification failed.

## Repository layout

```
src/            one module per concern; see docs/ARCHITECTURE.md
examples/       annotated plan
tests/cli.rs    end-to-end tests that run the built binary
tests/fixtures/ a synthetic four-CPU node, plus the generator that writes it
tools/          boot tuner and cyclictest harness for the node
docs/           architecture and runbook
bin/isolera     launcher that finds the binary in a checkout or on a node
```

## Development

```sh
cargo test                      # unit tests plus the end-to-end CLI tests
cargo clippy --all-targets      # no warnings, by policy
cargo run -- audit --root tests/fixtures/sysroot --config tests/fixtures/isolera.toml
```

The fixture node is a real directory tree, not a mock, so every command can be
exercised without privileges:

```sh
cp -r tests/fixtures/sysroot /tmp/node
cargo run -- plan     --root /tmp/node --config tests/fixtures/isolera.toml
cargo run -- apply    --root /tmp/node --config tests/fixtures/isolera.toml --yes
cargo run -- rollback --root /tmp/node --config tests/fixtures/isolera.toml --yes
```

## Honest limits

* `cyclictest` is a separate package (`rt-tests`): Isolera renders and judges the
  measurement but does not ship the timer.
* Affinity and priority are applied through `taskset` and `chrt`, so util-linux
  must be installed for `pin --apply`.
* `nohz_full` only delivers its full benefit on a kernel built with
  `CONFIG_NO_HZ_FULL=y`. Isolera reports what the kernel says
  (`/sys/devices/system/cpu/nohz_full`) instead of assuming.
* Nothing here replaces a hardware review: SMI, ECC scrubbing and firmware power
  management live below the operating system, and no amount of `isolcpus` will
  quiet them.
