#!/usr/bin/env bash
# Bump the release version in every file that names it (the release commit of
# the process in the release memory): the three crate manifests and their path
# dependencies, Cargo.lock, and the STARTFS_VERSION examples in README.md and
# scripts/install.sh. scripts/test-install.sh keeps its fixture versions.
# Usage: bump-version.sh X.Y.Z
set -euo pipefail
new=${1:?usage: bump-version.sh X.Y.Z}
cd "$(dirname "$0")/.."
old=$(sed -n 's/^version = "\(.*\)"/\1/p' crates/startfs/Cargo.toml | head -1)
[ "$old" != "$new" ] || { echo "already at $new" >&2; exit 1; }
sed -i "s/${old//./\\.}/$new/g" crates/*/Cargo.toml README.md scripts/install.sh
cargo build --quiet
echo "bumped $old -> $new ($(git diff --name-only | wc -l) files)"
