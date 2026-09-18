# Load test report — 2026-09-18, round two

Second follow-up load-ramp test of `server`, run locally against
`skilj-core` HEAD `e089d52` ("Fix the permanent-hang regression in
group-commit batching, and close the cross-instance routing
investigation alongside it", Codeberg issues #36/#35 — the very next
commit after the `0451fd0` build that `docs/load-test-report-2026-09-18.md`
found permanently wedging under load), to check whether that fix holds
and whether a new bottleneck shows up in its place.

## Summary

- **The permanent wedge is fixed.** All five ramp steps (up to 1600/s
  nominal offered load) ran to completion with the server still healthy
  and responsive at the end — no server restart needed between steps for
  any reason other than the deliberate DB reset. Directly confirmed via
  `pg_stat_activity` polling every 15s through every step: max
  `idle in transaction` age never exceeded ~2s at any sample, versus the
  previous report's connections stuck for 4+ minutes. Root cause matches
  what the `e089d52` commit message describes (`run_as_leader` now locks
  first, drains second) — that fix does what it says.
- **Throughput ceiling: real and roughly 2x better than the last *working*
  baseline, ~5x better than the broken batching build.** Step 3 (20
  workers, 80/s nominal — the exact step that wedged last time) now
  sustains **~23.6 commands/s accepted** (6685/267s) instead of wedging
  after ~5/s. That number holds essentially flat through step 4 (40
  workers/400/s nominal, ~25.3/s accepted) and step 5 (80 workers/1600/s
  nominal, ~25.1/s accepted, the rest correctly `503`'d by load-shed) —
  i.e., **the new steady-state ceiling for this bounded context is
  ~25-30 commands/s total**, roughly double the pre-batching 09-17
  report's ~12-16/s ceiling, not the unbounded improvement batching's own
  module doc comment might suggest is possible.
- **This ceiling is architectural, same root cause as before, not a new
  bug** — see "Why the ceiling didn't move further" below. It's a
  `skilj-core` question, not something fixable in `skilj-helpdesk`,
  exactly like the 09-17 report's finding, just less severe now.
- **New observation, not confirmed as a bug**: server RSS grew
  substantially more under load in this run than either prior report
  (up to ~2GB at step 5, vs ~53MB in the 09-17 report's post-load-shed-fix
  step 5). The likely explanation is mundane — this run actually
  *completes* 5-6x more real work than the wedged 09-18 run could, so
  proportionally more request/tracing/allocation overhead accumulates —
  but the absolute numbers are large enough to flag rather than wave
  away. See "RSS growth" below for what would confirm or rule this out.
- No panics, no `ERROR`-level log lines, no OOM, connection pool stayed
  at 13-14 of 90 throughout every step (ruling out pool exhaustion as a
  factor in either the ceiling or the RSS growth).

## Setup

Same throwaway-Postgres/local-Dex approach as both prior reports:
`cargo build --release --bin server` against `skilj-core` HEAD `e089d52`
(confirmed via `git log` before building), Postgres 18.6 (`~/.theseus`,
needs `LD_LIBRARY_PATH` pointed at a compatible `libxml2.so.2` in this
sandbox — see project notes), local Dex, `dropdb`/`createdb` between
every step (all exit codes checked). `DATABASE_MAX_CONNECTIONS=90`/
`HTTP_MAX_IN_FLIGHT_REQUESTS=70` in effect throughout, same as both
prior reports. Same `SEED_DEMO_TRAFFIC` ramp shape as the original
09-17 report, all 5 steps this time (the 09-18 report only got through
step 3 before wedging): 8/20/40/80 workers at 500/250/100/50ms
(16/80/400/1600/s nominal), 4 minutes/step, plus the manual step-1
baseline. Skipped OTel/Grafana again (same reasoning as 09-18: log-line
counting + RSS sampling + direct `psql` inspection is sufficient for
this question, and the dashboard's REST-latency histogram still isn't
trustworthy — see 09-17 report).

## Step 1 (manual baseline)

Five sequential `SignUpCompany` calls against a freshly-migrated, idle
server: 22-23ms each (`http=200`) — a bit higher than both prior
reports' ~11-15ms, within normal sandbox variance, not a regression
signal on its own (nothing else in this run suggests idle-path
overhead changed).

## Step 2 (8 workers, 16/s nominal) — healthy, matches prior reports

3337 accepted / 543 rejected over 284s: **11.75/s accepted**, 1.91/s
rejected, 0 slow-statement warnings, 0 errors. In the same range as
both prior reports' step 2 (12.2/s and 13.4/s respectively) — nothing
notable changed at this concurrency, as expected.

## Step 3 (20 workers, 80/s nominal) — the step that wedged last time

6685 accepted / 1070 rejected over 267s: **23.6/s accepted**, 4.01/s
rejected, **0 slow-statement warnings, 0 wedges, 0 HTTP 500s**. Direct
`pg_stat_activity` polling every 15s for the full step:

```
elapsed_s,pg_conns,max_idle_xact_age_s
15,14,0
30,14,0.008
...
240,14,0.032
```

Max observed `idle in transaction` age across all 16 samples: 0.15s.
Postgres connection count held flat at 14 throughout. Confirmed clean
shutdown afterward too — `pg_stat_activity` showed no lingering
transactions or held locks once the server exited, unlike the 09-18
report where the bounded context stayed wedged for minutes after the
test step ended.

## Step 4 (40 workers, 400/s nominal) — same ceiling, not higher

6755 accepted / 1008 rejected over 267s: **25.3/s accepted** — within
noise of step 3's number despite offering 5x the nominal load. 0 slow
statements, 0 wedges, 0 real errors (192 "request failed" lines, all
timestamped in the final 2 seconds — the 40 demo-seed workers' own
in-flight requests failing as `kill` tore the server down mid-request,
not a runtime failure). RSS: 386MB → 1022MB over the 4 minutes (see
"RSS growth" below).

## Step 5 (80 workers, 1600/s nominal) — load-shed absorbs the rest, ceiling holds

6701 accepted / 969 rejected (still ~25/s accepted — the ceiling holds)
plus **48,593 `503 Service Unavailable`** responses — `load_shed`
correctly rejecting the overwhelming majority of offered load instead
of queueing it, exactly as designed. 57 slow-statement warnings
appeared here for the first time this run, all on the same
`SELECT next_value FROM "bc_helpdesk".sequence FOR UPDATE` lock
acquisition, elapsed times **1.0-2.4 seconds** — real queuing delay
under genuine overload, but critically: **not the escalating
30s/60s/90s/.../240s pattern** that was the 09-18 report's actual wedge
signature. These are ordinary slow queries that complete, not a stuck
leader. `pg_stat_activity` confirms: max idle-in-transaction age across
all 16 samples was 2.02s — again, queuing, not a hang. 1461 more
"request failed (connection error)" lines, all in the final ~3 seconds
(shutdown artifact, same as step 4, scaled to 80 workers). RSS reached
~2GB by the end (see below).

## Why the ceiling didn't move further

`skilj-core/src/command_batcher.rs`'s own module doc comment describes
batching's mechanism precisely: a batch leader pays the lock-acquisition
wait once, then drains whoever queued up in that window into one shared
transaction, so N concurrent callers pay for one lock acquisition
instead of N. That should mean throughput scales with concurrency far
past the pre-batching ~12-16/s ceiling — and it does, roughly 2x. But
it doesn't mean *unbounded* scaling, because **the lock is still held
for the combined processing time of every command in the batch, not
just the acquisition** (per `db::commit_command_batch`'s own doc
comment: dispatch + insert-command + insert-events + per-command
`SAVEPOINT`, run serially inside the one transaction, for every command
in the batch). Once concurrency is high enough that batches are already
large, adding more concurrent callers just makes each batch bigger and
its own hold time longer in lockstep — so total throughput converges to
`1 / (average per-command processing time inside the transaction)`,
same shape of ceiling the 09-17 report described for the unbatched
case, just with the lock-*acquisition* overhead (not the per-command
processing itself) amortized away. That the ceiling landed at nearly
the identical number (23.6/25.3/25.1 accepted/s) across three very
different offered-load levels (80/400/1600 nominal) is exactly the
signature of a saturated, self-tuning batcher already forming
near-maximal batches — not evidence that something is broken.

**This is not a new bug** — it's the same architectural bound the 09-17
report already raised as a `skilj-core` design question (one bounded
context serializing all writes), now quantified with batching in place:
raised from ~12-16/s to ~25-30/s, not eliminated. Nothing in
`skilj-helpdesk` can move this further; it would need either a cheaper
per-command critical section in `skilj-core` (e.g., deferring
`sync_projections_for_bounded_context` further, or batching the
per-command inserts themselves rather than looping `SAVEPOINT`s) or a
different bounded-context topology (per-tenant contexts), exactly the
options the 09-17 report already named.

## RSS growth (flagged, not confirmed as a bug)

| step | nominal | RSS start → end | real completions (accepted+rejected) |
|---|---|---|---|
| 2 (8w) | 16/s | ~30MB → 165MB | 3880 |
| 3 (20w) | 80/s | 150MB → 525MB | 7755 |
| 4 (40w) | 400/s | 386MB → 1022MB | 7763 |
| 5 (80w) | 1600/s | 606MB → 2022MB | 7670 (+48,593 cheap `503`s) |

Two things stand out enough to name, not enough to call a leak outright:

1. **This is much higher than the 09-17 report's post-load-shed-fix
   numbers** (e.g., its step 5 at the *same* 1600/s nominal only reached
   43MB → 53MB, because `load_shed` rejected almost everything before
   any real work happened). The difference here is that this run's
   batching path is now *succeeding* at doing 5-6x more real database
   work per step than either prior report managed (7755-7763 real
   completions here vs. the 09-18 report's 1406 total before wedging) —
   so at least part of this is "more real work happened, more memory
   was used for it," not new inefficiency.
2. **But step 5's growth doesn't cleanly track that story**: it did the
   *same* ~7670 real completions as steps 3/4, plus 48,593 `503`s that
   should be cheap (rejected by `load_shed` before touching the DB or
   the batcher), yet ended with roughly double step 4's RSS. Growth
   *did* decelerate within each step (step 5's 15s-interval deltas
   shrank from ~180MB early to ~50-90MB later) rather than climbing
   linearly forever, which is more consistent with the allocator
   retaining freed arena pages under sustained load (common, and not
   itself a leak) than with an unbounded per-request leak — but this
   run's step durations (4 minutes) are too short to tell the two apart
   with confidence.

**Not investigated further this session** — pinning down the ~36KB/request
(step 5) to ~131KB/completion (step 4) overhead to a specific
allocation site would need a heap profiler (`heaptrack`/`jemalloc`
stats) attached to a longer run, which wasn't part of this test. Given
it never caused an OOM, crash, or pool exhaustion, and growth
decelerated rather than compounding within each 4-minute step, this
doesn't block anything — but the recommended 24h soak (still open from
the 09-17 report) is exactly what would confirm whether it plateaus or
keeps climbing.

## Recommendation

1. **The wedge fix is real and safe to rely on** — confirmed independently
   via direct `pg_stat_activity` inspection, not just absence of visible
   errors. Nothing here contradicts the `e089d52` commit's own testing
   claims.
2. **Update capacity planning**: ~25-30 commands/s sustained for the
   whole app (not per tenant), roughly double the pre-batching number,
   is the number to use now — not the old 12-16/s figure.
3. **The remaining ceiling is a `skilj-core` architecture question**,
   same category the 09-17 report already raised — worth deciding
   there whether ~25-30/s per bounded context is an acceptable
   permanent ceiling for this app's real expected load, or whether the
   per-command critical section needs to shrink further / per-tenant
   bounded contexts are worth adopting.
4. **Before the 24h soak**, consider a heap-profiled run (even 20-30
   minutes at a fixed ~20/s, under this run's new ceiling) specifically
   to resolve the RSS question above — it's the one open item this
   report couldn't close with confidence.

## Raw data

Server logs (`server-step{1..5}.log`, plus ANSI-stripped
`.clean.log` copies used for `grep`), RSS/connection/idle-transaction
samples (`step{2..5}-rss.csv`), Postgres and Dex logs are in the
session's scratchpad — not committed here, same reasoning as both
prior reports.
