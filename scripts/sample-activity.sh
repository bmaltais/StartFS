#!/usr/bin/env bash
# Run a command against the live cluster (a build on the mount) and sample `startfs activity` on
# this node and on each HOST (over ssh) while it runs; then print, per node, the worst seen: busy,
# receive phase, inbox, longest tick, sends timed out in the 60 s window. Also how many `status`
# samples of this node said AT RISK. The command is stopped after --secs (exit 124): a few minutes
# of load answer whether a member falls behind, a whole build rarely adds to that.
# Usage: sample-activity.sh [--secs 180] [--every 8] HOST... -- COMMAND...
#   e.g. sample-activity.sh mini1 vps2 -- cargo build
# Environment: STARTFS=startfs (here), REMOTE_STARTFS='~/.local/bin/startfs' (on the hosts).
set -euo pipefail
secs=180; every=8; hosts=()
while [ $# -gt 0 ]; do
  case $1 in
    --secs) secs=$2; shift 2 ;;
    --every) every=$2; shift 2 ;;
    --) shift; break ;;
    *) hosts+=("$1"); shift ;;
  esac
done
[ $# -gt 0 ] || { sed -n 7,9p "$0" >&2; exit 2; }
local_sf=${STARTFS:-startfs}
# shellcheck disable=SC2088 # expanded by the remote shell
remote_sf=${REMOTE_STARTFS:-'~/.local/bin/startfs'}
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT
trap 'exit 130' INT TERM

sample() {
  local n=0 h
  while [ ! -e "$out/stop" ]; do
    "$local_sf" activity > "$out/local.$n" 2>&1 &
    "$local_sf" status > "$out/status.$n" 2>&1 &
    # shellcheck disable=SC2029 # the path is meant for the remote shell, as given
    for h in "${hosts[@]}"; do ssh "$h" "$remote_sf activity" > "$out/$h.$n" 2>&1 & done
    wait
    n=$((n + 1))
    sleep "$every"
  done
}
sample & sampler=$!
status=0
timeout "$secs" "$@" || status=$?
touch "$out/stop"
wait "$sampler" || true

for node in local "${hosts[@]}"; do
  cat "$out/$node".* | awk -v node="$node" '
    /^tick:/ {
      n++
      for (i = 1; i <= NF; i++) {
        if ($i == "longest" && $(i + 1) + 0 > longest) longest = $(i + 1) + 0
        if ($i == "busy" && $(i + 1) + 0 > busy) busy = $(i + 1) + 0
      }
      if (to > timed) timed = to
      to = 0
    }
    /phases/ { for (i = 1; i <= NF; i++) if ($i == "receive" && $(i + 1) + 0 > receive) receive = $(i + 1) + 0 }
    /inbox:/ && $2 + 0 > inbox { inbox = $2 + 0 }
    /sends:/ { to += $4 }
    END {
      printf "%s: %d samples; max busy %d%%, receive %.0f ms, inbox %d, longest tick %.0f ms, timed out %d\n",
        node, n, busy, receive, inbox, longest, timed
    }'
done
total=$(find "$out" -name 'status.*' | wc -l)
risk=$( (grep -l 'AT RISK' "$out"/status.* 2>/dev/null || true) | wc -l)
echo "this node AT RISK in $risk of $total status samples"
exit "$status"
