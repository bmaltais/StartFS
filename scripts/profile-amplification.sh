#!/usr/bin/env bash
# Write amplification through a real FUSE mount on one isolated node (1 copy): the bytes written
# to storage per byte of payload for copies, a patch and a delete. Two figures per workload: what
# the daemon says it wrote (/proc/PID/io write_bytes) and what reached the disk under its data
# directory (the device's sectors written, so other writers on the machine add noise; keep it idle).
# Usage: profile-amplification.sh   (needs /dev/fuse, fusermount3, python3)
set -euo pipefail
source "$(dirname "$0")/lib.sh"
build; setup_env
mkdir -p "$WORK/A/mnt" "$WORK/src16k" "$WORK/src1m"
start_node A "$WORK/A/mnt" "${NODE_CONFIG:-}"
sf A create --name amp --copies 1 >/dev/null
M="$WORK/A/mnt"
for i in $(seq 1000); do head -c 16384 /dev/urandom > "$WORK/src16k/f$i"; done
for i in $(seq 100); do head -c 1048576 /dev/urandom > "$WORK/src1m/f$i"; done

dev=$(basename "$(readlink -f "$(findmnt -n -T "$WORK" -o SOURCE | sed 's/\[.*//')")")
wb() { awk '/^write_bytes/ {print $2}' "/proc/$PID_A/io"; }
disk() { awk -v d="$dev" '$3 == d {print $10 * 512}' /proc/diskstats; }

# run LABEL PAYLOAD_BYTES MAX COMMAND...: writes during the command and the flush after it.
# A MAX above 0 makes a daemon figure above MAX times the payload fail the script.
FAILED=0
run() {
  local label=$1 payload=$2 max=$3; shift 3
  sync; sleep 2
  local w0 d0; w0=$(wb); d0=$(disk)
  "$@" >/dev/null 2>&1
  sync; sleep 3; sync
  python3 - "$label" "$payload" "$w0" "$(wb)" "$d0" "$(disk)" "$max" <<'PY' || FAILED=1
import sys
l, p, w0, w1, d0, d1, mx = sys.argv[1], *map(int, sys.argv[2:])
mib = 2**20
ratio = lambda n: f"{n / p:7.2f}x" if p > 1000 else f"{n / mib:7.1f} MiB"
print(f"{l:30s} daemon {(w1 - w0) / mib:7.1f} MiB {ratio(w1 - w0)}  disk {(d1 - d0) / mib:7.1f} MiB {ratio(d1 - d0)}")
if mx and (w1 - w0) > mx * p:
    print(f"  FAIL: {(w1 - w0) / p:.2f}x is above the {mx}x bound")
    sys.exit(1)
PY
}

run "1000 x 16 KiB copy"       $((1000 * 16384)) 0 cp -r "$WORK/src16k" "$M/s16k"
run "100 x 1 MiB copy"         $((100 * 1048576)) 0 cp -r "$WORK/src1m" "$M/s1m"
# shellcheck disable=SC2016 # $(seq 50) and $i are for the inner bash
run "patch 50 files by 14 B"   1 0 bash -c 'for i in $(seq 50); do printf "patched-header" | dd of="'"$M"'/s1m/f$i" bs=1 conv=notrunc status=none; done'
run "delete 1100 files"        1 0 rm -rf "$M/s16k" "$M/s1m"

# A build, in steps. Payload is the bytes each step leaves in the tree (1 for a step that leaves none).
# The steps with a MAX are bounded: the daemon writes at most MAX times the payload, or the script fails.
OBJS=300
obj_size() { echo $((20000 + $1 * 300)); }
# objs DIR: writes (or, run again, replaces) the objects; a second run is the relink.
# shellcheck disable=SC2329 # `run` calls it
objs() { for i in $(seq "$OBJS"); do head -c "$(obj_size "$i")" /dev/urandom > "$1/o$i"; done; }
# shellcheck disable=SC2329 # `run` calls it
temps() { for i in $(seq 200); do head -c 65536 /dev/urandom > "$1/t$i"; done; rm -f "$1"/t*; }
# shellcheck disable=SC2329 # `run` calls it
archive() { for i in $(seq 400); do head -c 4096 /dev/urandom >> "$1"; done; }
objbytes=0; for i in $(seq "$OBJS"); do objbytes=$((objbytes + $(obj_size "$i"))); done
mkdir -p "$M/build/obj" "$M/build/tmp"
run "build: write 300 objects" "$objbytes" 2 objs "$M/build/obj"
run "build: rewrite them (relink)" "$objbytes" 2 objs "$M/build/obj"
run "build: 200 temps, removed" 1 0 temps "$M/build/tmp"
run "build: append 400 x 4 KiB" $((400 * 4096)) 3 archive "$M/build/lib.a"
exit "$FAILED"
