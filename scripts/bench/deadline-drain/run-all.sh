#!/usr/bin/env bash
# The run sequence behind docs/deadline-drain-report-2026-10-13.md:
# auto-close drains alternating with equally long control runs, then one
# drain with no foreground load and one SLA-breach drain (100k deadlines). ~3 hours.
#
# usage: run-all.sh <server-binary> <out-dir>
# Expects seed.py's templates, settled with settle.py.
set -euo pipefail
BIN=$1
OUT=$2
cd "$(dirname "$0")"
mkdir -p "$OUT"
run() {
  local label=$1 template=$2; shift 2
  echo "=== $label $(date +%T) load $(cut -d' ' -f1-3 /proc/loadavg)"
  python3 drain.py "$BIN" "$template" "$label" "$OUT" "$@" > "$OUT/$label.out" 2>&1 || {
    echo "$label failed, see $OUT/$label.out"; exit 1; }
}
for r in a b; do
  run "ac-$r" dd_resolved --variant autoclose --retention
  run "ctl-$r" dd_resolved --variant none --max-drain 1000
done
run ac-noload dd_resolved --variant autoclose --fg-workers 0
run sla-a dd_created --variant sla --max-drain 3600
echo "=== done $(date +%T)"
