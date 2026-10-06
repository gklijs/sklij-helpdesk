# Partitioned projection report — 2026-10-05 (issue #15, skilj 0.0.9)

> **Since this report**: `CompanyTicketQueue` was renamed to
> `CompanyActiveTickets` and now drops merged tickets, and closed
> tickets once rated - recommendation 2 below. The numbers here were
> measured before that, on the queue that kept every ticket. On the
> 12-company history below, a fresh fold of `CompanyActiveTickets`
> holds 9,314 of the 10,664 tickets (rows avg 143 KB instead of
> 164 KB). That's only 13% smaller, because the seed creates tickets
> faster than it finishes them, so most really are still active. What
> the change fixes is growth over time: a row is now bounded by a
> company's active tickets, not its whole history.

Issue #15 asked whether `Projection::PARTITION_COUNT > 1` pays off for
the company ticket queue (now `CompanyTicketQueue`). Every projection
here used to be sync, and `PARTITION_COUNT` does nothing for a sync
projection. So this pass makes `CompanyTicketQueue` async, sets
`PARTITION_COUNT = 4`, and measures how fast 1, 2 and 3 `server`
instances sharing one database catch up on an existing history, with
1 vs 4 partitions.

## Summary

- **Keep `PARTITION_COUNT = 4`.** With 12 companies and 3 instances,
  4 partitions drain at **~1030 events/s** against **~115 events/s**
  for 1 partition, about 9x faster. Even a single instance is ~3x
  faster partitioned (584 vs 194 events/s).
- **Unpartitioned catch-up gets *slower* with every instance you add**
  (12 companies: 194 → 156 → 115 events/s for 1 → 2 → 3 instances).
  Every instance walks the same events and contends for the same
  `projections` row lock, so the extra instances add contention and do
  no useful work. Before this change, any async projection run on more
  than one instance behaved like this.
- **Partitioned catch-up scales with instances, up to the number of
  non-empty partitions** (12 companies: 584 → 908 → 1027; 3 companies:
  363 → 528 → 744). With one company everything lands in one
  partition, so extra instances add nothing (272 → 274 → 286).
- **Partitioning still helps with a single key or a single instance.**
  The partitioned path commits once per partition per tick (up to 1000
  events). The unpartitioned path commits once per event. On a
  disk-backed cluster that alone is ~1.9x (1 company, 1 instance:
  147 → 272 events/s).
- **Correct in every run.** All 36 runs over the same history ended in
  byte-identical `CompanyTicketQueue` state (md5 over every row), for
  every partition and instance count.
- **The real cost is row size, not partitioning.** Keyed by company,
  one queue row holds every ticket that company ever had, closed ones
  included, and grows to 160–560 KB here. skilj rewrites the whole row
  for every folded event, so folding cost grows with company size and
  a drain is roughly quadratic. This also explains the WAL volume (see
  "Methodology notes"). Fixed alongside this report by dropping
  finished tickets from the queue (see the note at the top).
- **No gain in tenant mode (#40).** A per-company tenant context has
  exactly one `CompanyTicketQueue` key, so all its work lands in one
  partition. It keeps only the batching gain. Partitioning helps the
  shared `helpdesk` context, where many companies share one projection.
- **Rebuilds don't get faster.** In skilj 0.0.9 the rebuild fold and the
  history fold for a new sync projection are single-instance
  (`catch_up_partitioned_projection`'s doc comment). The Marten/Ecotone
  "rebuild time scales with workers" comparison in the issue doesn't
  apply. Rebuild timing stays with #26.

## What changed

- `CompanyTicketQueue` is async (`sync()` override removed) with
  `PARTITION_COUNT = 4`. In skilj, sync → async is a trivial
  re-registration: no rebuild, and catch-up continues from the existing
  `caught_up_to`.
- **Read-your-writes is gone for this projection.** A ticket can be
  missing from the queue for up to one `async_projection_poll_interval`
  (500ms). The dashboard didn't poll; it only refetched immediately
  after the user's own write, which would now usually miss that write.
  `frontend/src/pages/dashboard.rs`'s `refresh_after_write` refetches
  once more after 750ms. (Since replaced by a read with
  `waitForSequence`, issue #12.)
- Two tests read the queue right after a command and now wait for it
  (`wait_until`): `cross_company_projection_scoping` and
  `customer_erasure`. A gotcha surfaced while fixing the first one:
  before catch-up a queue read returns the *default* (empty) state
  rather than an error, so "the read succeeded" is not proof the
  instance exists. Wait for the ticket itself. Until the row exists,
  a company-scoped customer's read is refused with
  `grant_scope_mismatch`, because a missing row has no owner.
- `SEED_DEMO_COMPANIES=N` (default 3) for the demo seed, so a load test
  can spread tickets over more company keys than the 3 named ones.

Full suite: 215 tests, 0 failures, 0 skips. `cargo clippy
--all-targets` is clean, and the frontend builds for `wasm32`.

## Setup

- `cargo build --release --bin server`, twice: once with
  `PARTITION_COUNT = 1`, once with `4`. Nothing else differed between
  the two binaries.
- Postgres 18.6 (`~/.theseus/postgresql/18.6.0`), one long-lived
  cluster on disk, `max_connections=300`, `shared_buffers=512MB`.
  `DATABASE_MAX_CONNECTIONS=30` per instance, `RUST_LOG=warn`.
- **Histories**: one seeding server, 80 workers @ 50ms
  (`SEED_DEMO_TRAFFIC=1`), 5 minutes, into template databases
  `hist_1`/`hist_3`/`hist_12` (by company count). Result: 6,458 /
  12,210 / 22,201 events, 3,088 / 5,927 / 10,680 tickets in the queue.
- **Drain**: copy the template; start N instances one after another
  (no seeding) and wait until all are serving; then, in one
  transaction, delete every `CompanyTicketQueue` state row and
  partition-progress row and set `caught_up_to = -1`. Sample the
  rolled-up `caught_up_to` every 250ms until it reaches the latest
  sequence. Starting the clock only after all instances are warm keeps
  startup out of the number.
- Every cell run twice; the second round in reverse order so slow drift
  on the box doesn't favour one cell. Table values are the mean of the
  two rounds, in events/s.

## Results

events/s to drain the full history (higher is better):

| companies | instances | 1 partition | 4 partitions | speedup |
|---|---|---|---|---|
| 1 | 1 | 147 | 272 | 1.9x |
| 1 | 2 | 113 | 274 | 2.4x |
| 1 | 3 | 90 | 286 | 3.2x |
| 3 | 1 | 124* | 363 | 2.9x |
| 3 | 2 | 111 | 528 | 4.8x |
| 3 | 3 | 94 | 744 | 7.9x |
| 12 | 1 | 194 | 584 | 3.0x |
| 12 | 2 | 156 | 908 | 5.8x |
| 12 | 3 | 115 | 1027 | 8.9x |

\* The two rounds disagree (82 and 166 events/s). Every other cell's
rounds are within ~20% of each other.

How the 12 company keys hash into 4 partitions (skilj's own FNV-1a,
`partition_for_key`): 3 / 4 / 4 / 1. With 3 companies, each lands in
its own partition (0, 1, 2). So 3 instances on 3 companies is close to
the best case, and 12 companies are limited by the 4-key partitions.

## Methodology notes

- **Smaller histories than planned.** A 30s smoke run with 12 companies
  managed ~175 events/s, but 5-minute runs averaged 22 / 41 / 74
  events/s for 1 / 3 / 12 companies. Fewer companies means more seed
  commands contend on the same `company` consistency tag. Seed
  throughput also falls as each company's rows grow (see
  "row size" in the summary). Drains are compared in events/s, so
  different history sizes per company count don't affect the
  comparison.
- **Don't put the cluster on `/tmp` here.** The first attempt ran
  Postgres from the scratchpad under `/tmp`, which in this WSL setup is
  a 7.7 GB RAM-backed tmpfs. Every queue fold rewrites a 100+ KB row,
  so WAL filled it within one round and Postgres crashed into
  recovery. Those partial numbers (all faster, since tmpfs commits cost
  nothing) are discarded; everything above is from the disk-backed
  rerun.
- Only catch-up throughput was measured, not lag under live load. At
  this box's command ceiling (~60 commands/s, see
  `load-test-report-2026-10-01.md`), even the slowest configuration
  here keeps up, so live lag is bounded by the 500ms poll interval.

## Recommendation

1. Keep `CompanyTicketQueue` async with `PARTITION_COUNT = 4`. It is
   faster at every instance count, and it removes a trap: without it,
   adding an instance makes async catch-up slower.
2. Bound the queue's row size, since that costs more than partitioning
   saves: drop finished tickets from it. Done in the same change as this
   report, as `CompanyActiveTickets` (see the note at the top).
3. When #40 makes tenant contexts the default, this projection keeps
   only the batching gain. Revisit whether it should stay async there.

## Raw data

Per-run CSVs (`caught_up_to` sampled every 250ms), server logs and the
`history.sh`/`drain.sh`/`matrix.sh` harness are in this session's
scratchpad. As with the earlier reports, they are not committed here.
