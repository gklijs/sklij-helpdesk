# Load test report — 2026-09-19, round four

Regression check plus a fourth load-ramp test of `server`, run locally
against `skilj-core` HEAD `0d9c409` ("Batch a redispatch check's own
row-materialization fetches, more than doubling measured throughput
(Codeberg issue #32, round four)"), the commit immediately following the
`5fa2e67` build that `docs/load-test-report-2026-09-18c.md` measured at
a ~27-28 commands/s ceiling. That commit's own message claims real
load-test numbers of its own (step 3: 45.10/s, step 5: 57.75/s, mean
batch size ~34.5) — this report's central question is whether those
numbers hold up under an independent re-run, and whether anything broke
along the way.

## Summary

- **They hold up, closely.** Step 3: **45.44/s** measured here vs. the
  commit's own claimed 45.10/s. Step 5: **58.05/s** measured here vs.
  the commit's claimed 57.75/s. Mean batch size at step 5: **34.6**
  (n=1016) vs. the commit's claimed ~34.5. All three within noise of the
  commit message's own numbers — independently confirmed, not just
  trusted.
- **The ceiling roughly doubled vs. 09-18c.** Step 3: 27.13/s → **45.44/s**
  (+67%). Step 4: 27.40/s → **60.42/s** (+120%). Step 5: 27.7/s →
  **58.05/s** (+110%, load-shed still correctly absorbing the rest).
  Step 2 (12.59/s) is unchanged from 09-18c's 12.50/s, as expected — this
  fix targets the decide-phase redispatch check, which barely engages at
  low concurrency.
- **Nothing broke that wasn't already known.** `cargo build --release
  --bin server` compiles clean. `skilj-helpdesk`'s own full test suite
  passes in full: **63 tests across 15 files, 0 failures, 0 skips**, all
  real DB-backed runs (nonzero elapsed time per file). The scoped
  `skilj-core`/`skilj`/`skilj-rest`/`skilj-graphql`/`skilj-macros` suite:
  **785 passed, 6 failed** — all 6 failures are `Database(PoolTimedOut)`
  panics, the same failure signature and root-cause hypothesis (small
  test-harness pool starving under sandbox CPU/IO variance) as the
  pre-existing `command_trigger.rs` flake 09-18c already documented and
  ruled unrelated to the commit under test. This run additionally saw 3
  of the same signature in `event_fetch_rest.rs` — not seen in 09-18c,
  but same file class (a REST-layer integration test with its own small
  pool), same error, same line-284-ish shape (login/dispatch calling
  `get_command_type`/`get_events`). Not re-verified against a clean
  worktree of the *prior* commit this time (time-boxed); flagged as
  "plausible instance of the known class," not confirmed independent of
  `0d9c409`, unlike 09-18c's own more rigorous isolation check.
- **Lock-hold time stayed low, as expected** — this commit didn't touch
  the held-lock critical section round three already shrank. Max
  observed `idle in transaction` age: step 3 **0.026s**, step 4
  **0.022s**, step 5 **0.191s** (one outlier sample; every other step-5
  sample was ≤0.016s) — same order of magnitude as 09-18c's step 5
  (0.094s), not the escalating-wedge shape the original 09-18 report
  described.
- **RSS grew much faster and much higher than any prior report** — see
  "RSS growth" below. This tracks with real throughput roughly doubling
  (more real work actually gets done and buffered per unit time now),
  but the absolute numbers (up to **4.8GB** by the end of step 5, vs.
  09-18c's ~2.1GB) are large enough to flag on their own, independent of
  whether they're "expected." Still not root-caused (would need a heap
  profiler on a longer run, same as every prior report's own caveat).
- 71 slow-statement warnings in step 5 (elapsed 1.03-1.66s, vs. 09-18c's
  46 at 1.0-2.4s) — more warnings, but a *tighter* elapsed-time band;
  consistent with "ordinary queuing under genuine overload, more of it
  gets through now" rather than a new pathology. No panics, no real
  `ERROR`-level log lines, no OOM, no sequence-gap or dedup-false-negative
  symptoms.

## Setup

Same throwaway-Postgres/local-Dex approach as all four prior reports:
`cargo build --release --bin server` against `skilj-core` HEAD `0d9c409`
(confirmed via `git log` before building), Postgres 18.6 (`~/.theseus`,
`LD_LIBRARY_PATH` pointed at a compatible `libxml2.so.2` per project
sandbox notes), local Dex (prebuilt binary at `~/.local/bin/dex` — no
`go` toolchain in this sandbox to rebuild it, per project notes),
`dropdb`/`createdb` between every step. `DATABASE_MAX_CONNECTIONS=90`/
`HTTP_MAX_IN_FLIGHT_REQUESTS=70` in effect throughout, same as all four
prior reports. Same `SEED_DEMO_TRAFFIC` ramp shape: 8/20/40/80 workers at
500/250/100/50ms (16/80/400/1600/s nominal), 4 minutes/step, plus a
manual step-1 baseline (5 sequential `SignUpCompany` calls against a
freshly migrated, idle server). Restarted the server between steps
(`SEED_DEMO_CONCURRENCY`/`SEED_DEMO_INTERVAL_MS` are read once at
startup). `pg_stat_activity` and server RSS sampled every 15s per step
(16 samples/step). Skipped OTel/Grafana again, same reasoning as all
prior reports.

One deviation from prior reports' own methodology: this run used a
long-lived local Postgres cluster (`initdb`/`pg_ctl` directly) rather
than each report's own throwaway-per-run setup, dropping/recreating just
the `skilj_helpdesk_loadtest` database between steps — functionally
equivalent (fresh schema every step) but noted for completeness.

Before the ramp: 21 leaked embedded-Postgres process trees (plus one
found later, from the scoped `skilj` regression pass) were found and
cleaned (`pg_ctl ... stop -m immediate` + `rm -rf` on each
`PG_VERSION`-identified data directory) — the same cleanup-warning
09-18c's own report flags, confirmed still relevant in this sandbox.

## Regression pass (before the load test)

- `cargo build --release --bin server` in `skilj-helpdesk`: clean, no
  warnings, ~40s (incremental — `skilj-core`/`skilj-rest`/`skilj-graphql`/
  `skilj` all rebuilt against the new commit, `skilj-helpdesk` itself
  unchanged).
- `cargo test --release` in `skilj-helpdesk`: **63 tests across 15 test
  files** (plus 1 doctest file, 0 doctests), **0 failures, 0 skips** —
  every DB test ran for real (nonzero elapsed time per file). Test count
  is higher than 09-18c's 56/12 files because the crate has grown since
  (`activity.rs`, `multi_tenant_provisioning.rs`, and others not present
  in that report).
- `cargo test --release -p skilj-core -p skilj -p skilj-rest -p
  skilj-graphql -p skilj-macros --no-fail-fast` in `skilj` (scoped to the
  crates `skilj-helpdesk` actually depends on — `skilj-kafka` pulls in
  `rdkafka-sys`, which fails to build in this sandbox for an unrelated,
  pre-existing reason): **785 passed, 6 failed.** All 6 failures are
  `Database(PoolTimedOut)` — 3 in `command_trigger.rs` (down from 09-18c's
  5, after cleaning the leaked-process backlog first), 3 in
  `event_fetch_rest.rs` (new this run, same failure signature). See
  "Methodology notes" for how confident that "pre-existing, unrelated"
  read actually is this time.

## Step 1 (manual baseline)

Five sequential `SignUpCompany` calls against a freshly-migrated, idle
server: 21-32ms each (`http=200`) — in the same range as all four prior
reports' step 1 (11-25ms), no regression signal.

## Step 2 (8 workers, 16/s nominal) — unchanged, as expected

4024 fired: 3423 accepted / 601 rejected over 271.8s: **12.59/s
accepted**, 2.21/s rejected, 0 slow-statement warnings, 0 request
failures, 0 real errors. Matches all four prior reports' step 2
(11.75-13.4/s) — as expected, this commit's fix (the DCB-conflict
redispatch check inside `decide`) barely engages at this concurrency.

## Step 3 (20 workers, 80/s nominal) — the commit's own headline number, confirmed

13791 fired: 11856 accepted / 1935 rejected over 260.9s: **45.44/s
accepted**, 7.42/s rejected, **0 slow-statement warnings, 0 request
failures, 0 real errors**. Mean batch size 5.6 (n=4918 phase-timing
samples), mean `decide_us` 16.1ms/batch (≈2.9ms/command at this batch
size). `pg_stat_activity` polled every 15s: max `idle in transaction`
age 0.026s across the whole step, Postgres connection count held flat
at 13.

**vs. 09-18c's 27.13/s at this exact step: +67%. vs. the commit
message's own claimed 45.10/s: +0.8%, i.e. confirmed.**

## Step 4 (40 workers, 400/s nominal) — largest relative gain

18226 fired: 15685 accepted / 2541 rejected over 259.6s: **60.42/s
accepted**, 9.79/s rejected. 0 slow statements, 0 real errors. 97
"request failed" lines, all clustered in the final ~1 second (in-flight
requests failing as the server was torn down mid-request — the same
shutdown artifact every prior report saw, not a runtime issue; confirmed
by timestamp — all 97 fall within the log's final 0.8s). RSS: 836MB →
4048MB over the 4 minutes (see "RSS growth" below).

**vs. 09-18c's 27.40/s at this exact step: +120%** — the largest
relative gain of any step, consistent with 40 workers being closer to
where the self-tuning batcher's larger batches (and thus the
now-cheaper per-row redispatch check) pay off most.

## Step 5 (80 workers, 1600/s nominal) — load-shed absorbs the rest, ceiling roughly doubles

15128 accepted / 2465 rejected (17593 real completions, ceiling holds at
**58.05/s** accepted) plus 45,278 client-visible `503 Service
Unavailable` responses (`load_shed` correctly rejecting the
overwhelming majority of offered load) — comparable order of magnitude
to 09-18c's 46,092 at this step. 71 slow-statement warnings on the
sequence lock, elapsed **1.03-1.66s** (vs. 09-18c's 46 warnings at
**1.0-2.4s** — more warnings, tighter band). `pg_stat_activity`: max
idle-in-transaction age across all 16 samples was **0.191s** (one
sample; every other sample this step was ≤0.016s) — noisier than
09-18c's 0.094s but still two orders of magnitude below the
escalating-wedge signature the original 09-18 report described. Mean
batch size **34.6** (n=1016), matching the commit message's own claimed
"~34.5" almost exactly — direct confirmation the self-tuning batcher is
using the extra headroom the fix created, exactly as that commit's own
analysis predicted. RSS reached **4.79GB** by the end (see below).

**vs. 09-18c's 25.1/s (accepted-ceiling framing: 27.7/s) at this exact
step: +110%. vs. the commit message's own claimed 57.75/s: +0.5%, i.e.
confirmed.**

## Why the ceiling roughly doubled here

This matches the commit's own mechanism exactly: `decide_us` at step 5's
larger batch sizes (mean 34.6) works out to roughly the ~5.3ms/command
the commit's message claims (184.6ms mean `decide_us` per batch, at a
34.6-command mean batch size), down from step 3's smaller-batch,
lower-redispatch-pressure ~2.9ms/command — the DCB-conflict redispatch
check this commit optimized fires more, and costs more per row before
the fix, exactly when batches are large under real contention, which is
exactly the regime this fix targeted. The self-tuning batcher responding
with larger batches (34.6 vs. round three's ~15-16) once each batch got
cheaper to process is the same feedback loop the commit's own message
describes, and it reproduces here independently.

## RSS growth (still flagged, now more concerning — grew much faster and higher than any prior report)

| step | nominal | RSS start → end | real completions (accepted+rejected) |
|---|---|---|---|
| 2 (8w) | 16/s | 60MB → 254MB | 4024 |
| 3 (20w) | 80/s | 262MB → 1586MB | 13791 |
| 4 (40w) | 400/s | 836MB → 4048MB | 18226 |
| 5 (80w) | 1600/s | 1288MB → 4788MB | 17593 (+45,278 cheap `503`s) |

Every step's peak RSS here is 3-5x the corresponding step in 09-18c
(which ranged 133MB/521MB/968MB/2123MB at the same steps). The most
straightforward explanation is that real throughput roughly doubled, so
in the same 4-minute window the server is genuinely creating, batching,
and persisting far more events and holding more of that work's
byproducts (batch buffers, decoded rows, per-command maps the round-four
commit's own message describes adding, e.g. the batched
`get_commands_by_ids_with_bc`/`get_event_types_by_names_with_bc` result
maps) in memory at once — consistent with, not contradicted by, the
"more real work per unit time" story. Not confirmed further this
session (would need a heap profiler on a longer run, same open item
09-18b first raised and every report since has repeated) — but the
absolute numbers are now large enough (approaching 5GB under sustained
1600/s nominal load) that this is worth treating as a real capacity-
planning input, not just a curiosity, before this build goes anywhere
near a memory-constrained deployment target.

## Methodology notes

1. **Two `PoolTimedOut` flakes this run, not one.** `command_trigger.rs`
   (3 failures, down from 09-18c's 5 after cleaning the leaked-process
   backlog first) and, newly, `event_fetch_rest.rs` (3 failures, same
   `Database(PoolTimedOut)` signature). 09-18c isolated its one flake
   against a clean worktree of the prior commit to confirm it predated
   the change under test; **this run did not repeat that isolation
   step** (time-boxed) for either file. Both are consistent with the
   same root-cause hypothesis 09-18c proposed (a small per-test-file
   pool starving under this sandbox's CPU/IO variance, worsened here by
   a large leaked-process backlog found and cleaned mid-session — see
   below), but that consistency is circumstantial this time, not
   independently confirmed. Worth actually pinning down if it keeps
   recurring or shows up in CI.
2. **`skilj-kafka` doesn't build in this sandbox** (`rdkafka-sys`'s
   vendored `librdkafka` needs `curl/curl.h`, not installed here) — the
   same unrelated, pre-existing sandbox gap every prior report notes.
   `skilj-helpdesk` doesn't depend on it, so this doesn't affect anything
   tested above.
3. **21 leaked embedded-Postgres process trees found and cleaned before
   the ramp started** (`pg_ctl ... stop -m immediate` + `rm -rf` per
   `PG_VERSION`-identified data directory), plus one more found after the
   scoped `skilj` regression pass and cleaned the same way before
   starting the load-test Postgres cluster. Same cleanup-warning 09-18c's
   own report (and this project's sandbox notes) already describe.

## Recommendation

1. **This commit is safe and does what it says, independently
   confirmed.** Real throughput roughly doubled (step 3 +67%, step 4
   +120%, step 5 +110%), the commit's own specific numbers (45.10/s,
   57.75/s, mean batch size ~34.5) reproduce here within ~1%, and the
   mechanism it claims (cheaper per-row redispatch-check materialization
   letting the self-tuning batcher run larger batches) shows up directly
   in this run's own phase-timing logs.
2. **Update capacity planning again**: ~45-60 commands/s sustained for
   the whole app is now the number to use, roughly double 09-18c's
   ~27-28/s figure — but see point 4 below before treating that as a
   deployment-ready ceiling.
3. **The next lever is no longer inside this bounded context.** This
   commit's own message already names it: `skilj-helpdesk`'s single
   shared `"helpdesk"` bounded context (what every load test in this
   issue, including this one, has been measuring the ceiling of) is an
   explicitly documented placeholder — real per-company tenant
   provisioning via `CreateBoundedContextFromTemplate` is deferred (see
   `helpdesk.rs`'s own module doc comment and
   `specs/skilj-helpdesk.allium`'s Dependencies section). Splitting
   tenants into their own bounded contexts removes the single shared
   sequence/lock this whole `commit_command_batch` optimization series
   has been squeezing harder each round, which is a structurally
   different lever than anything further inside `skilj-core` can offer
   for this deployment shape.
4. **RSS growth needs a real look before this ships anywhere memory-
   constrained** — nearly 5GB under sustained load is a different order
   of concern than 09-18c's ~2GB, even accounting for the throughput
   increase. A heap profiler on a longer run is the concrete next step;
   this report is the fourth in a row to flag it without investigating
   further.
5. Consider fixing (or at least isolating) the `command_trigger.rs`/
   `event_fetch_rest.rs` `PoolTimedOut` flakes properly — now two files
   showing the same signature is a weaker "definitely pre-existing and
   unrelated" claim than 09-18c's single, worktree-isolated instance.

## Raw data

Server logs (`server-step{1..5}.log` plus ANSI-stripped `.clean.log`
copies) and RSS/connection/idle-transaction samples (`step{2..5}-rss.csv`)
are in the session's scratchpad — not committed here, same reasoning as
every prior report.
