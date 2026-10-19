#!/usr/bin/env bash
# Ingest saturation (#74): copy a tree of small files into one node of a 5-node loopback cluster
# (3 copies) and sample every node's `activity` while it replicates. The writer's tick should stay
# short and its inbox and replication queue should drain; on 1.5.2 the replicate phase costs time
# proportional to the queue, so the writer sits near 99% busy with an inbox that only grows.
# Prints a CSV row per node every 2 s and, at the end, each node's peaks and a verdict: RED when the
# longest tick passes TICK_MAX_MS (default 500) or the inbox passes INBOX_MAX (default 20000).
# Environment: FILES=6000 BIG=0 SEED=74, WATCH=120 (seconds sampled after the copy ends),
# CSV=path (default $WORK/ingest.csv), KEEP_WORK=1, BIN=path/to/startfs.
# Usage: profile-ingest.sh   (needs /dev/fuse, fusermount3)
set -euo pipefail
given=${BIN:-}
source "$(dirname "$0")/lib.sh"
FILES=${FILES:-6000}; BIG=${BIG:-0}; SEED=${SEED:-74}; WATCH=${WATCH:-120}
TICK_MAX_MS=${TICK_MAX_MS:-500}; INBOX_MAX=${INBOX_MAX:-20000}
[ -n "$given" ] || build
setup_env
T0=$(now)
say() { printf '[%5.0fs] %s\n' "$(python3 -c "print($(now) - $T0)")" "$*"; }
names="A B C D E"
base=$((20000 + RANDOM % 20000)); i=0
for n in $names; do eval "PORT_$n=$((base + i))"; i=$((i + 1)); mkdir -p "$WORK/$n/mnt"; done
start_node A "$WORK/A/mnt"
TOKEN=$(sf A create --name ingest --copies 3)
sf A config --offer 10G >/dev/null
for n in B C; do start_node "$n" "$WORK/$n/mnt"; sf "$n" join "$TOKEN" --offer 10G >/dev/null; done
for n in D E; do start_node "$n" "$WORK/$n/mnt"; sf "$n" join "$TOKEN" --offer 100G >/dev/null; done
until sf A status | grep -q "(5 nodes)"; do sleep 0.2; done
python3 "$SF_ROOT/scripts/make-tree.py" "$WORK/src" --files "$FILES" --big "$BIG" --seed "$SEED" >/dev/null
CSV=${CSV:-$WORK/ingest.csv}
echo "t,node,inbox,busy,longest_ms,replicate_ms,queue,data" > "$CSV"
sample() { # NODE T
  local n=$1 act st
  act=$(timeout 10 "$BIN" --data-dir "$WORK/$n/data" activity 2>&1) || act=""
  st=$(timeout 10 "$BIN" --data-dir "$WORK/$n/data" status 2>&1 | sed -n 's/^data: *//p' | head -1) || st=""
  local longest
  longest=$(echo "$act" | sed -n 's/.*longest \([0-9.]*\) ms.*/\1/p' | head -1)
  [ -n "$longest" ] || longest=$(echo "$act" | sed -n 's/.*longest \([0-9.]*\) s,.*/\1/p' | head -1 | awk '{print $1*1000}')
  echo "$2,$n,$(echo "$act" | awk '/inbox:/{print $2; exit}'),$(echo "$act" | sed -n 's/.*busy \([0-9]*\)% of the time.*/\1/p' | head -1),${longest:-},$(echo "$act" | sed -n 's/.*replicate \([0-9.]*\),.*/\1/p' | head -1),$(echo "$act" | sed -n 's/^replication queue: \([0-9]*\) chunks.*/\1/p' | head -1),${st}"
}
t0=$(now)
say "copying $FILES files into A"
cp -r "$WORK/src" "$WORK/A/mnt/copy" & cpid=$!
quiet=0
while :; do
  t=$(python3 -c "print(int($(now) - $t0))")
  rows=$(for n in $names; do sample "$n" "$t" & done; wait)
  echo "$rows" >> "$CSV"
  say "$(echo "$rows" | awk -F, '{printf "%s i=%s b=%s%% q=%s %s | ", $2,$3,$4,$7,$8}')"
  if ! kill -0 $cpid 2>/dev/null; then
    [ -n "${copy_end:-}" ] || { copy_end=$t; say "copy finished"; }
    [ $((t - copy_end)) -ge "$WATCH" ] && break
    [ "$(echo "$rows" | awk -F, '$8=="SAFE" && $3==0' | wc -l)" -eq 5 ] && { quiet=$((quiet + 1)); [ $quiet -ge 3 ] && break; } || quiet=0
  fi
  sleep 1
done
echo "seconds_to_settle=$(elapsed "$t0") csv=$CSV"
red=0
for n in $names; do
  awk -F, -v n="$n" '$2==n { for (c = 3; c <= 7; c++) if ($c != "" && $c + 0 > m[c]) m[c] = $c + 0 }
    END { printf "  %s peak inbox %d busy %d%% longest tick %d ms replicate %.0f ms queue %d\n", n, m[3], m[4], m[5], m[6], m[7] }' "$CSV"
done
awk -F, -v t="$TICK_MAX_MS" -v i="$INBOX_MAX" 'NR>1 && ($5+0 > t || $3+0 > i) {bad=1} END {print bad ? "RED: a tick or inbox passed its limit" : "GREEN"; exit bad}' "$CSV" || red=1
exit $red
