#!/bin/sh
# Latency harness: measure the trading cores and judge the result.
#
# This is the script `isolera bench --script` prints, wrapped in the checks that a
# measurement on a *production* node needs: it refuses to run while the strategy is
# busy, pins itself out of the way, writes a timestamped capture, and hands that
# capture straight to `isolera bench --input` so the budget is applied by the same
# code the plan carries.
#
# Usage:
#   cyclictest-harness.sh [--config /etc/isolera.toml] [--duration 300]
#                         [--out /var/lib/isolera/captures]
set -eu

BIN=${ISOLERA_BIN:-isolera}
CONFIG=/etc/isolera.toml
DURATION=
OUT=/var/lib/isolera/captures

while [ $# -gt 0 ]; do
    case "$1" in
        --config) CONFIG=$2; shift 2 ;;
        --duration) DURATION=$2; shift 2 ;;
        --out) OUT=$2; shift 2 ;;
        -h|--help)
            sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "cyclictest-harness.sh: unknown option $1" >&2; exit 2 ;;
    esac
done

log() {
    echo "cyclictest-harness: $*"
}

if [ "$(id -u)" -ne 0 ]; then
    log "must run as root: cyclictest wants SCHED_FIFO and mlockall"
    exit 1
fi

if ! command -v cyclictest >/dev/null 2>&1; then
    log "cyclictest is missing; install rt-tests and run again"
    exit 7
fi

# The measurement must describe the kernel, not the load: a busy strategy would be
# measured, not the isolation.
load=$(cut -d' ' -f1 /proc/loadavg)
log "current load average: $load"
if [ "$(echo "$load > 1.0" | bc 2>/dev/null || echo 0)" = "1" ]; then
    log "the node is busy; quiesce the strategy before measuring, then run again"
    exit 1
fi

mkdir -p "$OUT"
capture=$OUT/cyclictest-$(date -u +%Y%m%dT%H%M%SZ).txt

# `isolera bench --script` renders the exact invocation from the plan, so the
# affinity, priority, interval and duration never drift from the approved config.
isolera_script=$OUT/harness-$(date -u +%Y%m%dT%H%M%SZ).sh
"$BIN" --config "$CONFIG" bench --script >"$isolera_script"

if [ -n "$DURATION" ]; then
    # Override only the duration, keeping everything else from the plan.
    sed -i "s/-D [0-9]*s/-D ${DURATION}s/" "$isolera_script"
fi

log "measuring; the capture goes to $capture"
sh "$isolera_script" >"$capture" 2>&1 || true

log "judging the capture"
if "$BIN" --config "$CONFIG" bench --input "$capture" --json \
        >"$capture.json" 2>&1; then
    log "within budget"
    exit 0
fi

log "over budget; details follow"
cat "$capture.json"
exit 8
