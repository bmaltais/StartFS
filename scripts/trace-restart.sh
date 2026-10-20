#!/usr/bin/env bash
# What `status` says while a node restarts (#132): three loopback nodes, B is killed and started
# again, and every change in B's verdict or in how it sees A and C is printed with the seconds
# since the restart, e.g. `4.8 Safe [Online, Online]`. Exits non-zero if B ever reads Degraded.
# The harness has no QUIC links, so only this shows what a restart looks like on a real transport.
# Environment: WATCH=40 (seconds to watch), BIN=path/to/startfs.
# Usage: trace-restart.sh   (needs /dev/fuse, fusermount3)
set -euo pipefail
given=${BIN:-}
source "$(dirname "$0")/lib.sh"
WATCH=${WATCH:-40}
[ -n "$given" ] || build
setup_env
PORT_A=$((20000 + RANDOM % 20000)); PORT_B=$((PORT_A + 1)); PORT_C=$((PORT_A + 2))
mkdir -p "$WORK/A/mnt" "$WORK/B/mnt" "$WORK/C/mnt"
start_node A "$WORK/A/mnt"
TOKEN=$(sf A create --name restart --copies 3)
start_node B "$WORK/B/mnt"; sf B join "$TOKEN" --offer 5G >/dev/null
start_node C "$WORK/C/mnt"; sf C join "$TOKEN" --offer 5G >/dev/null
until sf A status | grep -q '(3 nodes)'; do sleep 0.2; done
# Long enough for A and C to hold a link to B.
sleep 15
kill "$PID_B"; wait "$PID_B" 2>/dev/null || true
fusermount3 -uz "$WORK/B/mnt" 2>/dev/null || true
t0=$(now)
start_node B "$WORK/B/mnt"
prev=""; degraded=0
while [ "$(python3 -c "print(int($(now) - $t0))")" -lt "$WATCH" ]; do
  cur=$(sf B status --json 2>/dev/null | python3 -c '
import json, sys
s = json.load(sys.stdin)
print(s["verdict"], [m["state"] for m in s["members"] if not m["is_self"]])' 2>&1 | tail -1)
  if [ "$cur" != "$prev" ]; then echo "$(elapsed "$t0") $cur"; prev=$cur; fi
  case $cur in Degraded*) degraded=1;; esac
  sleep 0.5
done
exit "$degraded"
