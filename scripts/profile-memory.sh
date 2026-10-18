#!/usr/bin/env bash
# Memory of three loopback daemons (3 copies) through a burst of writes: peak and settled RSS
# of the writer and the two receivers (#65). Prints a CSV: seconds, phase, RSS in MiB per node,
# the writer's peak RSS and its staging.
#   MODE=burst   write a mixed tree through A's mount, then watch (default)
#   MODE=stall   freeze C (SIGSTOP) before the write and resume it at the end, so the others
#                push to a peer that does not answer, then C catches up
# Environment: FILES=4000 (8 KiB to 72 KiB each), BIG=200 (1 MiB each), WATCH=90 (samples, 2 s
# apart); malloc settings pass through to the daemons, for example
#   MALLOC_ARENA_MAX=1 MALLOC_TRIM_THRESHOLD_=131072 scripts/profile-memory.sh
# Usage: profile-memory.sh   (needs /dev/fuse, fusermount3)
set -euo pipefail
source "$(dirname "$0")/lib.sh"
MODE=${MODE:-burst}
FILES=${FILES:-4000}; BIG=${BIG:-200}; WATCH=${WATCH:-90}
build; setup_env
mkdir -p "$WORK/A/mnt" "$WORK/B/mnt" "$WORK/C/mnt" "$WORK/src"
start_node A "$WORK/A/mnt"
TOKEN=$(sf A create --name memory --copies 3)
start_node B "$WORK/B/mnt"; sf B join "$TOKEN" --offer 5G >/dev/null
start_node C "$WORK/C/mnt"; sf C join "$TOKEN" --offer 5G >/dev/null
until sf A status | grep -q '(3 nodes)'; do sleep 0.2; done
for i in $(seq "$FILES"); do head -c $((8192 + RANDOM * 2)) /dev/urandom > "$WORK/src/f$i"; done
for i in $(seq "$BIG"); do head -c 1048576 /dev/urandom > "$WORK/src/b$i"; done

echo "seconds,phase,rssA,rssB,rssC,peakA,staging_on_A"
t0=$(date +%s)
sample() {
  echo "$(( $(date +%s) - t0 )),$1,$(( $(rss_kb "$PID_A") / 1024 )),$(( $(rss_kb "$PID_B") / 1024 )),$(( $(rss_kb "$PID_C") / 1024 )),$(( $(hwm_kb "$PID_A") / 1024 )),$(sf A status | awk '/^staging/{print $2 $3}')"
}
sample idle
[ "$MODE" != stall ] || kill -STOP "$PID_C"
cp -r "$WORK/src" "$WORK/A/mnt/copy" &
copier=$!
while kill -0 "$copier" 2>/dev/null; do sample write; sleep 2; done
for _ in $(seq "$WATCH"); do sample after; sleep 2; done
if [ "$MODE" = stall ]; then
  kill -CONT "$PID_C"
  for _ in $(seq 45); do sample resumed; sleep 2; done
fi
