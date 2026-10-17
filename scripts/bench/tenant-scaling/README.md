# Tenant-scaling throughput benchmark (issues #32 and #33)

Measures aggregate command throughput against 1, 4 and 16 per-company
tenant bounded contexts plus a shared-context baseline, all with a pgbench
TPC-B workload running against a separate database on the same cluster.
Results: `docs/tenant-scaling-report-2026-10-15.md`.

Standard library Python only; reuses `../deadline-drain/common.py` (server
start-up, REST client, psql helpers), so the same `PGHOST`/`PGPORT`/`PGUSER`/
`PGBIN` variables apply (default `127.0.0.1:5440`, user `postgres`).

## One run

    python3 scale.py <server> <provisioner> <replicator> <template|-> <label> <out-dir> \
        --tenants N [--fg-workers 16] [--fg-interval-ms 50] [--seconds 240]

`-` starts from an empty database; otherwise the template is copied (the
`ts_base` template from `seed.py`, settled with `settle.py`). `--tenants 0`
is the shared-context baseline (`TICKET_ROUTING=shared`); otherwise the
server starts with `TICKET_ROUTING=tenant`, the provisioner and
lifecycle-replicator run alongside, and every company gets a tenant
provisioned + lifecycle mirrored synchronously before load starts (the same
GraphQL/REST calls those binaries make, without waiting on their 5s ticks).

Tenant names are derived in Python (`ts_<company>` normalised); they must
match `routing::tenant_name_for` for companies that only use `[a-z0-9_]`,
which the `ts-*` companies do. Traffic: per-worker ticket walk (create ->
assign -> note -> resolve) with per-tenant command tokens; pgbench
(`-c 4 -j 2 -s 10 -T <seconds>`) against `<db>_pgbench` the whole time.

Outputs: `<label>.json` (aggregate accepted/s + pgbench TPS/latency),
`<label>-samples.csv` (per-second sequence heads per context),
`<label>-latency.csv` (every foreground command).

## Campaign

    python3 seed.py <server> <out-dir>            # ts_base template (~10 min)
    ./run-all.sh <server> <provisioner> <replicator> <out-dir>

Shared baseline first, then 1/4/16 tenants. Never run two at once; keep the
box quiet.
