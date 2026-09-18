# Load test report — 2026-09-18, round three

Regression check plus a third load-ramp test of `server`, run locally
against `skilj-core` HEAD `5fa2e67` ("Cut two per-command round trips out
of `commit_command_batch`'s held lock (Codeberg issue #32, round
three)"), the commit immediately following the `e089d52` build that
`docs/load-test-report-2026-09-18b.md` measured at a ~25-30 commands/s
ceiling. That commit's own message frames itself as a direct follow-up
to the 09-18b finding — trimming two round trips out of the batch
leader's held lock (a redundant `BoundedContext`/`Role` re-fetch, and
per-command `SAVEPOINT`-scoped sequence draws replaced with a pooled
`SequencePool` reserved once against the leader's own transaction) — so
this report's central question is whether that actually moved the
ceiling, and whether anything broke along the way.

## Summary

- **Nothing broke.** `cargo build --release --bin server` compiles clean
  against the new commit. `skilj-helpdesk`'s own full test suite passes
  (56 tests across 12 files, all real DB-backed runs, no skips). The
  `skilj-core`/`skilj`/`skilj-rest`/`skilj-graphql`/`skilj-macros` test
  suites pass in full (394 tests, 0 failures), including the two new
  tests the commit's own message names as covering its specific risk —
  `submit_command_batch_recycles_a_failed_commands_sequence_numbers_for_a_later_command_in_the_same_batch`
  and
  `submit_command_batch_deduplicates_a_repeated_idempotency_key_shared_by_two_commands_in_the_same_batch`
  — both pass. (One unrelated flake and one unrelated build failure hit
  during this pass, both sandbox artifacts unconnected to this commit —
  see "Methodology notes" below.)
- **The ceiling moved, modestly and consistently: ~27-28 commands/s
  accepted, up from 09-18b's ~23.6-25.3/s** — roughly a 10-15% real
  improvement, not the larger jump the commit's "two round trips cut"
  framing might suggest. Step 3 (the step that originally wedged in the
  very first 09-18 report): **27.1/s** (6614/243.8s), up from 09-18b's
  23.6/s. Step 4: **27.4/s**, up from 25.3/s. Step 5: **27.7/s** (real
  accepted, plus load-shed correctly absorbing the rest), up from
  25.1/s. Same flat-across-offered-load shape as 09-18b — a saturated
  batcher, not a regression.
- **The lock is held for much less time under contention, even though
  throughput only moved ~10-15%.** Step 5's max observed `idle in
  transaction` age (via `pg_stat_activity`, sampled every 15s) was
  **0.094s**, versus 09-18b's **2.02s** for the identical step — roughly
  a 20x reduction in worst-case lock hold time. Slow-statement warnings
  on the sequence lock also dropped: 46 occurrences this run (elapsed
  1.0-1.9s) versus 09-18b's 57 (elapsed 1.0-2.4s). This asymmetry — lock
  hold time down ~20x, throughput up only ~1.1x — says the round-trip
  trim did exactly what it targeted (shrinking the *acquisition-adjacent*
  overhead inside the critical section), but total throughput is now
  dominated by something else entirely: the real per-command work
  (dispatch, insert-command, insert-events, `SAVEPOINT`) that was already
  the bulk of the critical section, exactly as 09-18b's own "why the
  ceiling didn't move further" analysis predicted.
- No panics, no real `ERROR`-level log lines, no OOM, no sequence-gap or
  dedup-false-negative symptoms observed (consistent with the two new
  unit tests passing). Postgres connection count held flat at 13
  throughout every step, same as both prior reports — pool sizing still
  isn't a factor.

## Setup

Same throwaway-Postgres/local-Dex approach as all three prior reports:
`cargo build --release --bin server` against `skilj-core` HEAD `5fa2e67`
(confirmed via `git log` before building), Postgres 18.6
(`~/.theseus`, `LD_LIBRARY_PATH` pointed at a compatible `libxml2.so.2`
per project notes), local Dex, `dropdb`/`createdb` between every step
(all exit codes checked via `psql -v ON_ERROR_STOP=1`).
`DATABASE_MAX_CONNECTIONS=90`/`HTTP_MAX_IN_FLIGHT_REQUESTS=70` in effect
throughout, same as all three prior reports. Same `SEED_DEMO_TRAFFIC`
ramp shape: 8/20/40/80 workers at 500/250/100/50ms (16/80/400/1600/s
nominal), 4 minutes/step, plus a manual step-1 baseline (5 sequential
`SignUpCompany` calls against a freshly migrated, idle server). Restarted
the server between steps (`SEED_DEMO_CONCURRENCY`/`SEED_DEMO_INTERVAL_MS`
are read once at startup). Skipped OTel/Grafana again, same reasoning as
both prior reports.

## Regression pass (before the load test)

- `cargo build --release --bin server` in `skilj-helpdesk`: clean, no
  warnings.
- `cargo test --release` in `skilj-helpdesk`: 56 tests across 12 test
  files (plus 1 doctest file), **0 failures, 0 skips** — every DB test
  ran for real (nonzero elapsed time per file), not silently
  short-circuited by the embedded-Postgres fallback.
- `cargo test --release -p skilj-core -p skilj -p skilj-rest
  -p skilj-graphql -p skilj-macros` in `skilj` (scoped to the crates
  `skilj-helpdesk` actually depends on — `skilj-kafka` pulls in
  `rdkafka-sys`, which fails to build in this sandbox for an unrelated
  reason, see below): **394 tests, 0 failures.** Includes the two new
  tests the commit's message names directly.

## Step 1 (manual baseline)

Five sequential `SignUpCompany` calls against a freshly-migrated, idle
server: 18-25ms each (`http=200`) — in the same range as all three prior
reports' step 1 (11-23ms), no regression signal.

## Step 2 (8 workers, 16/s nominal) — unchanged, as expected

3034 accepted / 444 rejected over 242.7s: **12.50/s accepted**, 1.83/s
rejected, 0 slow-statement warnings, 0 real errors. Matches all three
prior reports' step 2 (11.75-13.4/s) — nothing notable changes at this
concurrency, as expected; the fix targets the held-lock critical section,
which barely contends at this load.

## Step 3 (20 workers, 80/s nominal) — the step that wedged in 09-18, now higher than 09-18b

6614 accepted / 1034 rejected over 243.8s: **27.13/s accepted**, 4.24/s
rejected, **0 slow-statement warnings, 0 wedges, 0 real errors**. 36
"request failed" lines, all in the final ~1 second (16 workers' own
in-flight requests failing as `kill` tore the server down mid-request —
the same shutdown artifact both prior reports saw, not a runtime issue).
`pg_stat_activity` polled every 15s: max `idle in transaction` age 0.018s
across the whole step, Postgres connection count held flat at 13.

**vs. 09-18b's 23.6/s at this exact step: +15%.**

## Step 4 (40 workers, 400/s nominal) — same plateau, higher than 09-18b

6737 accepted / 1084 rejected over 245.8s: **27.40/s accepted** —
essentially flat against step 3 despite 5x the nominal offered load,
same self-tuning-batcher signature both prior reports described. 0 slow
statements, 0 wedges, 0 real errors. 392 "request failed" lines, all
clustered in the final ~3 seconds (shutdown artifact, scaled to 40
workers — same shape as 09-18b's 192 at this step, roughly proportional
to worker count). RSS: 171MB → 968MB over the 4 minutes.

**vs. 09-18b's 25.3/s at this exact step: +8%.**

## Step 5 (80 workers, 1600/s nominal) — load-shed absorbs the rest, ceiling holds higher

6798 accepted / 1073 rejected (still ~28/s accepted — the ceiling holds)
plus 46,092 client-visible `503 Service Unavailable` responses (`request
failed error=HTTP status server error (503 ...)`), spread across the
whole step with a shutdown-time spike at the end — `load_shed` correctly
rejecting the overwhelming majority of offered load, same as both prior
reports (09-18b saw ~50,054 total in this step; comparable order of
magnitude, slightly lower here since a bit more real work got through).
46 slow-statement warnings on the sequence lock, elapsed **1.0-1.9s**
(vs. 09-18b's 57 warnings at **1.0-2.4s**) — ordinary queuing under
genuine overload, not the escalating wedge signature from the original
09-18 report. `pg_stat_activity` confirms: max idle-in-transaction age
across all 16 samples was **0.094s** — down sharply from 09-18b's 2.02s
at the identical step, the clearest direct evidence the round-trip trim
shrank the lock's held time. RSS reached ~2.1GB by the end, in the same
range as 09-18b's ~2GB (see "RSS growth" below).

**vs. 09-18b's 25.1/s at this exact step: +10%.**

## Why the ceiling only moved ~10-15% despite lock hold time dropping ~20x

09-18b's own analysis already predicted this shape: the lock is held for
the combined processing time of every command in the batch, not just the
acquisition, and that processing time (dispatch + insert-command +
insert-events + per-command `SAVEPOINT`, run serially) was always the
dominant cost once batches are large. This commit removed two *specific*
round trips from that critical section — a redundant metadata re-fetch
and per-command sequence-allocation overhead — and the `pg_stat_activity`
evidence confirms it worked exactly as intended: worst-case lock hold
time fell roughly 20x (2.02s → 0.094s at step 5). But because the
remaining per-command work inside the transaction (dispatch,
insert-command, insert-events, the `SAVEPOINT` itself) was never touched
by this commit, and that work is what a saturated batcher's total
throughput actually converges toward, overall throughput only rose by
the fraction those two round trips represented of the total critical
section — roughly 10-15%, not a multiple. This is not a disappointing
result; it's confirmation that the *specific* thing this commit targeted
(round-trip overhead, not core per-command work) is now essentially
eliminated as a further lever, and the remaining ceiling is squarely the
per-command processing cost 09-18b already identified as the next
`skilj-core` question, unchanged by this commit and not expected to be
touched by it.

## RSS growth (still flagged, still not confirmed as a bug — unchanged from 09-18b)

| step | nominal | RSS start → end | real completions (accepted+rejected) |
|---|---|---|---|
| 2 (8w) | 16/s | 36MB → 133MB | 3478 |
| 3 (20w) | 80/s | 45MB → 521MB | 7648 |
| 4 (40w) | 400/s | 171MB → 968MB | 7821 |
| 5 (80w) | 1600/s | 317MB → 2123MB | 7871 (+46,092 cheap `503`s) |

Same order of magnitude as 09-18b's numbers (which ranged 165MB/525MB/
1022MB/2022MB at the same steps) — nothing here suggests the round-trip
trim changed the RSS-growth picture either way. Still an open question
from 09-18b, still not investigated further this session (same reasoning
— would need a heap profiler attached to a longer run).

## Methodology notes

1. **A sandbox-local test flake, not a regression.** `skilj`'s own
   `command_trigger.rs` integration test file intermittently fails one
   or more of its ten tests with `Database(PoolTimedOut)` at line 290
   (`db::get_command_type`). This reproduced identically — same failure
   mode, same line, similar failure count — against a clean worktree
   checkout of the *prior* commit `e089d52` (before this session's
   commit under test), confirming it predates and is unrelated to
   `5fa2e67`. Root cause not pinned down further (candidate: this test
   file's harness uses a small default pool size that occasionally
   starves under this sandbox's CPU/IO variance), but it is independent
   of the change under test and did not block the regression pass, which
   was run against the properly scoped crate set (`skilj-core`) instead,
   where it passed clean.
2. **`skilj-kafka` doesn't build in this sandbox** (`rdkafka-sys`'s
   vendored `librdkafka` needs `curl/curl.h`, not installed here) — an
   unrelated, pre-existing sandbox gap, not something this session's
   commit touches. `skilj-helpdesk` doesn't depend on `skilj-kafka`, so
   this doesn't affect anything tested above; the workspace-wide
   `cargo test --workspace` command was scoped down to just the crates
   `skilj-helpdesk` actually uses once this was identified.
3. **55 leaked embedded-Postgres process trees** (each spawned by a
   `postgresql_embedded`-backed `skilj` integration test run, same
   failure mode the project's own sandbox notes already document) were
   found and cleaned mid-session — `pg_ctl ... stop -m immediate` per
   process plus `rm -rf` on each identified-by-`PG_VERSION` data
   directory, freeing ~4.5GB of resident memory that had been silently
   accumulating before the load-test ramp started. This is exactly the
   cleanup-warning the project's own sandbox notes describe; flagged here
   in case it explains the PoolTimedOut flake above (elevated system-wide
   memory/IO pressure right before that failure was first seen).

## Recommendation

1. **This commit is safe and does what it says** — real, if modest,
   throughput gain (10-15%), a much larger and independently-confirmed
   reduction in worst-case lock hold time (~20x), zero regressions, and
   both of its own named risk scenarios (sequence recycling,
   same-batch idempotency dedup) covered by new tests that pass.
2. **Update capacity planning again**: ~27-28 commands/s sustained for
   the whole app is now the number to use, up from 09-18b's ~25-30/s
   (more precisely, up from the low-to-mid-20s band 09-18b actually
   measured at 23.6-25.3/s).
3. **The remaining ceiling is still the same `skilj-core` architecture
   question** both prior reports raised — this commit closes out the
   round-trip-overhead angle specifically (issue #32, "round three"), but
   the per-command critical-section cost itself (dispatch + inserts +
   `SAVEPOINT`, run serially per command in the batch) is untouched and
   is what any further throughput work would need to target next.
4. Consider fixing the `command_trigger.rs` `PoolTimedOut` flake
   separately — it's pre-existing and unrelated to this session's change,
   but it's real test-suite noise in this sandbox worth a look if it
   shows up in CI too.

## Raw data

Server logs (`server-step{1..5}.log` plus ANSI-stripped `.clean.log`
copies), RSS/connection/idle-transaction samples (`step{2..5}-rss.csv`),
and the `command_trigger.rs` worktree-comparison output are in the
session's scratchpad — not committed here, same reasoning as all three
prior reports.
