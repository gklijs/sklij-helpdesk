#!/usr/bin/env bash
# The run sequence behind docs/tenant-scaling-report-2026-10-15.md:
# shared baseline, then 1/4/16 per-company tenants, each with pgbench
# TPC-B alongside on a sibling database. ~40 minutes plus the template.
#
# usage: run-all.sh <server-binary> <provisioner-binary> <replicator-binary> <out-dir>
set -euo pipefail
SERVER=$1
PROV=$2
REPL=$3
OUT=$4
cd "$(dirname "$0")"
mkdir -p "$OUT"
run() {
  local label=$1 template=$2 tenants=$3; shift 3
  echo "=== $label $(date +%T) load $(cut -d' ' -f1-3 /proc/loadavg)"
  python3 scale.py "$SERVER" "$PROV" "$REPL" "$template" "$label" "$OUT" \
    --tenants "$tenants" "$@" > "$OUT/$label.out" 2>&1 || {
    echo "$label failed, see $OUT/$label.out"; exit 1; }
  echo "    done $(date +%T) load $(cut -d' ' -f1-3 /proc/loadavg)"
}
run "shared" ts_base 0 --fg-workers 16 --fg-interval-ms 50 --seconds 240
run "tenant1" ts_base 1 --fg-workers 16 --fg-interval-ms 50 --seconds 240
run "tenant4" ts_base 4 --fg-workers 16 --fg-interval-ms 50 --seconds 240
run "tenant16" ts_base 16 --fg-workers 16 --fg-interval-ms 50 --seconds 240
echo "=== done $(date +%T)"
