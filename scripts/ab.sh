#!/usr/bin/env bash
# Run a scripts/ profile on a base ref and on this tree, alternately, and print the lines of each
# run that match a pattern: one block per run, so noise between runs shows.
# Usage: ab.sh [-n ROUNDS=3] [-r REF=main] [-g PATTERN=wall|wakeups|cpu=] SCRIPT [ARGS...]
#   e.g. ab.sh validate-idle.sh 20 64      ab.sh -g 'fuse wall' profile-write.sh
set -euo pipefail
rounds=3; ref=main; pat='wall|wakeups|cpu='
while getopts n:r:g: o; do
  case $o in n) rounds=$OPTARG ;; r) ref=$OPTARG ;; g) pat=$OPTARG ;; *) exit 2 ;; esac
done
shift $((OPTIND - 1))
[ $# -ge 1 ] || { sed -n 2,6p "$0" >&2; exit 2; }
script=$1; shift
here="$(cd "$(dirname "$0")/.." && pwd)"
base=$(mktemp -d -p "$HOME/.cache" sfab.XXXXXX)
trap 'git -C "$here" worktree remove --force "$base" 2>/dev/null || true; rm -rf "$base"' EXIT
git -C "$here" worktree add -q --detach "$base" "$ref"
# The base needs this tree's copy of the script, which may not exist at the ref.
cp "$here"/scripts/lib.sh "$here/scripts/$script" "$base/scripts/"
for r in $(seq "$rounds"); do
  for tree in "$base" "$here"; do
    label=$([ "$tree" = "$base" ] && echo "$ref" || echo "this")
    echo "== $label r$r"
    "$tree/scripts/$script" "$@" 2>&1 | grep -E "$pat" | sed 's/  */ /g' || true
  done
done
