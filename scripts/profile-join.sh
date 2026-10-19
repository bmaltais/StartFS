#!/usr/bin/env bash
# What a new empty member costs the cluster and itself (#91, #78): four loopback nodes (10 GiB
# offers, like the live desktops) take a copied tree, then node J joins with a 100 GiB offer (like
# the weak VPSes, which offer-weighted placement gives the most data). Prints a CSV of every node's
# RSS while J fills, then J's peak RSS, how long J took to settle and its `activity`.
# J is capped like the live vps2 with CAP=1: 2 vCPU, 4 GiB, 500 IOPS and 60 MB/s (needs sudo -n).
# Environment: FILES=2000 BIG=50 SEED=91, NODES=4 (existing members), WATCH=180 (max seconds after
# the join), CAP=1, DEV=/dev/mapper/root2 (the device holding ~/.cache), BIN=path/to/startfs,
# HOLD=n keeps the nodes running n seconds at the end (to attach gdb), malloc settings pass through to the daemons.
# Usage: profile-join.sh   (needs /dev/fuse, fusermount3)
set -euo pipefail
given=${BIN:-}
source "$(dirname "$0")/lib.sh"
FILES=${FILES:-2000}; BIG=${BIG:-50}; SEED=${SEED:-91}; NODES=${NODES:-4}; WATCH=${WATCH:-180}
DEV=${DEV:-$(findmnt -no SOURCE -T "$HOME/.cache")}
[ -n "$given" ] || build
setup_env
T0=$(now)
say() { printf '[%5.0fs] %s\n' "$(python3 -c "print($(now) - $T0)")" "$*"; }
names=$(printf '%s ' $(echo A B C D E F G H | cut -d' ' -f1-"$NODES"))
base=$((20000 + RANDOM % 20000)); i=0
for n in $names J; do eval "PORT_$n=$((base + i))"; i=$((i + 1)); done
for n in $names J; do mkdir -p "$WORK/$n/mnt"; done
if [ -n "${CAP:-}" ]; then
  # one script, so the property values keep their spaces when start_node word-splits WRAP_J
  cat > "$WORK/capped" <<CAPPED
#!/bin/sh
exec sudo -n systemd-run --scope -q --uid=$(id -u) --gid=$(id -g) -p CPUQuota=200% -p MemoryMax=4G \\
  -p MemorySwapMax=0 -p "IOWriteIOPSMax=$DEV 500" -p "IOWriteBandwidthMax=$DEV 60M" \\
  env HOME=$HOME PATH=$PATH "\$@"
CAPPED
  chmod +x "$WORK/capped"
  WRAP_J="$WORK/capped"
fi
say "starting $NODES nodes"
start_node A "$WORK/A/mnt"
TOKEN=$(sf A create --name join --copies 3)
sf A config --offer 10G >/dev/null
for n in ${names#A }; do
  start_node "$n" "$WORK/$n/mnt"; sf "$n" join "$TOKEN" --offer 10G >/dev/null
done
until sf A status | grep -q "($NODES nodes)"; do sleep 0.2; done
say "making the tree"
python3 "$SF_ROOT/scripts/make-tree.py" "$WORK/src" --files "$FILES" --big "$BIG" --seed "$SEED" >/dev/null
want=$((FILES + BIG))
say "copying $want files into A"
cp -r "$WORK/src" "$WORK/A/mnt/copy"
say "copied; waiting for replication to $NODES nodes"
for n in $names; do
  until [ "$(find "$WORK/$n/mnt/copy" -type f 2>/dev/null | wc -l)" -ge "$want" ]; do sleep 0.5; done
  until sf "$n" status | grep -q 'data: *SAFE'; do sleep 1; done
  say "$n has the tree and is SAFE"
done
say "tree stored; joiner starts (WATCH=$WATCH s max)"

line() { local out="$1"; for n in $names J; do eval "out=\$out,\$(( \$(rss_kb \$PID_$n) / 1024 ))"; done; echo "$out"; }
echo "seconds,$(echo $names J | tr ' ' ',')"
t0=$(now)
start_node J "$WORK/J/mnt"
sf J join "$(sf A token)" --offer 100G >/dev/null
settled=0
while [ "$(python3 -c "print(int($(now) - $t0))")" -lt "$WATCH" ]; do
  say "joining: $(sf J status 2>/dev/null | grep -E "data:" | tr -s " ")"
  line "$(python3 -c "print(int($(now) - $t0))")"
  # settled: J holds data and every node says SAFE, three samples running
  if sf J status 2>/dev/null | grep -q 'data: *SAFE' && [ "$(sf J stats 2>/dev/null | grep -c .)" -gt 0 ]; then
    settled=$((settled + 1)); [ "$settled" -ge 3 ] && break
  else settled=0; fi
  sleep 2
done
echo "seconds_to_safe=$(elapsed "$t0") peak_mib=$(( $(hwm_kb "$PID_J") / 1024 )) cpu_s=$(( $(cpu_ticks "$PID_J") / 100 ))"
for n in $names J; do eval "echo peak \$n \$(( \$(hwm_kb \$PID_$n) / 1024 )) MiB"; done
grep -E '^(Rss|Anonymous|Private_Dirty|Shared_Clean|Private_Clean):' "/proc/$PID_J/smaps_rollup" | tr -s ' ' | tr '\n' ' '; echo
sf J status | grep -E 'data:|repair|log' || true
sf J activity 2>&1 | head -40
if [ -n "${HOLD:-}" ]; then echo "holding $HOLD s; joiner pid $PID_J, log $WORK/J/log"; sleep "$HOLD"; fi
