# Cross-context route report — 2026-10-13 (issue #29, skilj 0.0.10, commit `75b0381`)

Issue #29 asked how long a helpdesk commit takes to become the routed event
in another context, at idle and under load, and what happens when the target
context is slow or unavailable: retries, parking, recovery, and whether any
event is lost or duplicated. Three routes were measured (`src/lib.rs`):

| route | source (helpdesk) | target |
|---|---|---|
| `HelpdeskExpiryToTrialLapse` | `CompanyExpired` | `marketing.TrialLapsed` |
| `HelpdeskActivationToTrialConversion` | `CompanyActivated` | `marketing.TrialConverted` |
| `HelpdeskResolutionToArticleSuggestion` (#45) | `TicketResolved` | `knowledge_base.ArticleSuggested` |

**Scope was cut to a ~20 minute budget: every number below is from a single
run, and part of the fault runs overlapped another process loading the box
(see Confounders).** Treat them as orders of magnitude and mechanisms, not as
precise latencies.

## Summary

- **Commit-to-routed-event latency is set by the 500 ms route poll interval.**
  p50 ~260–300 ms, p95 ~500–550 ms, at idle and under a 32 cmd/s foreground
  step, with an empty database and with 50k resolved tickets alike. Load adds
  ~30–40 ms at p50 and some p99/max outliers (up to ~2 s once, empty run).

  | run | phase | expiry p50/p95/p99/max | activation | resolution → kb |
  |---|---|---|---|---|
  | lat-empty-a | idle (2 companies/s) | 264/507/523/528 | 259/504/522/531 | – |
  | lat-empty-a | load (+8 workers, ~32 cmd/s) | 301/536/571/603 | 290/549/749/1920 | 317/542/829/2130 |
  | lat-50k-a | idle | 266/504/521/533 | 273/506/530/614 | – |
  | lat-50k-a | load | 299/554/595/821 | 295/548/648/970 | 319/548/627/733 |

  (ms; n = 240/360 lifecycle pairs and ~1,280 resolutions per run.) The
  50k template's 5 MB `KnowledgeBaseReview` row (#28) did not show up in the
  knowledge_base route's steady-state lag; it costs RSS (809 MB vs 102 MB)
  and, per #28, restart catch-up stalls, which these runs avoided by settling
  the template first.

- **No event lost or duplicated in any run.** Every source event was either
  routed exactly once or parked (parked identifier = source sequence, matched
  one to one); after `retryParkedDelivery` every source event had exactly one
  routed event. 0 duplicates, 0 orphans, 0 missing in all five runs.

- **Outage (target insert raises): retry, park one event per ~16 s, keep
  going.** skilj retries the route's head-of-line event with backoff
  1/2/4/8 s and parks it on the 5th failure, then moves to the next event,
  which goes through the same ~16 s cycle. A 60 s marketing outage parked
  **3 events per route** (6 in all); everything else waited, then drained
  **20.5 s after the outage ended** (up to 8 s of remaining backoff plus
  ~117 queued events). Parked events stay parked until an operator redrives
  them (`retryParkedDelivery` redrove all 6 in 0.4 s). The knowledge_base
  route was unaffected while marketing was down (p50 550 ms).

- **Slow target (200 ms per inserted event): no errors, lag ~2.5–3 s.** Nothing
  retried or parked. The knowledge_base route, whose target was *not* slow,
  also went from ~400 ms to 1.7 s p50 (see next point). Backlog gone within
  ~0.7 s of the fault ending.

- **Lock on the target's sequence row: every route stops, with no timeout.**
  One transaction holding `bc_marketing.sequence FOR UPDATE` for 60 s blocked
  the marketing routes for the full 60 s (lag p50 32 s, max 60.0 s) — and
  **the knowledge_base route too** (p50 34.6 s, max 60.2 s), although
  knowledge_base was healthy. No error is raised, so nothing retries or
  parks: skilj's route loop has no statement/lock timeout. Recovery once the
  lock went was quick (backlog cleared in 3–8.5 s). Foreground commands were
  unaffected (helpdesk p50 22 ms).

  Cause: skilj runs all routes of a tick with `for_each_concurrent(...).await`
  and only then sleeps and starts the next tick
  (`skilj-0.0.10/src/lib.rs` ~3185), so one route blocked inside
  `decide_and_submit_command` holds back every other route's next tick.

- **Correctness bug: the two marketing routes reorder a company's lifecycle.**
  `CompanyExpired` and `CompanyActivated` go through two independent routes
  with independent cursors and retry state. In the outage run, 2 companies
  (5 after redrive) ended up with `TrialConverted` before `TrialLapsed` in
  marketing, and `MarketingSignals.trial_status = "lapsed"` for companies
  helpdesk says are active. A redriven park always lands last, so *every*
  parked `TrialLapsed` for a company that has since reactivated produces this.
  It also happens without faults: a probe that expired and reactivated a
  company within one 500 ms tick got the two routed concurrently, in the wrong
  order (seen once during harness bring-up; the planned ordering runs were cut).
  `lapsed_at`/`converted_at` are stamped at routing time (`marketing.rs`), so
  the projection cannot reorder by timestamp either.

## What a route does (skilj 0.0.10)

`catch_up_cross_context_route` (`skilj-core/src/db/mod.rs` ~12600), per route
and tick: take a per-route advisory lock on its own connection; read up to
1000 source events past the cursor; for each, `route()`, then
`decide_and_submit_command` on the target (unbatched, own commit, takes the
target's `sequence` row lock), then advance the cursor (one more commit). An
`Err` records retry state (`cross_context_route_cursors.retry_*`) and returns;
later ticks skip the route until `retry_next_attempt_at`. Default policy
(`RetryPolicy::default()`): 1 s doubling, 5 attempts, then
`parked_deliveries` in the *target* context and the cursor advances. A
rejection is an answer, not an error. Idempotency key
`"{route}:{source sequence}"` makes a crash between submit and cursor advance
a dedup, not a duplicate.

## Results

### Faults (marketing; foreground 8 x 250 ms throughout; 30 s before, 60 s fault, 60 s after)

| run | expiry lag during fault p50/p99/max | resolution→kb during fault p50/max | retries | parked | backlog cleared after fault end | events lost / dup after redrive |
|---|---|---|---|---|---|---|
| f-outage | 45.2 s / 65.7 / 65.9 s | 0.55 s / 1.4 s | attempts 1–4, cycle ~16.5 s | 3 + 3 | 20.5 s (+ parks need redrive) | 0 / 0 |
| f-slow (200 ms) | 2.5 s / 3.2 / 3.2 s | 1.7 s / 3.6 s | none | 0 | 0.6–0.7 s | 0 / 0 |
| f-lock | 31.8 s / 59.6 / 60.0 s | 34.6 s / 60.2 s | none | 0 | 3.0 s marketing, 8.5 s kb | 0 / 0 |

Outage retry timeline (seconds after the trigger went in → attempt, parked
total in marketing): 1.0 → 1; 2.5 → 2; 5.1 → 3; 9.7 → 4; 18.1 → park (2);
… 34.7 → park (4); 51.1 → park (6); last attempt 60.1, resolved by 67.9.

Foreground and probe commands had no failures or rejections in any run.
Their latency in f-outage (p50 69–143 ms) is not attributable to the fault:
the box was loaded by another process during that run (load average 12–39).

## Method

- Release `server` (`75b0381`), skilj 0.0.10, Postgres 18.6 on disk
  (`shared_buffers=512MB`, TCP only). skilj pool = `DATABASE_MAX_CONNECTIONS`
  default 80; route poll interval 500 ms, default retry policy.
- `scripts/bench/cross-context-route/route.py`: lifecycle probes on fresh
  companies (sign up, expire, 1 s, reactivate) at 2 companies/s for the whole
  run; the deadline-drain ticket walk (create, assign, 2 notes, resolve;
  8 workers x 250 ms ≈ 32 cmd/s) in `load` phases. Latency = routed event
  `metadata_created_at` − source event `metadata_created_at`, matched on
  `company_id` / `ticket_id`, bucketed by the source commit's phase.
  Faults: a `BEFORE INSERT` trigger on `bc_marketing.events` that raises
  (outage) or `pg_sleep(0.2)`s (slow); a psql session holding
  `bc_marketing.sequence FOR UPDATE` (lock). After traffic stops the harness
  waits for every route cursor to reach its head, redrives parked deliveries
  over GraphQL, and recounts.
- 50k template: `../deadline-drain/seed.py` (500 companies, 50k tickets
  resolved, 150,499 helpdesk events) then `settle.py`.

## Not run (cut for time)

Repeat runs of everything; a heavier foreground step (16 x 100 ms); fault
runs on the 50k template; `flaky:0.3` (intermittent errors); an outage in
knowledge_base; a 10-minute outage (expected: ~36 parks per route); the
ordering sweep (expire→reactivate gap 0/100/300/1000 ms). All are wired in
`route.py` (`--fault flaky:P`, `--fault-context knowledge_base`,
`--gap-ms`, `--fg-workers`).

## Confounders and noise

- Single runs only. Idle numbers are dominated by the 500 ms tick and are
  stable; load-phase tails (p99/max) are noise-sensitive.
- Another process (an unrelated test binary, `tag_indexed_eve…`) loaded the
  box during the fault runs: load average 11.8 / 39.4 / 23.8 before f-outage,
  f-slow, f-lock. The route mechanisms (retry cadence, parks, lock blocking
  for exactly the lock duration) don't depend on that; the command latencies
  and drain seconds of those runs do.
- `recovery` in the JSON (cursor within 2 helpdesk sequence numbers of the
  source head) is unreliable under foreground load; the "backlog cleared"
  column is computed from the pairs instead (last routed event of a source
  committed before the fault ended, excluding redriven parks).

## Recommendations / follow-up issues

1. **Here: make marketing robust to reordering** (bug). Either route both
   lifecycle events through one route (one cursor, source order kept), or
   carry the source event's sequence / commit time in `TrialLapsed`/
   `TrialConverted` and have `MarketingSignals` keep the newest. Today a
   redriven park reliably leaves `trial_status` wrong.
2. **skilj: one blocked route must not stall the others.** Give each route
   its own tick (spawned task / independent schedule) instead of awaiting the
   whole `for_each_concurrent` set, and put a `lock_timeout`/
   `statement_timeout` on route submissions so a stuck target becomes an
   error that retries and parks instead of an unbounded wait.
3. **skilj: parking costs ~16 s per event per route under the default
   policy** — during a long outage the route parks a trickle and holds the
   rest, which is the right outcome, but an operator then has N parks to
   redrive (and, per 1., in the wrong order). Consider a circuit-breaker:
   after a park, stop attempting (don't park more) until a probe succeeds.
4. Re-run the cut runs (repeats, 50k faults, flaky, ordering sweep) on a quiet
   box when time allows; the harness supports them as is.

## Files

- `scripts/bench/cross-context-route/` — `README.md`, `route.py` (one run),
  `run-all.sh` (the runs above, ~25 min), `summarize.py`. Reuses
  `scripts/bench/deadline-drain/common.py`.
- Raw JSON/CSV and server logs were in `~/.cache/cross-context-route` and are
  deleted; numbers above are copied from them.
