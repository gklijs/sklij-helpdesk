#!/usr/bin/env bash
# A bounded 0.4 baseline: fresh DBs, sustained traffic + idle, tenant scaling,
# read latency and websocket fan-out. Dedicated disposable cluster required.
set -euo pipefail
ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
cd "$ROOT"
OUT="${1:?usage: run.sh /absolute/output-directory}"
mkdir -p "$OUT"
export TICKET_ROUTING=shared
unset OTEL_EXPORTER_OTLP_ENDPOINT SEED_DEMO_TRAFFIC OIDC_ISSUER_URL
python3 scripts/bench/current/memory.py target/release/server "$OUT" --seconds 180 --idle-seconds 60
for tenants in 0 1 4 16; do
    python3 scripts/bench/tenant-scaling/scale.py \
        "$ROOT/target/release/server" "$ROOT/target/release/provisioner" \
        "$ROOT/target/release/lifecycle-replicator" - "v040_t${tenants}" "$OUT" \
        --tenants "$tenants" --fg-workers 16 --fg-interval-ms 50 \
        --seconds 60 --pgbench-scale 1
done
python3 scripts/bench/read-side/read_side.py \
    "$ROOT/target/release/server" "$ROOT/target/release/alerter" \
    "$ROOT/target/release/engagement-watcher" "$OUT" \
    --events 1000 --load-seconds 30 --fanout 1,8,32,64 --fanout-events 5
