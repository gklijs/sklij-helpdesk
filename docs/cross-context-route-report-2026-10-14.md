# Cross-context route report — 2026-10-14 (issue #29, skilj 0.0.10, commit `18f302c` + fix)

The second pass on issue #29. The first pass
(`docs/cross-context-route-report-2026-10-13.md`) was cut to a ~20 minute
budget, its fault runs overlapped another process loading the box, and it
found a correctness bug it didn't have time to fix. This pass:

1. fixes that bug (marketing reporting `trial_status = "lapsed"` for active
   companies) and checks the fix against the pre-fix build;
2. repeats the first pass's runs on a quieter box;
3. runs what the first pass cut: the ordering sweep, intermittent errors, an
   outage in `knowledge_base`, faults on the 50k-ticket history, a heavier
   foreground step and a 10-minute outage.

Routes, method and harness are unchanged (see the first report's "What a
route does" and "Method"). In short: 500 ms route poll, default retry policy
(1 s doubling, 5 attempts, then park), latency = routed event commit time −
source event commit time.

## Summary

- **The reordering bug is far more common than the first pass suggested, and
  it's fixed.** `CompanyExpired` and `CompanyActivated` go through two
  independent routes, so marketing can receive a company's `TrialConverted`
  before its `TrialLapsed`. Without any fault, when the expire and the
  reactivate land in the same 500 ms route tick, the two routes deliver
  concurrently and **61% of companies (298/488) ended up `lapsed` in
  `MarketingSignals` although helpdesk has them active**. Every lapse parked
  during an outage and redriven later does the same, and so do 16% of
  companies under 30% intermittent errors, with nothing parked.

  | run | build | inverted in marketing | `trial_status` wrong |
  |---|---|---|---|
  | 60 s outage + redrive | pre-fix | 3 / 306 | **3** |
  | 60 s outage + redrive | fixed (×2) | 3 / 306, 3 / 306 | 0, 0 |
  | expire→reactivate gap 0 ms | pre-fix | 298 / 488 | **298** |
  | gap 0 ms | fixed | 156 / 488 | 0 |
  | gap 100 ms | fixed | 161 / 488 | 0 |
  | gap 300 ms | fixed | 0 / 487 | 0 |
  | flaky 30% | fixed | 48 / 306 | 0 |
  | 10 min outage + redrive | fixed | 37 / 1386 | 0 |

  The fix (`src/marketing.rs`, `MarketingSignals::project`): a lapse never
  replaces `converted`. Helpdesk's lifecycle only moves forward (`trialing →
  expired → active`, `active` terminal, `specs/skilj-helpdesk.allium`), so a
  conversion is always the later fact, whatever order marketing sees. This is
  the same "never backwards" rule `RecordTenantLifecycle` already uses for the
  lifecycle replicator's feeds. Both events are still recorded and counted.
  The events stay inverted in marketing's own log; only the read model is
  order-independent. Tests: `tests/fixture/activity_marketing.rs`
  (`marketing_signals_never_go_back_from_converted_and_count_each`, which
  previously asserted the old last-one-wins fold, and
  `marketing_signals_read_lapsed_until_a_conversion_lands`).

- **Latency is set by the 500 ms poll, confirmed.** p50 ~270–300 ms, p95
  ~505–550 ms at idle and under the 32 cmd/s step, empty and 50k history
  alike. A 5× heavier step (16 workers × 100 ms, ~160 cmd/s) raises it to p50
  ~440 ms (lifecycle) and ~580 ms (knowledge_base), p99 ~1 s.

- **No event lost or duplicated in any of 21 runs**, including a 10-minute
  outage (74 parks, all redriven in 0.86 s) and the 30% error rate.

- **Outage: parks ~3 per route per minute, backlog clears in ~5 s.** The first
  pass's "20.5 s after the outage" was the loaded box: on a quiet box the
  routes are caught up 4.8–5.1 s after a 60 s outage (four runs, empty and 50k
  history) and 14.5 s after a 10-minute one. The 10-minute outage parked 37
  events per route, matching the predicted ~36 (one per ~16 s retry cycle).
  An outage in `knowledge_base` behaves identically (3 parks, 6.8 s) and
  leaves marketing untouched, and vice versa.

- **Intermittent errors (30% of marketing inserts fail): slow, but no parks.**
  Retry attempts peaked at 3 of 5, so nothing parked, but each failure holds
  the route through its backoff: lag p50 8–12 s, max 24.6 s. Knowledge_base
  was unaffected (p50 330 ms).

- **One slow or locked target stalls every route, confirmed twice each.**
  Marketing at 200 ms per insert: knowledge_base, whose target is healthy,
  went from ~300 ms to p50 ~1.4 s, max 3 s. A 60 s lock on marketing's
  sequence row: knowledge_base blocked for the full 60 s (p50 32 s, max
  60.3 s) in both runs, with no error, retry or park. Cause as in the first
  report: skilj awaits all routes of a tick together before the next tick.

## Results

### Faults (marketing unless noted; foreground 8 × 250 ms throughout; 30 s before, fault, 60 s after)

Lag in seconds, for source events committed during the fault.

| run | expiry lag p50/p99/max | kb lag p50/max | max retry | parked | caught up after fault | lost / dup after redrive |
|---|---|---|---|---|---|---|
| f-outage-fix-a | 33.9 / 61.9 / 62.3 | 0.29 / 0.62 | 4 | 3 + 3 | 5.0 s | 0 / 0 |
| f-outage-old-a | 33.8 / 61.7 / 62.2 | 0.30 / 0.64 | 4 | 3 + 3 | 4.9 s | 0 / 0 |
| f-outage-fix-b | 33.7 / 61.6 / 62.1 | 0.27 / 0.62 | 4 | 3 + 3 | 4.8 s | 0 / 0 |
| f-outage-50k | 34.0 / 61.9 / 62.4 | 0.30 / 0.62 | 4 | 3 + 3 | 5.1 s | 0 / 0 |
| f-outage-10m (600 s) | 298.8 / 577.5 / 582.9 | 0.33 / 1.5 | 4 | 37 + 37 | 14.5 s | 0 / 0 |
| f-kb-outage | 0.29 / 0.52 / 0.52 | 35.0 / 63.0 | 4 (kb) | 3 (kb) | 6.8 s (kb) | 0 / 0 |
| f-flaky (30%) | 12.3 / 24.0 / 24.6 | 0.33 / 1.0 | 3 | 0 | 0.7–1.5 s | 0 / 0 |
| f-slow-b (200 ms) | 2.2 / 2.6 / 2.6 | 1.39 / 3.0 | 0 | 0 | 0.6 s | 0 / 0 |
| f-slow-c | 2.3 / 2.8 / 2.8 | 1.41 / 3.0 | 0 | 0 | 0.6 s | 0 / 0 |
| f-lock-b (60 s) | 31.2 / 59.5 / 60.0 | 32.5 / 60.3 | 0 | 0 | 1.8 s, kb 4.3 s | 0 / 0 |
| f-lock-c | 31.3 / 59.5 / 60.0 | 32.2 / 60.0 | 0 | 0 | 1.9 s, kb 4.3 s | 0 / 0 |

"Caught up after fault" = the last routed event of a source committed before
the fault ended, minus the fault's end, leaving out the parked events (which
land only when redriven). The first report's 20.5 s (outage) and 3.0/8.5 s
(lock) were measured under load average 12–39.

Foreground commands: no failures or rejections in any run. Helpdesk p50
8–9 ms throughout. It rose to 13–16 ms during a lock and stayed at 8–9 ms
during outages.

### Latency (lag in ms, p50/p95/p99/max)

| run | phase | expiry | activation | resolution → kb |
|---|---|---|---|---|
| lat-empty-d | idle | 281/506/524/525 | 271/509/523/591 | – |
| lat-empty-d | load (8 × 250 ms) | 292/537/571/668 | 282/534/663/1496 | 307/535/577/1510 |
| lat-50k-c | idle | 282/507/525/526 | 277/505/532/588 | – |
| lat-50k-c | load | 300/541/608/984 | 295/548/771/1480 | 307/550/677/1445 |
| lat-empty-heavy | idle | 279/507/521/523 | 290/525/1213/1766 | – |
| lat-empty-heavy | load (16 × 100 ms) | 444/819/919/1144 | 436/818/1050/1150 | 584/833/977/1273 |

n = 240 / 360 lifecycle pairs per phase, ~1,280 resolutions (6,058 in the
heavy step). Peak server RSS: ~100 MB empty, 680–810 MB on the 50k
history (the `KnowledgeBaseReview` row, #28), 274 MB in the heavy step.

## Method

- Release `server` from the main checkout at `18f302c` with the
  `MarketingSignals` fix (uncommitted); the pre-fix build is `75b0381`
  (same `src/` as `18f302c`). skilj 0.0.10, Postgres 18.6 on disk
  (`shared_buffers=512MB`, `max_connections=400`, TCP only), skilj pool
  `DATABASE_MAX_CONNECTIONS` default 80.
- `scripts/bench/cross-context-route/run-repeat.sh` (new) runs the sequence
  above. The ordering sweep uses `--phases run:120 --probe-rate 4
  --gap-ms 0|100|300`, no foreground load. All other options are as in the
  first report.
- 50k template: `../deadline-drain/seed.py` then `settle.py`, as before
  (150,499 helpdesk events, 50k resolved tickets). Seeding overlapped a
  heavy compile and `seed.py`'s 10-minute wait for the SLA cancels timed out
  at 33k of 50k. It then left its server running; `seed.py` now stops its
  servers on failure. The half-seeded database was settled with
  `settle.py` (340 s) until every deadline row was final, then used.

## Confounders and noise

- Another session on the same machine compiled and ran tests in bursts
  (sampled every 10 s, `~/.cache/cross-context-route/noise.log`). It hit
  `lat-empty-b`, `lat-50k-b`, `lat-empty-c` (load 28–41 during their load
  phases), briefly `f-lock-c` (pre-fault p99 ~1 s), and the second half of
  `lat-empty-e`'s load phase. Those latency runs are left out of the table
  above. Their idle phases agree with the clean runs (p50 270–300 ms); their
  load phases are worse (p50 up to 470 ms, p99 up to 1.2 s). Every fault
  run except `f-lock-c` and all the ordering runs had a quiet box (load
  < 2, no compile).
- Two clean latency runs at 8 × 250 ms (one empty, one 50k) plus the first
  report's two. They agree within ~30 ms at p50/p95; p99/max still vary
  run to run.
- Ordering counts depend on the probe timing. The gap-0 run had 298
  inversions pre-fix and 156 with the fix, which is noise in when the two
  routes' transactions commit, not an effect of the fix. The fix changes
  only the read model, not delivery order.
- `MarketingSignals` rows written by a pre-fix build stay wrong until the
  projection is rebuilt (`rebuildProjection`). The fold change alters no
  registered shape, so startup does not stage a rebuild by itself.

## Recommendations / follow-up issues

1. **Here: done** — `MarketingSignals` is order-independent. After deploying,
   rebuild `MarketingSignals` once to correct existing rows. If helpdesk ever
   gains an `active → expired` edge (recurring billing), the rank no longer
   holds and the source order must travel in the payload instead (note in
   `specs/marketing.allium`).
2. **skilj: one blocked route must not stall the others** (unchanged from the
   first report, now confirmed twice for slow and lock). Give each route its
   own tick, and put a `lock_timeout`/`statement_timeout` on route
   submissions.
3. **skilj: intermittent errors cost seconds of head-of-line lag per
   failure.** At 30% failures the route fell up to 25 s behind without
   parking anything. The backoff blocks the whole route, not just the failing
   event. Worth a note in skilj's docs if not a change.
4. **skilj: parking trickle during long outages** (first report's 3.): a
   10-minute outage left 74 parks to redrive. They redrove cleanly in 0.86 s,
   so this is operator inconvenience, not data risk.

## Files

- `src/marketing.rs`, `tests/fixture/activity_marketing.rs`, `specs/marketing.allium` —
  the fix, its test, and the corrected spec note.
- `scripts/bench/cross-context-route/run-repeat.sh` — this pass's run
  sequence. `scripts/bench/deadline-drain/seed.py` — stops its servers on
  failure.
- Raw JSON/CSV were in `~/.cache/cross-context-route` and are deleted; the
  numbers above are copied from them.
