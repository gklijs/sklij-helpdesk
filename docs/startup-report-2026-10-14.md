# Startup report — 2026-10-14 (issue #30, skilj 0.0.10, commit `18f302c` + #29 fix)

Issue #30 asked how long the server takes to start with 1M+ events across
the bounded contexts: `Skilj::build()`'s reconciliation, schema patches and
event-cache warm-up. It also asked whether 0.0.9's startup locking change
(skilj docs/architecture.md §158) holds up, and what a restart under load
costs.

## Summary

- **Startup time doesn't grow with history.** From an empty database to
  3.2M events the server is ready in a median 1.4–1.6 s, and `build()`
  takes 1.2–1.4 s, at every size:

  | history | events (helpdesk / activity / knowledge_base) | ready, median (min–max) | `build()` | of which schema patches | token minting after | RSS at ready |
  |---|---|---|---|---|---|---|
  | empty | 0 | 1.59 s (1.28–2.15) | 1.35 s | 0.40 s | 0.24 s | 32 MB |
  | base | 245k (213k / 13k / 19k) | 1.62 s (1.49–1.94) | 1.37 s | 0.58 s | 0.25 s | 51 MB |
  | h1m | 1.23M (1.07M / 66k / 95k) | 1.43 s (1.33–1.65) | 1.17 s | 0.32 s | 0.26 s | 50 MB |
  | h3m | 3.19M (2.77M / 173k / 247k) | 1.56 s (1.46–1.66) | 1.24 s | 0.46 s | 0.28 s | 50 MB |

  (6 starts per size, two passes in opposite size order.) Nothing in
  `build()` reads history in proportion to its size: the event-cache
  warm-up reads the last 1,000 events per context by primary key, the
  §158 patches are catalog lookups when nothing is missing, and
  reconciliation reads only registration tables. The ~0.75 s after the
  patches is reconciliation and warm-up over 5 contexts and ~80
  registrations; it doesn't move with size either.

- **A restart under load costs clients ~2–2.8 s; a second instance hides it
  completely.** Restarting the only instance while 8 clients send commands
  (8 × 250 ms, the ticket walk): clients see 2.0–2.8 s between their last
  success before the stop and the first after, at 1.2M and 3.2M alike.
  That is the 1.5–2.2 s to ready plus their reconnect pacing. Starting a
  second instance next to one serving the same clients: **0 failed
  requests** in 12 starts, and the serving instance's p99 during a start
  (20–130 ms) is no worse than before the start (44–94 ms in clean runs). No startup patch queued
  behind the running load, which is what §158 promised.

- **Found: skilj's due-deadline poll scans the whole `deadlines` table every
  tick.** It is not a startup cost, but it is the one thing in this issue
  that grows with history, and it showed up first in startup's Postgres
  samples. The query (`skilj-core` `db/mod.rs` ~13505):

  ```sql
  SELECT ... FROM {schema}.deadlines
  WHERE (status = 'pending' OR (status = 'firing' AND firing_at <= $1)) AND fire_at <= $2
    AND (next_attempt_at IS NULL OR next_attempt_at <= $2)
  ORDER BY fire_at LIMIT 1000
  ```

  The only index is `deadlines_due (fire_at) WHERE status = 'pending'`, and
  the `OR status = 'firing'` arm can't use it, so Postgres does a parallel
  sequential scan of every deadline row, every 500 ms, whether or not
  anything is due. Every ticket keeps three deadline rows (two SLA, one
  auto-close) until retention deletes them, so the table grows with
  history: 2.9M rows / 1.3 GB at 3.2M events. One poll there took
  **117 ms and read ~114k buffers (~900 MB)** to return nothing.

  **Adding `CREATE INDEX deadlines_firing ON deadlines (fire_at) WHERE
  status = 'firing'` makes the same, unchanged query a BitmapOr of two
  index scans: 0.02 ms.** (Tested on the 3.2M copy inside a rolled-back
  transaction.) That is a one-line addition to skilj's per-context
  provisioning/patch list.

  **What it costs at rest** (`idle_cost.py`: server running, no traffic,
  nothing due; Postgres CPU over 60 s; two runs each, alternating):

  | history | deadline rows | Postgres CPU, as is | the poll alone (pg_stat_statements) | with the `firing` index |
  |---|---|---|---|---|
  | 245k events | 225k | 19–20% of a core | 2.4–2.7 s per minute | 7–8% |
  | 3.2M events | 2.9M | **116–119% of a core** | 14.1–14.6 s per minute | 7–8% |

  An idle server on a 3.2M-event history keeps more than one Postgres core
  busy just looking for due deadlines, and the cost grows linearly with
  history. With the index, the remaining 7–8% is the same at both sizes
  (registration and cursor polls of the other loops). The server's own CPU
  is 3.4 s per minute in every case.

## Phases of a start

Timestamps come from the server's own output (`RUST_LOG=warn,
sqlx::postgres::notice=info`, nothing per request):

- spawn → `migrate`'s notice: the server's set-up before `build()` (pools,
  demo roles). ~0.01 s on a restart, ~0.7 s on the very first start against
  a database (it creates the contexts then).
- → last "already exists, skipping" notice: the per-context schema patches
  (§158), 0.3–0.6 s. Every patch is now a catalog check when there is
  nothing to do; the table locks they used to take are gone.
- → "reconciliation complete": warm-up and reconciliation, ~0.75 s.
- → "listening": the server mints and prints its demo tokens, ~0.25 s.
- → first accepted command: ~0.02 s.

With Postgres restarted just before (shared buffers empty; every run's
database is a fresh template copy anyway, so its buffers always start
cold), the 3.2M start was ready in 1.82 s and 2.06 s.

Lock waits: none during a cold start. During peer-load starts, up to 4–7
backends were seen waiting on a lock at once, but the serving instance's
latency shows no matching stall. These are most likely the clients' own
contention on the `sequence` row, which is present under load with or
without a start. The sampler doesn't record which lock, so this isn't
proven.

## Method

- Release `server` (`18f302c` + the #29 `MarketingSignals` fix, which
  doesn't touch startup), skilj 0.0.10, Postgres 18.6 on disk
  (`shared_buffers=512MB`, `max_connections=400`, `max_wal_size=8GB`,
  `log_min_duration_statement=100`), TCP only. skilj pool = default
  `DATABASE_MAX_CONNECTIONS` 80.
- **History.** `scripts/bench/startup/seed_base.py`: 6 minutes of the
  server's built-in demo traffic (80 workers × 50 ms, 50 companies), plus
  `RecordDailyActivity` over REST for `activity`. Then settled: projections
  and routes (`settle.py`), then the deadline cursors. Those took
  **12 minutes idle to catch up** with the 6-minute burst, at ~130 source
  events/s (see recommendations). Marketing has no events: demo companies
  never expire. Then `clone.py` multiplies the base in SQL: 4 copies →
  1.23M, 12 copies → 3.19M events. Each copy shifts sequences and ids and
  suffixes every id string, and every cursor and id sequence moves past
  the copies, so the result is settled. 1.2M took 2.5 minutes, 3.2M
  7.5 minutes.
- **Starts.** `scripts/bench/startup/startup.py` copies the template, starts
  the server, timestamps every output line, samples `pg_stat_activity`
  every 100 ms, and sends one command once ready. `--mode cold`: three
  starts per run, SIGINT between them. `restart-load`: 8 clients drive
  the ticket walk (create, assign, note, resolve) against the server while
  it's stopped and started three times. `peer-load`: a second instance on
  the same database serves the clients while the first is started three
  times. `run-all.sh` runs the sequence.

## Raw numbers

Restart under load (client gap = last success before stop → first success
after start):

| run | ready (s) | first accepted after spawn (s) | client gap (s) | failed requests (all 3 restarts) |
|---|---|---|---|---|
| restart-load-h1m | 1.46, 2.16, 1.52 | 1.81, 2.52, 2.01 | 1.96, 2.75, 2.23 | 169 connection errors of 1,728 |
| restart-load-h3m | 1.70, 2.02, 1.81 | 1.79, 2.64, 2.48 | 1.96, 2.76, 2.69 | 184 of 1,696 |

24–31 commands were also rejected: they were the follow-on steps for
tickets whose `CreateTicket` hit the restart.

Peer under load (serving instance's latency, ms, p50 / p99 / max),
`peer-load-h3m-b`, the clean run (see confounders):

| start | ready (s) | 18 s before | during the start | 10 s after |
|---|---|---|---|---|
| 1 | 1.81 | 11.1 / 61 / 104 | 13.5 / 115 / 115 | 11.4 / 41 / 54 |
| 2 | 2.16 | 12.0 / 72 / 145 | 11.8 / 20 / 20 | 11.2 / 36 / 44 |
| 3 | 1.58 | 10.7 / 68 / 181 | 12.2 / 28 / 28 | 11.0 / 50 / 68 |

0 failures across all four peer-load runs (≈3,500–3,900 requests each). The
other three runs agree: ready 1.6–2.5 s, during-start p99 20–130 ms. "During
the start" holds only 48–106 requests, so its p99 is close to its max.

## Confounders and noise

- **Due deadlines in the template.** The seed's first-response SLA
  deadlines started falling due at 12:33. Every template copy carries
  them, so from then on a server started on a copy also fired SLA
  breaches at ~70/s. The cold starts (12:04–12:18) and restart-load runs
  (12:30–12:32) ran before that. The first two peer-load runs
  (12:32–12:37) and the first idle measurements overlapped it, and
  measured firing, not idling. Every pending deadline in the templates
  was then moved 7 days out, and the idle runs and peer-load runs
  (`-b`) were repeated.
- Another session on the same machine compiled and ran tests in bursts
  (sampled every 5–10 s). The cold, restart-load, idle and
  `peer-load-h3m-b` runs had a quiet box: the idle and `-b` runs waited
  for one. `peer-load-h1m-b` overlapped compiles; its 3.65 s third start
  is left out.
- One run per size and mode, three starts each (six for cold). Starts
  vary by ±0.3 s run to run at every size, which is larger than any
  difference between sizes.
- The OS page cache is warm for every start: each run first copies its
  template, which reads it. A truly cold disk (after a host reboot) would
  add reads, but `build()` reads so little that this should be small; not
  measured (no root to drop caches).
- The clone's projection state is close to, not exactly, what folding its
  history would give (`clone.py`'s doc comment). Startup doesn't fold
  history, so it doesn't matter here.

## Recommendations / follow-up issues

1. **skilj: index the due-deadline poll.** Add `deadlines (fire_at) WHERE
   status = 'firing'` to the per-context patches. It is tiny (only rows
   mid-fire) and turns the per-tick full scan into two index lookups.
   Without it, an idle server's Postgres cost grows linearly with history:
   >1 core at 3.2M events, against 7–8% with the index (see summary).
2. **skilj: deadline schedule/cancel loops catch up slowly.** After the
   6-minute seed burst (~590 helpdesk events/s), the SLA schedule/cancel
   cursors were 140k events behind and caught up at ~130 events/s
   (12 minutes). #28 saw the same. A restart doesn't change that rate, so
   after a burst, deadlines (SLA breaches, auto-closes) get scheduled
   minutes late.
3. Nothing to do for startup itself: it's ~1.5 s, flat in history size,
   and a rolling restart with two instances is invisible to clients.

## Files

- `scripts/bench/startup/` — `seed_base.py`, `clone.py`, `startup.py`,
  `idle_cost.py`, `run-all.sh`. Reuses `../deadline-drain/common.py`.
- Raw JSON and logs were in `~/.cache/startup-bench` and are deleted; the
  numbers above are copied from them.
