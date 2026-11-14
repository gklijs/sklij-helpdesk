# Dashboard refresh, paging and erasure (#101–#103)

The browser uses `POST /v1/helpdesk/tickets` with its verified OIDC bearer token.
The server resolves `companyId` to its current tenant, checks the active role,
context and company grant, respects registered projection capability gates,
and checks both queue and content owners. Customers receive only their own
tickets. Staff need the `staff` capability for the company queue and sensitive
read permission for other customers' text. Discovery does not grant access.

The response contains at most 25 queue entries and title summaries. Details
are selected only for explicitly opened tickets on that page. Each conversation
page contains at most 20 messages. Titles in summaries are limited to 200
characters; detail text is a 16,000-character preview per field. The UI labels
truncated details. No shared company projection stores customer plaintext.

One SQL query pages the company queue and selects only the requested encrypted
fields from customer rows. Historical tickets and omitted messages are not
transferred to the application or browser. The database still parses existing
JSON projection rows: this bounds requests, decryption and response payload,
not database CPU independently of history size. A dedicated relational index
would be a separate optimization if those parses become the measured bottleneck.

A refresh aborts its preceding browser request. A generation and disposal check
also prevents older successes or failures from publishing. Queue, content and
error state are published in one reactive batch. `waitForSequence` is preserved;
a catch-up timeout fails visibly without substituting a stale read. Pagination
clears open-detail selections. Live bursts coalesce and re-read the latest state.

## Content availability

The adapter decrypts declared text fields with the exact customer's active
`customer` data key. It returns plaintext only after successful authenticated
decryption. It never infers encryption from base64 shape, and never sends
ciphertext to the dashboard. Missing/destroyed keys, denied sensitive access
and key-provider failures all return `available: false` and **Content unavailable**.
The API deliberately does not disclose whether a subject was permanently erased.
Existing legacy plaintext without an authenticated encrypted representation is
also unavailable through this adapter; no string-shape fallback is used.

Erasure does not push a notification to every open browser. Click **Refresh
tickets** or reload after `forgetSubject`; the resulting page replaces cached
plaintext. Failed refreshes clear cached ticket content. A browser that has not
refreshed can still display its previously delivered plaintext; server-side key
destruction cannot retract information already delivered to a client.

## Reproducible measurement

Run against a disposable PostgreSQL database:

```sh
DATABASE_URL=postgres://... cargo test --locked --test ticket_reads -- --nocapture
```

The fixture creates 100 customers' active tickets through real encrypted domain
writes, then enlarges one encrypted customer projection to 1,000 historical
tickets and 1,000 messages. The old 101-request serial GraphQL read and the new
adapter use real localhost HTTP, the same staff identity and database. This is
a bounded fixture measurement, not a production percentile or a dependency
performance comparison. Hardware: Intel Core Ultra 9 185H, Linux/WSL, Docker
PostgreSQL 16, debug application, skilj 0.4.0, no telemetry exporters.

| Read | HTTP requests | Response bytes | Elapsed |
|---|---:|---:|---:|
| Old full queue + serial customer projections | 101 | 337,210 | 1,378.91 ms |
| New first queue page | 1 | 12,248 | 55.19 ms |
| One opened conversation, 20 of 1,000 messages | 1 | 13,753 | 64.66 ms* |

\* Detail timing calls the same server read function directly; it excludes HTTP
handling. The enlarged customer's stored encrypted row alone is 387,206 bytes.
Pages change what is rendered: the table shows 25 tickets at a time, and details
load on demand. The old read eagerly loaded all 100 tickets and all history.

The integration test also checks legitimate base64-like text, customer and
company isolation, warm-key-cache erasure, another customer's readability,
and revocation. The browser suite delays an older success and an older failure
until a newer priority/status/conversation is visible; another journey loads
protected title/description/messages/rating, erases the customer, refreshes and
checks the unavailable state alongside another customer's unchanged content.
