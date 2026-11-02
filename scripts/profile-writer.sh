#!/usr/bin/env bash
# What a build costs the member writing it (#166): writer A and PEERS peers on loopback, 3 copies,
# no replication hold, a build-like load through A's mount. Prints A's inbox, sends that timed
# out and tick once a second, then the time to write and to replicate, A's tick phases and its
# sends per peer. The writer's tick falling behind (inbox growing, sends timing out) is what a
# build on the mount looked like live.
# LOAD=small: 300 "crates" of objects written to a temp dir, an rlib renamed out, the rest deleted.
# LOAD=link: one 256 MiB file written in 64 KiB pieces at shuffled offsets, as a linker does.
# PRE=20000 first fills A with that many tiny files (a big tree) and waits for them to replicate.
# Environment: LOAD=small, PEERS=4 (1 to 7), PRE=0, BIN=path/to/startfs (skips the build).
# Usage: profile-writer.sh   (needs /dev/fuse, fusermount3)
set -euo pipefail
given=${BIN:-}
source "$(dirname "$0")/lib.sh"
LOAD=${LOAD:-small}; PEERS=${PEERS:-4}; PRE=${PRE:-0}
case $LOAD in small | link) ;; *) echo "LOAD is small or link" >&2; exit 2 ;; esac
[ "$PEERS" -ge 1 ] && [ "$PEERS" -le 7 ] || { echo "PEERS is 1 to 7" >&2; exit 2; }
[ -n "$given" ] || build
setup_env
mkdir -p "$WORK/A/mnt"
start_node A "$WORK/A/mnt" "replication_hold_secs = 0"
TOKEN=$(sf A create --name writer --copies 3)
sf A config --offer 20G >/dev/null
peers=(B C D E F G H)
for p in "${peers[@]:0:$PEERS}"; do
  start_node "$p" "" "replication_hold_secs = 0"
  sf "$p" join "$TOKEN" --offer 20G >/dev/null
done
until sf A status | grep -q "($((PEERS + 1)) nodes)"; do sleep 0.2; done
if [ "$PRE" -gt 0 ]; then
  for d in $(seq $((PRE / 1000))); do
    mkdir -p "$WORK/A/mnt/pre/d$d"
    for i in $(seq 1000); do echo "file $d $i" > "$WORK/A/mnt/pre/d$d/f$i"; done
  done
  sync -f "$WORK/A/mnt/pre"
  until ! sf A status | grep -q 'not fully replicated'; do sleep 0.5; done
fi

# One line a second: seconds since the load began, A's inbox, sends timed out in the window, tick.
sample() {
  while [ ! -e "$WORK/stop" ]; do
    if sf A activity > "$WORK/act" 2>/dev/null; then
      awk -v t="$(elapsed "$t0")" '/^tick:/ {tk = $0} /inbox:/ {ib = $2} /sends:/ {to += $4}
        END {print t "s inbox " ib " timed-out " to + 0 " " tk}' "$WORK/act"
    fi
    sleep 1
  done
}
t0=$(now)
sample & sampler=$!
case $LOAD in
  small) python3 - "$WORK/A/mnt" <<'PY'
import os, random, sys
root, r = sys.argv[1], random.Random(1)
for c in range(300):
    tmp = f"{root}/rustc{c}"
    os.mkdir(tmp)
    for i in range(20):
        with open(f"{tmp}/o{i}.o", "wb") as f:
            f.write(os.urandom(r.choice([4096, 65536, 262144])))
    with open(f"{tmp}/lib.rlib", "wb") as f:
        f.write(os.urandom(1 << 20))
    os.rename(f"{tmp}/lib.rlib", f"{root}/lib{c}.rlib")
    for i in range(20):
        os.unlink(f"{tmp}/o{i}.o")
    os.rmdir(tmp)
PY
    ;;
  link) python3 - "$WORK/A/mnt" <<'PY'
import os, random, sys
root, r = sys.argv[1], random.Random(1)
size, piece = 256 << 20, 64 << 10
fd = os.open(f"{root}/bin", os.O_CREAT | os.O_RDWR)
os.ftruncate(fd, size)
offsets = list(range(0, size, piece))
r.shuffle(offsets)
for o in offsets:
    os.pwrite(fd, os.urandom(piece), o)
os.close(fd)
PY
    ;;
esac
tw=$(elapsed "$t0")
sync -f "$WORK/A/mnt"
until ! sf A status | grep -q 'not fully replicated'; do sleep 0.2; done
touch "$WORK/stop"
wait "$sampler" || true
echo "load $LOAD, $PEERS peers, pre $PRE: written ${tw}s, replicated $(elapsed "$t0")s"
sf A activity | grep -E '^tick:|phases|sends:' || true
