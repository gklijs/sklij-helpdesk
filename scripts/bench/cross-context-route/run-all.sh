#!/usr/bin/env bash
# The run sequence behind docs/cross-context-route-report-2026-10-13.md. ~25 minutes.
#
# usage: run-all.sh <server-binary> <out-dir>
# Expects the deadline-drain `dd_resolved` template (seed.py + settle.py in
# ../deadline-drain) for the 50k-ticket runs; `-` runs start empty.
set -euo pipefail
BIN=$1
OUT=$2
cd "$(dirname "$0")"
mkdir -p "$OUT"
run() {
  local label=$1 template=$2; shift 2
  echo "=== $label $(date +%T) load $(cut -d' ' -f1-3 /proc/loadavg)"
  python3 route.py "$BIN" "$template" "$label" "$OUT" "$@" > "$OUT/$label.out" 2>&1 || {
    echo "$label failed, see $OUT/$label.out"; exit 1; }
}
LAT=(--phases idle:120,load:180:load,tail:60)
FAULT=(--phases pre:30:load,fault:60:load+fault,recover:60:load --redrive)

# 1. Latency at idle and under a foreground step (8 workers x 250ms),
#    empty vs the 50k-ticket history. One run each.
run lat-empty-a - "${LAT[@]}"
run lat-50k-a dd_resolved "${LAT[@]}"

# 2. Faults in marketing, foreground on throughout. One run each.
run f-outage - "${FAULT[@]}" --fault outage
run f-slow - "${FAULT[@]}" --fault slow:200
run f-lock - "${FAULT[@]}" --fault lock
echo "=== done $(date +%T)"
