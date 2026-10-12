# Consistency query report — 2026-10-11 (skilj main, `7f18dfb`, then `ba0b3e2`; released in 0.0.10)

skilj's main branch lets a command type declare a consistency query
(`CommandType::consistency_query()`, skilj docs/architecture.md §198):
which event types under which tags `decide()` needs, optionally only the
newest n. `decide()` then gets only those events, and only those
conflict with the command. This applies it to every command type here,
and measures it.

## Summary

- **Every command type with a growing read declares a query.** The ones
  that read a company's whole history now read one or two events:
  - `CreateTicket` and the company commands (`SignUpCompany`,
    `RecordCompanyTenant`, `RecordTenantLifecycle`, `ConvertCompanyTrial`,
    `ExpireCompanyTrial`, `ReactivateCompany`) read the company's latest
    lifecycle event instead of every event with its tag, which includes
    every ticket it ever filed. `CreateTicket` also reads whether this
    company already used the ticket id, by the ticket's own tag (now one
    of its `tag_mappings()`, listed first since an item is looked up by
    its first tag). Uniqueness stays per company, as before.
  - `RecordDailyActivity` reads this person's records at this company;
    without a query it read every record of the company *or* the person.
  - `activity::RecordEngagementDecline` reads the company's latest flag.
  - The three marketing commands read nothing in `decide()`, so each
    names the smallest read skilj allows (its own event type, company,
    latest).
  - `MergeTickets` reads, per ticket, the latest status event plus the few
    events a ticket has at most a handful of (created, escalated, rated,
    resolutions, merges) - not its back-and-forth or internal notes.
- **The nine single-ticket commands keep `TicketSnapshot`.** skilj refuses
  a query together with `snapshot()`. A query for them was built, passed
  every test, and measured about 1 ms *slower* for a rejected command
  (table below), with accepted ones even. Not worth dropping the snapshot
  for.
- **Measured** (sequential client, p50, 270k-event history):
  `CreateTicket` at a company's 2,000th ticket 61–65 → 18–25 ms; a
  company command after 2,000 tickets 58 → 5–8 ms; a daily-activity ping
  in a company of 2,000 records 41–62 → 7–12 ms.
- **Correctness**: a new fixture test, `tests/fixture/consistency_query.rs`,
  selects each command's query from mixed histories by skilj's rules and
  checks `decide()` decides the same as from every event with the
  command's tags, at every prefix. Removing one item from the ticket query
  makes it fail. Full suite: 240 tests, 0 failures, 0 skips. `cargo clippy
  --all-targets -- -D warnings` and `cargo fmt --check` are clean.

## What else changed

- This was built against a local `../skilj` checkout through
  `[patch.crates-io]`. skilj 0.0.10 released the queries, and
  `Cargo.toml` now depends on it from crates.io.
- `tests/projection_rebuild.rs` asserted a skilj 0.0.9 bug (a rebuilt
  projection's new field missing from the GraphQL type until a restart)
  and said to drop that half once skilj fixed it. On skilj main the field
  is there at the switch-over, so the test now asserts that.

## Benchmark

Release builds of `server`, both against the same skilj commit:
*baseline* is `main` with no query; *query* gave all twenty command types
queries, including the nine single-ticket ones in place of the snapshot.
Postgres 18.6 on disk. Each run copies a template database holding
270,300 synthetic events (300 companies, 30,000 tickets of 9 events,
interleaved) with every projection, snapshot and deadline cursor moved to
its head, then starts the server and drives it with one sequential
keep-alive HTTP client:

- A: sign up a company, file 2,000 tickets for it, then 100 rejected
  `SignUpCompany` for it;
- B: a ticket of 500 events and one of 2; for each, 300 rejected
  `ReopenTicket` and 100 accepted `AddInternalNote`;
- C: 200 people's activity at one company over 10 days (2,000 records),
  then 300 rejected and 100 accepted pings.

Two runs per build, alternating. Other work was running on the box (load
average 6–20), so compare builds within the table, not to other reports.

p50 in ms, run 1 / run 2:

| scenario | baseline | query |
|---|---|---|
| `CreateTicket`, tickets 1–250 | 18.9 / 13.1 | 11.3 / 10.6 |
| `CreateTicket`, tickets 1751–2000 | 64.8 / 60.9 | 18.4 / 25.3 |
| `SignUpCompany` rejected, after 2,000 tickets | 58.3 / 58.0 | 6.5 / 8.0 |
| 500-event ticket, rejected `ReopenTicket` | 5.2 / 6.3 | 7.1 / 7.0 |
| 500-event ticket, accepted `AddInternalNote` | 8.0 / 10.5 | 8.2 / 9.3 |
| 2-event ticket, rejected `ReopenTicket` | 4.5 / 6.6 | 6.2 / 7.1 |
| 2-event ticket, accepted `AddInternalNote` | 7.5 / 9.6 | 8.6 / 9.2 |
| `RecordDailyActivity` rejected | 43.8 / 61.7 | 6.9 / 7.3 |
| `RecordDailyActivity` accepted | 40.7 / 42.8 | 8.0 / 8.3 |

One run of the committed build (queries, snapshot for the single-ticket
commands), load average 10–15: `CreateTicket` 11.1 → 17.6 ms, company
command 5.1, rejected ticket command 4.3 (500 events) / 4.2 (2 events),
accepted note 7.0 / 6.9, activity 10.0 / 12.4.

Notes:

- **`CreateTicket` still grows** (about 11 → 18 ms over 2,000 tickets),
  where its read no longer does. Not looked into. A likely cause is
  `CompanyActiveTickets`, the async per-company projection whose one row
  holds every open ticket and is rewritten after each one; the benchmark
  never closes a ticket.
- **Why the query lost for a ticket command.** `EXPLAIN ANALYZE` of skilj's
  query read for the 500-event ticket shows two costs. `.latest()` over
  several types can't use the index's order (`event_type_name` comes
  before `sequence` in `event_tags_lookup`), so it reads all 500 index
  entries and keeps the newest. On a small `events` table the outer
  `sequence IN (...)` also becomes a sequential scan: 1.5 ms on 2,700
  events, against 0.18 ms (index lookups) on 273,000. The first trial
  ran on a fresh database and showed that sequential scan, which is why
  the table above uses the 270k template. Even there the snapshot stayed
  about 1 ms ahead, probably because its read starts at the snapshot
  rather than at the beginning, which the event cache can serve. That
  last part wasn't checked.

## Raw data

The driver (`bench.py`) and per-run JSON are in this session's
scratchpad and `~/bench-cq`, not committed, as with the earlier reports.

## Update: snapshot items (skilj `ba0b3e2`)

skilj then added snapshot items (skilj docs/architecture.md §202): a
`QueryItemMapping::snapshot::<S>(field)` in a consistency query hands
`decide_with()` snapshot `S`'s state for the value the field derives,
with skilj folding the events since through `Snapshot::fold_for`. It
also added `QueryItemMapping::nothing()` (§201). With those:

- **Every command type now declares a query.** The nine single-ticket
  commands' query is `ticket_snapshot_query()`, the ticket's
  `TicketSnapshot` item, in place of `snapshot()` +
  `decide_from_snapshot`. `MergeTickets` has one item per ticket, in
  place of `ticket_facts_query` (removed). The marketing commands
  read `nothing()`.
- **`TicketSnapshot` implements `fold_for`**, taking the ticket id from
  the tag value, so a state no longer needs its `TicketCreated` to learn
  which ticket it is. skilj now folds a `TicketsMerged` into both
  tickets' rows; before, only into the first value's. Rows written
  before are distrusted and refolded by skilj (a flag bit in
  `snapshot_version`), so `VERSION` stays 1.
  `tests/ticket_snapshot.rs` accounts for that flag.
- **Tests.** `decide_from_snapshot` is gone, and with it the
  every-split equivalence check: skilj folds now, not the decider.
  `tests/fixture/ticket_snapshot.rs` instead checks the fold reaches
  `TicketFacts::of` from the tag value alone, with both sides of a
  merge, and that a missing or unreadable state is rejected. Its
  `GivenTicket` folds the ticket tests' given events into the states
  `decide_with()` is handed. Removing `fold_for`'s tag-value line fails
  them. Full suite: 242 passed, 0 failed, 0 skipped; clippy and fmt
  clean.

Measured as above, plus a rejected `MergeTickets` (duplicate doesn't
exist). *HEAD* is the commit before this work (old `snapshot()`, no
queries), built against the same skilj; *morning* is the build
measured above, on `7f18dfb`. A quiet box this time (load average
about 1), so these runs are cleaner than the table above; p50 in ms,
run 1 / run 2:

| scenario | HEAD | snapshot items | morning |
|---|---|---|---|
| `CreateTicket`, tickets 1–250 | 11.3 / 11.3 | 8.0 / 8.0 | 9.6 / 7.7 |
| `CreateTicket`, tickets 1751–2000 | 56.1 / 57.8 | 15.8 / 15.6 | 16.2 / 15.5 |
| `SignUpCompany` rejected, after 2,000 tickets | 53.0 / 54.0 | 4.4 / 4.5 | 4.4 / 4.3 |
| 500-event ticket, rejected `ReopenTicket` | 3.5 / 3.7 | 3.9 / 3.8 | 3.7 / 3.6 |
| 500-event ticket, accepted `AddInternalNote` | 6.4 / 6.3 | 6.2 / 6.2 | 6.2 / 6.2 |
| 2-event ticket, rejected `ReopenTicket` | 3.8 / 3.7 | 3.9 / 3.8 | 3.7 / 3.5 |
| 2-event ticket, accepted `AddInternalNote` | 6.4 / 6.6 | 6.2 / 6.1 | 6.2 / 6.1 |
| `MergeTickets` rejected | 16.4 / 16.8 | 5.1 / 5.0 | 4.8 / 4.5 |
| `RecordDailyActivity` rejected | 41.0 / 40.9 | 4.9 / 4.8 | 5.1 / 4.8 |
| `RecordDailyActivity` accepted | 38.3 / 37.9 | 6.1 / 5.7 | 6.3 / 5.6 |

- A ticket command through a snapshot item costs what it did through
  `decide_from_snapshot`: about 0.2 ms more for a rejection, 0.2 ms less
  for an accepted note. skilj's own bench (§202) found the same.
- A merge through two snapshot items is about as fast as through
  `ticket_facts_query`, and 3× faster than reading both tickets'
  histories. It conflicts with every event of either ticket, as before
  the queries; `ticket_facts_query` conflicted with fewer. Merges are
  rare, so that was taken for one read path for every ticket command.
- The marketing commands are submitted from routes, not REST, so
  `nothing()` wasn't measured here.
