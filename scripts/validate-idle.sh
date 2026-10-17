#!/usr/bin/env bash
# Criterion 4: RSS and CPU of real idle daemons (2 nodes, 2 copies) read from /proc.
# Usage: validate-idle.sh [SECONDS_PER_SAMPLE=30] [DATA_MB=200]
set -euo pipefail
source "$(dirname "$0")/lib.sh"
SECS=${1:-30}
DATA_MB=${2:-128}
build; setup_env
mkdir -p "$WORK/A/mnt" "$WORK/B/mnt"
start_node A "$WORK/A/mnt"
TOKEN=$(sf A create --name home --copies 2)
start_node B "$WORK/B/mnt"
sf B join "$TOKEN" --offer 10G >/dev/null
sleep 2

sample() { # label
  local t0 c0a c0b
  c0a=$(cpu_ticks "$PID_A"); c0b=$(cpu_ticks "$PID_B"); t0=$(now)
  sleep "$SECS"
  local dt; dt=$(elapsed "$t0")
  python3 - "$1" "$dt" "$c0a" "$(cpu_ticks "$PID_A")" "$c0b" "$(cpu_ticks "$PID_B")" \
    "$(rss_kb "$PID_A")" "$(hwm_kb "$PID_A")" "$(rss_kb "$PID_B")" "$(hwm_kb "$PID_B")" \
    "$(ls /proc/$PID_A/task | wc -l)" <<'PY'
import sys
l, dt, a0, a1, b0, b1, ra, ha, rb, hb, th = sys.argv[1:]
dt = float(dt)
print(f"{l}: A rss={int(ra)/1024:.1f}MiB (peak {int(ha)/1024:.1f}) cpu={(int(a1)-int(a0))/100/dt*100:.2f}% | "
      f"B rss={int(rb)/1024:.1f}MiB (peak {int(hb)/1024:.1f}) cpu={(int(b1)-int(b0))/100/dt*100:.2f}% | "
      f"threads(A)={th} window={dt:.0f}s")
PY
}

sample "idle, empty filesystem"
head -c $((DATA_MB * 1024 * 1024)) /dev/urandom > "$WORK/src.bin"
dd if="$WORK/src.bin" of="$WORK/A/mnt/big.bin" bs=1M 2>/dev/null
for i in $(seq 200); do
  [ "$(sf A status | grep -c 'not fully replicated')" = 0 ] && break; sleep 0.5
done
mkdir "$WORK/A/mnt/small"
for i in $(seq 2000); do echo "file $i" > "$WORK/A/mnt/small/f$i.txt"; done
sleep 3
sf A status | sed -n 1,6p
sample "idle after writing ${DATA_MB}MB + 2000 small files"
cat "$WORK/B/mnt/big.bin" > /dev/null
sample "idle after reading ${DATA_MB}MB on B"
