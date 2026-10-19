#!/usr/bin/env bash
# Live join test: add a weak node (default vps2) as a new empty member and watch every member,
# with explicit stop rules. A tripped rule stops the joiner and removes it from the cluster.
# Stop rules (what hurt on 2026-10-19, not queue length: a long inbox is small in bytes):
#   - a healthy node busy over BUSY_MAX percent (60 s average) for BUSY_SECS in a row
#   - a healthy node's longest tick over TICK_MAX_MS
#   - a healthy node unreachable, or another member shown not `up` for over DOWN_SECS
#   - a healthy node over HEALTHY_RSS_MIB resident, or the joiner over JOINER_RSS_MIB
#   - the joiner not ready (SAFE, empty inbox, all members up, busy under 50%, GOOD samples in a row)
#     within DEADLINE seconds
# Usage: watch-join.sh vX.Y.Z        (the version every member runs; the joiner gets it installed)
# Environment: JOINER=vps2 NODES='local mini2 surface ocilab1 mini1' OFFER=10G BUSY_MAX=90
#   BUSY_SECS=180 TICK_MAX_MS=10000 DOWN_SECS=45 HEALTHY_RSS_MIB=1024 JOINER_RSS_MIB=2600
#   GOOD=1 DEADLINE=1200 SKIP_JOIN=1 (watch a joiner that is already joining) LOG=file
set -uo pipefail
[ $# -eq 1 ] || { sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//'; exit 2; }
TAG=$1; WANT=${TAG#v}
JOINER=${JOINER:-vps2}; OFFER=${OFFER:-10G}
BUSY_MAX=${BUSY_MAX:-90}; BUSY_SECS=${BUSY_SECS:-180}; TICK_MAX_MS=${TICK_MAX_MS:-10000}; DOWN_SECS=${DOWN_SECS:-45}
HEALTHY_RSS_MIB=${HEALTHY_RSS_MIB:-1024}; JOINER_RSS_MIB=${JOINER_RSS_MIB:-2600}; DEADLINE=${DEADLINE:-1200}; GOOD=${GOOD:-1}
read -ra HEALTHY <<< "${NODES:-local mini2 surface ocilab1 mini1}"
BIN='~/.local/bin/startfs'
LOG=${LOG:-/dev/null}
T0=$(date +%s)
say() { printf '[%4ds] %s\n' $(( $(date +%s) - T0 )) "$*" | tee -a "$LOG"; }
on() { local n=$1; shift; if [ "$n" = local ]; then bash -c "$*"; else ssh -o BatchMode=yes -o ConnectTimeout=8 "$n" "$*"; fi; }

# sample NODE: key=value fields on one line
sample() {
  local n=$1 out
  out=$(on "$n" "$BIN --version; echo ==; $BIN status; echo ==; $BIN activity" 2>&1) || { echo "ver=? data=unreachable up=0/0 downothers=0 inbox=0 busy=0 tick=0 rss=0 queue=0"; return; }
  python3 - "$out" "$JOINER" <<'PY'
import re,sys
t,joiner=sys.argv[1],sys.argv[2]
g=lambda p:(re.search(p,t,re.M) or [None,None])[1]
members=re.findall(r'^\s+\d+\s+(\S+):\d+\s+(\S+)',t,re.M)
rss=re.search(r'daemon: ([\d.]+) (KiB|MiB|GiB) resident',t)
mib=float(rss.group(1))*{'KiB':1/1024,'MiB':1,'GiB':1024}[rss.group(2)] if rss else 0
print("ver=%s data=%s up=%d/%d downothers=%d inbox=%s busy=%s tick=%d rss=%d queue=%s"%(
  g(r'^startfs (\S+)') or '?', (g(r'^data:\s*(\S+(?: \S+)?)') or '?').replace(' ','_'),
  sum(1 for _,s in members if s=='up'),len(members),
  sum(1 for h,s in members if s!='up' and not h.startswith(joiner)),
  g(r'inbox: (\d+) entries') or '?', g(r'busy (\d+)% of the time') or '?',
  float(g(r'longest ([\d.]+) ms') or 0), mib, g(r'replication queue: (\d+) chunks') or '?'))
PY
}
field() { echo "$1" | tr ' ' '\n' | sed -n "s/^$2=//p"; }

abort() {
  say "!!! TRIPPED: $1"
  say "stopping $JOINER and removing it from the cluster"
  on "$JOINER" 'systemctl --user stop startfs' 2>&1 | tee -a "$LOG"
  id=$(on local "$BIN status" | awk -v j="$JOINER" '$2 ~ "^"j {print $1; exit}')
  if [ -n "$id" ]; then on local "$BIN remove $id" 2>&1 | tee -a "$LOG"; else say "could not find $JOINER in the member list; remove it by hand"; fi
  say "final samples:"; for n in "${HEALTHY[@]}" "$JOINER"; do say "  $n: $(sample "$n")"; done
  exit 1
}

if [ -z "${SKIP_JOIN:-}" ]; then
  say "=== pre-flight: the members must be quiet and on $WANT"
  for n in "${HEALTHY[@]}"; do
    s=$(sample "$n"); say "$n: $s"
    [ "$(field "$s" ver)" = "$WANT" ] && [ "$(field "$s" data)" = SAFE ] && [ "$(field "$s" inbox)" = 0 ] || { say "$n is not ready, not starting"; exit 1; }
  done
  [ "$(on "$JOINER" "systemctl --user is-active startfs")" != active ] || { say "$JOINER daemon already running, not starting"; exit 1; }
  say "=== $JOINER: install $TAG without touching the service, keep the previous data dir aside"
  on "$JOINER" "curl -fsSL https://raw.githubusercontent.com/bmaltais/StartFS/main/scripts/install.sh | STARTFS_VERSION=$TAG STARTFS_NO_SERVICE=1 sh" 2>&1 | tail -3 | tee -a "$LOG"
  on "$JOINER" "$BIN --version" | grep -q "$WANT" || { say "$JOINER is not on $WANT, stopping"; exit 1; }
  on "$JOINER" 'd=~/.local/share/startfs; if [ -e $d ]; then mv $d $d.attempt-$(date +%H%M%S) && echo moved; fi' | tee -a "$LOG"
  say "=== $JOINER: start the daemon and join with offer $OFFER"
  TOKEN=$(on local "$BIN token" | grep -o 'startfs1-[A-Za-z0-9_.@:-]*' | head -1)
  [ -n "$TOKEN" ] || { say "no join token"; exit 1; }
  on "$JOINER" 'systemctl --user daemon-reload; systemctl --user start startfs'
  for _ in $(seq 30); do on "$JOINER" "$BIN status" >/dev/null 2>&1 && break; sleep 1; done
  out=$(on "$JOINER" "$BIN join $TOKEN --offer $OFFER" 2>&1); rc=$?
  echo "$out" | tail -2 | tee -a "$LOG"
  on local "$BIN revoke $TOKEN" >/dev/null 2>&1 && say "join token revoked"
  if [ $rc -ne 0 ]; then say "join FAILED (rc $rc); stopping $JOINER, nothing joined"; on "$JOINER" 'systemctl --user stop startfs'; exit 1; fi
fi
JOIN=$(date +%s)

good=0; down=; js=
declare -A hot_since
while :; do
  el=$(( $(date +%s) - JOIN )); now=$(date +%s)
  line=""; worst=""
  for n in "${HEALTHY[@]}" "$JOINER"; do
    s=$(sample "$n"); d=$(field "$s" data); r=$(field "$s" rss); b=$(field "$s" busy); t=$(field "$s" tick)
    line="$line ${n}[$d in=$(field "$s" inbox) b=${b}% tick=${t}ms ${r}M]"
    if [ "$n" = "$JOINER" ]; then
      js=$s; [ "${r:-0}" -gt "$JOINER_RSS_MIB" ] && worst="$JOINER resident $r MiB"
      continue
    fi
    [ "$d" = unreachable ] && worst="$n unreachable"
    [ "${r:-0}" -gt "$HEALTHY_RSS_MIB" ] && worst="$n resident $r MiB"
    [ "${t:-0}" -gt "$TICK_MAX_MS" ] && worst="$n longest tick $t ms"
    if [ "${b:-0}" -gt "$BUSY_MAX" ] 2>/dev/null; then
      hot_since[$n]=${hot_since[$n]:-$now}
      [ $(( now - hot_since[$n] )) -ge "$BUSY_SECS" ] && worst="$n over $BUSY_MAX% busy for ${BUSY_SECS}s"
    else unset "hot_since[$n]"; fi
    if [ "$(field "$s" downothers)" != 0 ]; then
      down=${down:-$now}; [ $(( now - down )) -gt "$DOWN_SECS" ] && worst="$n sees a member not up for over ${DOWN_SECS}s"
    fi
  done
  [ "$(field "$js" downothers)" = 0 ] && down=
  say "+${el}s$line"
  [ -n "$worst" ] && abort "$worst"
  if [ "$(field "$js" data)" = SAFE ] && [ "$(field "$js" inbox)" = 0 ] && [ "$(field "$js" up)" = "$(( ${#HEALTHY[@]} + 1 ))/$(( ${#HEALTHY[@]} + 1 ))" ] && [ "$(field "$js" busy)" -lt 50 ] 2>/dev/null; then good=$((good + 1)); else good=0; fi
  [ "$good" -ge "$GOOD" ] && break
  [ "$el" -ge "$DEADLINE" ] && abort "$JOINER not ready within ${DEADLINE}s"
  sleep 10
done
say "=== SUCCESS: $JOINER joined and is ready after $(( $(date +%s) - JOIN ))s"
for n in "${HEALTHY[@]}" "$JOINER"; do say "  $n: $(sample "$n")"; done
