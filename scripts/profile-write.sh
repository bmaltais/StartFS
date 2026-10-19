#!/usr/bin/env bash
# Write-path profile through a real FUSE mount: wall time and daemon CPU for a large file and for
# many files, against a plain directory on the same filesystem. One node, 1 copy.
# With `sudo -n strace` available (ptrace_scope blocks attaching without root), also prints how
# many syncs and directory scans the daemon made per file written.
# NODE_CONFIG adds lines to the daemon's config.toml (for example `activity = false`).
# Usage: profile-write.sh   (needs /dev/fuse, fusermount3, python3)
set -euo pipefail
source "$(dirname "$0")/lib.sh"
build; setup_env
mkdir -p "$WORK/A/mnt" "$WORK/base" "$WORK/src16k" "$WORK/src1m" "$WORK/src8b"
start_node A "$WORK/A/mnt" "${NODE_CONFIG:-}"
sf A create --name profile --copies 1 >/dev/null
M="$WORK/A/mnt"; B="$WORK/base"

head -c $((128 * 1024 * 1024)) /dev/urandom > "$WORK/big.bin"
for i in $(seq 1000); do head -c 16384 /dev/urandom > "$WORK/src16k/f$i"; echo "file $i" > "$WORK/src8b/f$i"; done
for i in $(seq 200); do head -c 1048576 /dev/urandom > "$WORK/src1m/f$i"; done

use_strace=
if command -v strace >/dev/null && sudo -n strace -V >/dev/null 2>&1; then use_strace=1; fi

# run LABEL ROOT COMMAND...: wall time, daemon CPU and, when tracing, sync and scan counts per file.
# FILES is the number of files the command writes.
run() {
  local label=$1 root=$2; shift 2
  local sp= t0 u0 s0 u1 s1 wall cpu=""
  read -r u0 s0 <<<"$(awk '{print $14, $15}' "/proc/$PID_A/stat")"
  if [ -n "$use_strace" ] && [ "$root" = "$M" ]; then
    sudo -n strace -f -c -o "$WORK/strace.out" -p "$PID_A" 2>/dev/null & sp=$!
    sleep 0.5
  fi
  t0=$(now); "$@" >/dev/null 2>&1; wall=$(elapsed "$t0")
  if [ -n "$sp" ]; then
    # Interrupt strace by PID; the [s] keeps the pattern from matching this shell.
    sudo -n kill -INT $(pgrep -f "[s]trace -f -c -o $WORK/strace.out") 2>/dev/null || true
    wait "$sp" 2>/dev/null || true
  fi
  read -r u1 s1 <<<"$(awk '{print $14, $15}' "/proc/$PID_A/stat")"
  if [ "$root" = "$M" ]; then
    cpu=$(python3 -c "print(f'  daemon user {($u1-$u0)/100:.2f}s sys {($s1-$s0)/100:.2f}s')")
  fi
  printf '%-26s %-5s wall %7ss%s\n' "$label" "$([ "$root" = "$M" ] && echo fuse || echo local)" "$wall" "$cpu"
  if [ -n "$sp" ] && [ -s "$WORK/strace.out" ]; then
    awk -v n="$FILES" '$NF ~ /^(fsync|fdatasync|getdents64|openat)$/ {printf "    %-12s %8d calls  %7.1f per file\n", $NF, $4, $4/n}' "$WORK/strace.out"
  fi
}

for root in "$B" "$M"; do
  d="$root/p"; mkdir -p "$d"
  FILES=1    run "128 MiB dd bs=1M"       "$root" dd if="$WORK/big.bin" of="$d/big" bs=1M
  FILES=1000 run "1000 x 8 B (inline)"    "$root" cp -r "$WORK/src8b" "$d/s8b"
  FILES=1000 run "1000 x 16 KiB (chunks)" "$root" cp -r "$WORK/src16k" "$d/s16k"
  FILES=200  run "200 x 1 MiB"            "$root" cp -r "$WORK/src1m" "$d/s1m"
  sync
done
