#!/usr/bin/env bash
# The checks CI runs (fmt, clippy with warnings denied, test, shared harness helpers),
# printing only what failed.
# Usage: check.sh   (exit status is non-zero when any check fails)
set -euo pipefail
cd "$(dirname "$0")/.."
out=$(mktemp)
trap 'rm -f "$out"' EXIT
for step in "cargo fmt --check" "cargo clippy --workspace --all-targets -- -D warnings" "cargo test --workspace" "scripts/check-helpers.sh"; do
  if $step >"$out" 2>&1; then
    echo "ok   $step"
  else
    echo "FAIL $step"
    grep -E '^(error|warning)|^test .* FAILED|panicked|^Diff in|^\s+--> |^crates/.*:[0-9]+:fn |^use startfs_harness' "$out" | head -40
    exit 1
  fi
done
