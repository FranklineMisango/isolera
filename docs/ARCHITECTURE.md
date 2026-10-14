# Isolera architecture

Isolera is one binary and one library with no third-party dependencies. This
document explains how the modules fit together, why the boundaries are where they
are, and how to extend the suite without breaking its guarantees.

## Module map

| Module | Responsibility |
| --- | --- |
| `error` | One error type whose `Kind` doubles as the process exit code. |
| `log` | Diagnostics on stderr; stdout stays reserved for documents. |
| `sysroot` | **The only path to the host.** Root-prefixed logical paths, dry-run journalling, `WriteRecord`s for rollback. |
| `toml` | Hand-written TOML reader/serialiser (tables, dotted keys, arrays, array-of-tables, inline tables). |
| `config` | Typed plan sections, a strict `Reader` that rejects unknown keys, and `validate()`. |
| `cpulist` | CPU-set parsing/formatting, hex affinity masks, set algebra. |
| `topology` | `/proc/cpuinfo` and `/sys` topology, NUMA, per-CPU setters, SMT pairs. |
| `irq` | `/proc/interrupts` parsing, fnmatch globs, affinity I/O, the steering policy and its decisions. |
| `kernel` | Command-line model and merging, cmdline findings, sysctl plans, kernel-thread discovery and pin commands. |
| `grub` | `/etc/default/grub` editing that preserves every untouched byte, plus timestamped backups. |
| `audit` | Phase 1: read-only inventory of the node. |
| `plan` | Phase 2, step 1: every intended change, built through the same functions `apply` uses. |
| `journal` | The apply record on disk (TOML), with previous values and a rolled-back marker. |
| `apply` | Phase 2, step 2: perform the plan in a fixed order and journal it. |
| `rollback` | Undo the journal newest-first, restoring or removing each path. |
| `verify` | Phase 3: compare the running node with the plan, sampling interrupt traffic. |
| `bench` | `cyclictest` command rendering, output parsing, budget judgement. |
| `pin` | Discover kernel threads, render and run `taskset` moves, read the mask back. |
| `report` | One document combining inventory, plan, static checks and a capture. |
| `render` | Output plumbing: `Format` (`text`/`json`) and the versioned JSON envelope. |
| `cli` | Hand-rolled argument parsing, dispatch, exit codes. |
| `tempdir` | Dependency-free temporary directory plus a recursive tree copy. |
| `testing` | The synthetic node used by the unit tests (compiled only for tests). |

## Data flow

```
                 isolera.toml
                      |
                  config::Config  (strict Reader, validate())
                      |
   +------------------+------------------+---------------------+
   |                  |                  |                     |
 topology::probe   Cmdline::read   irq::read_interrupts   sysctl state
   |                  |                  |                     |
   +------------------+------------------+---------------------+
                      |
                 plan::build  ──────────────►  Plan (read-only)
                      |                             |
                      |                        plan::render / to_json
                      v
                 apply::run  ──────────────►  Applied + journal.toml
                      |                             |
                      v                        rollback::run
                 the node                           |
                      |                             v
              verify::run / bench             restored node
                      |
                 report::collect ─────────►  report document / file
```

Every arrow out of `plan::build` and `apply::run` goes through `SysRoot`, which is
what makes the diagram testable: point the root at `tests/fixtures/sysroot` and the
same code runs against a directory of files instead of a kernel.

## The `SysRoot` boundary

`SysRoot` owns three guarantees the rest of the crate relies on:

1. **Root translation.** All paths are logical absolute paths (`/proc/cmdline`).
   `abs()` applies the prefix and `logical()` goes back for messages, so `--root`
   is a first-class feature rather than a test hack.
2. **Dry-run journalling.** `write` either performs the change or appends a
   `WriteRecord` to the in-memory journal. `plan` and `apply` share one code path,
   so a dry run cannot describe something `apply` would not do.
3. **Rollback data.** A record keeps the previous value and whether the kernel will
   accept a restore (`is_reversible`). `/proc/cmdline` is explicitly irreversible.

Pseudo-files are written in place when the root is the live `/`, because the kernel
rejects a temp-file-plus-rename over `/proc` and `/sys`. Under a prefixed root those
same paths are ordinary files, so they are replaced atomically - which also
truncates them, so writing `0` over `0-3\n` cannot leave a stale tail behind.

## Plan, apply, rollback

`plan::build` is read-only by construction: it walks the same functions `apply`
calls, then throws the results away. That is deliberate - a plan computed by a
second implementation would eventually describe something else.

`apply::run` orders its steps so a failure leaves the node in a sensible state:

1. **governor** - the hardware capability,
2. **SMT offlines** - what the scheduler may use,
3. **IRQ affinity** - re-homes what offlining displaced,
4. **sysctls** - the runtime knobs,
5. **grub file** - last, so a failure earlier in the run never touches the boot
   configuration,
6. **journal** - written after the fact, and never lists itself.

`apply` is not transactional. It is *auditable*: a failing step stops the run, every
completed write is in the journal, and `rollback` reverses the journal newest-first
so two writes to the same path unwind to the oldest recorded value.

## Interrupt steering semantics

`irq::plan` evaluates each interrupt once, in this order:

| Situation | Action | Behaviour |
| --- | --- | --- |
| kernel-internal (`LOC`, `NMI`, `RES`, ...) | `unsteerable` | Reported, never written: only the command line influences them. |
| `irq.ignore` pattern | `ignore` | Left exactly as found. |
| `irq.overrides` pattern | `override` | Moved to the listed CPUs, even onto the trading cores - this is how a feed handler or the matching engine is pointed at the strategy. |
| `irq.exclude` pattern, mask touches a trading core | `exclude` | Moved to `irq.housekeeping`. |
| `irq.exclude` pattern, mask already clear | `exclude` | Recorded, not written: *widening* a mask to the whole housekeeping set would spread a device onto cores it does not use today. |
| mask touches a trading core | `pin` | Moved to housekeeping. |
| anything else | `keep` | Nothing written. |

When an override and an exclusion both match, `irq.precedence` decides
(`override_wins` by default). A configured pattern that matched nothing is reported
as a warning, so a typo is visible instead of silent.

## Verification model

`verify` compares the plan with the running kernel in five layers: the command line
(`inspect_cmdline`, `managed_drift`), the kernel's own view
(`/sys/devices/system/cpu/isolated`, `nohz_full`), SMT siblings, the governor, and
sysctls. On top of that it takes two samples of `/proc/interrupts`:

* counters are **monotonic**, so a cumulative total says nothing about the last
  minute - only the delta between two samples does;
* an interrupt with an `irq.overrides` entry is *allowed* on the trading cores;
  anything else aimed at them fails the check;
* an overrun reported by `cyclictest` invalidates a measurement whose numbers
  otherwise look fine.

`bench` never measures anything itself. It renders the `cyclictest` invocation from
the plan, parses whatever `cyclictest` printed (tolerating both `Min: 1` and
`Min:1` spacing), and judges the worst case per core against `verify.budget_us`.

## Determinism

The suite is meant to be diffed, so:

* configuration tables and IRQ decisions live in `BTreeMap`s, and every renderer
  sorts explicitly;
* `Table` pads columns to the widest cell and strips trailing whitespace, so a
  report is byte-identical across runs and between nodes;
* generated TOML (the journal) is emitted in sorted key order;
* JSON is assembled by hand in a fixed field order and every document carries a
  schema version.

## Extending the suite

To add a tuned surface (say `kernel.perf_event_paranoid`):

1. add the typed field to the relevant section in `config.rs` - the strict `Reader`
   then accepts it there and rejects it everywhere else;
2. put the behaviour in the module that owns the surface, as a `plan()`/`apply()`
   pair modelled on `kernel::sysctl_plan`/`kernel::apply_sysctls`;
3. call the planner from `plan::build` and the applier from `apply::run`, at the
   right position in the order above;
4. add a check to `verify::run` if the surface can drift after provisioning;
5. cover it in that module's tests plus one end-to-end test in `tests/cli.rs`;
6. document it in the README table and in the runbook.

Nothing else changes: the CLI dispatches whole commands, `plan`/`apply` compose
modules, and the fixture node grows one file.
