#!/usr/bin/env bash
# What applying a burst of a peer's metadata costs the receivers (#85): write syscalls (syscw)
# and CPU of the two receiving daemons from the start of the copy until both mounts list every
# file, on three loopback nodes (3 copies). Prints one line.
# TRACE=1 also counts the receiver B's pwrite64 calls to meta.redb (needs sudo for strace), and
# the line ends with B's tick and inbox from `startfs activity`.
# Environment: FILES=4000 BIG=200 (see make-tree.py), SEED=85, BIN=path/to/startfs to compare
# builds (default: target/release/startfs, built here).
# Usage: profile-apply.sh   (needs /dev/fuse, fusermount3)
set -euo pipefail
given=${BIN:-}
source "$(dirname "$0")/lib.sh"
FILES=${FILES:-4000}; BIG=${BIG:-200}; SEED=${SEED:-85}
[ -n "$given" ] || build
setup_env
mkdir -p "$WORK/A/mnt" "$WORK/B/mnt" "$WORK/C/mnt"
start_node A "$WORK/A/mnt"
TOKEN=$(sf A create --name apply --copies 3)
start_node B "$WORK/B/mnt"; sf B join "$TOKEN" --offer 5G >/dev/null
start_node C "$WORK/C/mnt"; sf C join "$TOKEN" --offer 5G >/dev/null
until sf A status | grep -q '(3 nodes)'; do sleep 0.2; done
python3 "$SF_ROOT/scripts/make-tree.py" "$WORK/src" --files "$FILES" --big "$BIG" --seed "$SEED" >/dev/null
want=$((FILES + BIG))
syscw() { awk '/^syscw/{print $2}' "/proc/$1/io"; }
w0=$(( $(syscw "$PID_B") + $(syscw "$PID_C") ))
c0=$(( $(cpu_ticks "$PID_B") + $(cpu_ticks "$PID_C") ))
if [ -n "${TRACE:-}" ]; then
  sudo -n strace -f -y -e trace=pwrite64 -o "$WORK/strace.out" -p "$PID_B" 2>/dev/null & TP=$!
  sleep 1
fi
t0=$(now)
cp -r "$WORK/src" "$WORK/A/mnt/copy"
for n in B C; do
  until [ "$(find "$WORK/$n/mnt/copy" -type f 2>/dev/null | wc -l)" -ge "$want" ]; do sleep 0.5; done
done
echo "files=$want seconds=$(elapsed "$t0") receiver_write_syscalls=$(( $(syscw "$PID_B") + $(syscw "$PID_C") - w0 )) receiver_cpu_s=$(( ($(cpu_ticks "$PID_B") + $(cpu_ticks "$PID_C") - c0) / 100 ))"
if [ -n "${TRACE:-}" ]; then
  sudo -n pkill strace || true; wait "$TP" 2>/dev/null || true
  echo "meta_redb_pwrite64=$(grep -c 'pwrite64(.*meta.redb' "$WORK/strace.out" || true)"
fi
sf B activity | grep -E '^tick:|inbox:'
