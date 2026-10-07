# Create-ticket latency report — 2026-10-09 (skilj main @ 61c84b8)

The load ramp's accepted/s looked like it had halved since
`docs/async-projection-report-2026-10-06.md`. It hadn't: the 10-06 build,
rerun today, gives the same numbers (`docs/projection-lag-report-2026-10-09.md`).
But that check showed where the time actually goes, and that can be
fixed.

## Summary

- **Throughput decays within every run**, on every build tried
  (10-06's, today's, skilj 0.0.9 and main): ~85 accepted/s in the first
  30 seconds, ~14/s after 4 minutes, at 40 workers @ 100ms. A 4-minute
  average mostly measures how far the decay got. That is also why
  numbers from different days don't compare.
- **The cause is `CreateTicket` reading its company's whole history**,
  under the bounded context's lock. It is tagged `company`, and so are
  `TicketCreated` and `TicketInternalNoteAdded` (projections need it to
  scope rows to their company). So each new ticket replays every ticket
  its company ever filed. With the seed's 3 companies, the batch's
  `decide` phase went from 49 to 604ms per batch in 2 minutes. Spreading
  the same load over 300 companies took a 2-minute run from 43 to 193
  accepted/s.
- **Fixed with `CompanySnapshot`:** the company's status and the ticket
  ids it has created, kept per company by skilj's snapshot catch-up.
  `CreateTicket` decides from that row plus the events since, the way
  every single-ticket command already uses `TicketSnapshot` (issue #14).
  No migration: the snapshot folds existing history in the background.
  **27.7 → 105 accepted/s** at step 4, back to back, both pairs.
- **The seed filed every ticket under 4 shared customers**, across all
  companies, which a customer can't do (`entity Customer` belongs to one
  company). That grew 4 `CustomerTickets` rows to several MB each,
  rewritten on every batch. Each company now has its own 25 customers.
  It removes the growing `persist` phase, but doesn't change
  throughput: the next bottleneck is reached first.
- **The next bottleneck is in skilj: snapshot catch-up folds one event
  per round trip.** Projection catch-up folds 100 events per
  transaction since skilj's Codeberg issue #52; snapshot catch-up still
  locks, folds and rewrites the row once per event, about 50 events/s
  here. At 100 commands/s it falls behind, so `CreateTicket`'s "events
  since the snapshot" grows again, and so does `decide`. The decay is
  much slower than before, but it's still there.

## Where the time went

skilj logs one line per command batch at `skilj_core::db=debug`
("command batch phase timing"), and the lock wait at
`skilj_core::command_batcher=debug`. Averages per 30s window, step 4, a
2-minute run on the build before the fix:

| window | batches | mean batch | decide ms | persist ms | commit ms | lock wait ms (median / p90) |
|---|---|---|---|---|---|---|
| 0–30s | 408 | 7.0 | 48.9 | 10.2 | 4.7 | 7.9 / 170.7 |
| 30–60s | 93 | 13.6 | 265.0 | 23.1 | 6.4 | 208.5 / 557.1 |
| 60–90s | 61 | 17.7 | 438.5 | 21.3 | 6.8 | 402.1 / 667.8 |
| 90–120s | 43 | 20.0 | 603.9 | 22.5 | 6.5 | 551.3 / 713.4 |

`decide` runs under the lock, so every other writer waits for it: the
lock wait grows with it, and the seed, which waits for each answer,
slows down. Commit (2.2ms per fsync on this disk, `pg_test_fsync`) is
never the problem.

## Back to back

Step 4 (40 workers @ 100ms), 4 minutes, OTel off, builds alternating:

| build | accepted/s per run | slow statements | lag median / max (SQL) | RSS peak |
|---|---|---|---|---|
| before | 27.54, 27.96 | 47, 40 | 18.5 / 70, 18.5 / 98 | 1035, 1147MB |
| `CompanySnapshot` | 103.83, 106.03 | 0, 0 | 966 / 2043, 1037 / 2319 | 516, 510MB |
| `CompanySnapshot` (second pair) | 102.27, 103.64 | 0, 0 | 845 / 2118, 1073 / 2223 | 529, 516MB |
| `CompanySnapshot` + seed customers | 97.06, 97.83 | 0, 0 | 2966 / 5733, 3178 / 5633 | 355, 382MB |

Accepted/s per 30s window:

| build | 0 | 30 | 60 | 90 | 120 | 150 | 180 | 210s |
|---|---|---|---|---|---|---|---|---|
| before | 82.6 | 31.7 | 25.5 | 19.3 | 18.7 | 15.2 | 13.9 | 14.5 |
| `CompanySnapshot` | 230.7 | 136.7 | 106.7 | 88.0 | 79.3 | 69.9 | 70.7 | 64.9 |
| + seed customers | 235.4 | 137.6 | 86.8 | 79.4 | 71.8 | 67.1 | 53.4 | 49.5 |

- The seed change is within noise on throughput (97 vs 103). Its
  `persist` phase stays at 3–15ms per batch instead of growing to over
  100ms, and RSS is lower. The lag is higher only because it reaches the
  catch-up bottleneck below sooner.
- Failures, not shown, are transport errors in the last seconds at
  `SIGINT`. The faster builds have more in flight when the server stops.

## What's left

**Snapshot catch-up (skilj).** `catch_up_partitioned_snapshot` takes
each event in its own `get_or_create_snapshot_state_for_update` and
`UPDATE`, and for the first event of a new tag value it reads that
value's whole history. The debug log shows one event resolved about
every 20ms. Projection catch-up had the same shape until Codeberg #52
chunked it: read each key once per chunk, fold in memory, write once.
The same change for snapshots is the next step, upstream.

**Async projection lag at saturation.** `CompanyActiveTickets` also
falls behind at 100 commands/s: a 1000-event tick took ~16s by the end
of a run. Not investigated here. The `skilj_helpdesk.projection_lag`
gauge (issue #27) is what shows it.

**The company lifecycle commands** (`SignUpCompany`, `ConvertCompanyTrial`,
`ExpireCompanyTrial`, `ReactivateCompany`, `RecordCompanyTenant`,
`RecordTenantLifecycle`) still replay the company's whole history. They
run a few times per company, so they were left alone; if that changes,
`CompanyFacts` can carry what they need.

## Method

Same harness as `docs/projection-lag-report-2026-10-09.md`: a fresh
database and server per run, Postgres 18.6 on disk, release builds,
`HTTP_MAX_IN_FLIGHT_REQUESTS=70`, `MALLOC_ARENA_MAX=2`,
`RUST_LOG=info,skilj_core::command_batcher=debug,skilj_core::db=debug`.
The 300-company run set `SEED_DEMO_COMPANIES=300`. Postgres activity was
sampled from `pg_stat_activity` every second for the catch-up findings.
