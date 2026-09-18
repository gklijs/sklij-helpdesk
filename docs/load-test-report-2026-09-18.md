# Load test report — 2026-09-18

Follow-up load-ramp test of `server`, run locally against `skilj-core`
HEAD `0451fd0` (Codeberg issue #32, "round two" - `8f0af55` "Shorten
`submit_command`'s own bounded-context lock hold" plus `0451fd0` "Add
group-commit command batching"), to check whether it moved the
~12-16 commands/sec whole-app ceiling found in
`docs/load-test-report-2026-09-17.md`.

## Summary

- **Not fixed, and not just "still capped" - a new, more severe failure
  mode.** Under concurrent load (step 3, 20 workers / 80 cmd/s nominal),
  a batch-leader transaction was repeatedly left `idle in transaction`
  in Postgres - holding the `bc_helpdesk.sequence` row's lock
  indefinitely, without committing, rolling back, or timing out.
  Confirmed directly via `pg_stat_activity`/`pg_locks`, at two snapshots
  several minutes apart, on **different connections each time** (not one
  unlucky fluke) - this is a real, reproducible hang in the new
  group-commit batching path, not present pre-batching.
- **This is worse than the original ceiling.** The 2026-09-17 report's
  finding was a throughput *ceiling* - the app degraded but every
  request still eventually got an answer. This is a *permanent wedge*:
  once one leader transaction stalls, every other writer to the
  `helpdesk` bounded context queues behind its lock and never proceeds.
  The bounded context was still wedged several minutes after the test
  step ended, with no sign of self-recovery - it needs a server restart.
- Steps 4 and 5 of the planned ramp were **not run**: the app was already
  wedged after step 3, and running more load against a bounded context
  that can't process writes wouldn't have produced new information.
- No panics, no OOM, no pool exhaustion (`pg_stat_activity` connection
  count stayed at 13-14 throughout, far under `DATABASE_MAX_CONNECTIONS=90`)
  - ruling out the previous report's fixed issues as a cause.

## Setup

Identical to `docs/load-test-report-2026-09-17.md`: `cargo build --release
--bin server` (this time against `skilj-core` HEAD `0451fd0`, confirmed
before building), a throwaway local Postgres 18.6, local Dex, DB reset
between every step (`dropdb`/`createdb`, exit code checked both times -
both succeeded, no repeat of the previous report's silent-reuse bug).
`DATABASE_MAX_CONNECTIONS=90`/`HTTP_MAX_IN_FLIGHT_REQUESTS=70` (the
2026-09-17 fix, already committed at `8389c88`) were in effect throughout
- this run is purely about what the batching change did on top of that.
Same `SEED_DEMO_TRAFFIC` ramp shape, 4 minutes/step: 8 workers/500ms
(step 2, 16/s nominal), 20 workers/250ms (step 3, 80/s nominal). Skipped
the OTel/Grafana stack this time - the previous report already
established that dashboard's REST-latency panel isn't trustworthy, and
log-line counting + RSS sampling + direct `psql` inspection was
sufficient to find what matters here.

## Step 1 (manual baseline)

Five sequential `SignUpCompany` calls against a freshly-migrated,
otherwise-idle server: 11-14ms each (`http=200`). Matches the previous
report's idle baseline - startup and the new pool/batcher wiring add no
measurable overhead at rest.

## Step 2 (8 workers, 16/s nominal) - healthy, roughly matches the old ceiling

| | this run (post-batching) | 2026-09-17 (pre-batching) |
|---|---|---|
| accepted/s | 13.4 (3205/240s) | 12.2 |
| rejected/s | 2.1 (500/240s) | 2.0 |
| client/HTTP-failed/s | 0 | 0.01 |
| slow-statement warnings | 0 | 0 |
| server RSS, start→end | 30MB → 134MB | 33MB → 112MB |
| pg connections | steady 13 | steady, pool no longer pinned |

A small, plausible improvement (~10% more accepted/s), well within what
a single run's variance could explain - this step's offered load (16/s)
was already close to the old ceiling, so it isn't a strong test of
whether the ceiling moved. Nothing here indicates any correctness
problem at this concurrency.

## Step 3 (20 workers, 80/s nominal) - the wedge

Total round trips completed: 1406/240s (5.86/s) - 1217 accepted (5.07/s),
189 rejected (0.79/s), and (new, not seen pre-batching) **45 outright
HTTP 500s** (0.19/s), plus 7 `sqlx` slow-statement warnings on
`SELECT next_value FROM "bc_helpdesk".sequence FOR UPDATE` with elapsed
times climbing in lockstep with wall-clock time - `30s, 60s, 90s, 120s,
150s, 180s, 210s, 240s` - i.e. not eight independent slow queries, but
every *later* arrival waiting exactly as long as the *original* stuck
holder had been stuck when it arrived. Direct confirmation:

```
$ psql ... -c "SELECT pid, state, wait_event, now()-xact_start AS xact_age FROM pg_stat_activity WHERE datname='skilj_helpdesk_dev';"
  pid  |        state        | wait_event |    xact_age
-------+---------------------+------------+-----------------
 69218 | idle in transaction | ClientRead | 00:04:27.075339   <- stuck
 69215 | active              | ...tuple.. | 00:03:27.021972   <- queued behind it
 69212 | active              | ...tuple.. | 00:03:27.021854
 ... (8 waiters total, all `AccessExclusiveLock` on bc_helpdesk.sequence, granted=false)
```

`idle in transaction` + `wait_event=ClientRead` means: Postgres already
answered this connection's last statement (the `FOR UPDATE` lock
acquisition succeeded) and is waiting for the *application* to send the
next one - the stall is on the Rust side, inside the batch-leader task,
not a Postgres-level deadlock or contention artifact. Re-checked minutes
later: the *specific* stuck connection had changed (a different pid, not
69218), but a connection was *again* idle-in-transaction holding the
same lock - including one case caught only 27 seconds into its own
stall. This is a recurring hang, not a single bad request.

**Likely locus** (not confirmed by code-level debugging, just the one
candidate that matches the evidence): `skilj-core/src/db/mod.rs`'s
`submit_command_batch` does `pool.begin()` + `lock_bounded_context_sequence`
on the transaction `tx`, then calls `sync_projections_for_bounded_context(pool, ...)`
- a *separate* pool checkout, on a different connection, before running
any command in the batch. If anything in that path (or the per-command
loop after it) fails to make forward progress without erroring - a
future that's dropped without completing, an unhandled cancellation, or
similar - `tx` never sees another statement and Postgres has no way to
know the client isn't coming back. `pg_stat_activity` connection count
staying at 13-14 (nowhere near the 90 cap) rules out ordinary pool
exhaustion as the reason that secondary checkout would stall - this
looks like an application-level hang, not a resource-starvation one.

## What wasn't tested

- Whether `8f0af55` (the lock-shortening commit) alone, without
  `0451fd0`'s batching on top, has this problem - both commits were
  already present at the `skilj-core` HEAD used here, so this run can't
  distinguish which one introduced the hang. Given the hang is
  specifically inside `submit_command_batch`/`CommandBatcher` -
  machinery `0451fd0` added - that's the more likely culprit, but this
  wasn't isolated.
- Steps 4-5 of the ramp, and the 24h soak - both still blocked on this
  being fixed first; running either now would just reproduce the wedge
  faster.

## Recommendation

1. **Do not deploy `0451fd0` as-is.** A throughput ceiling that degrades
   gracefully is a capacity-planning problem; a lock that a live
   production server can wedge itself into permanently, with no
   automatic recovery, is an outage waiting to happen the first time
   real traffic bursts past ~15-20 concurrent writers.
2. Bisect: re-run this same step-3 load against `8f0af55` alone (before
   `0451fd0`) to confirm the batching commit specifically is what
   introduced the hang, not the lock-shortening one.
3. Regardless of root cause, add a defense-in-depth backstop:
   Postgres's own `idle_in_transaction_session_timeout` (or
   `statement_timeout`) on the connection/pool used for this lock, so a
   stalled leader gets killed and its lock released automatically rather
   than wedging the bounded context until someone notices and restarts
   the server.
4. Once fixed, re-run this exact step 2/3 ramp before trusting any
   throughput-ceiling numbers from the batching change - none of the
   "did it help" question from the original issue has actually been
   answered yet, since step 3 never got to run to completion in a
   healthy state.

## Raw data

Server logs (`server-step2.log`, `server-step3.log`), RSS/connection
samples (`step2-rss.csv`, `step3-rss.csv`) are in the session's
scratchpad - not committed here, same reasoning as the previous report.
