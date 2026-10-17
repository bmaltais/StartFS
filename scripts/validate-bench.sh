#!/usr/bin/env bash
# Criterion 5: reads and writes through a real FUSE mount versus a plain local
# directory on the same filesystem as the daemon's data dir. One node, 1 copy.
# Usage: validate-bench.sh [SIZES_MB="1 4 16 32"]
set -euo pipefail
source "$(dirname "$0")/lib.sh"
SIZES=${1:-"1 4 16 32"}
build; setup_env
mkdir -p "$WORK/A/mnt" "$WORK/base"
start_node A "$WORK/A/mnt"
sf A create --name bench --copies 1 >/dev/null
M="$WORK/A/mnt"; B="$WORK/base"

# secs CMD...: wall time of a command, in seconds.
secs() { local t0; t0=$(now); "$@" >/dev/null 2>&1; elapsed "$t0"; }
wr() { dd if="$WORK/src.bin" of="$1" bs=1M count="$2" 2>/dev/null; }
rd() { dd if="$1" of=/dev/null bs=1M 2>/dev/null; }
rd_direct() { dd if="$1" of=/dev/null bs=1M iflag=direct 2>/dev/null; }
ratio() { python3 -c "print(f'{$1 / max($2, 1e-9):.1f}x')"; }

head -c $((256 * 1024 * 1024)) /dev/urandom > "$WORK/src.bin"

printf '%-8s %-30s %9s %9s %8s\n' size op local fuse slowdown
for mb in $SIZES; do
  lw=$(secs wr "$B/f$mb" "$mb"); fw=$(secs wr "$M/f$mb" "$mb")
  printf '%-8s %-30s %8ss %8ss %8s\n' "${mb}MiB" "write (dd bs=1M)" "$lw" "$fw" "$(ratio "$fw" "$lw")"
  lr=$(secs rd "$B/f$mb"); fr=$(secs rd "$M/f$mb")
  printf '%-8s %-30s %8ss %8ss %8s\n' "${mb}MiB" "read, warm page cache" "$lr" "$fr" "$(ratio "$fr" "$lr")"
  lr=$(secs rd_direct "$B/f$mb"); fr=$(secs rd_direct "$M/f$mb" || true)
  printf '%-8s %-30s %8ss %8ss %8s\n' "${mb}MiB" "read, O_DIRECT" "$lr" "$fr" "$(ratio "$fr" "$lr")"
done

# Many small files (inline in metadata, no chunk).
small() { for i in $(seq 1000); do echo "file $i" > "$1/s$i"; done; }
mkdir -p "$M/small" "$B/small"
lw=$(secs small "$B/small"); fw=$(secs small "$M/small")
printf '%-8s %-30s %8ss %8ss %8s\n' "1000x8B" "create+write" "$lw" "$fw" "$(ratio "$fw" "$lw")"
sr() { cat "$1"/s* ; }
lr=$(secs sr "$B/small"); fr=$(secs sr "$M/small")
printf '%-8s %-30s %8ss %8ss %8s\n' "1000x8B" "read" "$lr" "$fr" "$(ratio "$fr" "$lr")"

echo
echo "daemon peak RSS after bench: $(( $(hwm_kb "$PID_A") / 1024 )) MiB"
