# Async projection report — 2026-10-06 (issue #12, skilj 0.0.9)

Issue #12 asked to make `CompanyTicketList` and `TenantDirectory` async,
re-run the load ramp against `docs/load-test-report-2026-10-01.md`, and
have the frontend use `waitForSequence` where it reads its own writes.

Where that stands:

- **`CompanyTicketList`** no longer exists. Its successor,
  `CompanyActiveTickets`, became async with `PARTITION_COUNT = 4` in
  issue #15 (`docs/partitioned-projection-report-2026-10-05.md`). That
  report measured catch-up speed, not command throughput, so this one
  measures the throughput side.
- **`TenantDirectory` stays sync.** The routing guard reads it to decide
  which bounded context a company's commands go to, and a stale read
  there splits one company's history across two contexts (see its doc
  comment in `src/helpdesk.rs`). Async would also buy nothing back: it
  folds only `CompanyTenantProvisioned`, once per company, so it adds
  nothing to an ordinary command's commit.
- **The frontend now uses `waitForSequence`.** See "Frontend" below.
- Projection lag is sampled here directly. The metric and dashboard
  panel are issue #27.

## Summary

- **Async `CompanyActiveTickets` is a little faster at saturation, and
  no slower anywhere.** A/B on today's code, the only difference being
  the projection's `sync()`:

  | step | offered (nominal) | sync | async | delta | 10-01 (all sync) |
  |---|---|---|---|---|---|
  | 2 (8w @ 500ms) | ~16/s | 13.11/s | 13.10/s | 0.0% | 13.39/s |
  | 3 (20w @ 250ms) | ~80/s | 44.35/s | 44.85/s | +1.1% | 50.86/s |
  | 4 (40w @ 100ms) | ~400/s | 51.36/s | **54.93/s** | +7.0% | 62.84/s |
  | 5 (80w @ 50ms) | ~1600/s | 51.82/s | **54.73/s** | +5.6% | 62.50/s |

  Steps 4 and 5 point the same way, and that's the expected mechanism:
  every ticket command's commit no longer rewrites the company's queue
  row. It is one run per variant, though, and 10-01 treated sub-10%
  deltas as noise. These two ran back to back on the same box and
  binary build, which makes them more comparable than two report
  rounds, but still read it as "a small gain", not a measured 6%.
- **Keep it async.** The cost is a median lag of about 20 events
  (~0.4s at these rates), worst sample 73 events (~1.3s), and the one
  place it showed to a user is now handled by `waitForSequence`.
- **Both variants are 12–18% below 10-01 at steps 3–5, and both use
  more memory.** Sync on today's code is slower than 10-01's sync, so
  this isn't the async change. Same box shape (22 cores / 15GB), same
  ramp, same Postgres 18.6. Between the two lie issues #14–#20 and #46:
  per-customer encryption of every ticket's text (#16), a `Snapshot` on
  every single-ticket command (#14), staff-role checks (#19) and more.
  Not bisected here; it needs its own pass.
- **Lock hold stays negligible.** Max `idle in transaction` age over
  17 samples per step: async 0.001–0.045s, sync 0.000–0.008s. The async
  side's higher numbers are its catch-up transactions, which fold up to
  1000 events per partition per commit. Still under 50ms.

## Step results

Accepted commands/s is accepted commands divided by the wall time from
first to last fired command (253–256s per step), the 10-01 convention.

| variant / step | fired | accepted | rejected | failures | accepted/s | max idle in tx | lag median / max (events) | RSS start → peak |
|---|---|---|---|---|---|---|---|---|
| sync 2 | 3869 | 3331 | 538 | 0 | 13.11 | 0.000s | 0 / 0 | 40 → 202MB |
| sync 3 | 12956 | 11289 | 1667 | 3 | 44.35 | 0.006s | 0 / 0 | 109 → 2305MB |
| sync 4 | 15346 | 13110 | 2236 | 116 | 51.36 | 0.007s | 0 / 0 | 632 → 4695MB |
| sync 5 | 15204 | 13271 | 1933 | 44951 | 51.82 | 0.008s | 0 / 0 | 886 → 5858MB |
| async 2 | 3888 | 3317 | 571 | 0 | 13.10 | 0.001s | 5 / 7 | 39 → 227MB |
| async 3 | 13216 | 11415 | 1801 | 11 | 44.85 | 0.016s | 19 / 28 | 104 → 2588MB |
| async 4 | 16186 | 14032 | 2154 | 169 | 54.93 | 0.025s | 19 / 73 | 585 → 4936MB |
| async 5 | 16173 | 14015 | 2158 | 44517 | 54.73 | 0.045s | 21 / 60 | 997 → 6120MB |

- **Lag** is `sequence.next_value - projections.caught_up_to` for
  `CompanyActiveTickets`, in events, sampled every 15s. Sync is always 0
  by construction.
- **Rejections** (13–15%) are the seed's own deliberate collisions, the
  same share in both variants and in 10-01.
- **Failures** have the same shape as 10-01. Steps 3–4: all within the
  last second, at `SIGINT`. Step 5: load-shed `503`s (43,313 of
  async's 44,517) plus connection errors at shutdown.
- 0 panics and 0 `ERROR` lines in any step.
- **RSS** peaks 28–40% above 10-01 at steps 4–5 (sync 28–33%, async
  34–40%; 10-01: 3531MB and 4574MB). Async adds ~4–12% on top of sync. Issue #34 owns
  the RSS question; this report only records that it got worse since
  10-01, independently of async.

## Frontend

`CompanyActiveTickets` going async meant a read right after the user's
own write usually missed it. Issue #15 worked around that with a second
refetch 750ms later. Now `api::submit_command` also selects
`triggeredEventSequences`, and the dashboard (`AfterWrite` in
`frontend/src/pages/dashboard.rs`) passes the highest one back as
`waitForSequence` on its next `CompanyActiveTickets` read. skilj holds
that read until the projection has folded the write. If the wait times
out, the dashboard falls back to a plain read rather than showing
nothing. `CustomerTickets` is sync and is read without waiting.

Checked in a real browser (headless Chromium, real Dex login as the
demo customer, `trunk serve`, debug `server`): after **Create**, the
dashboard sent one `CompanyActiveTickets` read with `waitForSequence`
set to the write's own sequence. skilj answered it in ~330–590ms (the
catch-up tick), and the new ticket was on screen 822ms and 838ms after
the click in two runs. No second request followed in the next 2s.
`tests/graphql.rs`'s
`a_read_waiting_for_the_writes_own_sequence_sees_the_write_in_an_async_projection`
covers the same contract against real Postgres.

## Setup

- `cargo build --release --bin server` twice from the same commit: as
  is (async), and with `fn sync() -> bool { true }` added to
  `CompanyActiveTickets` (sync, local patch, reverted).
- Postgres 18.6 from `~/.theseus/postgresql/18.6.0`, one disk-backed
  cluster, database dropped and recreated before every step.
- Same ramp as 10-01: `SEED_DEMO_TRAFFIC=1`, 8/20/40/80 workers at
  500/250/100/50ms, 4 minutes per step, a fresh server per step,
  `DATABASE_MAX_CONNECTIONS=90`, `HTTP_MAX_IN_FLIGHT_REQUESTS=70`,
  `RUST_LOG=info,skilj_core::db=debug`. All async steps ran first, then
  all sync steps.
- Samples every 15s (17 per step): server RSS, max `idle in
  transaction` age, connection count, projection lag.
- Counts come from the seed's own `demo-seed: fired fake command`
  (`accepted=true/false`) and `demo-seed: request failed` log lines. The
  seed keeps its own state and never reads a projection, so async lag
  can't change which commands it fires.
