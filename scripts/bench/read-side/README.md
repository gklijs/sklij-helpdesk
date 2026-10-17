# Read-side benchmarks

Issue #31's read-side counterpart to `deadline-drain/`, `startup/`,
`cross-context-route/` and `tenant-scaling/`. It measures:

1. `queryEvents` paging over a 5,000-event history, at the server's
   `max_events_per_read` cap (skilj's default is 1,000);
2. catch-up of the real `alerter` and `engagement-watcher` binaries from
   their own fresh `mode=auto` event-token cursor (sequence 0) over the
   same-sized backlog;
3. `TicketSummary` / `CompanyActiveTickets` GraphQL projection latency
   while six writer threads submit `CreateTicket` + `AssignTicket`;
4. websocket `allEvents` fan-out to 1, 8, 32 and 64 concurrent
   graphql-transport-ws subscribers.

## Run

The box needs the same local Postgres the other benches use (port 5440,
`postgres`/trust), Node with the already-installed `ws` package, and the
release binaries:

```sh
cargo build --release --bins
python3 scripts/bench/read-side/read_side.py \
  target/release/server \
  target/release/alerter \
  target/release/engagement-watcher \
  /tmp/read-side-out \
  --events 5000 --load-seconds 45 --fanout 1,8,32,64
```

It drops/recreates `rs_readside`, writes `read-side.json` plus per-process
logs into the output directory, and prints progress. The driver uses only
Python's standard library; `fanout.js` is the one Node helper.

## Reading the numbers

- `DATABASE_MAX_CONNECTIONS` is deliberately **80**, below this box's
  Postgres `max_connections = 100`. At 120, a 64-subscriber fan-out after
  the write-load phase lost a subset of otherwise-ready subscriptions
  (they received the readiness replay, then none of the benchmark
  events); at 80 every subscriber received every event. If you raise the
  pool, raise Postgres's limit with it, or the fan-out number becomes a
  measurement of pool exhaustion rather than of fan-out.
- Each fan-out level gets a fresh server process. Closed websocket
  connections are not necessarily reaped before the next level's N
  upgrades; measuring 64 on top of the previous 1+8+32 made "64" mean
  "105 concurrent subscriptions", not 64.
- Fan-out readiness uses `fromSequence = latest TicketCreated - 1`: every
  subscriber first receives one replay event. Only after all N have
  received that replay does the helper submit the benchmark commands, so
  a slow `require_read_mapping` cannot be mistaken for a missed fan-out.
- Projection latency is sampled with the query's normal read path (no
  `waitForSequence`); the writer threads are the load, not a barrier.
- Catch-up timings include process startup and the binary's state-file
  writes, so they are upper bounds. Poll interval is lowered to 50ms to
  keep idle time out of the number.
