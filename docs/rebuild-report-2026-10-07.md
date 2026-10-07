# Projection rebuild report — 2026-10-07 (issue #26, skilj main @ f6ed7c3)

Every load test so far measured command throughput. Issue #26 asked
how long `rebuildProjection` takes on 100k / 1M / 10M events, its
events/s, its peak memory, how much command throughput drops while it
runs, and how sync vs async projections and `PARTITION_COUNT` 1 vs N
compare.

The final numbers are on unreleased skilj from the sibling checkout
(`[patch.crates-io]` in `Cargo.toml`, `main` at `f6ed7c3`). Its async
catch-up folds up to 100 events per transaction instead of one (skilj's
CHANGELOG, Codeberg issue #52), and rebuilds go through that same fold.
skilj 0.0.9 was measured first and is kept below for comparison.

## Summary

- **A rebuild runs at ~1,400–1,700 events/s on one instance.** That's
  10.3 minutes for `TicketSummary` over 1M events, 12 minutes for
  `CompanyActiveTickets`. The rate is the same at 100k and at 1M, so
  10M extrapolates to **~1h40m** (`TicketSummary`) and **~2h**
  (`CompanyActiveTickets`). 10M was not run.
- **skilj 0.0.9 was 6–7x slower** (206–239 events/s): one transaction,
  and one commit fsync, per event. 1M would take ~70 minutes, 10M
  ~12 hours.
- **The ceiling now is the poll interval, not Postgres.** Catch-up
  folds at most 1000 events per tick, then sleeps
  `async_projection_poll_interval` (500ms). Folding takes ~50–150ms, so
  the sleep is about 80% of a rebuild. With 50ms: **7,143 events/s at
  100k, 5,581 at 1M** (1M in 3 minutes, 10M in ~30). See
  "Recommendations".
- **A rebuild costs running commands almost nothing.** Under the
  saturating 40 workers @ 100ms ramp step, accepted throughput while
  rebuilding was within 2% of a control run without a rebuild, at 100k
  and at 1M.
- **Under steady writes, a caught-up rebuild is never promoted.** In
  every load run it caught up and then sat 17–74 events behind the head
  for 6+ minutes, until the run ended. Once writes stopped it promoted
  at once. This is a skilj bug; see "Promotion starves under writes".
- **The switch-over blocks every command in the context for a few
  seconds.** Promotion copies the rebuilt state while holding the
  bounded context's `sequence` row lock: ~0.45s for `TicketSummary` at
  100k (52k rows), **~5.5s at 1M** (491k rows), so likely **~1 minute
  at 10M**.
- **Peak memory: 79–148MB.** It was 2–11GB until this pass, but not
  because of the rebuild. `tenant_access::recorded_company_tenants`
  read the bounded context's **entire history** every 5s to find
  `CompanyTenantProvisioned` events: 1.5s per read and ~7.6GB RSS on a
  1M history, even idle. Fixed here (it reads that one type through
  its index). An idle server is now 37MB at 100k and at 1M. Before,
  it sat at 918MB idle on 100k, and reached 7–8GB within two minutes
  on 1M. Very likely the root cause of issue #34.
- **Sync vs async, and `PARTITION_COUNT`, make no difference to a
  rebuild.** A rebuild folds in the background whatever its
  projection's `sync`, and it is never partitioned. `TicketSummary`
  sync 1,677 vs async 1,691 events/s; `CompanyActiveTickets`
  `PARTITION_COUNT` 4 vs 1: 1,385 vs 1,408. On 0.0.9 an unpartitioned
  async projection made every rebuild in its context 13–22% slower (an
  extra lock per event); chunked folding removed that.
- **Correct in every run.** Each no-load rebuild ended in exactly the
  state it replaced (md5 over key, canonical state JSON, owner and
  `as_of_sequence` of every row).

## Results

events/s = events in the history / seconds from the `rebuildProjection`
response to the new `schemaVersion` going live. One instance, no load.

| run | history | skilj 0.0.9 | main, before #34 fix | **main, final** | final peak RSS |
|---|---|---|---|---|---|
| `TicketSummary` (sync) | 100k | 239 | 1,570 | **1,677** | 79MB |
| `CompanyActiveTickets` (async, P=4) | 100k | 206 | 1,298 | **1,385** | 126MB |
| `CompanyActiveTickets` (async, P=1) | 100k | 161 | 1,302 | **1,408** | 117MB |
| `TicketSummary` made async | 100k | 207 | 1,386 | **1,691** | 88MB |
| `TicketSummary`, `synchronous_commit=off` | 100k | 884 | 1,662 | **1,761** | 79MB |
| `TicketSummary`, 50ms poll interval | 100k | – | – | **7,143** | 80MB |
| `TicketSummary` (sync) | 1M | – | 957* | **1,661** | 88MB |
| `CompanyActiveTickets` (async, P=4) | 1M | – | 876* | **1,411** | 144MB |
| `TicketSummary`, 50ms poll interval | 1M | – | – | **5,581** | 89MB |

\* Run with `MALLOC_ARENA_MAX=2`; with the default allocator RSS went
past 9GB within 3 minutes and the run was stopped. The background
full-history read slowed these by ~40%.

Wall time for the final column: 64s / 78s at 100k, 615s / 724s at 1M,
15s and 183s with the 50ms poll interval.

How to read the diagnostics:

- **`synchronous_commit=off`** removes the per-commit fsync. On 0.0.9 it
  made rebuilds 3.7x faster, so the per-event commit was the cost. Now
  it buys 5%: one commit per 100 events no longer matters.
- **50ms poll interval** (a local patch to `server.rs`'s builder, not
  committed). Progress moves in steps of exactly 1000 events every
  ~0.53s at the default 500ms, so a tick folds in ~30–150ms and sleeps
  the rest. Cutting the sleep is a 4.3x (100k) and 3.4x (1M) speedup.
  The smaller gain at 1M suggests folding itself gets somewhat slower
  as the rebuild's state table grows (491k rows at 1M).

### Under load

The demo seed at 40 workers @ 100ms (step 4 of the usual ramp,
saturating), 50 companies, rebuild triggered at 120s. Control runs were
identical except for the trigger. accepted/s is committed events/s
from the `sequence` row, sampled every 0.5s.

| run | events/s 30–120s | events/s 120s–end | vs control | rebuild events/s | caught up after | then |
|---|---|---|---|---|---|---|
| control 100k (600s) | 90.5 | 40.3 | – | – | – | – |
| `TicketSummary` 100k | 89.8 | 40.5 | +0.5% | 1,394 | 90s | 390s unpromoted (gap avg 25, max 68) |
| `CompanyActiveTickets` 100k | 89.3 | 40.0 | −0.7% | 982 | 129s | 351s unpromoted (gap avg 26, max 74) |
| control 1M (900s) | 90.7 | 34.3 | – | – | – | – |
| `TicketSummary` 1M | 91.0 | 33.6 | −2.0% | 1,275 | not within 780s (995k of 1.06M) | – |

The drop from ~90 to ~35–40 events/s happens in the control too: seed
throughput falls as its company and customer rows grow (see
`docs/partitioned-projection-report-2026-10-05.md`). It is not the
rebuild.

The load runs used `MALLOC_ARENA_MAX=2` throughout, to keep the box
safe while the memory problem was still open. Peak RSS: 339–356MB at
100k, 405–411MB at 1M. Before the #34 fix, the same 100k load reached
1.8GB with that setting, and 12GB on the default allocator before the
run had to be stopped.

## Promotion starves under writes

`catch_up_bounded_context` (skilj-core `db/mod.rs`) promotes a rebuild
only when two things line up:

1. At the end of a tick, the rebuild's `caught_up_to == latest`, where
   `latest` was read at the **start** of the tick. But the events folded
   in that tick are read without an upper bound
   (`WHERE sequence > $1 ORDER BY sequence LIMIT $2`), so under writes
   the fold usually passes `latest`, and `==` fails. Promotion isn't
   even attempted.
2. `promote_projection_rebuild`, under the `sequence` row lock, needs
   `caught_up_to == next_value`. Any command committed between the
   fold's commit and that lock defeats it.

At ~35 commands/s both hold almost never. Promotion happened within
3s of restarting the server without traffic (checked once, on 0.0.9,
same code path). The fix belongs in skilj: let promotion fold the
remaining tail itself while it holds the `sequence` lock, the way
registering a new sync projection over existing history already does.
At minimum, `==` in (1) should be `>=`.

Until then, an operator's way through is a lull in writes. On a busy
context that may mean pausing traffic for a few seconds.

## Recommendations

- **skilj: fix promotion under writes** (above). Without it a rebuild
  in a busy context can't finish.
- **skilj: don't sleep after a full tick.** If a tick folded the full
  1000 events, more are probably waiting. Polling again at once would
  give the 50ms-interval speed (3–4x) without polling an idle context
  more often.
- **skilj: shorten the switch-over lock.** Copying the rebuilt state
  with `INSERT ... SELECT` under the `sequence` lock stalls every
  command for ~10µs per row. Swapping the tables (or a generation
  column) instead of copying would make it constant.
- **Here: keep the 500ms default.** Lowering
  `async_projection_poll_interval` speeds up rebuilds but also polls
  every context 10x as often when idle. Better fixed in skilj as above.
- **Operations: budget ~10–12 minutes per million events** for a
  rebuild, and plan the switch-over (a few seconds of blocked writes
  per million rows) for a quiet moment. That moment is also what lets
  it promote, for now.

## Setup

- 22 cores / 15GB WSL2 box. Postgres 18.6 (`~/.theseus/postgresql/18.6.0`)
  on disk, `shared_buffers=512MB`, `max_wal_size=8GB`,
  `max_connections=300`. `cargo build --release --bin server`.
  `DATABASE_MAX_CONNECTIONS=90`, `HTTP_MAX_IN_FLIGHT_REQUESTS=70`,
  `RUST_LOG=warn`.
- **History.** One real seed run (80 workers @ 50ms, 50 companies,
  5.5 minutes): 26,891 events, 12,921 tickets. Then cloned in SQL into
  4 / 38 copies (107,564 / 1,021,858 events). Every company, ticket and
  customer id gets a per-copy suffix, sequences shift, and the live
  projection state, snapshot progress and deadline cursors are copied
  and advanced the same way. The copies share no key, so the cloned
  live state is exactly what folding the cloned history gives, and the
  md5 checks confirm it: every rebuild reproduced it byte for byte.
  The original's ids are suffixed too, so live seed traffic never
  collides with history.
- **Staging a rebuild** the way a deploy does: one optional property is
  removed from the projection's stored schema
  (`TicketSummary.first_responder_staff_id`, or `rating` in
  `CompanyActiveTickets`' entries), so the build's own schema differs
  and startup reconciliation stages a rebuild. Then `rebuildProjection`
  over GraphQL, and wait for the new `schemaVersion`.
- **Variants** are one-line local patches, reverted after building:
  `CompanyActiveTickets::PARTITION_COUNT = 1`, `TicketSummary::sync()`
  returning `false`, and `.async_projection_poll_interval(50ms)` on the
  server's builder.
- Each run starts from a fresh copy of its template and a fresh server.
  Sampled every 0.25s (0.5s under load): latest sequence, rebuild
  `caught_up_to`, server `VmRSS`; peak is `VmHWM` at the end.
  Postgres logged statements over 200ms, which is where the
  full-history read and the switch-over's copy times come from.
- One run per cell. The same rebuild at 100k and at 1M agrees within
  1–2% (1,677 vs 1,661; 1,385 vs 1,411 events/s), which is the best
  repeatability check here. Read the load-run deltas (±2%) as "no
  measurable effect", not as exact numbers.
