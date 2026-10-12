# Deadline drain benchmark (issue #28)

Measures what happens when tens of thousands of skilj deadlines come due at
once: how long the drain takes, what it does to foreground command latency,
and what skilj's `deadline_retention` sweep (§163) costs afterwards. The
results are in `docs/deadline-drain-report-2026-10-13.md`.

Standard library Python only. Needs a release `server`
(`cargo build --release --bin server`) and a Postgres reachable over TCP
(`PGHOST`/`PGPORT`/`PGUSER`, default `127.0.0.1:5440`/`postgres`, `PGBIN`
for `psql`/`createdb`).

## 1. Seed the templates (once, ~10 minutes for 50k)

    python3 seed.py ../../../target/release/server 50000 500 32 <workdir>

Real tickets through REST, so skilj's own schedule/cancel loops write every
deadline row. Leaves two template databases:

- `dd_created`: 50k tickets created and assigned, each with its two pending
  SLA deadlines (100k rows).
- `dd_resolved`: the same tickets resolved, each with one pending
  `ScheduleTicketAutoClose` deadline (7 days out) and two cancelled SLA
  deadlines (150k rows in all).

Then settle both, so no run starts with background catch-up (an unsettled
`dd_resolved` makes every run start with 1-2 minutes of stalls while the
`knowledge_base` route catches up - see the report):

    python3 settle.py <server> dd_created  <workdir>/settle-created.log
    python3 settle.py <server> dd_resolved <workdir>/settle-resolved.log

## 2. Run a drain

    python3 drain.py <server> dd_resolved ac-1 <outdir> --variant autoclose --retention
    python3 drain.py <server> dd_created  sla-1 <outdir> --variant sla
    python3 drain.py <server> dd_resolved ctl-1 <outdir> --variant none   # control

Each run copies the template, starts the server, then moves every pending
deadline of the variant's schedules to `now + --due-in` (180s); the last
60s before the due time are the "before" window. The foreground load is
`--fg-workers` (8) keep-alive clients, each paced at `--fg-interval-ms`
(250ms, ~32 commands/s in all), each walking its own tickets through
create -> assign -> note -> note -> resolve. Latency is client-side, from
request sent to response read, so it includes HTTP and the command batcher.

`--retention` then ages every resolved deadline past the default 30-day
retention and runs skilj 0.0.10's own retention statement
(`delete_expired_deadlines`) in `retention_tick`'s batches of 10,000, with
the foreground load still running. It replays the statement rather than
waiting on the server's own task, which only runs at startup and then
hourly.

Outputs per run: `<label>.json` (summary: drain time, fire rate, latency
p50/p95/p99 before/during/after, retention batches), `<label>-samples.csv`
(every 0.5s: sequence head, due/firing/fired counts, async projection lag,
active connections, lock waits, server RSS) and `<label>-latency.csv`
(every foreground command).

`run-all.sh <server> <outdir>` runs the report's sequence (drains
alternating with equally long controls, a no-load drain, an SLA drain).
`summarize.py <outdir>/<label>.json ...` prints the headline numbers,
`timeline.py <outdir> <label> [bucket-s]` latency and fires per bucket.

Never run two at once, and keep the box quiet: the numbers are wall-clock.
