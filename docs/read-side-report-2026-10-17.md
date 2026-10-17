# Read-side benchmarks — issue #31

**Date:** 2026-10-17
**Status:** complete. Harness in `scripts/bench/read-side/`; raw output
from this run is `read-side.json` in the run directory.

This is the read-side counterpart to the existing write/startup benches.
It answers the four questions in #31 against one live server:

- `queryEvents` paging over a long history under the
  `max_events_per_read` cap;
- catch-up of `alerter` and `engagement-watcher` from sequence 0;
- GraphQL projection-query latency while writes are running;
- websocket fan-out to N concurrent subscribers.

## Environment

| | |
|---|---|
| Machine | WSL2, Linux 6.18.33.2-microsoft-standard-WSL2 |
| CPU | 22 logical cores available to WSL |
| Memory | 15 GiB |
| Postgres | 18.6 (local cluster, port 5440) |
| Server config | `TICKET_ROUTING=shared`, `DATABASE_MAX_CONNECTIONS=80`, `HTTP_MAX_IN_FLIGHT_REQUESTS=200` |
| Build | `cargo build --release --bins` |
| Node | Node with the installed `ws` package for `fanout.js` |

`DATABASE_MAX_CONNECTIONS=80` is below this box's Postgres
`max_connections = 100`. It matters: at 120 the 64-subscriber fan-out lost
a subset of subscriptions after the write-load phase, while at 80 every
subscriber received every event. See the README's "Reading the numbers".

## Workload

- 5,000 `TicketCreated` events in the shared `helpdesk` context
  (`rs-company`), plus 100 more for `rs-small` used by the projection and
  fan-out phases.
- 5,000 `DailyActivityRecorded` events in `activity`, each with a
  distinct company/person so `engagement-watcher`'s per-company state has
  5,000 entries.
- `max_events_per_read` was left at skilj's default of **1,000**.
- Every catch-up run starts from a fresh `mode=auto` event token and a
  fresh state file, so it replays the whole backlog.
- The fan-out phase runs on a fresh server process per level. Before the
  three benchmark commands, each subscriber replays exactly one existing
  event (`fromSequence = latest TicketCreated - 1`); the helper only
  starts the commands after all N subscribers received that replay.

## Results

### `queryEvents` paging

5,000 `TicketCreated` events, page cap 1,000, five pages:

| Page | Events | Latency |
|---|---:|---:|
| 1 | 1,000 | 61.3 ms |
| 2 | 1,000 | 31.7 ms |
| 3 | 1,000 | 32.2 ms |
| 4 | 1,000 | 35.9 ms |
| 5 | 1,000 | 33.3 ms |

Total wall time **0.204 s** for 5,000 events, i.e. **~24,500
events/s**; page latency p50 33.3 ms, p95 61.3 ms. The first page is the
coldest; steady-state pages are ~32–36 ms.

### Catch-up from sequence 0

| Binary | Backlog | Elapsed | Rate | State |
|---|---:|---:|---:|---|
| `alerter` | 5,000 `TicketCreated` | 0.571 s | 8,759/s | 5,000 `created_at` entries |
| `engagement-watcher` | 5,000 `DailyActivityRecorded` | 0.357 s | 14,024/s | 5,000 `last_customer_activity` entries |

Both rates include process startup, the first poll, five capped pages,
and the state-file write; they are upper bounds on the feed-consumption
rate. At the default 1,000-event cap that is five HTTP round trips, so
the result is consistent with roughly 30–60 ms per page plus local
processing. A 50,000-event backlog would be ~50 pages and roughly
ten times the elapsed time at the same per-page cost.

### GraphQL projection latency under write load

45 seconds, six writer threads (`CreateTicket` followed by
`AssignTicket`), four concurrent projection readers, no
`waitForSequence`:

| Query | Idle p50 | Load p50 | Load p95 | Load p99 | Load max | Samples |
|---|---:|---:|---:|---:|---:|---:|
| `TicketSummary` | 6.74 ms | 7.95 ms | 11.66 ms | 16.60 ms | 56.88 ms | 4,790 |
| `CompanyActiveTickets` | 5.87 ms | 9.66 ms | 14.13 ms | 17.99 ms | 60.82 ms | 4,317 |

Zero query errors. The writers accepted **2,081** command iterations
(`CreateTicket` + `AssignTicket`) in 45 s, **46.2 iterations/s**, with
zero rejections. `CompanyActiveTickets` pays more under load than
`TicketSummary`, but the tail stays in the teens of milliseconds.

### Websocket fan-out

Three `TicketCreated` events per level, with every subscriber confirmed
ready before the first command:

| Subscribers | Events received | Delivery p50 | Delivery p95 | Delivery max |
|---:|---:|---:|---:|---:|
| 1 | 3 / 3 | 43–53 ms | 43–53 ms | 53 ms |
| 8 | 3 / 3 | 47–54 ms | 48–55 ms | 55 ms |
| 32 | 3 / 3 | 49–58 ms | 51–59 ms | 59 ms |
| 64 | 3 / 3 | 53–61 ms | 55–62 ms | 63 ms |

No missed deliveries, no subscription errors. These timings are measured
from the start of the REST `CreateTicket` request, and the trigger itself
takes 41–57 ms in the same runs, so actual fan-out delivery adds roughly
**2–10 ms** at 64 subscribers. Going from 1 to 64 subscribers moves the
p95 by about 9 ms on this box.

## Findings

1. **Read paging is not the bottleneck.** 5,000 events page at
   ~24k events/s, and both consumers catch up at thousands of events per
   second. The 1,000-event cap is the main lever: catch-up cost is
   linear in `backlog / max_events_per_read`.
2. **Projection reads stay fast under write load.** p95 stays under
   15 ms and p99 under 18 ms for both projections. The heavier
   `CompanyActiveTickets` read degrades more (p50 5.9 → 9.7 ms), which is
   the one to watch as ticket volumes grow.
3. **Websocket fan-out scales cheaply to 64 subscribers** in this
   workload: all events delivered, p95 +9 ms versus one subscriber, and
   no errors.
4. **Operational caveat found while benchmarking:** a database pool
   larger than Postgres's `max_connections` produced partial fan-out
   delivery at 64 subscribers (ready subscriptions then missing events)
   with no application error surfaced. Keep `DATABASE_MAX_CONNECTIONS`
   below the Postgres limit, or raise the limit with it, and monitor pool
   wait/connection errors.

## Reproduce

```sh
cargo build --release --bins
python3 scripts/bench/read-side/read_side.py \
  target/release/server \
  target/release/alerter \
  target/release/engagement-watcher \
  /tmp/read-side-out \
  --events 5000 --load-seconds 45 --fanout 1,8,32,64
```

The driver writes `read-side.json` and per-process logs; see
`scripts/bench/read-side/README.md` for the phase-by-phase details.
