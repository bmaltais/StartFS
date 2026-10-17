#!/usr/bin/env bash
# Criterion 1: new filesystem plus one joined machine, real binaries on loopback.
set -euo pipefail
source "$(dirname "$0")/lib.sh"
build; setup_env
mkdir -p "$WORK/A/mnt" "$WORK/B/mnt"

T0=$(now)
# 1. daemon on machine A (in real use: `systemctl --user enable --now startfs`)
start_node A "$WORK/A/mnt"
# 2. create
TOKEN=$(sf A create --name home --copies 2)
T1=$(now)
# 3. daemon on machine B
start_node B "$WORK/B/mnt"
# 4. join
sf B join "$TOKEN" --offer 1G
until sf B status | grep -q '(2 nodes)' && sf A status | grep -q '(2 nodes)'; do sleep 0.1; done

echo "A daemon + create: $(python3 -c "print(f'{$T1 - $T0:.2f}')")s"
echo "whole flow (A daemon+create, B daemon+join, both list 2 nodes): $(elapsed "$T0")s"
echo "commands: 4 (daemon, create, daemon, join); 2 if daemons come from a service unit"
echo "--- status on B ---"; sf B status
echo "hello" > "$WORK/A/mnt/x.txt"
for i in $(seq 100); do [ -f "$WORK/B/mnt/x.txt" ] && break; sleep 0.1; done
echo "file written on A readable on B: $(cat "$WORK/B/mnt/x.txt")"
