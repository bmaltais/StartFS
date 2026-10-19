#!/usr/bin/env bash
# Upgrade every member one at a time, and wait for each to be ready before the next. Ready means:
# the new version, `data: SAFE`, every member up, an empty inbox, the tick under BUSY_MAX percent,
# for GOOD samples in a row 10 s apart, and at least QUIET_SECS since the restart. A node that is
# not ready after TIMEOUT seconds stops the run (exit 1); nothing after it is touched.
# `busy` is the 60 s rolling average of `startfs activity`, so it lags a restart by about a minute.
# Run it in a terminal you can watch: the installer asks before restarting a daemon whose mount is
# in use (STARTFS_FORCE=1 skips the question). Needs ssh access (keys) to every remote node.
#
# Usage: rolling-upgrade.sh vX.Y.Z NODE...
#   NODE is an ssh host, or `local` for this machine. Put the node that matters most last:
#   it is upgraded with the most evidence from the others.
# Environment: BUSY_MAX=50  QUIET_SECS=60  GOOD=4  TIMEOUT=900  STARTFS_BIN='~/.local/bin/startfs'
set -uo pipefail
[ $# -ge 2 ] || { sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
TAG=$1; shift
WANT=${TAG#v}
BUSY_MAX=${BUSY_MAX:-50}; QUIET_SECS=${QUIET_SECS:-60}; GOOD=${GOOD:-4}; TIMEOUT=${TIMEOUT:-900}
BIN=${STARTFS_BIN:-'~/.local/bin/startfs'}
INSTALL="curl -fsSL https://raw.githubusercontent.com/bmaltais/StartFS/main/scripts/install.sh | STARTFS_VERSION=$TAG sh"
T0=$(date +%s)
say() { printf '[%4ds] %s\n' $(( $(date +%s) - T0 )) "$*"; }

# on NODE CMD: run CMD there. ont: the same with a terminal, so the installer can ask its question.
on() { local n=$1; shift; if [ "$n" = local ]; then bash -c "$*"; else ssh -o BatchMode=yes -o ConnectTimeout=8 "$n" "$*"; fi; }
ont() { local n=$1; shift; if [ "$n" = local ]; then bash -c "$*"; else ssh -t "$n" "$*"; fi; }

# sample NODE VERSION: print one status line; succeed when the node is ready at that version.
sample() {
  local n=$1 want=$2 out ver data up inbox busy rss queue
  out=$(on "$n" "$BIN --version; echo ==; $BIN status; echo ==; $BIN activity" 2>&1) || { echo unreachable; return 1; }
  ver=$(echo "$out" | awk 'NR==1{print $2}')
  data=$(echo "$out" | awk '/^data:/{print $2; exit}')
  up=$(echo "$out" | awk '/^ +[0-9]+ +[a-z0-9.-]+:[0-9]+ +up /{u++} /^ +[0-9]+ +[a-z0-9.-]+:[0-9]+ /{t++} END{print u+0 "/" t+0}')
  inbox=$(echo "$out" | awk '/inbox:/{print $2; exit}')
  busy=$(echo "$out" | sed -n 's/.*busy \([0-9]*\)% of the time.*/\1/p' | head -1)
  rss=$(echo "$out" | sed -n 's/.*daemon: \([0-9.]* [KMG]iB\) resident.*/\1/p' | head -1 | tr -d ' ')
  queue=$(echo "$out" | sed -n 's/^replication queue: \([0-9]*\) chunks.*/\1/p' | head -1)
  echo "ver=${ver:-?} data=${data:-?} up=$up inbox=${inbox:-?} busy=${busy:-?}% rss=${rss:-?} queue=${queue:-?}"
  [ "$ver" = "$want" ] && [ "$data" = SAFE ] && [ "${up%/*}" = "${up#*/}" ] \
    && [ "${inbox:-1}" = 0 ] && [ "${busy:-100}" -lt "$BUSY_MAX" ]
}

# wait_ready NODE MIN_SECS VERSION
wait_ready() {
  local n=$1 since=$2 want=$3 good=0 start line el
  start=$(date +%s)
  while :; do
    line=$(sample "$n" "$want") && ok=1 || ok=0
    el=$(( $(date +%s) - start ))
    if [ $ok = 1 ] && [ "$el" -ge "$since" ]; then good=$((good + 1)); else good=0; fi
    say "$n: $line  [good $good/$GOOD, ${el}s]"
    [ "$good" -ge "$GOOD" ] && return 0
    [ "$el" -ge "$TIMEOUT" ] && { say "$n: NOT READY after ${TIMEOUT}s, stopping here"; return 1; }
    sleep 10
  done
}

say "rolling upgrade to $TAG: $*"
for n in "$@"; do
  say "=== $n: checking it is quiet before touching it"
  old=$(on "$n" "$BIN --version" | awk '{print $2}')
  [ -n "$old" ] || { say "$n: cannot run $BIN, stopping here"; exit 1; }
  wait_ready "$n" 0 "$old" || exit 1
  if [ "$old" = "$WANT" ]; then say "=== $n: already on $WANT"; continue; fi
  say "=== $n: upgrading $old -> $WANT"
  ont "$n" "$INSTALL" || { say "$n: INSTALL FAILED, stopping here"; exit 1; }
  say "$n: installed; waiting for it to be ready"
  wait_ready "$n" "$QUIET_SECS" "$WANT" || exit 1
  say "=== $n: ready"
done
say "ALL DONE: $*"
