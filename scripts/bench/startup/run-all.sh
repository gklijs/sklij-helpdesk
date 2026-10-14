#!/usr/bin/env bash
# The run sequence behind docs/startup-report-2026-10-14.md.
#
# usage: run-all.sh <server-binary> <out-dir>
# Expects templates `base` (seed_base.py, then settled), `h1m` and `h3m`
# (clone.py base h1m 4 / clone.py base h3m 12: 1.2M / 3.2M events
# from the 245k base).
set -euo pipefail
BIN=$1
OUT=$2
cd "$(dirname "$0")"
mkdir -p "$OUT"
run() {
  local label=$1 template=$2; shift 2
  echo "=== $label $(date +%T) load $(cut -d' ' -f1-3 /proc/loadavg)"
  python3 startup.py "$BIN" "$template" "$label" "$OUT" "$@" > "$OUT/$label.out" 2>&1 || {
    echo "$label failed, see $OUT/$label.out"; exit 1; }
  echo "    done $(date +%T) load $(cut -d' ' -f1-3 /proc/loadavg)"
}

# 1. Cold starts, nothing else running: 3 starts per size, two passes in
#    opposite order so drift doesn't line up with size.
for size in - base h1m h3m; do run "cold-${size/-/empty}-a" "$size"; done
for size in h3m h1m base -; do run "cold-${size/-/empty}-b" "$size"; done

# 2. Restart of the only instance while 8 clients keep sending commands.
run restart-load-h1m h1m --mode restart-load
run restart-load-h3m h3m --mode restart-load

# 3. A second instance serves the clients; the first one starts beside it.
run peer-load-h1m h1m --mode peer-load
run peer-load-h3m h3m --mode peer-load
echo "=== done $(date +%T)"
