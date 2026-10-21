#!/usr/bin/env bash
# Where the suite's time goes, one line per test binary, slowest first: seconds, tests, binary.
# Usage: test-times.sh [TOP]            the TOP slowest binaries (default 10)
#        test-times.sh -t PKG BIN [TOP] the TOP slowest tests in one binary of a package
set -euo pipefail
cd "$(dirname "$0")/.."
if [ "${1:-}" = "-t" ]; then
  RUSTC_BOOTSTRAP=1 cargo test -p "$2" --test "$3" -- -Z unstable-options --report-time --test-threads=1 2>&1 |
    sed -nE 's/^test (.*) \.\.\. ok <([0-9.]+)s>$/\2 \1/p' | sort -rn | head -"${4:-10}"
  exit 0
fi
cargo test --workspace 2>&1 |
  awk '/Running /{b=$2} /^test result/{for(i=1;i<=NF;i++){if($i=="passed;")n=$(i-1); if($i=="in")t=$(i+1)}; sub("s","",t); if(n>0) printf "%s %s %s\n", t, n, b}' |
  sort -rn | head -"${1:-10}"
