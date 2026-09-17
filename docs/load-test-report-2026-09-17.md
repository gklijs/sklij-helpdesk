# Load test report — 2026-09-17

Load-ramp testing of `server` under increasing demo-traffic, with the full
OTel/Prometheus/Grafana/Tempo/Loki stack attached, run locally in a
sandboxed dev environment. Three ramps were run in total as the
investigation went deeper; this report is the final, corrected picture.
See "Open decision" at the end for what happens next (a 24h soak was the
original ask — see why it hasn't run yet).

## Summary

- A connection-pool fix (sized `max_connections`, plus a load-shed/
  concurrency-limit layer) was implemented and **did fix what it targeted**:
  the pool no longer sits pinned at 10 connections, and unbounded memory
  growth under overload is gone (confirmed below).
- That fix did **not** raise the app's real throughput ceiling. Digging
  into why revealed the actual dominant bottleneck: `skilj-core` commits
  every single write command in a bounded context through **one lock**
  (a `SELECT ... FOR UPDATE` on that bounded context's own sequence row),
  serializing all writes — across every company, every ticket — through a
  single point, by design (it's what keeps the event sequence gapless).
  No pool size or concurrency limit in skilj-helpdesk can change that.
- Measured real throughput: comfortably handles ~12-16 commands/sec for
  the whole app, degrades hard by ~80/s offered, and collapses entirely
  above a few hundred/sec offered.
- Two testing-methodology bugs (both in my own test harness, not the app)
  were found and corrected mid-investigation — see "Methodology
  corrections" below. They inflated earlier readings; this report's
  numbers are from the corrected run.

## Setup

- **App**: `cargo build --release --bin server`, against a throwaway local
  Postgres 18.6 instance and a local Dex OIDC instance.
- **Telemetry**: `observability/docker-compose.yml` (OTel Collector,
  Prometheus, Tempo, Loki, Grafana). Prometheus remapped to host port 9091
  for this run only (9090 was already bound by an unrelated local tool);
  nothing in the repo changed.
- **Load**: the repo's own built-in fake-traffic generator
  (`SEED_DEMO_TRAFFIC=1`, `src/demo_seed.rs`), ramped by restarting the
  server between steps with increasing `SEED_DEMO_CONCURRENCY` / decreasing
  `SEED_DEMO_INTERVAL_MS` (both read once at startup).
- **Steps** (4 min each): concurrency×interval → nominal target rate — 2,
  8, 20, 40, 80 workers at 1000/500/250/100/50ms intervals → 2, 16, 80,
  400, 1600 commands/sec nominal. "Nominal" because each worker waits for
  its response before its next attempt, so real offered load self-throttles
  once responses get slow — itself one of the findings.

## The fix that was applied

1. **`DATABASE_MAX_CONNECTIONS`** (`src/bin/server.rs`): `server.rs` used
   to call `skilj_core::db::connect`, sqlx's bare `PgPoolOptions::new()`
   default — **10 max connections**, no override, ever. Now uses
   `db::connect_with(..., PgPoolOptions::new().max_connections(N))`, env-
   configurable, default 90 (comfortably under Postgres's own server-side
   default `max_connections` of 100, leaving headroom for `alerter`/
   `engagement-watcher`/manual `psql`).
2. **`HTTP_MAX_IN_FLIGHT_REQUESTS`** (`src/bin/server.rs`): a
   `tower::load_shed` + `tower::limit::ConcurrencyLimitLayer` pair around
   the merged REST+GraphQL router, default 70. Once that many requests are
   already in flight, any further request gets an immediate `503` instead
   of queuing in memory waiting on the DB pool.
3. `tower` moved from a dev-only dependency to a real one, features
   `limit` + `load-shed` (`Cargo.toml`).

These are real, working changes (compiles clean, verified live below) —
not yet committed to git, pending your go-ahead.

### Confirmed: the memory-growth risk from the first pass is fixed

Pre-fix, step 5 (1600/s nominal) grew the server process from 54MB to
**487MB in 4 minutes** while genuinely completing only ~2 commands/sec —
unbounded queuing with nothing shedding load. Post-fix, same step:

| step | server RSS, start → end (post-fix) | server RSS, start → end (pre-fix) |
|---|---|---|
| 2 (16/s nominal) | 33MB → 112MB | 42MB → 127MB |
| 3 (80/s nominal) | 36MB → 95MB | 54MB → 149MB |
| 4 (400/s nominal) | 37MB → **45MB** | 59MB → 269MB |
| 5 (1600/s nominal) | 43MB → **53MB** | 54MB → **487MB** |

Postgres's own connection count also stopped pinning at the old ~13-14
ceiling regardless of load — pool sizing is no longer the constraint.
**This part of the fix is validated and worth keeping regardless of
anything below.**

## Headline finding: one lock serializes every write in the bounded context

After the pool/load-shed fix, a manual test (20 workers, pool nowhere near
exhausted — `pg_stat_activity` showed only ~13 connections in use out of
90 available) still showed severely degraded throughput. The actual cause,
found in `skilj-core`'s own code and doc comments
(`skilj-core/src/db/mod.rs`, `submit_command`):

> Opens one transaction and locks `bounded_context`'s own `sequence` row
> up front — `SELECT ... FOR UPDATE` — ... no concurrent committer for the
> same bounded context can be interleaved while the row is locked.

This is deliberate: it's what makes `SequenceIsGaplessPerBoundedContext`
hold. But the practical effect is that **every command write in the
`helpdesk` bounded context — every company, every ticket — commits through
one shared lock, one at a time**, for the entire duration of dispatch +
insert-command + insert-events + sync-projection-updates. Direct evidence
from the server log under contention:

```
sqlx::query: slow statement: execution time exceeded alert threshold
  summary="SELECT next_value FROM \"bc_helpdesk\".sequence …"
  elapsed=60.201023934s slow_threshold=1s
```

(a second occurrence in the same window: `elapsed=30.056854584s`)

That's a single query — the lock acquisition — taking up to a minute under
load, with the connection pool nowhere near its own limit. **No pool size
or concurrency-limit tuning in skilj-helpdesk can fix this**: it's a
property of how `skilj-core` commits writes for an entire bounded context,
not of skilj-helpdesk's own connection handling.

### What this means for capacity

Total write throughput for the whole app is bounded by
`1 / (average time one command's full critical section takes while
holding the lock)` — not by worker count, connection pool size, or company/
ticket count. Real, corrected measurements (see "Methodology corrections"
for why step 1 is excluded and steps are counted from log lines, not the
earlier — unreliable — latency histogram):

| step | nominal target | accepted/s | rejected/s | client-failed/s |
|---|---|---|---|---|
| 2 (8 workers) | 16/s | **12.2** | 2.0 | 0.01 |
| 3 (20 workers) | 80/s | **4.8** | 0.68 | 0.39 |
| 4 (40 workers) | 400/s | **0.04** | — | 1.15 |
| 5 (80 workers) | 1600/s | **0** | — | 198.7 |

The app handles real, sustained load up to roughly the step-2 level
cleanly (~12-16 commands/sec across the whole app, not per company). By
step 3 it's already shedding/failing a meaningful fraction. By step 4 it's
essentially not completing new work. Step 5's very high failure rate is
actually the load-shed fix working as intended — fast `503`s instead of
the pre-fix 24-second hangs — but the underlying ceiling itself is
unchanged.

This is a single-bounded-context ceiling: this whole SaaS app — every
tenant company sharing one `helpdesk` bounded context — tops out around
12-16 sustained commands/sec **in total**, not per customer. That's the
number to have in mind for any real capacity planning, and it's a
`skilj-core` architecture question (one bounded context per deployment vs.
per tenant, batching commits, etc.), not something fixable inside
skilj-helpdesk.

## Second finding (unchanged): the request-duration histogram can't see real latency

`http.server.request.duration` (`skilj-rest`) uses OpenTelemetry's generic
default histogram bucket boundaries `[0, 5, 10, 25, 50, 75, 100, 250, 500,
750, 1000, 2500, 5000, 7500, 10000]` — sized for millisecond-scale values —
but the instrument records true fractional seconds. A real 11ms request
(`0.011`) is smaller than the *first* real bucket boundary (`5`), so
`histogram_quantile()` over this metric is actively misleading below
multi-second resolution: querying it during a light-load window (confirmed
healthy by direct `curl` timing) reported a p50 of ~2.5s and p95 of ~4.75s
— purely a bucket-interpolation artifact, not real latency. **The
provisioned Grafana dashboard's REST latency panel is not currently
trustworthy at low-to-moderate load.** This is a `skilj-rest` fix (a
custom View with sub-second bucket boundaries), not a skilj-helpdesk one,
but worth raising there. (This is also why this report's throughput table
above counts real log lines rather than trusting that histogram.)

*(Side note: the same histogram also carries `/v1/events/consume`, a
long-poll endpoint whose duration is long by design — any dashboard/alert
on this metric that doesn't filter by `http_route` will be dominated by
that route rather than real command latency.)*

## Methodology corrections (found mid-investigation, both in my own harness)

Being transparent about these since they affect how much to trust the
numbers above and in the original version of this report:

1. **Ticket-ID collision across restarts.** The ramp restarts the server
   between steps (needed — `SEED_DEMO_CONCURRENCY`/`SEED_DEMO_INTERVAL_MS`
   are read once at startup). `demo_seed.rs` derives each fake ticket's ID
   from its worker index (`seed-ticket-w{n}-0`, ...), which resets to 0 on
   every restart. The very first version of this ramp kept the *same*
   Postgres database across all restarts (specifically to let data volume
   grow realistically) — but that meant a later step's worker `n` would
   immediately collide with worker `n`'s identical ticket ID from an
   earlier step, get rejected, and (since the seed loop only advances its
   local state on success) retry that exact same doomed ID forever for the
   rest of the step. This inflated apparent contention in both of the
   first two ramps and made "the pool fix didn't help" look worse than
   reality. **Fix**: reset the database before every step.
2. **A silent `DROP DATABASE` failure.** The very first reset (start of
   the corrected ramp) failed — `database "skilj_helpdesk_dev" is being
   accessed by other users`, a leftover connection from my own manual
   debugging a moment earlier — but my script didn't check the exit code,
   so it silently proceeded against the *old*, not-actually-reset
   database. That contaminated step 1 of the corrected ramp with the same
   collision problem above. Confirmed via `postgres`'s own log
   (`grep -i "being accessed by" pgdata/log.txt`), and replaced with a
   clean, manually-verified light-load reading instead (all commands
   accepted, ~15ms each, matching the idle baseline) once a genuinely
   fresh database was confirmed.

Neither of these change the headline finding (the bounded-context lock is
directly confirmed in `skilj-core`'s own code and doc comments, independent
of any test-harness bug), but both inflated exactly how bad the measured
throughput collapse looked at the lower end. The table above is from the
corrected run.

## What this still doesn't tell us

- Whether the app has a slow leak or gradual degradation *independent* of
  the lock-contention ceiling — no step here ran long enough or held
  steady enough below the ceiling to see that.
- Anything about a deployment topology that isn't "one shared bounded
  context for the whole app" (e.g. per-tenant bounded contexts, if that's
  ever adopted) — the ceiling found here is specific to this app's current
  single-bounded-context design.

## Recommendation

1. **Keep the pool/load-shed fix** — it's real, validated, low-risk, and
   fixes a genuine unbounded-memory-growth failure mode regardless of the
   bigger finding above. Ready to commit.
2. **The throughput ceiling itself is a `skilj-core` question, not a
   skilj-helpdesk one** — raise it there. Options worth them evaluating:
   batching multiple commands into one lock acquisition, shortening the
   critical section (e.g. deferring sync-projection updates out of the
   locked transaction), or accepting the ceiling as a documented
   per-bounded-context limit and recommending per-tenant bounded contexts
   for higher-throughput deployments.
3. For the 24h soak specifically: running it *above* ~16 commands/sec
   would just re-confirm the same lock ceiling for 24 hours, not teach us
   anything new. It's more informative run **at or just under** the real
   ceiling (say, 10-12/s sustained) — that's the regime where a slow leak
   or gradual degradation would actually be visible instead of masked by
   lock contention.

## Open decision

Given the ceiling is architectural (in `skilj-core`, not something I can
fix inside this repo), how do you want to proceed:

- **(a)** Commit the pool/load-shed fix now, then run the 24h soak at a
  sustained ~10-12 commands/sec (under the discovered ceiling) to look for
  slow degradation independent of lock contention — the most informative
  option for "does this app degrade over 24 hours" specifically.
- **(b)** Run the 24h soak at the original increasing-load ramp shape
  anyway, to have a full-day record of exactly what "hitting the ceiling"
  looks like sustained (mostly re-confirms today's finding, but as a real
  24h artifact rather than a 20-minute one).
- **(c)** Hold off on the 24h soak until the `skilj-core` ceiling question
  is resolved there, since anything above ~16/s isn't really testing this
  app's own behavior right now.

## Raw data

Step boundaries, resource samples, server logs, and Prometheus query
results from all three ramps are in the session's scratchpad (`steps*.csv`,
`resource*.csv`, `server-step*.log`, `prom/*.json`) — not committed here
since they're single-run artifacts, not durable repo content. Ask if
you'd like them pulled into a durable location.
