# Isolera runbook

Everything an operator needs on a provisioning night: the pre-flight checks, the
exact commands, what the output should look like, and what to do when it does not.

Throughout, `ISOLERA` is the binary (`target/release/isolera` from a checkout, or
`/usr/local/bin/isolera` after installing it), and `CONFIG` is the plan
(`/etc/isolera.toml`).

## 0. Pre-flight

```sh
$ISOLERA --version
$ISOLERA --config $CONFIG plan            # exits 0 when the plan validates
nproc; grep -c ^processor /proc/cpuinfo   # does the CPU count match the plan?
cat /sys/devices/system/cpu/present /sys/devices/system/cpu/online
```

Then read the audit before touching anything:

```sh
$ISOLERA --config $CONFIG audit
```

What to look for:

* **`kernel command line`** - `isolcpus is not active` is expected on a fresh node;
  it is what provisioning fixes.
* **`smt siblings to offline`** - if this is non-empty you are about to take CPUs
  offline. Confirm nobody is running there (`ps -eLo psr,comm`).
* **`interrupts on the isolated cores`** - the devices whose traffic has to move.
  Write them down: after provisioning this list must be empty, or explained by an
  `irq.overrides` entry.
* **`movable kernel threads`** - the count `pin` will act on.

Stop here if the node is under load: provisioning offlines cores and moves IRQs, and
both are disruptive to a live strategy.

## 1. What will change

```sh
$ISOLERA --config $CONFIG plan
```

Read every section: the command line before/after, the grub file changes, the cores
to offline, the interrupt moves, the sysctls, the kernel-thread work list, and the
warnings (`irq pattern "..." matched no interrupt` means a typo).

To see exactly which files and values would be written:

```sh
$ISOLERA --config $CONFIG apply --dry-run
$ISOLERA --config $CONFIG apply --dry-run --json   # one-line envelope, for tooling
```

## 2. Apply

```sh
$ISOLERA --config $CONFIG apply --yes
```

Expected shape of the output:

```
8 writes
path                                     value              reversible
---------------------------------------  -----------------  ----------
/proc/irq/124/smp_affinity_list           2,3                yes
...
follow-up
  - run `isolera pin --apply` to move 1 kernel thread(s) off 2,3
  - run `update-grub` to regenerate the bootloader menu
  - reboot to boot the new kernel command line; isolation is only active after that
```

If a step fails the run stops there. Nothing is left half-written: what completed is
in the journal, and `rollback --yes` undoes it.

```sh
$ISOLERA --config $CONFIG rollback --dry-run   # what would be restored
$ISOLERA --config $CONFIG rollback --yes
```

## 3. Move the kernel threads

`apply` lists the kernel threads that still sit on the trading cores; it does not
move them, because that is a `taskset` call rather than a file write.

```sh
$ISOLERA --config $CONFIG pin                 # review the work list and the script
$ISOLERA --config $CONFIG pin --apply --yes   # or run the printed script yourself
```

Kernel threads move between reboots, so repeat this after every boot - the boot
tuner (step 6) does it automatically.

## 4. Reboot and verify

```sh
grep -o 'isolcpus=[^ ]*' /proc/cmdline   # the new parameter is in the file
update-grub                              # or whatever grub.regenerate names
systemctl reboot
```

After the reboot:

```sh
$ISOLERA --config $CONFIG verify --sample 10
```

The verdict is `isolated` when every check passes. Each failing check names what to
look at:

| Check | Failure means | First thing to look at |
| --- | --- | --- |
| `cmdline` | `/proc/cmdline` does not carry the plan | `update-grub` really ran, and the node booted the entry you edited |
| `isolated-sysfs` | the kernel's isolated set differs from the plan | boot entry, `isolcpus=` spelling, `CONFIG_CPU_ISOLATION` |
| `nohz-full` | the tick still fires on trading cores | `CONFIG_NO_HZ_FULL=y`, and `nohz_full=` on the boot line |
| `smt-siblings` | a sibling came back online | the boot tuner ran, or `apply --yes` after the reboot |
| `governor` | the CPU is not in `performance` | the cpufreq driver, and whether the firmware overrides it |
| `sysctls` | a tuned value is not in place | `apply --yes` after the reboot, or a later boot script overwriting it |
| `irq-affinity` | an interrupt without an override is on a trading core | add it to `irq.exclude`, or pin it in `irq.overrides` if it belongs there |
| `irq-traffic` | a device is still firing on trading cores | its queue count, and whether one pattern matches every queue |

## 5. Measure

```sh
$ISOLERA --config $CONFIG bench --script > /tmp/measure.sh
sh /tmp/measure.sh | tee /tmp/cyclictest.txt     # as root, strategy quiesced
$ISOLERA --config $CONFIG bench --input /tmp/cyclictest.txt
```

Or use the harness, which adds the load check and files the capture:

```sh
tools/cyclictest-harness.sh --config $CONFIG --duration 300
```

`verify.budget_us` is the pass/fail line. A single core over budget fails the run
with exit code 8 and the table marks which one. An `overrun` line from `cyclictest`
fails the run regardless of the numbers: the timer could not keep up, so its
statistics are not trustworthy.

## 6. Keep it applied across reboots

```sh
sed -e 's|@BINDIR@|/usr/local/bin|' -e "s|@CONFIG@|$CONFIG|" -e 's|@DELAY@|20|' \
    tools/isolera-boot-tuner.sh.in > /usr/local/sbin/isolera-boot-tuner
chmod 0755 /usr/local/sbin/isolera-boot-tuner
```

Install the systemd unit from that script's header comment. It re-asserts the runtime
half of the plan (sysctls, IRQ affinity, governor, offlines, kernel-thread pins) on
every boot, and refuses to run if the plan no longer validates.

## 7. Periodic checks

A weekly `isolera report` is enough to catch drift: a device renaming its queues, a
driver resetting its affinity, a colleague restarting `irqbalance`.

```
# /etc/cron.d/isolera
SHELL=/bin/sh
PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
17 4 * * 1 root /usr/local/bin/isolera --config /etc/isolera.toml --quiet report --out
```

The report lands in `/var/lib/isolera/state/report-<timestamp>.txt`, next to the
journal. Exit code 8 means it found something.

## Failure playbook

| Symptom | Likely cause | Action |
| --- | --- | --- |
| `isolcpus is not active` after a reboot | grub was not regenerated, or the node booted another entry | run `grub.regenerate`, reboot, confirm with `grep isolcpus /proc/cmdline` |
| A trading core is busy with `kworker` again after a reboot | kernel threads are not pinned by any file; they are `taskset` calls | boot tuner, or `pin --apply --yes` |
| `irq-traffic` fails on a device you excluded | the device has more queues than the pattern matches | widen the pattern (`nvme*`, not `nvme0q0`) and re-apply |
| `irq-affinity` fails right after provisioning | a driver reset its queues (common after a firmware reload) | re-apply; if it recurs, exclude the device in the driver too |
| `sysctls` are not applied although `apply` wrote them | something later in boot rewrote them | find it (`grep -r sched_rt_runtime_us /etc`) and order the boot tuner after it |
| `apply` reports "no writable affinity file" | the device was removed between plan and apply | re-run `plan`, check the device list, then `apply` |
| `pin --apply` reports "taskset is not installed" | util-linux is missing | install it, or run the printed script on a host that has it |
| `rollback --yes` restores everything except `/proc/cmdline` | by design: a running command line cannot be restored | the grub file *is* restored; reboot to pick it up |
| The node will not boot after provisioning | a bad kernel parameter | boot the previous grub entry from the menu, restore `/etc/default/grub` from the timestamped copy under `grub.backup_dir`, run `grub.regenerate` |
| `verify` passes but latency is still high | the jitter is below the OS: SMI, firmware power management, ECC scrubbing, or the strategy itself | measure again with the strategy stopped before blaming the kernel |

## Escalation evidence

When opening a ticket, attach one document:

```sh
$ISOLERA --config $CONFIG report --out --bench /var/lib/isolera/captures/<capture>.txt
```

It carries the inventory, the intended changes, every check with its detail, and the
latency capture - which is usually enough to avoid a second round trip.
