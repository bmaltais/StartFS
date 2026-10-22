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

# run LABEL PAYLOAD_BYTES COMMAND...: writes during the command and the flush after it.
run() {
  local label=$1 payload=$2; shift 2
  sync; sleep 2
  local w0 d0; w0=$(wb); d0=$(disk)
  "$@" >/dev/null 2>&1
  sync; sleep 3; sync
  python3 - "$label" "$payload" "$w0" "$(wb)" "$d0" "$(disk)" <<'PY'
import sys
l, p, w0, w1, d0, d1 = sys.argv[1], *map(int, sys.argv[2:])
mib = 2**20
ratio = lambda n: f"{n / p:7.2f}x" if p > 1000 else f"{n / mib:7.1f} MiB"
print(f"{l:30s} daemon {(w1 - w0) / mib:7.1f} MiB {ratio(w1 - w0)}  disk {(d1 - d0) / mib:7.1f} MiB {ratio(d1 - d0)}")
PY
}

run "1000 x 16 KiB copy"       $((1000 * 16384))   cp -r "$WORK/src16k" "$M/s16k"
run "100 x 1 MiB copy"         $((100 * 1048576))  cp -r "$WORK/src1m" "$M/s1m"
run "patch 50 files by 14 B"   1 bash -c 'for i in $(seq 50); do printf "patched-header" | dd of="'"$M"'/s1m/f$i" bs=1 conv=notrunc status=none; done'
run "delete 1100 files"        1 rm -rf "$M/s16k" "$M/s1m"
