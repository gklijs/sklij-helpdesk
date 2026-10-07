# Projection lag report — 2026-10-09 (issue #27, skilj main @ 61c84b8)

Issue #27 asked for the gap between a bounded context's latest committed
sequence and an async projection's `caught_up_to` as a metric, a Grafana
panel for it, and the lag at each load-ramp step.

## What was added

- **`src/projection_lag.rs`** samples every bounded context, shared and
  tenant alike, through skilj's public `db` API: the latest sequence
  minus `caught_up_to` for each projection whose `sync()` is false. For
  a partitioned projection `caught_up_to` is already the slowest
  partition's position. Sync projections are left out (0 by
  construction). Today that means `CompanyActiveTickets`, once per
  context.
- **`skilj_helpdesk.projection_lag`**, a gauge labelled by
  `bounded_context` and `projection`, recorded every 5s by
  `server.rs`'s `run_projection_lag_gauge_loop` when telemetry is on.
  A series that disappears (a rebuild made the projection sync) is
  recorded as 0 once, the same way the parked-deliveries gauge clears.
- **Grafana:** an "Async projection lag" row with the worst lag
  (yellow from 200, red from 1000) and the ten furthest-behind series.
- **`tests/projection_lag.rs`:** lag shows while catch-up hasn't run,
  and goes back to 0 once it has, even when the last event is one the
  projection doesn't consume (catch-up advances `caught_up_to` to the
  head, not to the last event it folded).

The gauge was checked against an independent SQL sample taken
alongside it: the medians in the table below agree within a few
sequences at every step.

## Step results

Same ramp as `docs/async-projection-report-2026-10-06.md`:
`SEED_DEMO_TRAFFIC=1`, 8/20/40/80 workers at 500/250/100/50ms, 4 minutes
per step, a fresh database and server per step, Postgres 18.6 on disk,
`HTTP_MAX_IN_FLIGHT_REQUESTS=70`, `MALLOC_ARENA_MAX=2`, release build.
Unlike earlier rounds, OTel export was on (the gauge is what's being
measured), to the `observability/` stack at a 5s export interval.

| step | fired | accepted | rejected | failures | accepted/s | lag median / p95 / max (gauge) | lag median / max (SQL, 5s) | RSS start → peak |
|---|---|---|---|---|---|---|---|---|
| 2 (8w @ 500ms) | 3582 | 3162 | 420 | 1 | 13.23 | 3.5 / 8 / 10 | 3 / 8 | 45 → 112MB |
| 3 (20w @ 250ms) | 8438 | 7309 | 1129 | 26 | 30.48 | 14.5 / 32 / 37 | 12 / 35 | 71 → 682MB |
| 4 (40w @ 100ms) | 7580 | 6592 | 988 | 275 | 27.38 | 13.5 / 66 / 70 | 19 / 87 | 235 → 913MB |
| 5 (80w @ 50ms) | 7147 | 6254 | 893 | 30890 | 25.77 | 29.5 / 35 / 56 | 29.5 / 71 | 292 → 1150MB |

- **Lag stays in the tens of sequences** on the build before the `CompanySnapshot` fix (see the next section for after). Median 4–30, worst sample 87,
  in line with 10-06's median ~20 / max 73. With catch-up polling every
  500ms, that's well under a second behind at these rates. Nothing
  grows over a step; catch-up keeps up at saturation.
- **Failures** are the usual shape: steps 3–4 are transport errors in
  the last 2 seconds, at `SIGINT`; step 5 is 28,401 load-shed `503`s
  plus transport errors at shutdown. 0 `ERROR` lines, 0 panics.
- **Accepted/s** is not comparable with earlier reports' numbers; see
  the next section.
- **RSS peaks 4–5x lower than 10-06** (step 4: 913MB vs 4.9GB). That is
  the `tenant_access::recorded_company_tenants` fix from issue #26's
  rebuild report, not this change.

## After the `CompanySnapshot` fix

`docs/create-ticket-latency-report-2026-10-09.md` found that
`CreateTicket` replayed its company's whole history and fixed it, and
gave the seed's companies their own customers. The same ramp on that
build, OTel on:

| step | fired | accepted | rejected | failures | accepted/s (before) | lag median / p95 / max (gauge) | lag median / max (SQL) | RSS start → peak |
|---|---|---|---|---|---|---|---|---|
| 2 (8w @ 500ms) | 3726 | 3211 | 515 | 0 | 13.48 (13.23) | 5 / 8 / 8 | 4.5 / 8 | 42 → 61MB |
| 3 (20w @ 250ms) | 17747 | 15115 | 2632 | 31 | 63.23 (30.48) | 25 / 61 / 72 | 28 / 60 | 56 → 104MB |
| 4 (40w @ 100ms) | 26478 | 22898 | 3580 | 3728 | 95.56 (27.38) | 3004 / 4943 / 5437 | 3065 / 5412 | 100 → 349MB |
| 5 (80w @ 50ms) | 23071 | 19730 | 3341 | 38015 | 81.79 (25.77) | 3088 / 4205 / 4677 | 2994 / 4594 | 167 → 460MB |

- **Up to step 3, lag stays in the tens**, as before, now at twice the
  throughput.
- **At steps 4–5 it grows to thousands**: commands now arrive faster
  than async catch-up folds them, so `CompanyActiveTickets` falls
  behind for as long as the load lasts. This is the case the gauge is
  for, and what its yellow (200) and red (1000) thresholds catch. The
  cause is upstream catch-up throughput; see the latency report's
  "What's left".
- Failures: steps 3–4 are transport errors at `SIGINT`; step 5 is
  30,850 load-shed `503`s plus transport errors at shutdown.

## Throughput: not comparable with 10-06, and skilj main is ~13% faster than 0.0.9

Accepted/s at steps 3–5 is 26–30, against 10-06's 45–55. That is the
environment, not the code: `4db65de`, the exact build 10-06 measured
(on released skilj 0.0.9), gets the same 26–27/s here today. So
absolute numbers from different days on this box aren't comparable;
only back-to-back runs are.

Back to back, step 4 (40 workers @ 100ms), OTel off, alternating:

| build | accepted/s per run | mean | slow statements per run |
|---|---|---|---|
| `4db65de` (10-06 code), skilj 0.0.9 | 27.37, 25.96 | 26.7 | 41, 42 |
| this change, released skilj 0.0.9 | 25.82, 26.19 | 26.0 | 48, 48 |
| this change, skilj main (`[patch.crates-io]`) | 28.90, 30.05 | 29.5 | 27, 26 |

- **skilj main is ~13% faster than 0.0.9** on the same helpdesk code,
  and has about half the slow statements (all of them waits on the
  batch lock, `SELECT next_value FROM "bc_helpdesk".sequence ... FOR
  UPDATE`). Consistent across both pairs, but modest.
- **The helpdesk changes since 10-06 cost nothing** (26.7 vs 26.0 on
  the same skilj).
- **Why the gain is modest:** 40 closed-loop workers at ~30/s is about
  1.3s per command, on a disk that fsyncs in 2.2ms (`pg_test_fsync`).
  The round trips skilj main saves are milliseconds each against that.
  Where the rest goes in the helpdesk's command path is not
  investigated here.

A further step-4 pair with OTel on: skilj pool 10 gave 30.81/s and pool
80 27.38/s, within noise of each other and of the OTel-off runs.

### The pool fix in this round

`server.rs` built a `DATABASE_MAX_CONNECTIONS`-sized pool (90) but never
passed it to `Skilj::builder`. So skilj itself always ran on sqlx's
default of 10 connections, and that 90-connection pool only served
`server.rs`'s own setup, the routing guard and the reconcilers. That
explains `docs/pool-tuning-report-2026-10-06.md`'s "it never grew past
14". It applies to every report since 09-17: their skilj pool was 10.

Now `DATABASE_MAX_CONNECTIONS` (default 80) sizes skilj's pool through
`.pool_options(skilj::default_pool_options()...)`, and the server's own
pool is `DATABASE_APP_MAX_CONNECTIONS` (default 10). The A/B above shows
it makes no measurable throughput difference at step 4: skilj caps
concurrent batch leaders at half the pool, and the batch lock, not the
pool, is the bottleneck. It does make the knob do what its comment
always said it did. (The "released skilj 0.0.9" arm above used
`PgPoolOptions::new()` instead of `skilj::default_pool_options()`,
which 0.0.9 doesn't have.)

## Setup notes

- Samples: the gauge read back from Prometheus over each step's window
  (46 samples per step), an SQL sample of the same quantity every 5s,
  and server RSS every 15s.
- Counts come from the seed's own `demo-seed: fired fake command` and
  `demo-seed: request failed` lines, as in 10-06.
