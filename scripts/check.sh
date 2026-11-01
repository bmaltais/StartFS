#!/usr/bin/env bash
# The checks CI runs (fmt, shellcheck, clippy with warnings denied, test, shared harness helpers),
# printing only what failed. A step still running after STEP_TIMEOUT seconds (default 1800) fails
# as hung, naming the test binary it was in.
# Usage: check.sh   (exit status is non-zero when any check fails)
set -euo pipefail
cd "$(dirname "$0")/.."
out=$(mktemp)
trap 'rm -f "$out"' EXIT
command -v shellcheck >/dev/null || { echo "FAIL shellcheck is not installed: install it (e.g. pacman -S shellcheck, apt install shellcheck) to lint scripts/"; exit 1; }
for step in "cargo fmt --check" "shellcheck -x -f gcc scripts/*.sh" "cargo clippy --workspace --all-targets -- -D warnings" "cargo test --workspace" "scripts/check-helpers.sh"; do
  status=0
  timeout "${STEP_TIMEOUT:-1800}" bash -c "$step" >"$out" 2>&1 || status=$?
  if [ "$status" = 0 ]; then
    echo "ok   $step"
  else
    echo "FAIL $step"
    [ "$status" = 124 ] && echo "hung: no exit after ${STEP_TIMEOUT:-1800}s; last started: $(grep -E '^\s+Running ' "$out" | tail -1)"
    grep -E '^(error|warning)|^test .* FAILED|panicked|^Diff in|^\s+--> |^crates/.*:[0-9]+:fn |^scripts/.*:[0-9]+:[0-9]+: |^use startfs_harness' "$out" | head -40
    exit 1
  fi
done
