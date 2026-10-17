#!/usr/bin/env bash
# Criteria 2 and 3 with real processes: three daemons on loopback, 2 copies.
# Kill one node with SIGKILL for good; with no user action the data is
# re-replicated onto the survivors; then kill a second node and read from the last.
# Usage: validate-loss.sh [GRACE_SECS=3]
set -euo pipefail
source "$(dirname "$0")/lib.sh"
GRACE=${1:-3}
build; setup_env
mkdir -p "$WORK"/{A,B,C}/mnt
start_node A "$WORK/A/mnt" "repair_grace_secs = $GRACE"
TOKEN=$(sf A create --name loss --copies 2)
start_node B "$WORK/B/mnt"; sf B join "$TOKEN" --offer 1G >/dev/null
start_node C "$WORK/C/mnt"; sf C join "$TOKEN" --offer 1G >/dev/null

head -c $((20 * 1024 * 1024)) /dev/urandom > "$WORK/src.bin"
cp "$WORK/src.bin" "$WORK/A/mnt/data.bin"
want=$(sha256sum < "$WORK/src.bin")

wait_for() { # description, timeout secs, command...
  local d=$1 limit=$2; shift 2
  local t0; t0=$(now)
  until "$@" >/dev/null 2>&1; do
    [ "$(python3 -c "print($(now) - $t0 > $limit)")" = True ] && { echo "TIMEOUT waiting for: $d"; sf A status; exit 1; }
    sleep 0.5
  done
  echo "$d: $(elapsed "$t0")s"
}
replicated() { ! sf A status | grep -q 'not fully replicated'; }
healed() { sf A status | grep -qE '^  2 .* lost' && replicated && ! sf A status | grep -q '^repair:'; }

wait_for "initial replication complete" 120 replicated
echo "--- before loss ---"; sf A status | grep -E "redundancy"; echo "stored before: A: $(sf A status | grep ^usage) / B: $(sf B status | grep ^usage)"

echo "kill -9 node C (permanent)"
kill -9 "$PID_C"
T=$(now)
wait_for "C marked lost and data back to 2 copies on A+B (grace=${GRACE}s)" 180 healed
echo "--- after repair ---"; echo "stored after:  A: $(sf A status | grep ^usage) / B: $(sf B status | grep ^usage)"; sf A status | grep -E "redundancy|^  [0-9]"

echo "kill -9 node A too (N-1 = 1 node fails of the remaining 2); read from B"
kill -9 "$PID_A"
got=$(sha256sum < "$WORK/B/mnt/data.bin")
if [ "$got" = "$want" ]; then echo "read from B matches original: OK"; else echo "MISMATCH"; exit 1; fi
