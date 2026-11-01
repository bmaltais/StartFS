#!/usr/bin/env bash
# What a burst of small chunks costs the member receiving it (#166): two loopback nodes with two
# copies and no replication hold, a copy of FILES files of KB KiB (half incompressible, half text,
# like build output), then the time until every chunk is on both, and B's `activity`: its tick
# and receive phase, the sends A saw time out, and the CPU of its tick thread (10 ms ticks).
# SLOW_FSYNC_US=2000 adds that much latency to every fsync and fdatasync on B (a disk where a
# flush is slow, like a small VPS), with `sudo -n strace` fault injection; without it B's disk
# is whatever this machine has.
# PRE=30000 first fills the filesystem with that many tiny files (and waits for them to replicate),
# so the burst lands on a member that holds a big tree, like the live one (about 26,000 objects).
# Environment: FILES=4000, KB=64, PRE=0, SLOW_FSYNC_US, BIN=path/to/startfs (skips the build).
# Usage: profile-burst.sh   (needs /dev/fuse, fusermount3; SLOW_FSYNC_US needs strace and sudo)
set -euo pipefail
given=${BIN:-}
source "$(dirname "$0")/lib.sh"
FILES=${FILES:-4000}; KB=${KB:-64}; SLOW_FSYNC_US=${SLOW_FSYNC_US:-}; PRE=${PRE:-0}
[ -n "$given" ] || build
setup_env
mkdir -p "$WORK"/{A,B}/mnt "$WORK/src"
start_node A "$WORK/A/mnt" "replication_hold_secs = 0"
TOKEN=$(sf A create --name burst --copies 2)
sf A config --offer 20G >/dev/null
start_node B "$WORK/B/mnt" "replication_hold_secs = 0"; sf B join "$TOKEN" --offer 20G >/dev/null
until sf A status | grep -q "(2 nodes)"; do sleep 0.2; done
for i in $(seq "$FILES"); do
  if [ $((i % 2)) = 0 ]; then head -c $((KB * 1024)) /dev/urandom > "$WORK/src/f$i"
  else python3 -c "import sys; n = $KB * 1024; sys.stdout.write((('line $i of some object file text\\n') * n)[:n])" > "$WORK/src/f$i"; fi
done
if [ "$PRE" -gt 0 ]; then
  mkdir -p "$WORK/pre"
  for d in $(seq $((PRE / 1000))); do
    mkdir -p "$WORK/A/mnt/pre/d$d"
    for i in $(seq 1000); do echo "file $d $i" > "$WORK/A/mnt/pre/d$d/f$i"; done
  done
  sync -f "$WORK/A/mnt/pre"
  until ! sf A status | grep -q 'not fully replicated'; do sleep 0.2; done
fi
tick_cpu() { # PID: user and system ticks of the tick thread
  local th
  for th in /proc/"$1"/task/*; do
    awk '{gsub(/[()]/, "", $2); if ($2 == "startfs-tick") print $14, $15}' "$th/stat"
  done
}
slow=
if [ -n "$SLOW_FSYNC_US" ]; then
  sudo -n strace -f --seccomp-bpf -e trace=fsync,fdatasync \
    -e "inject=fsync,fdatasync:delay_enter=$SLOW_FSYNC_US" -o /dev/null -p "$PID_B" 2>/dev/null & slow=$!
  sleep 1
fi
read -r cpu0_u cpu0_s <<<"$(tick_cpu "$PID_B")"
t0=$(now)
cp -r "$WORK/src" "$WORK/A/mnt/s"; sync -f "$WORK/A/mnt/s"
tw=$(elapsed "$t0")
until ! sf A status | grep -q 'not fully replicated'; do sleep 0.05; done
t=$(elapsed "$t0")
if [ -n "$slow" ]; then
  # Interrupt strace by PID; the [s] keeps the pattern from matching this shell.
  sudo -n kill -INT "$(pgrep -f "[s]trace -f --seccomp-bpf")" 2>/dev/null || true
  wait "$slow" 2>/dev/null || true
fi
echo "burst: $FILES x $KB KiB, fsync +${SLOW_FSYNC_US:-0} us: write ${tw}s, replicated ${t}s"
echo "A sends: $(sf A activity | grep -c 'sends:') peers, $(sf A activity | awk '/sends:/ {a += $2; t += $4; r += $7} END {print a " acknowledged, " t " timed out, " r " retried"}')"
sf B activity | grep -E "^tick:|phases"
tick_cpu "$PID_B" | awk -v u0="$cpu0_u" -v s0="$cpu0_s" '{print "B tick thread during the burst: user " $1 - u0 " system " $2 - s0 " (10 ms ticks)"}'
