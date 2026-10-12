# Deadline drain report — 2026-10-13 (issue #28, skilj 0.0.10, commit `75b0381`)

Issue #28 asked what happens when ~50k ticket auto-closes come due at the
same moment: how long until all have fired, what that does to command
latency while they fire, and what skilj's `deadline_retention` sweep
(skilj docs/architecture.md §163) then costs over the resolved rows. Since
this round each new ticket also schedules two SLA deadlines
(`src/helpdesk/sla_policy.rs`), so those were measured too: their rows
count toward retention, and an SLA-breach drain (100k deadlines, each
first breach also escalating) was run once.

## Summary

- **50k auto-closes drain in 15–16 minutes under load, ~13 minutes
  without.** skilj fires a context's due deadlines one at a time, each
  through its own unbatched command commit, at most 1000 per 500ms tick.
  That runs at **65 fires/s** on an idle server and **51–54/s** next to a
  steady 32 commands/s of foreground traffic. The first fire lands
  within ~0.6s of the due time.

  | run | deadlines | foreground | drain | fires/s |
  |---|---|---|---|---|
  | ac-a | 50k auto-close | 8 workers, 32 cmd/s | 928s | 53.9 |
  | ac-b | 50k auto-close | 8 workers, 32 cmd/s | 985s | 50.7 |
  | ac-noload | 50k auto-close | none | 764s | 65.4 |
  | sla-a | 100k SLA breach | 8 workers, 32 cmd/s | 2,631s | 38.0 (60 falling to ~25) |

- **Foreground latency barely notices an auto-close drain.** p50 rises by
  about 1–3 ms, p95/p99 move within the run-to-run noise. Against an
  equally long control run without a drain (same template, same load):

  | foreground, ms | control ctl-b | control ctl-d | drain ac-a | drain ac-b |
  |---|---|---|---|---|
  | p50 | 9.0 | 8.9 | 9.9 | 10.6 |
  | p95 | 51.7 | 46.4 | 46.5 | 59.3 |
  | p99 | 125.0 | 117.9 | 119.0 | 130.2 |
  | max | 687 | 796 | 537 | 482 |

  No failures, no rejections, at most 2 connections waiting on a lock in
  any 0.5s sample, async projection lag under 80 events throughout.

- **An SLA-breach drain is heavier and slows down as it goes.** 100k
  breaches (each ticket's first one also writes a `TicketEscalated`)
  took 44 minutes, the fire rate falling from 60/s to ~25/s, while
  foreground p95 climbed from 16 ms to ~200 ms and `CreateTicket` p50
  from 15 to ~90–170 ms. **Most of that is not the drain**: an equally
  long control on the same template, without a drain, degraded nearly as
  much (p95 16 → ~180 ms, `CreateTicket` p50 81 ms over the window). The
  drain's own share is p50 13.8 vs 11.3 ms, p95 157 vs 136 ms, p99 241 vs
  197 ms.

- **Retention is cheap.** 165k resolved deadlines (50k fired auto-closes,
  100k cancelled SLA rows, the foreground's own) aged past 30 days went in
  17 batches of 10,000, **0.45 s of statement time in all, 30 ms for the
  slowest batch**. The foreground did not notice. The sweep has no index
  on `resolved_at`, so each batch is a sequential scan that stops at
  10,000 hits; that stays cheap while resolved rows dominate the table
  (see "Retention" for when it wouldn't).

- **Two findings that are not the drain, but showed up in it** (follow-up
  issues suggested below):
  1. **`KnowledgeBaseReview` keeps every pending article suggestion in one
     row** (`src/knowledge_base.rs`, one shared instance, no `keys()`).
     After 50k resolved tickets that row is **4.6 MB of JSON**, rewritten
     on every fold. On a server restart with the `knowledge_base` route a
     few thousand events behind, every foreground command stalled
     **~1.1–1.6 s every few seconds for the first 1–2 minutes**
     (p95 1.2 s), on both drain and control runs. Settling the template
     first made it go away. It is the most likely cause of the ~900 MB
     RSS seen in every run with foreground traffic on this template
     (68 MB for the same drain with no foreground traffic, ~650 MB on the
     unresolved template whose review queue is nearly empty) - suggestive,
     not proven.
  2. **skilj's schedule/cancel loops write ~90–96 deadline rows/s per
     `ScheduleDeadline`**, one autocommitted `INSERT` per row. While seeding
     at ~450–1000 tickets/s, the SLA and auto-close schedules fell 9–10
     minutes behind. `fire_at` is computed when the loop gets to the event
     ("now + 7 days", "now + SLA minutes"), so in a burst like that every
     deadline is set late by the backlog: an SLA clock that starts 9
     minutes after the ticket was filed.

## What a fire costs

`skilj_core::db::fire_due_deadlines` (0.0.10), per bounded context and
tick:

1. `SELECT ... FROM deadlines WHERE (status = 'pending' OR stale firing)
   AND fire_at <= now ORDER BY fire_at LIMIT 1000`.
2. Per row, sequentially: the §131 cancel-backlog check (once per tick per
   cancel), look up the target command type, `UPDATE ... SET status =
   'firing'` (the claim), `decide_and_submit_command` - the direct,
   unbatched path, which takes the context's `sequence` row lock and
   commits like any command, sync projections included - then
   `UPDATE ... SET status = 'fired'`.
3. Sleep `deadline_poll_interval` (500ms) and repeat.

So one fire is three commits and the sequence lock once. On this box that
is ~15 ms with nothing else running (65/s). Under foreground load the fire
waits behind the command batcher's batches for the sequence lock, and the
foreground waits behind the fire: hence 51–54/s and +1–3 ms foreground
p50. 1000 rows take ~15–20 s, so the 500ms sleep between ticks costs
almost nothing here (unlike the projection rebuild in
`docs/rebuild-report-2026-10-07.md`, where it dominated).

The fire rate fell during ac-b (60/s in the first 4 minutes, 44/s in the
last) and much more during the SLA drain; ac-noload stayed flat at
64–67/s. The foreground's own commands get slower over a run -
`CreateTicket` p50 goes from ~13 ms to ~40 ms in 17 minutes in the
control too, as each foreground company's ticket count grows (the
`CreateTicket` growth `docs/consistency-query-report-2026-10-11.md`
already noted) - so the fires wait longer for the lock.

## Results

### Auto-close drain (50k, `dd_resolved`)

Foreground windows: *before* = the 60s before the due time, *during* =
due time to the last fire, *after* = the 60s after. ms, all foreground
commands together (`CreateTicket`, `AssignTicket`, 2x `AddInternalNote`,
`ResolveTicket` per ticket, 8 workers each paced at 250ms).

| run | drain | fires/s | before p50/p95/p99 | during p50/p95/p99 (max) | after p50/p95/p99 | peak RSS |
|---|---|---|---|---|---|---|
| ac-a | 928s | 53.9 | 10.2 / 71.7 / 143.6 | 9.9 / 46.5 / 119.0 (537) | 8.6 / 58.2 / 114.2 | 938 MB |
| ac-b | 985s | 50.7 | 8.8 / 80.0 / 203.7 | 10.6 / 59.3 / 130.2 (482) | 8.9 / 56.0 / 122.1 | 967 MB |
| ctl-b (no drain) | (998s window) | – | 8.8 / 36.6 / 121.0 | 9.0 / 51.7 / 125.0 (687) | 10.1 / 104.7 / 199.2 | 939 MB |
| ac-noload | 764s | 65.4 | – | – | – | 68 MB |
| ctl-d (no drain) | (998s window) | – | 8.9 / 25.6 / 107.9 | 8.9 / 46.4 / 117.9 (796) | 9.1 / 76.4 / 118.7 | 952 MB |

Per command, p50 during the window (drain ac-a / ac-b vs control ctl-b;
ctl-d gave 8.0 / 7.8 / 10.0 / 25.8 for the same four):
`AddInternalNote` 8.2 / 9.0 vs 8.2, `ResolveTicket` 7.8 / 8.5 vs 8.0,
`AssignTicket` 10.7 / 11.6 vs 10.3, `CreateTicket` 26.7 / 31.5 vs 26.4.

Every 2 minutes of ac-b vs ctl-b (p50 / p95, ms): drain 9.6/24, 9.3/23,
10.2/44, 10.8/45, 12.1/57, 11.1/62, 12.0/70, 11.2/68; control 8.9/29,
8.8/36, 8.8/30, 9.0/40, 9.0/43, 9.0/34, 9.3/54, 9.3/72. The drain's cost
is visible in p50 (+1–3 ms) and not beyond noise in p95.

### SLA-breach drain (100k, `dd_created`)

All 50k tickets in progress, both SLA deadlines of each moved to the same
due time. Each ticket's first breach writes `TicketSlaBreached` +
`TicketEscalated`, its second only `TicketSlaBreached` (150k events from
the drain, 234k with the foreground's).

| run | drain | fires/s | before p50/p95/p99 | during p50/p95/p99 (max) | after p50/p95/p99 | peak RSS |
|---|---|---|---|---|---|---|
| sla-a | 2,631s | 38.0 | 8.6 / 16.5 / 42.8 | 13.8 / 157.3 / 240.8 (884) | 11.9 / 212.6 / 250.9 | 648 MB |
| sla-ctl (no drain) | (2,638s window) | – | 9.3 / 16.4 / 21.2 | 11.3 / 135.9 / 197.3 (3,442) | 10.5 / 179.1 / 231.4 | 660 MB |

Every 3 minutes (fires/s; foreground p50/p95): 61; 10/23 → 59; 10/28 →
52; 11/45 → 47; 11/61 → 38; 14/93 → 33; 14/107 → 32; 15/113 → 30;
15/136 → 30; 16/146 → 25; 18/311 → 25; 17/211 → 28; 16/203.

The control, every 3 minutes (p50/p95): 9.5/20, 9.3/25, 9.5/29, 9.4/34,
9.5/44, 9.6/80, 12.1/149, 12.4/108, 12.3/123, 12.7/134, 13.4/155,
13.1/161, 13.6/178, 14.1/180, 12.1/182. So the foreground's own growth
over 44 minutes on this template (all 50k seeded tickets still open; why
this template degrades faster than `dd_resolved` was not looked into)
explains most of the SLA run's latency, and probably part of the falling
fire rate: the fires wait behind ever slower foreground batches for the
sequence lock. Load average rose to 3–4.5 for ~5 minutes of the control
(15:18–15:22, no compiler running), so its tail is a little noisier.

### Retention (`--retention`, after each auto-close drain)

Every resolved row's `resolved_at` moved back 31 days, 30s of settling,
then skilj 0.0.10's own statement replayed the way `retention_tick` runs
it: `DELETE ... WHERE ctid IN (SELECT ctid ... WHERE status NOT IN
('pending','firing') AND resolved_at < cutoff LIMIT 10000)` until a batch
deletes fewer than 10,000.

| run | rows aged | table before → after | batches | statement time | slowest batch | foreground p50/p95 30s before → after |
|---|---|---|---|---|---|---|
| ac-a | 164,928 | 172,940 → 9,172 | 17 | 445 ms | 30.8 ms | 8.4/50 → 8.6/52 |
| ac-b | 165,664 | 174,044 → 9,544 | 17 | 446 ms | 29.5 ms | 9.4/56 → 9.7/62 |

Batches got slightly slower toward the end (27 → 38 ms in an earlier
trial) as each scan skips the dead tuples the previous batches left,
until autovacuum runs. The server's own retention task runs the same
statement once at startup and then hourly; it was not timed separately.

When it would cost more: the subquery has no index to use (`deadlines_due`
is on `fire_at WHERE status = 'pending'`), so each batch scans until it
has 10,000 matches. With resolved rows the majority, as here and as in
any steady state, that is a few thousand pages. A table that is mostly
*pending* rows with a thin layer of expired ones (say millions of
far-future SLA deadlines) would scan most of the table per batch; still
one batch at a time per context, holding no lock commands need. Not
measured.

## Startup stall (unsettled template)

The first runs (2026-10-12, `ac-1`, `ctl-1`) used the template exactly as
`seed.py` left it: the `HelpdeskResolutionToArticleSuggestion` route
3,832 events behind and `KnowledgeBaseReview` 382 behind. For the first
60–120 s after each server start, all eight foreground workers stalled
together for 1.0–1.7 s every 3–6 s (p95 1.17 s, p99 1.3–1.6 s in that
window), with or without a drain. `pg_stat_activity` showed
`knowledge_base` transactions writing `projection_state` held open for
~1 s each and an advisory-lock transaction open for 6–13 s; Postgres
logged no slow statement, so the time is spent in the server between
statements (serialising and deserialising the 4.6 MB state is the obvious
candidate). Once the catch-up finished the stalls stopped. `settle.py`
(run the server idle until every cursor stops moving) fixed the templates,
and every number above is from settled templates. How a `knowledge_base`
fold stalls `helpdesk` commands was not pinned down.

## Method

- Release `server` from the perf worktree (`75b0381`), skilj 0.0.10.
  Postgres 18.6 on disk (`shared_buffers=512MB`, `max_wal_size=8GB`,
  `log_min_duration_statement=200`), TCP only, one cluster for all runs.
  skilj's own pool is the server's `DATABASE_MAX_CONNECTIONS` (default
  80) at this commit; `DATABASE_APP_MAX_CONNECTIONS` (10) is the server's
  setup pool. Defaults otherwise: `deadline_poll_interval` 500ms,
  retention 30 days, `RUST_LOG=warn`.
- **Templates** (`scripts/bench/deadline-drain/seed.py`): 500 companies,
  50,000 real tickets through REST (create + assign, then resolve), so
  skilj's own loops wrote every deadline row. `dd_created`: 100k pending
  SLA deadlines; `dd_resolved`: 50k pending auto-closes (7 days out) and
  100k cancelled SLA deadlines, 150,499 events. Then `settle.py`.
- **A run** (`drain.py`): copy the template, `VACUUM ANALYZE`, start the
  server, then move every pending deadline of the variant's schedules to
  `now + 180s` with one `UPDATE`. Foreground load starts at once:
  8 keep-alive clients, each on its own company, one command every 250ms
  (~32/s), walking its own tickets through create → assign → note → note
  → resolve. Latency is measured client-side, request sent to response
  read. A sampler records, every 0.5s, the sequence head, the due rows by
  status, async projection lag, active connections and lock waits, and
  server RSS. A drain ends at the first sample with no due row pending or
  firing.
- **Controls** are the same run with nothing moved, as long as a drain.
- **Order** (`run-all.sh`): ac-a, ctl-a, ac-b, ctl-b, ac-noload, sla-a,
  then ctl-c, ctl-d, sla-ctl. ~17–20 minutes each, 44 for the SLA runs.

## Confounders and noise

- **Disturbed runs, discarded:** `ctl-a` overlapped a cargo build and an
  embedded-Postgres test in the main checkout; `ctl-c` and a first
  `sla-ctl` overlapped another session's `cargo test --workspace` in the
  skilj repo (load average 30, foreground p50 40 ms, max 17.5 s). `ctl-d` and
  `sla-ctl` were re-run once the box was quiet again (load logged every
  30s: no compiler; load under 1 for ctl-d).
  The 2026-10-12 runs (`ac-1`, `ctl-1`, `ac-2`) used the unsettled
  template or are from another day and serve only the startup-stall
  section.
- Two clean controls (ctl-b, ctl-d) against two drains on `dd_resolved`;
  one SLA drain against one control. Run-to-run
  spread between ac-a and ac-b is about as large as the drain's effect on
  p95/p99 (46.5 vs 59.3 ms p95), so read "p95/p99 unchanged within noise",
  and only the p50 shift (consistent in both, and in every 2-minute
  bucket of ac-b) as the drain's measured cost.
- The foreground slows itself over a run (`CreateTicket` per-company
  growth), in drain and control alike. Comparing equal-length windows
  from the same start keeps that out of the drain/control difference;
  it does not keep it out of the fire rate's decline.
- WSL2 disk: a commit's fsync is a large part of a fire's ~15 ms.
  `synchronous_commit` was not varied.
- The SLA drain's due rows include both SLAs of each ticket at one
  instant, which is not how SLAs come due in practice (they spread over
  business hours). It bounds the burst after, say, an outage of the
  server across a busy morning.

## Recommendations

1. **No change needed here for auto-close bursts.** 50k due at once costs
   15 minutes of background firing and ~1–3 ms foreground p50. Auto-close
   due times follow resolves seven days earlier (shifted by the schedule
   loop's own lag), so a burst this size needs 50k resolves in a few
   minutes first.
2. **skilj: fire deadlines through the command batcher, or several at
   once.** Serial unbatched fires (three commits each) cap a context at
   ~65/s on this box regardless of how idle it is. An outage of an hour
   with a few thousand due deadlines per minute would take longer than
   the outage to drain. Batching the claim/resolve updates and submitting
   through `CommandBatcher` like REST does would multiply that.
3. **skilj: batch the schedule/cancel loops' inserts/updates** (one
   multi-row `INSERT ... ON CONFLICT DO NOTHING` per tick instead of one
   autocommit per row), and consider deriving `fire_at` from the source
   event's `created_at` rather than processing time, so a backlog doesn't
   shift every deadline (SLA timers especially).
4. **Here: give `KnowledgeBaseReview` a key** (per company, or one row per
   suggestion) instead of one shared, ever-growing row, and drop resolved
   tickets' suggestions once they're acted on. Then re-measure RSS and the
   restart stall. Follow-up issue.
5. **Retention: leave the default.** 165k rows in half a second. If a
   context ever holds millions of pending deadlines, an index on
   `(resolved_at) WHERE status NOT IN ('pending','firing')` would keep the
   sweep's scans short; not needed at this size.

## Files

- `scripts/bench/deadline-drain/` - `README.md` (how to run), `common.py`,
  `seed.py`, `settle.py`, `drain.py`, `run-all.sh`, `summarize.py`,
  `timeline.py`. Standard-library Python only.
- Raw per-run JSON/CSV and server logs were in `~/.cache/deadline-drain`
  and are deleted; the numbers above are copied from them.
