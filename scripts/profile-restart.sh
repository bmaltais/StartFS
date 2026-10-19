#!/usr/bin/env bash
# What a restart costs a node that holds a tree (#82, #85): three loopback nodes (3 copies) take a
# copied tree, then node B is stopped and started again. Prints B's write syscalls, CPU and
# seconds until `data: SAFE` again, and with TRACE=1 B's pwrite64/fdatasync calls by file (needs
# sudo for strace).
# LAG=1 takes B down for the second half of the copy instead, so it catches up after.
# Environment: FILES=1500 BIG=50 SEED=85, TRACE=1, BIN=path/to/startfs, WATCH=90 (max seconds).
# For a disk like the live VPSes (Azure Standard HDD: 500 IOPS, 60 MB/s) run the whole script under
#   sudo systemd-run --scope --uid=$UID --gid=$(id -g) -p "IOWriteIOPSMax=/dev/mapper/DEV 500" -p "IOWriteBandwidthMax=/dev/mapper/DEV 60M" \
#     env HOME=$HOME PATH=$PATH scripts/profile-restart.sh
# Usage: profile-restart.sh   (needs /dev/fuse, fusermount3)
set -euo pipefail
given=${BIN:-}
source "$(dirname "$0")/lib.sh"
FILES=${FILES:-1500}; BIG=${BIG:-50}; SEED=${SEED:-85}; WATCH=${WATCH:-90}
[ -n "$given" ] || build
setup_env
PORT_A=$((20000 + RANDOM % 20000)); PORT_B=$((PORT_A + 1)); PORT_C=$((PORT_A + 2))
mkdir -p "$WORK/A/mnt" "$WORK/B/mnt" "$WORK/C/mnt"
start_node A "$WORK/A/mnt"
TOKEN=$(sf A create --name restart --copies 3)
start_node B "$WORK/B/mnt"; sf B join "$TOKEN" --offer 5G >/dev/null
start_node C "$WORK/C/mnt"; sf C join "$TOKEN" --offer 5G >/dev/null
until sf A status | grep -q '(3 nodes)'; do sleep 0.2; done
python3 "$SF_ROOT/scripts/make-tree.py" "$WORK/src" --files "$FILES" --big "$BIG" --seed "$SEED" >/dev/null
want=$((FILES + BIG))
stop_b() {
  kill "$PID_B"; wait "$PID_B" 2>/dev/null || true
  fusermount3 -uz "$WORK/B/mnt" 2>/dev/null || true
}
if [ -n "${LAG:-}" ]; then
  # B is down for the second half of the copy and catches up after.
  cp -r "$WORK/src" "$WORK/A/mnt/copy" &
  copier=$!
  until [ "$(find "$WORK/A/mnt/copy" -type f 2>/dev/null | wc -l)" -ge $((want / 2)) ]; do sleep 0.2; done
  stop_b
  wait "$copier"
else
  cp -r "$WORK/src" "$WORK/A/mnt/copy"
fi
for n in A C; do
  until [ "$(find "$WORK/$n/mnt/copy" -type f 2>/dev/null | wc -l)" -ge "$want" ]; do sleep 0.5; done
done
if [ -z "${LAG:-}" ]; then
  until [ "$(find "$WORK/B/mnt/copy" -type f 2>/dev/null | wc -l)" -ge "$want" ]; do sleep 0.5; done
  until sf B status | grep -q 'data: *SAFE'; do sleep 1; done
  stop_b
fi
t0=$(now)
start_node B "$WORK/B/mnt"
# Write syscalls of the daemon's own threads: the FUSE threads answer every read of the mount,
# including this script's polling.
syscw() {
  for t in /proc/"$1"/task/*; do
    case $(cat "$t/comm") in fuser*) ;; *) awk '/^syscw/{print $2}' "$t/io";; esac
  done | awk '{s+=$1} END{print s}'
}
if [ -n "${TRACE:-}" ]; then
  sudo -n strace -f -y -e trace=write,pwrite64,pwritev,sendto,sendmsg,fsync,fdatasync -o "$WORK/strace.out" -p "$PID_B" 2>/dev/null & TP=$!
fi
until { [ "$(find "$WORK/B/mnt/copy" -type f 2>/dev/null | wc -l)" -ge "$want" ] && sf B status 2>/dev/null | grep -q 'data: *SAFE'; } || [ "$(python3 -c "print(int($(now) - $t0))")" -ge "$WATCH" ]; do sleep 1; done
echo "seconds_to_caught_up_and_safe=$(elapsed "$t0") write_syscalls=$(syscw "$PID_B") cpu_s=$(( $(cpu_ticks "$PID_B") / 100 ))"
sf B status | grep -E 'data:|repair|log' || true
echo "files on B: $(find "$WORK/B/mnt/copy" -type f 2>/dev/null | wc -l) of $want"
for t in /proc/"$PID_B"/task/*; do
  echo "$(awk '/^syscw/{print $2}' "$t/io") $(cat "$t/comm")"
done | sort -rn | head -5
sf B activity 2>&1 | head -20
if [ -n "${TRACE:-}" ]; then
  sudo -n pkill strace || true; wait "$TP" 2>/dev/null || true
  grep -oE "(write|pwrite64|pwritev|sendto|sendmsg|fsync|fdatasync)\([0-9]+<[^>]*>" "$WORK/strace.out" \
    | sed -E 's#/home/[^>]*/(B/data/)##; s#[0-9]+<#<#' | sort | uniq -c | sort -rn | head
fi
