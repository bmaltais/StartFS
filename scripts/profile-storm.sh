#!/usr/bin/env bash
# The join storm (#74): what a new member does to the EXISTING nodes. Five loopback nodes like the
# live cluster (A, B, C offer 10 GiB; D, E offer 100 GiB, like ocilab1 and mini1) take a copied tree
# of about 22,000 objects, then node J joins with --offer JOFFER. Every existing node's `activity`
# and `status` are sampled every 2 s into a CSV: inbox entries, busy %, longest tick, replicate
# phase ms, replication queue, data verdict. Prints the peak of each column at the end.
# CAPBIG=1 caps D and E to CAPCPU % CPU (default 100) with systemd-run (needs sudo -n); CAP=1 caps J
# like the live vps2 (2 vCPU, 4 GiB, 500 IOPS, 60 MB/s).
# Environment: FILES=21900 BIG=50 SEED=74, JOFFER=10G, WATCH=300 (max seconds after the join),
# CSV=path (default $WORK/storm.csv, kept next to KEEP_WORK=1), BIN=path/to/startfs.
# Usage: profile-storm.sh   (needs /dev/fuse, fusermount3)
set -euo pipefail
given=${BIN:-}
source "$(dirname "$0")/lib.sh"
FILES=${FILES:-21900}; BIG=${BIG:-50}; SEED=${SEED:-74}; JOFFER=${JOFFER:-10G}; WATCH=${WATCH:-300}
CAPCPU=${CAPCPU:-100}
DEV=${DEV:-$(findmnt -no SOURCE -T "$HOME/.cache")}
[ -n "$given" ] || build
setup_env
T0=$(now)
say() { printf '[%5.0fs] %s\n' "$(python3 -c "print($(now) - $T0)")" "$*"; }
names="A B C D E"
base=$((20000 + RANDOM % 20000)); i=0
for n in $names J; do eval "PORT_$n=$((base + i))"; i=$((i + 1)); done
for n in $names J; do mkdir -p "$WORK/$n/mnt"; done
capscript() { # NAME CPU% [io caps]: a wrapper script (spaces would not survive word splitting)
  local extra=${3:-}
  cat > "$WORK/cap-$1" <<CAPPED
#!/bin/sh
exec sudo -n systemd-run --scope -q --uid=$(id -u) --gid=$(id -g) -p CPUQuota=$2% -p MemoryMax=4G \\
  -p MemorySwapMax=0 $extra \\
  env HOME=$HOME PATH=$PATH "\$@"
CAPPED
  chmod +x "$WORK/cap-$1"
}
if [ -n "${CAPBIG:-}" ]; then
  for n in D E; do capscript $n "$CAPCPU"; eval "WRAP_$n=$WORK/cap-$n"; done
fi
if [ -n "${CAP:-}" ]; then
  capscript J 200 "-p \"IOWriteIOPSMax=$DEV 500\" -p \"IOWriteBandwidthMax=$DEV 60M\""
  WRAP_J="$WORK/cap-J"
fi
say "starting 5 nodes"
start_node A "$WORK/A/mnt"
TOKEN=$(sf A create --name storm --copies 3)
sf A config --offer 10G >/dev/null
for n in B C; do start_node "$n" "$WORK/$n/mnt"; sf "$n" join "$TOKEN" --offer 10G >/dev/null; done
for n in D E; do start_node "$n" "$WORK/$n/mnt"; sf "$n" join "$TOKEN" --offer 100G >/dev/null; done
until sf A status | grep -q "(5 nodes)"; do sleep 0.2; done
say "making the tree"
python3 "$SF_ROOT/scripts/make-tree.py" "$WORK/src" --files "$FILES" --big "$BIG" --seed "$SEED" >/dev/null
want=$((FILES + BIG))
say "copying $want files into A"
cp -r "$WORK/src" "$WORK/A/mnt/copy"
for n in $names; do
  until [ "$(find "$WORK/$n/mnt/copy" -type f 2>/dev/null | wc -l)" -ge "$want" ]; do sleep 1; done
  until sf "$n" status | grep -q 'data: *SAFE'; do sleep 1; done
  say "$n has the tree and is SAFE"
done
for n in $names; do sf "$n" status | grep -E 'metadata log' | sed "s/^/$n /"; done
# let the inboxes drain and the busy average fall before the join
until [ "$(for n in $names; do sf "$n" activity | sed -n 's/.*busy \([0-9]*\)% of the time.*/\1/p'; done | sort -n | tail -1)" -lt 10 ]; do sleep 2; done
say "quiet; joiner starts with --offer $JOFFER"

CSV=${CSV:-$WORK/storm.csv}
echo "t,node,inbox,busy,longest_ms,replicate_ms,queue,data" > "$CSV"
# sample NODE: one CSV row from activity and status
sample() {
  local n=$1 act st
  act=$(timeout 10 "$BIN" --data-dir "$WORK/$n/data" activity 2>&1) || act=""
  st=$(timeout 10 "$BIN" --data-dir "$WORK/$n/data" status 2>&1 | sed -n 's/^data: *//p' | head -1) || st=""
  local inbox busy longest repl queue
  inbox=$(echo "$act" | awk '/inbox:/{print $2; exit}')
  busy=$(echo "$act" | sed -n 's/.*busy \([0-9]*\)% of the time.*/\1/p' | head -1)
  longest=$(echo "$act" | sed -n 's/.*longest \([0-9.]*\) ms.*/\1/p' | head -1)
  [ -n "$longest" ] || longest=$(echo "$act" | sed -n 's/.*longest \([0-9.]*\) s,.*/\1/p' | head -1 | awk '{print $1*1000}')
  repl=$(echo "$act" | sed -n 's/.*replicate \([0-9.]*\)[,)].*/\1/p' | head -1)
  queue=$(echo "$act" | sed -n 's/^replication queue: \([0-9]*\) chunks.*/\1/p' | head -1)
  echo "$2,$n,${inbox:-na},${busy:-na},${longest:-na},${repl:-na},${queue:-na},${st:-na}"
}
t0=$(now)
start_node J "$WORK/J/mnt"
sf J join "$(sf A token)" --offer "$JOFFER" >/dev/null
settled=0
while [ "$(python3 -c "print(int($(now) - $t0))")" -lt "$WATCH" ]; do
  t=$(python3 -c "print(int($(now) - $t0))")
  rows=$(for n in $names J; do sample "$n" "$t" & done; wait)
  echo "$rows" >> "$CSV"
  say "$(echo "$rows" | awk -F, '{printf "%s i=%s b=%s%% q=%s %s | ", $2,$3,$4,$7,$8}')"
  # settled: every node says SAFE with an empty inbox, three samples running
  if [ "$(echo "$rows" | awk -F, '$8=="SAFE" && $3==0' | wc -l)" -eq 6 ]; then
    settled=$((settled + 1)); [ "$settled" -ge 3 ] && break
  else settled=0; fi
  sleep 1
done
echo "seconds_to_settle=$(elapsed "$t0") csv=$CSV"
echo "peaks per node (inbox, busy %, longest tick ms, replicate ms, queue):"
for n in $names J; do
  awk -F, -v n="$n" '$2==n { for (c = 3; c <= 7; c++) if ($c != "na" && $c + 0 > m[c]) m[c] = $c + 0 }
    END { printf "  %s inbox %d busy %d longest %d replicate %.1f queue %d\n", n, m[3], m[4], m[5], m[6], m[7] }' "$CSV"
done
