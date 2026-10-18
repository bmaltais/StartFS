#!/usr/bin/env bash
# The harness tests share their helpers (`startfs_harness::noise`, `Cluster::with_filesystem`);
# a test file that defines its own copy fails here.
# Usage: check-helpers.sh   (exit status is non-zero when a copy is found)
set -euo pipefail
cd "$(dirname "$0")/.."
copies=$(grep -nE '^fn (noise|cluster_with)\(' crates/startfs-harness/tests/*.rs || true)
if [ -n "$copies" ]; then
  echo "$copies"
  echo "use startfs_harness::noise and Cluster::with_filesystem instead of defining a copy" >&2
  exit 1
fi
