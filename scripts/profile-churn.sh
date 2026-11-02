#!/usr/bin/env bash
# What short-lived files cost the other members: three loopback nodes (3 copies); on A, for each
# lifetime in LIFETIMES (seconds), COUNT files of 1 MiB are written, kept that long, then deleted.
# Per lifetime: bytes the other two daemons wrote (/proc/PID/io write_bytes) and the replica bytes
# they keep after (deleted data stays until GC retention), against the payload. NODE_CONFIG adds
# config lines to every node (e.g. "replication_hold_secs = 30").
# Environment: LIFETIMES="1 5 15 45" COUNT=50 BIN=path/to/startfs
# Usage: profile-churn.sh   (needs /dev/fuse, fusermount3, python3)
set -euo pipefail
given=${BIN:-}
source "$(dirname "$0")/lib.sh"
LIFETIMES=${LIFETIMES:-1 5 15 45}; COUNT=${COUNT:-50}
[ -n "$given" ] || build
setup_env
for n in A B C; do mkdir -p "$WORK/$n/mnt"; done
start_node A "$WORK/A/mnt" "${NODE_CONFIG:-}"
TOKEN=$(sf A create --name churn --copies 3)
start_node B "" "${NODE_CONFIG:-}"; sf B join "$TOKEN" --offer 5G >/dev/null
start_node C "" "${NODE_CONFIG:-}"; sf C join "$TOKEN" --offer 5G >/dev/null
until sf A status | grep -q '3 nodes,'; do sleep 0.2; done
M="$WORK/A/mnt"
# Fresh bytes for every round: the peers keep deleted chunks for the retention, so reused
# content would be deduplicated and cost nothing.
fresh() {
  rm -rf "$WORK/src"; mkdir -p "$WORK/src"
  for i in $(seq "$COUNT"); do head -c 1048576 /dev/urandom > "$WORK/src/f$i"; done
}

wb() { awk '/^write_bytes/ {print $2}' "/proc/$1/io"; }
stored() { sf "$1" status --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["usage"]["stored_bytes"])'; }
# Until A has nothing staged or queued: what was going to be sent has been.
quiet() {
  until [ "$(sf A status --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["staging"]["chunks"])')" = 0 ] &&
    [ "$(sf A activity --json | python3 -c 'import json, sys; print(json.load(sys.stdin)["queue"]["chunks"])')" = 0 ]; do
    sleep 1
  done
}

payload=$((COUNT * 1048576))
echo "lifetime_s,payload_MiB,peers_wrote_MiB,peers_kept_MiB"
for life in $LIFETIMES; do
  fresh; quiet
  w0=$(( $(wb "$PID_B") + $(wb "$PID_C") )); s0=$(( $(stored B) + $(stored C) ))
  cp -r "$WORK/src" "$M/t$life"
  sleep "$life"
  rm -rf "${M:?}/t$life"
  sleep 5
  quiet
  w1=$(( $(wb "$PID_B") + $(wb "$PID_C") )); s1=$(( $(stored B) + $(stored C) ))
  python3 -c "m = 2**20; print(f'$life,{$payload / m:.0f},{($w1 - $w0) / m:.1f},{($s1 - $s0) / m:.1f}')"
done
