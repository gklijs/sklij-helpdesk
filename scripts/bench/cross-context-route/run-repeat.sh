#!/usr/bin/env bash
# The second run sequence (2026-10-14): repeats of run-all.sh's runs on a
# quiet box, the runs the first report cut, and the before/after check of
# the MarketingSignals ordering fix. ~100 minutes.
#
# usage: run-repeat.sh <fixed-server> <pre-fix-server> <out-dir>
# Expects the deadline-drain `dd_resolved` template for the 50k runs.
set -euo pipefail
FIX=$1
OLD=$2
OUT=$3
cd "$(dirname "$0")"
mkdir -p "$OUT"
run() {
  local label=$1 bin=$2 template=$3; shift 3
  echo "=== $label $(date +%T) load $(cut -d' ' -f1-3 /proc/loadavg)"
  python3 route.py "$bin" "$template" "$label" "$OUT" "$@" > "$OUT/$label.out" 2>&1 || {
    echo "$label failed, see $OUT/$label.out"; exit 1; }
  echo "    done $(date +%T) load $(cut -d' ' -f1-3 /proc/loadavg)"
}
LAT=(--phases idle:120,load:180:load,tail:60)
FAULT=(--phases pre:30:load,fault:60:load+fault,recover:60:load --redrive)
SWEEP=(--phases run:120 --probe-rate 4)

# 1. The ordering fix: a marketing outage parks lapses, the redrive lands
#    them after the conversions. Pre-fix build between two fixed runs.
run f-outage-fix-a "$FIX" - "${FAULT[@]}" --fault outage
run f-outage-old-a "$OLD" - "${FAULT[@]}" --fault outage
run f-outage-fix-b "$FIX" - "${FAULT[@]}" --fault outage

# 2. Ordering without faults: expire -> reactivate gap 0/100/300 ms.
run order-gap0-old "$OLD" - "${SWEEP[@]}" --gap-ms 0
run order-gap0-fix "$FIX" - "${SWEEP[@]}" --gap-ms 0
run order-gap100-fix "$FIX" - "${SWEEP[@]}" --gap-ms 100
run order-gap300-fix "$FIX" - "${SWEEP[@]}" --gap-ms 300

# 3. Fault repeats on a quiet box.
run f-slow-b "$FIX" - "${FAULT[@]}" --fault slow:200
run f-lock-b "$FIX" - "${FAULT[@]}" --fault lock
run f-slow-c "$FIX" - "${FAULT[@]}" --fault slow:200
run f-lock-c "$FIX" - "${FAULT[@]}" --fault lock

# 4. Latency repeats, empty and 50k history.
run lat-empty-b "$FIX" - "${LAT[@]}"
run lat-50k-b "$FIX" dd_resolved "${LAT[@]}"
run lat-empty-c "$FIX" - "${LAT[@]}"
run lat-50k-c "$FIX" dd_resolved "${LAT[@]}"

# 5. Cut in the first report.
run f-flaky "$FIX" - "${FAULT[@]}" --fault flaky:0.3
run f-kb-outage "$FIX" - "${FAULT[@]}" --fault outage --fault-context knowledge_base
run f-outage-50k "$FIX" dd_resolved "${FAULT[@]}" --fault outage
run lat-empty-heavy "$FIX" - "${LAT[@]}" --fg-workers 16 --fg-interval-ms 100
run f-outage-10m "$FIX" - --phases pre:30:load,fault:600:load+fault,recover:60:load --redrive --fault outage
echo "=== done $(date +%T)"
