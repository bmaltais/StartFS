#!/usr/bin/env bash
# Live ingest test (#74): copy a generated tree into this machine's mount and watch every member.
# Stop rules: a longest tick over TICK_MAX_MS, a node busy (60 s average) over BUSY_MAX percent for
# BUSY_SECS, a node unreachable, a member not `up` for DOWN_SECS. A trip kills the copy; nothing is
# removed. Ends when every member is SAFE with empty queues and inboxes (or after WATCH_MAX seconds),
# prints each node's peaks and a verdict, and exits non-zero unless GREEN.
# The test files stay in the mount: delete them, then run `startfs purge`.
# Usage: watch-ingest.sh
# Environment: NODES='local mini2 surface ocilab1 mini1 vps2 vps1' FILES=6000 SEED=random WATCH_MAX=900
#   TICK_MAX_MS=10000 BUSY_MAX=90 BUSY_SECS=180 DOWN_SECS=45 SRC=dir for the generated tree
set -uo pipefail
NODES=${NODES:-local mini2 surface ocilab1 mini1 vps2 vps1}; read -ra N <<< "$NODES"
FILES=${FILES:-6000}; SEED=${SEED:-$RANDOM$RANDOM}; WATCH_MAX=${WATCH_MAX:-900}
TICK_MAX_MS=${TICK_MAX_MS:-10000}; BUSY_MAX=${BUSY_MAX:-90}; BUSY_SECS=${BUSY_SECS:-180}; DOWN_SECS=${DOWN_SECS:-45}
SRC=${SRC:-${TMPDIR:-/tmp}/startfs-ingest-src}
DEST=$HOME/StartFS/ingest-test-$SEED
BIN='~/.local/bin/startfs'; T0=$(date +%s)
say() { printf '[%4ds] %s\n' $(( $(date +%s) - T0 )) "$*"; }
on() { local n=$1; shift; if [ "$n" = local ]; then bash -c "$*"; else ssh -o BatchMode=yes -o ConnectTimeout=8 "$n" "$*"; fi; }
sample() { # node -> "node inbox busy tickms data downothers queue"
  local out; out=$(on "$1" "$BIN status; echo ==; $BIN activity" 2>&1) || { echo "$1 ? 0 0 unreachable 9 ?"; return; }
  echo "$out" | awk -v n="$1" '
    /^data:/ && !d {d=$2; if($3!="")d=d"_"$3}
    /^ +[0-9]+ +[a-z0-9.-]+:[0-9]+ / && !/ up / && !/this node/ {down++}
    /inbox:/ && !i {i=$2}
    /busy [0-9]+% of the time/ {match($0,/busy [0-9]+/); b=substr($0,RSTART+5,RLENGTH-5)}
    /longest/ {match($0,/longest [0-9.]+ (ms|s)/); s=substr($0,RSTART+8,RLENGTH-8); split(s,a," "); t=a[1]*(a[2]=="s"?1000:1)}
    /^replication queue:/ {q=$3}
    END{printf "%s %s %s %d %s %d %s\n", n, i+0, b+0, t, d, down+0, q+0}'
}
declare -A PEAK_INBOX PEAK_BUSY PEAK_TICK PEAK_Q BUSYRUN; for n in "${N[@]}"; do PEAK_INBOX[$n]=0; PEAK_BUSY[$n]=0; PEAK_TICK[$n]=0; PEAK_Q[$n]=0; BUSYRUN[$n]=0; done
mkdir -p "$SRC"; [ -d "$SRC/$SEED" ] || python3 "$(dirname "$0")/make-tree.py" "$SRC/$SEED" --files "$FILES" --big 0 --seed "$SEED" >/dev/null
say "tree: $(du -sh "$SRC/$SEED" | cut -f1), $(find "$SRC/$SEED" -type f | wc -l) files; copying into $DEST"
mkdir -p "$DEST"; cp -r "$SRC/$SEED/." "$DEST/" & CP=$!
VERDICT=GREEN; REASON=""; DOWNSINCE=0; SETTLED=0
while :; do
  tmp=$(mktemp -d); for n in "${N[@]}"; do sample "$n" > "$tmp/$n" & done; wait
  line="copy=$(kill -0 $CP 2>/dev/null && echo running || echo done)"
  for n in "${N[@]}"; do
    read -r _ inbox busy tick data down queue < "$tmp/$n"
    line+=" | $n i=$inbox b=$busy t=${tick}ms q=$queue $data"
    (( inbox > PEAK_INBOX[$n] )) && PEAK_INBOX[$n]=$inbox; (( busy > PEAK_BUSY[$n] )) && PEAK_BUSY[$n]=$busy
    (( tick > PEAK_TICK[$n] )) && PEAK_TICK[$n]=$tick; (( queue > PEAK_Q[$n] )) && PEAK_Q[$n]=$queue
    if (( busy > BUSY_MAX )); then BUSYRUN[$n]=$(( BUSYRUN[$n] + 10 )); else BUSYRUN[$n]=0; fi
    (( BUSYRUN[$n] >= BUSY_SECS )) && { VERDICT=STOPPED; REASON="$n busy over $BUSY_MAX% for ${BUSY_SECS}s"; }
    (( tick > TICK_MAX_MS )) && { VERDICT=STOPPED; REASON="$n tick ${tick}ms"; }
    [ "$data" = unreachable ] && { VERDICT=STOPPED; REASON="$n unreachable"; }
    (( down > 0 )) && DOWNSINCE=$(( DOWNSINCE + 10 )) || DOWNSINCE=0
  done
  (( DOWNSINCE >= DOWN_SECS )) && { VERDICT=STOPPED; REASON="a member not up for ${DOWN_SECS}s"; }
  say "$line"; allq=0; alli=0; alls=1
  for n in "${N[@]}"; do read -r _ inbox _ _ data _ queue < "$tmp/$n"; allq=$((allq+queue)); alli=$((alli+inbox)); [ "$data" = SAFE ] || alls=0; done
  rm -rf "$tmp"
  [ $VERDICT = STOPPED ] && { kill $CP 2>/dev/null; say "STOP: $REASON"; break; }
  if ! kill -0 $CP 2>/dev/null && (( allq == 0 && alli == 0 && alls == 1 )); then SETTLED=$((SETTLED+1)); else SETTLED=0; fi
  (( SETTLED >= 2 )) && { say "all members SAFE, queues and inboxes empty"; break; }
  (( $(date +%s) - T0 > WATCH_MAX )) && { VERDICT=SLOW; REASON="not settled after ${WATCH_MAX}s"; say "$REASON"; break; }
  sleep 10
done
say "peaks: node inbox busy% tick_ms queue"
for n in "${N[@]}"; do printf '        %-8s %7s %5s %7s %6s\n' "$n" "${PEAK_INBOX[$n]}" "${PEAK_BUSY[$n]}" "${PEAK_TICK[$n]}" "${PEAK_Q[$n]}"; done
rm -rf "$SRC/$SEED"
say "VERDICT: $VERDICT $REASON; test files are in $DEST (delete them, then run startfs purge)"
[ $VERDICT = GREEN ]
