#!/usr/bin/env bash
# Dead peer (#74): 3 loopback nodes, 3 copies, one frozen with SIGSTOP, then a tree copied into another.
# Samples the writer's activity: tick average, busy, replicate phase, timeouts and retries per minute.
# Verdict GREEN when, once the copy ends, the writer's busy stays under BUSY_MAX (default 50),
# its replicate phase under REPL_MAX_MS (default 10) and the retries under RETRY_MAX in the last minute reported (default 5000; with a retry every 5 s it was
# about 12 per chunk, 50,000 for 4,200 chunks; the figure is activity's one-minute window, not a rate).
# Environment: FILES=4000 BIG=200 SEED=74 SECS=90 BIN=path KEEP_WORK=1
# Usage: profile-deadpeer.sh   (needs /dev/fuse, fusermount3)
set -euo pipefail
given=${BIN:-}
source "$(dirname "$0")/lib.sh"
FILES=${FILES:-4000}; BIG=${BIG:-200}; SEED=${SEED:-74}; SECS=${SECS:-90}
BUSY_MAX=${BUSY_MAX:-50}; REPL_MAX_MS=${REPL_MAX_MS:-10}; RETRY_MAX=${RETRY_MAX:-5000}
[ -n "$given" ] || build
setup_env
say() { printf '[%s] %s\n' "$(date +%T)" "$*"; }
base=$((20000 + RANDOM % 20000)); i=0
for n in A B C; do eval "PORT_$n=$((base + i))"; i=$((i + 1)); mkdir -p "$WORK/$n/mnt"; done
start_node A "$WORK/A/mnt"
TOKEN=$(sf A create --name deadpeer --copies 3)
sf A config --offer 10G >/dev/null
for n in B C; do start_node "$n" "$WORK/$n/mnt"; sf "$n" join "$TOKEN" --offer 10G >/dev/null; done
until sf A status | grep -q "(3 nodes)"; do sleep 0.2; done
python3 "$SF_ROOT/scripts/make-tree.py" "$WORK/src" --files "$FILES" --big "$BIG" --seed "$SEED" >/dev/null
say "tree: $(du -sh "$WORK/src" | cut -f1); freezing C (pid $PID_C)"
kill -STOP "$PID_C"
cp -r "$WORK/src" "$WORK/A/mnt/copy" & cpid=$!
CSV="$WORK/deadpeer.csv"; echo "t,busy,avg_ms,replicate_ms,retries,timeouts,queue,b_chunks" > "$CSV"
t0=$(now); copy_end=
while :; do
  t=$(python3 -c "print(int($(now) - $t0))")
  act=$(timeout 10 "$BIN" --data-dir "$WORK/A/data" activity 2>&1) || act=""
  busy=$(echo "$act" | sed -n 's/.*busy \([0-9]*\)% of the time.*/\1/p' | head -1)
  avg=$(echo "$act" | sed -n 's/.*average \([0-9.]*\) ms.*/\1/p' | head -1)
  repl=$(echo "$act" | sed -n 's/.*replicate \([0-9.]*\),.*/\1/p' | head -1)
  retries=$(echo "$act" | sed -n 's/.*sends: .* \([0-9]*\) retried.*/\1/p' | awk '{s+=$1} END{print s+0}')
  tmo=$(echo "$act" | sed -n 's/.*sends: .* \([0-9]*\) timed out.*/\1/p' | awk '{s+=$1} END{print s+0}')
  q=$(echo "$act" | sed -n 's/^replication queue: \([0-9]*\) chunks.*/\1/p' | head -1)
  bch=$(timeout 10 "$BIN" --data-dir "$WORK/B/data" status 2>&1 | sed -n 's/^usage: .* stored in \([0-9]*\) chunks.*/\1/p' | head -1)
  echo "$t,${busy:-},${avg:-},${repl:-},$retries,$tmo,${q:-},${bch:-}" >> "$CSV"
  say "t=$t busy=${busy:-?}% avg=${avg:-?}ms replicate=${repl:-?}ms retried=$retries timedout=$tmo queue=${q:-?} B_chunks=${bch:-?}"
  echo "$act" | grep -m2 "sends:" >> "$WORK/sends.txt" || true
  if ! kill -0 $cpid 2>/dev/null; then
    [ -n "$copy_end" ] || { copy_end=$t; say "copy finished"; }
    [ $((t - copy_end)) -ge "$SECS" ] && break
  fi
  sleep 5
done
kill -CONT "$PID_C" 2>/dev/null || true
echo "--- A's last sends lines:"; tail -2 "$WORK/sends.txt"
awk -F, -v e="$copy_end" -v b="$BUSY_MAX" -v r="$REPL_MAX_MS" -v m="$RETRY_MAX" -v want="$((FILES + BIG))" '
  NR>1 {t[NR]=$1; rt[NR]=$5; bc[NR]=$8; if($8+0>=want*0.99 && !done) done=$1; last=NR}
  NR>1 && $1>=e+30 {n++; if($2+0>b)bb=1; if($4+0>r)rr=1; if($2+0>pb)pb=$2+0; if($4+0>pr)pr=$4+0}
  END{ d=rt[last]; printf "samples %d, peak busy %d%%, peak replicate %.1f ms, retried in the last minute: %d, B reached 99%% of its chunks at t=%s (copy ended t=%d)\n", n, pb, pr, d, (done==""?"never":done), e;
       print (bb||rr||d>m||!n||done=="") ? "RED" : "GREEN"; exit (bb||rr||d>m||!n||done=="") }' "$CSV"
