# Ticket snapshot report — 2026-10-05 (issue #14, skilj 0.0.9)

Every ticket command's `decide()` re-read the ticket's whole history.
Issue #14 asked for a `Snapshot` so they don't have to, a test that
bumping `Snapshot::VERSION` refolds a stored row from the full history,
and a latency comparison on a ticket with ~500 events.

## Summary

- **`TicketSnapshot` (tag `ticket`) is used by all nine single-ticket
  commands**: Assign, Resolve, Reopen, RequestInfoFromCustomer,
  CustomerRespondsToTicket, Close, Escalate, Rate and AddInternalNote.
- **On a 500-event ticket, commands are 31–46% faster**: rejected
  15.9 → 8.5 ms, accepted 21.3 → 14.6 ms (median p50 over 5 runs). With
  the snapshot, the 500-event ticket costs the same as a 2-event one.
  Small tickets are unchanged (9.6 vs 9.7 ms).
- **Both decision paths go through one fold.** `TicketFacts` gathers
  everything those commands read from a ticket's history. `decide()`
  folds it from the full history; `decide_from_snapshot()` resumes it
  from the stored row plus the events since. Both then run the same
  `decide_with`. That gives skilj's "identical decision" requirement by
  construction, and a fixture test checks it at every possible split
  of nine histories, for every command.
- **A `VERSION` bump is handled**, tested against a real database: an
  older-version row is not trusted by a decision, and the next catch-up
  refolds it from the ticket's whole history (§152).
- **`CreateTicket` and `MergeTickets` keep plain `decide()`.** skilj
  only uses a snapshot for a command whose only tag is the snapshot's.
  `MergeTickets` tags two tickets; `CreateTicket` tags company and
  ticket.
- **No company snapshot.** The `company` tag's history includes every
  `TicketCreated` of that company, so it grows with the company.
  `CreateTicket` reads it on every ticket, but it's two-tag and can't
  use a snapshot. The company-only commands (sign-up, trial
  conversion/expiry, reactivation, tenant records) are rare, so a
  company snapshot would speed up only commands that hardly ever run.
  Speeding up `CreateTicket` needs skilj to compose single-tag snapshots
  (listed as unbuilt in the `Snapshot` trait's doc comment).

## What changed

- `src/helpdesk.rs`:
  - `TicketFacts` (status, company, requester, created priority,
    resolution count, escalated, rated, and the ticket's own id) replaces
    `ticket_status`/`resolution_count`/`company_id_for_ticket`/
    `requester_id_for_ticket`/`reject_unless_requester`.
  - `TicketSnapshot`: `TAG_KEY = "ticket"`, `OWNER_TAG_KEY =
    Some("company")` (so `inspectSnapshot` is company-scoped like the
    projections), `VERSION = 1`, `PARTITION_COUNT = 4`. Its catch-up is
    keyed per ticket, so it spreads over every partition; see
    `docs/partitioned-projection-report-2026-10-05.md` for why that
    matters with more than one instance.
  - Each single-ticket command: `snapshot()`, `decide()` and
    `decide_from_snapshot()` all delegate to an inherent `decide_with`.
    The decision logic itself is unchanged.
  - An unreadable stored row is rejected (`snapshot_unreadable`) rather
    than decided on. skilj never hands over an older-version row, so
    this only fires on a bug.
- `TicketsMerged` is the one event tagged with two tickets, and skilj's
  snapshot `fold` gets no key. So `TicketFacts` records its own ticket id
  from `TicketCreated` and only lets a merge mark it merged when it's the
  duplicate.
- Tenant contexts get the snapshot too: skilj resolves snapshots through
  the template (`SnapshotDispatcherImpl` in skilj 0.0.9), so a tenant
  context uses `helpdesk`'s registration. This is from reading skilj's
  code, not from a test here.

## Tests

- `tests/fixture/ticket_snapshot.rs` (no database):
  - every single-ticket command, with valid and invalid payloads
    (including a wrong `requester_id`), against nine histories (empty,
    each status, a 23-event mixed history, and both sides of a merge),
    split at every point, folded the way the catch-up folds;
  - the snapshot folding a ticket without being told which one;
  - an unreadable row being rejected.
- `tests/ticket_snapshot.rs` (Postgres):
  - the `VERSION` bump: a row set back to an older version with a
    foreign shape. The next `ResolveTicket` still numbers itself 2 (so
    it saw the full history, not the row), and the row is then refolded
    to the full history (status, company, priority, `resolutions = 2`).
    `VERSION` is a const, so a stale row is the only way to test this
    inside one binary.
  - proof the stored row is really read: plant a current-version row
    claiming 41 resolutions; the next `ResolveTicket` is number 42.
    Without this, a misconfiguration that made skilj fall back to
    `decide()` would pass every other test silently.

Full suite: 222 tests, 0 failures, 0 skips. `cargo clippy --locked
--all-targets -- -D warnings` and `cargo fmt --check` are clean.

## Benchmark

Two release builds of `server`, identical except that one returns
`None` from the nine commands' `snapshot()`. Postgres 18.6 on disk, a
fresh database per run, one server, one sequential HTTP client
(keep-alive), so every batch holds one command.

Per run: sign up a company; create ticket `big` (create, assign, then
249 rounds of `RequestInfoFromCustomer` + `CustomerRespondsToTicket` =
500 events) and ticket `small` (2 events); wait 3s for the catch-up.
Then for each ticket: 20 warm-up and 300 timed `ReopenTicket` (rejected,
since the ticket is in progress: reads and decides only), then 100
timed `AddInternalNote` (accepted: also writes). Five runs per build,
alternating builds.

Client-side latency, median of the five runs' p50 (ms):

| ticket | command | without snapshot | with snapshot | change |
|---|---|---|---|---|
| 500 events | rejected | 15.9 | 8.5 | −46% |
| 500 events | accepted | 21.3 | 14.6 | −31% |
| 2 events | rejected | 9.6 | 9.7 | — |
| 2 events | accepted | 13.1 | 12.0 | — |

Per run, the 500-event/2-event p50 ratio for rejected commands was
1.78 / 1.35 / 1.65 / 1.70 / 1.73 without the snapshot, and
0.84 / 0.90 / 1.00 / 0.91 / 0.81 with it.

Notes:

- **Run 2 of both builds was disturbed** by other work on the box
  (load average ~12 when the next round started). Its p95s reached
  200 ms, and its small ticket came out slower than its big one. It's
  kept in the medians, which it doesn't move.
- The saving is in loading `matching_events`, which happens before
  skilj's own `decide_us` timing starts. That's why client-side latency
  is the number reported. The server's debug timing went to stdout,
  which the driver discarded, so no `decide_us` was captured.
- The gain grows with history length: without the snapshot the cost
  grows with the ticket's events; with it, only with the events since
  the last catch-up tick (at most ~500ms of them).

## Raw data

Driver (`bench.py`) and per-run output are in this session's scratchpad,
not committed, as with the earlier reports.
