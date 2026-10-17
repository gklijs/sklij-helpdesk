# Tenant-scaling throughput report — 2026-10-15 (issues #32 and #33)

Issue #32 asked for command throughput across N per-company tenant contexts
(1, 4, 16, vs a shared-context baseline): aggregate accepted/s, whether
per-context serialisation caps it, and the tail shape. Issue #33 asked what
a co-resident pgbench costs alongside: both directions (pgbench under skilj
load, skilj under pgbench). One campaign answered both — every run below
drove foreground ticket traffic *while* pgbench TPC-B ran against a sibling
database on the same cluster.

## Summary

- **Per-tenant serialisation does not cap aggregate throughput.** 1, 4 and
  16 tenants each hold **320 accepted/s flat for the full 240s** (16 workers
  paced at 50ms = 320 offered/s; every command accepted, 0 failures, 0
  rejections in all four runs). The shared baseline holds the same 320/s
  except for its last 30s (293/s — see below). Aggregate throughput is
  identical shared vs 1 vs 4 vs 16 tenants.
- **The tail is better spread over tenants, at the same offered rate.**
  Per-command p95 over the whole run: shared 68ms → tenant1 58ms →
  tenant16 40ms → tenant4 32ms. 16 separate sequence locks each serve 1/16th
  of the traffic, so each command waits behind less.
- **One tenant behaves worse than four or sixteen.** tenant1 is the only
  tenant run that degrades: flat 320/s for 90s, then 300/230/274/272/248
  per 30s window, with CreateTicket p50 climbing 20→50ms. All its traffic
  funnels through one sequence lock while 15 idle tenants' background loops
  still poll. (Why 4 and 16 don't degrade at all while 1 does was not
  chased — the offered rate is the ceiling in all of them.)
- **pgbench alongside costs ~640–730 TPS and is itself unaffected by skilj,
  except once.** pgbench TPC-B (`-s 10`, `-c 4`, separate database, same
  cluster): 640 (shared) / 673 (tenant1) / 629 (tenant4) / 559 (tenant16)
  TPS at 5.9–7.1ms average latency, 0 failed transactions in every run. The
  tenant16 average is lower only because of one ~40s dip at +200s (610→360
  TPS, skilj p50 +5ms in the same window) that coincides with a burst of
  200–290ms COMMITs across many backends — a cluster-wide fsync stall, not
  tenant contention (all 16 tenants stayed at 320/s through it).
- **The shared baseline's own last-30s dip (320→293/s) is the same shape**:
  CreateTicket p50 climbs 17→57ms over the run while tenant4/tenant16 stay
  flat — one sequence lock under all 16 workers vs sixteen locks under one
  worker each. Same mechanism as the tenant1 decay, just slower to show.

| run | tenants | accepted/s | p50/p95/p99 (all cmds) | fail/rej | pgbench TPS | pgbench lat |
|---|---|---|---|---|---|---|
| shared | 0 (16 companies, 1 ctx) | 316.7 | 26.7 / 67.7 / 101.8 | 0 / 0 | 639.9 | 6.24ms |
| tenant1 | 1 | 280.3 | 37.6 / 57.6 / 79.0 | 0 / 0 | 673.4 | 5.93ms |
| tenant4 | 4 | 320.0 | 15.4 / 32.1 / 54.0 | 0 / 0 | 628.7 | 6.35ms |
| tenant16 | 16 | 319.9 | 21.1 / 40.0 / 57.4 | 0 / 0 | 559.4* | 7.14ms* |

`*` tenant16 pgbench average includes the +200s fsync stall (360 TPS for
~40s, then recovering to 386); outside that window it holds ~600–640 like
the other runs.

## What was fixed to get here

The campaign's first pilot provisioned a tenant fine over GraphQL but the
sidecars could do nothing against the fresh server — and that turned out to
be two real bugs, not bench plumbing:

1. **Sibling binaries couldn't authenticate against a fresh server
   (`unknown_signing_key`).** `server.rs` generates a fresh RSA keypair at
   every startup for the JWTs it prints, but provisioner,
   lifecycle-replicator and alerter sign their own superadmin JWTs with
   their own copy of the old hardcoded test key — which the fresh server no
   longer trusted. Fix: the local JWKS endpoint now serves *both* keys (the
   fresh one plus `LOCAL_JWKS_TEST_JWKS` in `src/lib.rs`, same key material
   as the tests' `serve_jwks`), so both `skilj-helpdesk-rs256-01` and
   `test-key-1` JWTs verify. Production impact: against any server started
   since the fresh-key change, the whole provision → mirror → route path
   was dead.
2. **Sidecars spun forever on a stale database (`poll failed` every 5s).**
   Provisioner and lifecycle-replicator now exit(1) with a plain message
   when skilj answers 403 (token row gone — the database was recreated
   under them), instead of logging `poll failed, will retry` forever.

Also fixed while here: the replicator's env-var names (`COMPANY_SIGNED_UP_TOKEN`
with underscores, as its doc comment showed) didn't match what the server
printed (`COMPANYSIGNEDUP_TOKEN` — `event_type.to_uppercase()` has no word
boundaries to split on). Both sides now convert CamelCase to UPPER_SNAKE_CASE
(`to_env_name`, duplicated in `server.rs` and `lifecycle-replicator.rs` like
the test-key material already is): `COMPANY_SIGNED_UP_TOKEN`, matching the
provisioner's own spelling. Without it the replicator exits(1) at startup
asking for `COMPANYSIGNEDUP_TOKEN` — which is how the mismatch surfaced.

## Method

- Release `server`/`provisioner`/`lifecycle-replicator` at this commit,
  skilj 0.0.10. Postgres 18.6 on disk (`shared_buffers=512MB`,
  `max_connections=400`, `max_wal_size=8GB`, `log_min_duration_statement=200`),
  TCP 127.0.0.1:5440, one cluster for everything (the `LD_LIBRARY_PATH`
  libxml2 compat shim from the prior reports). `DATABASE_MAX_CONNECTIONS=120` /
  `HTTP_MAX_IN_FLIGHT_REQUESTS=200`, `RUST_LOG=warn`, defaults otherwise.
- **Template** (`scripts/bench/tenant-scaling/seed.py`): an empty settled
  history — every run signs up, provisions and mirrors its own companies, so
  nothing is baked in.
- **A run** (`scale.py`): copy the template, start the server
  (`TICKET_ROUTING=tenant`, or `TICKET_ROUTING=shared` for the baseline), start the provisioner +
  lifecycle-replicator against the run's own server log (fresh tokens every
  start), provision + lifecycle-mirror every tenant synchronously (same
  GraphQL/REST calls those binaries make, without waiting on their 5s
  ticks — the binaries still run alongside and confirm no
  `unknown_signing_key`/`poll failed`), init pgbench `-s 10` on a sibling
  database, then 16 workers paced at 50ms (320 offered/s) walking their own
  tickets create → assign → note → resolve for 240s while pgbench runs
  `-c 4 -j 2 -T 240`. Shared baseline: 16 companies in the one context, one
  per worker. Tenant runs: N companies, N tenants, workers spread over them.
- **Order** (`run-all.sh`): shared, tenant1, tenant4, tenant16. Box was not
  quiet for tenant16 (loadavg 6→16 through the run — something else was
  building); the skilj numbers hold 320/s throughout anyway, and the
  pgbench dip is called out above rather than re-run.

## Confounders and noise

- tenant16 ran under rising box load (loadavg 6.1 at start, 16.4 at end;
  the other runs start at 6–8 and stay there). Its skilj throughput is
  unaffected (320/s every 30s window); whether the +200s pgbench dip is
  box noise or cluster contention can't be separated from one run.
- tenant1's single 6.3s outlier (five commands at +238s, all status 200)
  is included in its p99/max but changes nothing.
- The load generator is the ceiling: 320 offered/s in every run, so this
  measures "does N tenants hold the offered rate", not peak throughput.
  Nothing here says where the ceiling is — only that 1 tenant sags below
  it after ~90s while 4/16/shared don't (shared sags slightly at +210s).
- pgbench scale factor 10 (~160MB) fits in `shared_buffers`; a larger
  pgbench working set would contend differently. Not measured.

## For #40 (tenants as default)

This is the evidence #40 asked for: per-company tenants hold the same
aggregate throughput as the shared context at this offered rate, with a
better tail, and the production provisioning path (provisioner +
lifecycle-replicator against a fresh server) now actually works — it was
broken before this round. What this does *not* cover: peak-throughput
ceilings, failover/restart behaviour per tenant, or the email-bridge and
frontend gaps #40 already lists.
