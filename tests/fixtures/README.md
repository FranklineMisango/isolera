# Test fixtures

`sysroot/` is a synthetic four-CPU SMT node. Point Isolera at it with
`--root tests/fixtures/sysroot` and the whole suite runs on a laptop with no
privileges:

```sh
cargo run -- audit  --config tests/fixtures/isolera.toml --root tests/fixtures/sysroot
cargo run -- plan   --config tests/fixtures/isolera.toml --root tests/fixtures/sysroot
cargo run -- apply  --config tests/fixtures/isolera.toml --root tests/fixtures/sysroot --dry-run
```

## The machine

| CPUs | Role | Notes |
| --- | --- | --- |
| 0, 1 | housekeeping | hyperthreads of the same physical core (`core id 0`) |
| 2, 3 | trading cores | hyperthreads of the same physical core (`core id 1`) |

`isolera.toml` isolates 2,3, so nothing needs offlining; change `isolation.cpus`
to `"2"` and CPU 3 becomes a sibling that must be offlined.

Reachable behaviour, all of it on disk so a test can assert on it:

* `proc/interrupts` - seven rows, including a kernel-internal `LOC`/`NMI` pair,
  two idle NVMe queues, a busy `eth0-rx-0` on housekeeping, and
  `matching-engine-0` firing 1200 times on CPU 2 (the noise the plan has to
  remove, and the IRQ the plan's override puts *back* on the trading cores).
* `proc/irq/*/smp_affinity_list` - present only for the four IRQs above, so
  steering is exercised without a driver.
* `proc/sys/kernel/*` - four writable sysctls with stock values.
* `proc/{2,15,16}` - `kthreadd` (kept by the plan) and two kworkers, one of which
  (`kworker/2:1`) sits on a trading core and must be moved with `taskset`.
* `etc/default/grub` - comments, blank lines and a `GRUB_CMDLINE_LINUX` with a
  pre-existing `quiet splash`, so cmdline merging is exercised.

`isolated` and `nohz_full` under `/sys` are empty, which is what an
unprovisioned node looks like. `isolera verify` and `isolera report` therefore
fail their checks until a test writes the post-reboot state by hand - see the
`verify` and `report` tests for that.

## Regenerating

`examples/make_fixture.rs` writes the whole tree from one table, so the fixture
stays readable and reviewable - and the generator is Rust, like everything else:

```sh
cargo run --example make_fixture                 # writes tests/fixtures/sysroot
cargo run --example make_fixture -- /tmp/regen   # or somewhere else
diff -r tests/fixtures/sysroot /tmp/regen        # prove a change is intentional
```

The generator is deliberately independent of `SysRoot`, so a bug in the write path
cannot silently redefine the data the tests assert on. `src/testing.rs` builds the
same machine in a temporary directory for the unit tests, so the two describe one
node.
