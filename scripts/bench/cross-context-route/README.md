# Cross-context route benchmark (issue #29)

Measures how long a helpdesk commit takes to show up as the routed event in
another bounded context, and what skilj's `CrossContextRoute` loop does when
the target context is slow or failing. Results:
`docs/cross-context-route-report-2026-10-13.md`.

Standard library Python only; reuses `../deadline-drain/common.py` (server
start-up, REST client, psql helpers), so the same `PGHOST`/`PGPORT`/`PGUSER`/
`PGBIN` variables apply (default `127.0.0.1:5440`, user `postgres`).

Routes covered (`src/lib.rs`):

| route | source (helpdesk) | target |
|---|---|---|
| `HelpdeskExpiryToTrialLapse` | `CompanyExpired` | `marketing.TrialLapsed` |
| `HelpdeskActivationToTrialConversion` | `CompanyActivated` | `marketing.TrialConverted` |
| `HelpdeskResolutionToArticleSuggestion` | `TicketResolved` | `knowledge_base.ArticleSuggested` |

## One run

    python3 route.py <server> <template-db|-> <label> <out-dir> [options]

`-` starts from an empty database; otherwise the template is copied (the
50k-ticket `dd_resolved` from `../deadline-drain/seed.py` + `settle.py`).
Traffic: lifecycle probes on fresh companies (sign up, expire, `--gap-ms`,
reactivate; `--probe-rate` companies/s) all run long, plus the deadline-drain
ticket walk (`--fg-workers` x `--fg-interval-ms`) in phases flagged `load`.
`--phases name:seconds[:load][+fault],...` sequences them; `--fault` is one of
`outage`, `flaky:P`, `slow:MS` (triggers on the target's `events`) or `lock`
(a transaction holding the target's `sequence` row), in `--fault-context`
(`marketing` by default). `--redrive` calls `retryParkedDelivery` over
GraphQL for everything parked once the routes have settled.

Latency is commit to commit: routed event `metadata_created_at` minus source
event `metadata_created_at`, matched on `company_id` / `ticket_id`, bucketed
by the phase the source event was committed in. Every source event is
accounted for: routed (exactly once, duplicates counted), parked (matched on
the parked delivery's identifier = source sequence) or missing.

Outputs: `<label>.json` (summary), `<label>-pairs.csv` (every source/target
pair), `<label>-samples.csv` (every 0.5s: route cursors, how far behind each
is in helpdesk sequence numbers, retry attempt, parked counts, lock waits,
RSS), `<label>-latency.csv` (every REST command).

`run-all.sh <server> <out-dir>` runs the report's sequence (~25 minutes);
`summarize.py <out-dir>/<label>.json ...` prints the headline tables.
Never run two at once; keep the box quiet.
