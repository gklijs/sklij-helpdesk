# Pool tuning report — 2026-10-06 (issue #23, skilj 0.0.9)

Issue #23 asked to run the load ramp with skilj's new
`SkiljBuilder::pool_options_performance_optimized()` (skilj `ee3086a`,
unreleased) against the current `DATABASE_MAX_CONNECTIONS=90` /
`HTTP_MAX_IN_FLIGHT_REQUESTS=70`, and either adopt it or say why not.

**Verdict: not adopted.** Throughput is the same within noise, the
profile holds about 2.5x as many Postgres connections for nothing, and
its size depends on the core count of whatever box it lands on. The
current 90/70 stays.

## What the profile is

`server.rs` builds its pool with `db::connect_with`, not through
`SkiljBuilder`, so the profile was reproduced with env vars instead of
calling the method. Its four settings, against what this server
already had:

| setting | profile | current |
|---|---|---|
| `max_connections` | 2x CPU (44 on this 22-core box) | 90 |
| `min_connections` | half of max (22) | 0 |
| `acquire_timeout` | 30s | 30s (sqlx 0.9 default) |
| `idle_timeout` | 10min | 10min (sqlx 0.9 default) |

Only the connection counts differ. `DATABASE_MIN_CONNECTIONS` (default
`0`) was added to `server.rs` to express the second one; it stays, as a
plain knob.

## Summary

- **Throughput: a tie.** Profile against current, same binary, same
  box, back to back:

  | step | offered (nominal) | current 90/0, cap 70 | profile 44/22, cap 70 | delta |
  |---|---|---|---|---|
  | 2 (8w @ 500ms) | ~16/s | 13.21/s | 13.16/s | −0.4% |
  | 3 (20w @ 250ms) | ~80/s | 43.68/s | 46.03/s | +5.4% |
  | 4 (40w @ 100ms) | ~400/s | 55.25/s | 54.52/s | −1.3% |
  | 5 (80w @ 50ms) | ~1600/s | 56.78/s | 55.13/s | −2.9% |

  The deltas change sign from step to step and stay well inside the
  sub-10% band earlier reports treated as noise. Steps 4–5 match
  `docs/async-projection-report-2026-10-06.md`'s async run (54.93/s,
  54.73/s).
- **The pool isn't the limit, at either size.** The current setup never
  opened more than 14 connections, and at most 6 were `active` in any
  sample. That matches 09-18's 13–14. A cap of 44 or 90 is never
  reached, so lowering it changes nothing. Zero `PoolTimedOut` and zero
  slow-acquire warnings in any step of any variant.
- **What the profile does change is idle connections.** Its 22 warm
  connections sat idle the whole run: 32–33 connections held against
  12–14. On a Postgres with the default `max_connections=100`, shared
  with `alerter`, `engagement-watcher` and the per-tenant instances in
  `docs/partitioned-projection-report-2026-10-05.md`, that's budget
  spent on nothing. The pool was never short of connections, so keeping
  some warm buys nothing.
- **"2x CPU" ties the pool size to the host.** It gives 44 here, but 8
  on a 4-core container. That's below the 10-connection sqlx default
  that `docs/load-test-report-2026-09-17.md` found to be the real
  ceiling, and far below `HTTP_MAX_IN_FLIGHT_REQUESTS=70`. The in-flight
  cap only works if it sits below the pool size (see the comment on
  `http_max_in_flight` in `server.rs`). With 70 in flight against a
  smaller pool, requests queue on `acquire_timeout` instead of getting a
  fast 503. The two limits have to be sized together, and a CPU-derived
  pool size can't follow an env-configured in-flight cap.
- **Lowering the in-flight cap to match makes things worse.** The third
  variant kept the profile but restored the current 20-connection gap
  (44 − 24). It sheds far too early:

  | step | profile, cap 70 | profile, cap 24 |
  |---|---|---|
  | 4 | 54.52/s, 146 failures (all at shutdown) | 55.03/s, **28,225 failures** (28,113 are 503s) |
  | 5 | 55.13/s, 41,724 failures | **45.44/s (−18%)**, **233,200 failures** |

  At step 5, 503s come back so fast that the seed's workers spin
  through ~4.3x as many requests, and the shedding itself costs enough
  to drop accepted throughput by 18%. The pool still had slack: at most
  4 connections were `active`. The cap of 24 just counts in-flight
  requests that are mostly not using a connection.

## Step results

Same conventions as 10-01 and the async report: accepted/s is accepted
commands divided by the wall time from first to last fired command;
connection counts are `pg_stat_activity` rows for the `helpdesk`
database (all states / `active` only), max over 17 samples.

| variant / step | fired | accepted | rejected | failures (503) | accepted/s | conns max (active max) | max idle in tx | RSS peak |
|---|---|---|---|---|---|---|---|---|
| current 2 | 3675 | 3152 | 523 | 0 (0) | 13.21 | 12 (2) | 0.000s | 217MB |
| current 3 | 12255 | 10454 | 1801 | 18 (1) | 43.68 | 13 (2) | 0.014s | 1980MB |
| current 4 | 15204 | 13227 | 1977 | 126 (1) | 55.25 | 14 (6) | 0.025s | 4305MB |
| current 5 | 15807 | 13680 | 2127 | 41618 (40604) | 56.78 | 13 (4) | 0.002s | 5688MB |
| profile, cap 70, 2 | 3677 | 3139 | 538 | 0 (0) | 13.16 | 32 (1) | 0.000s | 204MB |
| profile, cap 70, 3 | 12831 | 10994 | 1837 | 14 (0) | 46.03 | 33 (6) | 0.003s | 1986MB |
| profile, cap 70, 4 | 15137 | 13070 | 2067 | 146 (0) | 54.52 | 33 (4) | 0.052s | 4443MB |
| profile, cap 70, 5 | 15223 | 13205 | 2018 | 41724 (40593) | 55.13 | 33 (6) | 0.300s | 5654MB |
| profile, cap 24, 2 | 3682 | 3155 | 527 | 1 (0) | 13.23 | 32 (1) | 0.000s | 208MB |
| profile, cap 24, 3 | 12874 | 11103 | 1771 | 7 (0) | 46.46 | 33 (3) | 0.023s | 2059MB |
| profile, cap 24, 4 | 15252 | 13168 | 2084 | 28225 (28113) | 55.03 | 33 (3) | 0.010s | 3510MB |
| profile, cap 24, 5 | 12763 | 10976 | 1787 | 233200 (230569) | 45.44 | 33 (4) | 0.103s | 2383MB |

- **Failures at steps 2–4 are shutdown artifacts**, except profile/cap
  24 step 4. Every one of them is logged after the server's own
  `shutdown signal received` line, within the last 0.6s. Step 5's are
  load-shed 503s, as in every prior report. The rest are connection
  errors at shutdown.
- 0 panics, 0 `ERROR` lines in any step.
- **Max idle in tx** is single-sample and noisy (one 0.300s sample in
  profile/cap 70 step 5). Every other sample was under 55ms.
- **RSS goes with requests held in memory, not with the pool.** Cap 24
  peaked at 3510MB/2383MB at steps 4/5 against ~4.4GB/~5.7GB for both
  cap-70 variants, while all three held the same connections. That's a
  lead for issue #34: memory growth follows the HTTP in-flight cap.

## Setup

- `cargo build --release --bin server` once; all three variants are the
  same binary, differing only by env.
- Postgres 18.6 from `~/.theseus/postgresql/18.6.0`, one disk-backed
  cluster, default `max_connections=100`, database dropped and
  recreated before every step.
- Same ramp as 10-01 and the async report: `SEED_DEMO_TRAFFIC=1`,
  8/20/40/80 workers at 500/250/100/50ms, 4 minutes per step, a fresh
  server per step, `RUST_LOG=info,skilj_core::db=debug`. Run order:
  profile/cap 70, current, profile/cap 24.
- Variants (`DATABASE_MAX_CONNECTIONS` / `DATABASE_MIN_CONNECTIONS` /
  `HTTP_MAX_IN_FLIGHT_REQUESTS`): current 90/0/70, profile/cap 70
  44/22/70, profile/cap 24 44/22/24.
- Samples every 15s (17 per step): server RSS, `pg_stat_activity`
  connection counts, max `idle in transaction` age.
- Counts come from the seed's `demo-seed: fired fake command`
  (`accepted=true/false`) and `demo-seed: request failed` log lines.

## If this is revisited

The profile could make sense if the pool ever becomes the limit:
`active` connections close to the cap, or `PoolTimedOut` under load.
Even then, the in-flight cap should be derived from the pool size
(pool minus the background loops' share) rather than set separately.
Any warm minimum should be small, because `alerter` and per-tenant
instances share the same Postgres.
