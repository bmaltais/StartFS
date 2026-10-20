#!/usr/bin/env bash
# Node-to-node transfer speed: write one large incompressible file through A's mount and time how
# long until it is fully replicated on B (2 nodes, 2 copies). An RTT is added to loopback with
# `tc netem` so the flow-control and congestion limits show; with RTT_MS=0 nothing is added.
# MODE=read measures the other direction: B offers no space and A holds the only copy, and the time
# is B reading the file back through its mount with O_DIRECT, every chunk fetched from A (each run
# reads a new file, so B's cache cannot help). The first run also checks the bytes.
# Environment: RTT_MS=50, SIZE_MB=256, RUNS=3, MODE=replicate, BIN=path/to/startfs (skips the build,
# to compare binaries).
# Usage: profile-transfer.sh   (needs /dev/fuse, fusermount3, and sudo for tc when RTT_MS > 0)
set -euo pipefail
given=${BIN:-}
source "$(dirname "$0")/lib.sh"
RTT_MS=${RTT_MS:-50}; SIZE_MB=${SIZE_MB:-256}; RUNS=${RUNS:-3}; MODE=${MODE:-replicate}
[ -n "$given" ] || build
setup_env
shaped=
unshape() { [ -z "$shaped" ] || sudo tc qdisc del dev lo root 2>/dev/null || true; shaped=; }
trap 'unshape; cleanup' EXIT
mkdir -p "$WORK"/{A,B}/mnt
start_node A "$WORK/A/mnt"
copies=2; offer=20G
[ "$MODE" = read ] && { copies=1; offer=0; }
TOKEN=$(sf A create --name transfer --copies "$copies")
sf A config --offer 20G >/dev/null
start_node B "$WORK/B/mnt"; sf B join "$TOKEN" --offer "$offer" >/dev/null
until sf A status | grep -q "(2 nodes)"; do sleep 0.2; done
if [ "$RTT_MS" -gt 0 ]; then
  sudo tc qdisc add dev lo root netem delay "$((RTT_MS / 2))ms" limit 200000; shaped=1
fi
for i in $(seq "$RUNS"); do
  if [ "$MODE" = read ]; then
    head -c $((SIZE_MB * 1024 * 1024)) /dev/urandom > "$WORK/src.bin"
    cp "$WORK/src.bin" "$WORK/A/mnt/f$i.bin"; sync -f "$WORK/A/mnt/f$i.bin"
    until ! sf A status | grep -q 'not fully replicated'; do sleep 0.05; done
    until [ "$(stat -c %s "$WORK/B/mnt/f$i.bin" 2>/dev/null || echo 0)" = $((SIZE_MB * 1024 * 1024)) ]; do sleep 0.05; done
    t0=$(now)
    dd if="$WORK/B/mnt/f$i.bin" of=/dev/null bs=1M iflag=direct status=none
    t=$(elapsed "$t0")
    [ "$i" != 1 ] || cmp -s "$WORK/src.bin" "$WORK/B/mnt/f$i.bin" || { echo "BYTES DIFFER"; exit 1; }
    echo "run $i: RTT ${RTT_MS}ms, ${SIZE_MB} MiB: remote read ${t}s, $(python3 -c "print(f'{$SIZE_MB / $t:.1f}')") MiB/s"
    continue
  fi
  head -c $((SIZE_MB * 1024 * 1024)) /dev/urandom > "$WORK/src.bin"
  t0=$(now)
  cp "$WORK/src.bin" "$WORK/A/mnt/f$i.bin"; sync -f "$WORK/A/mnt/f$i.bin"
  tw=$(elapsed "$t0")
  until ! sf A status | grep -q 'not fully replicated'; do sleep 0.05; done
  t=$(elapsed "$t0")
  echo "run $i: RTT ${RTT_MS}ms, ${SIZE_MB} MiB: write ${tw}s, replicated ${t}s, $(python3 -c "print(f'{$SIZE_MB / $t:.1f}')") MiB/s"
done
for n in A B; do
  eval "pid=\$PID_$n"
  echo "node $n: peak RSS $(( $(hwm_kb "$pid") / 1024 )) MiB, CPU $(python3 -c "print(f'{$(cpu_ticks "$pid") / 100:.1f}')") s over $RUNS runs of $SIZE_MB MiB"
done
