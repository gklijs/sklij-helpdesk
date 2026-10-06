# skilj-helpdesk

[![CI](https://github.com/gklijs/sklij-helpdesk/actions/workflows/ci.yml/badge.svg)](https://github.com/gklijs/sklij-helpdesk/actions/workflows/ci.yml)

A showcase SaaS helpdesk built on [skilj](https://crates.io/crates/skilj) (the released 0.0.9 from crates.io) — a Rust library for
event-sourced, DDD-style applications. This project exists to exercise
skilj for real: every piece below was built, run, and verified against
a live stack, not just written and assumed to work.

**Start here:** [`specs/skilj-helpdesk.allium`](specs/skilj-helpdesk.allium)
is the domain spec — scope, entities, rules, and surfaces, written with
[Allium](https://allium-lang.org/). Every rule in it has a real
implementation; the spec's own comments note the couple of deliberate
simplifications (see "What's not built" below).

![The staff dashboard: a ticket waiting on the customer with a real message thread, and one still open](docs/screenshot.png)

<details>
<summary>More screens: dark mode, the customer view, and the monitoring dashboard</summary>

|  |  |
|---|---|
| ![The same staff dashboard in dark mode, toggled with the theme switch next to Log out](docs/screenshot-dark.png) | ![The customer view: only your own tickets, replying to a staff request for more info](docs/screenshot-customer.png) |
| Dark mode — a manual override on top of the system default, see "A real login" below | The customer view — same app, same login page, a different role |

![The provisioned Grafana dashboard: command/event throughput, REST latency, and the escalation/merge/CSAT/notes panels added beyond the original spec, all against a live local stack](docs/screenshot-monitoring.png)

</details>

## What this is

- **A company** signs up for a free trial, then must convert to a paid
  subscription (mocked payment) or lose the ability to create new
  tickets — reversibly; they can always come back.
- **Customers** file support tickets; **staff** work through them
  (assign → resolve → close, with a request-info/waiting-on-customer
  detour).
- **Alerting**: urgent tickets page a lead immediately; a ticket
  nobody's handled in a while gets escalated (priority bumped, a real
  persisted `TicketEscalated`, not just a console alert) and paged too —
  via a separately-deployable consumer of skilj's own REST event feed,
  not anything built into skilj itself.
- **Two deadline-driven rules** (trial conversion, ticket auto-close)
  run through skilj's own native per-entity deadline mechanism
  (`ScheduleDeadline`) - no separate process needed, unlike alerting:
  skilj's cron-based scheduling primitive fires on a shared schedule for
  a whole event type, not per-entity deadlines, but 0.0.7 added a
  one-shot timer scoped to a single entity's own tags, purpose-built for
  exactly this.
- **Three more flows beyond the original spec** — ticket merging (a real
  showcase of skilj's own DCB model reading two tickets' own histories
  in one command, no aggregate boundary needed), CSAT ratings, and
  staff-only internal notes — each grounded in how Zendesk/Freshdesk/
  Jira Service Management actually work, added to give the telemetry
  work below genuinely varied traffic to show.
- **A `Snapshot` for long-lived tickets** (`TicketSnapshot`): every
  single-ticket command decides from a stored per-ticket state plus the
  events since, instead of re-reading the ticket's whole history. On a
  500-event ticket that's 31–46% faster, and as fast as a new ticket —
  see `docs/ticket-snapshot-report-2026-10-05.md`.
- **A real login**: a self-hosted OIDC provider (Dex), Authorization
  Code + PKCE, a real customer/staff dashboard in the browser — with a
  dark mode (`frontend/src/theme.rs`) that follows the system's own
  `prefers-color-scheme` by default, overridable per browser via the
  toggle next to "Log out"/"Log in". The dashboard is live: it
  subscribes to skilj's `allEvents` over a websocket and re-reads on
  every ticket event, so other people's changes (and escalations from
  the alerter) show up without a reload (`frontend/src/live.rs`).

### GDPR erasure

Everything a customer writes or is asked — ticket title and
description, the request-info conversation, their rating comment, and
optional `requester_name`/`requester_email` — is a skilj
`sensitive_field`, encrypted under one key per customer (subject
`customer`, keyed by `requester_id`). To erase a customer:

```graphql
mutation { forgetSubject(boundedContext: "helpdesk", subjectKey: "customer",
                         subjectValue: "<requester_id>") { status } }
```

skilj destroys the key, so that customer's data reads back as
ciphertext everywhere from then on, and their pending auto-close
deadline is resolved as `forgotten` (`tests/customer_erasure.rs`).

That's why the frontend reads two projections. `CompanyActiveTickets`
(per company) has each ticket's status, priority and assignee, but no
text. It only holds tickets that can still change: a merged duplicate
leaves at once, and a closed ticket leaves once it's rated. `CustomerTickets` (per customer) has the content. skilj decrypts a
projection row only against its own key: a customer reads their own row
because their IdP subject is their `requester_id`, and staff read every
row through `can_read_sensitive`. Another customer of the same company
sees only ciphertext.

Not covered: ticket text stored before this was introduced stays
plaintext (skilj has no backfill), and so does the retired
`CompanyTicketList` projection's last stored state. The also-retired
`CompanyTicketQueue` holds no text. Internal notes are
staff-written and gated separately (`TEAM_ONLY`), not encrypted. The
frontend can't tell ciphertext from text, so an erased customer's
tickets show base64 rather than "erased".

### Inbound email over NATS

Customers can email the helpdesk: `support+<company_id>@...` opens a
ticket, `ticket+<ticket_id>@...` answers one that's waiting on them.
`src/bin/email-bridge.rs` reads emails from a NATS JetStream stream and
uses skilj's own broker bridge, `skilj-nats`, to trigger `CreateTicket`
and `CustomerRespondsToTicket`. The bridge takes care of retries,
`Idempotency-Key` and parking failed deliveries. Everything is keyed on
a hash of the email's `Message-ID`, so a second copy of the same email
changes nothing, and that key is recorded as the events' correlation
id. Emails it can't place stay on `helpdesk.email.unroutable` with the
reason. An email customer is identified by address (`email:<address>`),
separately from their portal login. It only works with the shared
`helpdesk` context, not per-company tenants. `src/email_channel.rs`
covers the design and its gaps, and `tests/email_channel.rs` runs it
end to end against a real NATS server in Docker.

### Changing a projection: zero-downtime rebuilds

`TicketSummary` gained `first_responder_staff_id` (the staff member
whose request for info was the customer's first reply) after tickets
already existed. A projection's stored state only ever reflects the code
that folded it, so tickets answered before the deploy need their history
replayed. skilj does that next to the live projection, without taking
reads down:

1. **Deploy.** On startup the new build's `TicketSummary` schema differs
   from the stored one, so reconciliation *stages* a rebuild instead of
   touching the live projection (`APPLICATION_VERSION` is bumped with
   it). Reads keep serving the old state. New events are folded into it
   by the new code, so the new field is only right for tickets that
   start after the deploy.
2. **Retire the old build first.** A rebuild is folded by whichever
   instance's catch-up reaches it first, using that instance's code. An
   older build still running would fold it into the old shape.
3. **Start it:**

   ```graphql
   mutation { rebuildProjection(boundedContext: "helpdesk", name: "TicketSummary") { status } }
   ```

   The background catch-up replays every event into a separate state.
   Reads still get the old state. Progress is visible on
   `projections(boundedContext: "helpdesk") { name schemaVersion buildingRebuild { caughtUpTo } }`.
4. **Switch-over.** Once the replay has caught up, skilj swaps the
   rebuilt state, schema and `schemaVersion` in, in one transaction.
   No step for you.
5. **Restart once.** In skilj 0.0.9 the switch-over doesn't refresh the
   GraphQL schema, so the new field shows up on `helpdesk_TicketSummary`
   only after each instance restarts. The REST/database state is
   already current.

If the deploy is rolled back instead, drop the staged rebuild:
`discardProjectionRebuild(boundedContext: "helpdesk", name: "TicketSummary")`.
The live projection never changed. The next deploy of the new shape
stages it again.

`tests/projection_rebuild.rs` runs this whole rollout against real
Postgres: discard, redeploy, rebuild, and the switch-over. It checks
that reads return the old state until the last moment, and that the
rebuilt state is what goes live. The test also pins the schema-refresh
gap from step 5, so it will fail once skilj fixes it.

## Layout

| Path | What |
|---|---|
| `specs/skilj-helpdesk.allium` | The domain spec |
| `src/helpdesk.rs` | Every `EventType`/`CommandType`/`Projection` — the actual domain logic |
| `src/alerting.rs` | Pure decision logic `src/bin/alerter.rs` drives |
| `src/scheduling.rs` | Env-driven durations + the mocked payment call `src/helpdesk.rs`'s own deadline reactors (`ScheduleCompanyTrialConversion`/`ScheduleTicketAutoClose`) use |
| `src/telemetry.rs` | Shared OTel wiring every binary calls into (see "Telemetry & dashboards" below) |
| `src/demo_seed.rs` | Pure decision logic behind the optional fake-traffic loop (`SEED_DEMO_TRAFFIC=1`) |
| `src/bin/server.rs` | The runnable server (REST + GraphQL) |
| `src/bin/alerter.rs` | Consumes the event feed, pages a lead on urgent tickets and escalates ones nobody's handled in time |
| `src/bin/engagement-watcher.rs` | Sweeps for companies whose customers have gone quiet and records an engagement decline (`activity` context) |
| `src/bin/provisioner.rs` | Reacts to `CompanySignedUp` by provisioning the company's own tenant bounded context |
| `src/bin/lifecycle-replicator.rs` | Mirrors a company's lifecycle (trial/active/expired) into its tenant context |
| `src/email_channel.rs` | Inbound email: which command an email becomes, plus the JetStream translator loop |
| `src/bin/email-bridge.rs` | Turns emails on NATS JetStream into tickets and replies, via `skilj-nats` |
| `tests/` | Integration tests (real HTTP, real Postgres) — split into several files by concern; see `tests/company.rs`'s own doc comment for why |
| `dex/config.yaml` | The real OIDC provider's config (two demo logins) |
| `frontend/` | The Leptos (WASM) web app |
| `observability/` | Local Grafana/Prometheus/Tempo/Loki stack + provisioned dashboard (see below) |

## Running it

You'll need: a Postgres database, Go (only to build Dex once — no
prebuilt binary exists), `wasm32-unknown-unknown` + [trunk](https://trunkrs.dev/)
(only for the frontend).

**Shortcut**: once Dex is built (step 1 below), `scripts/dev.sh` does
steps 2-3's server half in one command — starts a throwaway local
Postgres (unless `DATABASE_URL` is already set), Dex if it finds a
built binary, then the server, and tears all three down cleanly on
Ctrl+C. Still a separate `cd frontend && trunk serve` for the UI. The
steps below are what it's actually doing, spelled out.

**1. Build Dex once** (a real OIDC provider — no prebuilt binary, no
Docker assumed):

```sh
git clone --depth 1 --branch v2.45.1 https://github.com/dexidp/dex.git
cd dex && go build -o dex ./cmd/dex
```

Run it against this project's config: `./dex serve dex/config.yaml`
(listens on `127.0.0.1:5556`).

**2. Start the server**, pointed at a real Postgres and at Dex:

```sh
DATABASE_URL=postgres://... OIDC_ISSUER_URL=http://127.0.0.1:5556/dex \
  ENCRYPTION_MASTER_KEY=$(openssl rand -hex 32) cargo run --bin server
```

`ENCRYPTION_MASTER_KEY` wraps the per-customer keys that everything a
customer wrote is encrypted under (see "GDPR erasure" above). Keep it
the same for as long as you keep the database; a new key makes every
stored ticket's content unreadable. `scripts/dev.sh` generates one only
for its throwaway Postgres.

It prints every credential the rest of this needs, and the exact
command to run `alerter` against it (trial conversion and ticket
auto-close run in-process, via skilj's own native per-entity deadlines
- no separate binary). Sign up the demo company it references (`curl`
example included in its own output) before there's anything to see.

**3. Run the frontend**:

```sh
cd frontend && trunk serve
```

Open `http://127.0.0.1:8081`. Log in as `customer@acme.example` /
`customer-demo-pw` (customer view) or `lead@acme.example` /
`staff-demo-pw` (staff view) — see `dex/config.yaml`.

**4. Optionally, the alerter** — prints its own required env vars when
`server` starts:

```sh
cargo run --bin alerter    # pages a lead on urgent/overdue tickets
```

`alerter`'s own paging is console-only by default; set `SLACK_WEBHOOK_URL`
(a real [Slack incoming webhook](https://api.slack.com/messaging/webhooks)
URL) to also post each alert there — a worked example of the real-channel
seam `src/bin/alerter.rs`'s own module doc comment describes.
`tests/alerter_slack_webhook.rs` verifies the actual HTTP contract
(a real `POST {"text": ...}`) against a fake webhook receiver, not a
real Slack workspace — that part's on you to point at your own.

**5. Optionally, the email bridge** — needs a NATS server with
JetStream (`nats-server -js`, or `docker run -p 4222:4222 nats -js`), and
also prints its env vars when `server` starts:

```sh
cargo run --bin email-bridge
```

**Tests**: `cargo test` — real integration tests against a real (or
[`postgresql_embedded`](https://crates.io/crates/postgresql_embedded))
Postgres; DB-dependent ones skip cleanly if neither is reachable.
`tests/email_channel.rs` also needs Docker, for its NATS server, and
skips without it.
`cargo test --test fixture` runs just the domain rules - every
command's accept/reject paths and every projection fold, through
`skilj-test-fixture`, with no database (`tests/fixture/`), so those
never skip.
`cd frontend && cargo test --target wasm32-unknown-unknown` isn't a
thing (no frontend unit tests this pass) — it's verified by actually
running it (see below).

## Telemetry & dashboards

`skilj-core`/`skilj-rest`/`skilj` already emit real `tracing` spans and
OTel metrics throughout (command outcomes, event throughput, REST
request latency, background-task health) — all three of this project's
binaries now wire that up for real (`src/telemetry.rs`, the same
reference pattern `skilj-demo/src/bin/server.rs` establishes), and
`observability/` is a local Grafana stack to actually look at it, plus
an optional fake-traffic generator so there's something moving.

**1. Bring up the stack** (an OTel Collector + Prometheus + Tempo + Loki
+ Grafana, entirely separate from the Postgres/Dex steps above — see
`observability/docker-compose.yml`'s own doc comment):

```sh
docker compose -f observability/docker-compose.yml up -d
```

**2. Point the binaries at it** — `OTEL_EXPORTER_OTLP_ENDPOINT` unset
(the default) still works exactly as before, console-only:

```sh
export OTEL_EXPORTER_OTLP_ENDPOINT=http://localhost:4318
cargo run --bin server      # and, in its own terminal, alerter
```

**3. Open Grafana** at <http://localhost:3000> (no login needed locally
— anonymous admin, see the compose file) — the "skilj-helpdesk overview"
dashboard is already there, auto-provisioned, refreshing every 5s.
Traces and logs are in Grafana's own Explore view against the Tempo/Loki
datasources (also auto-provisioned, cross-linked from a trace to its own
logs). The "Average CSAT"/"CSAT distribution" panels are a real domain
metric, not just command throughput — `server.rs`'s own
`run_csat_metrics_loop` consumes `TicketRated` off the event feed
(`skilj-core`'s generic counters only ever see *that* a rating happened,
never the 1-5 value) and records it as `skilj_helpdesk_ticket_ratings_total`,
labelled by rating.

**4. Optionally, generate fake traffic** so the dashboard actually has
something to show without driving curl by hand — `SEED_DEMO_TRAFFIC=1`
on `server` spawns a background loop (`src/demo_seed.rs`) that signs up
a small cast of fake companies (`wonka-industries`, `stark-labs`,
`hooli` — distinct from this README's own `acme` walkthrough company)
and keeps creating/assigning/resolving fake tickets against this same
server's own REST surface, occasionally urgent (so `alerter` has
something to page on) and occasionally a deliberately invalid
transition (so the dashboard's rejection-rate panel isn't always zero):

```sh
SEED_DEMO_TRAFFIC=1 cargo run --bin server
# SEED_DEMO_INTERVAL_MS=... to change the pace (default 4000)
```

**Want more load?** `SEED_DEMO_CONCURRENCY=N` runs `N` independent
fake-traffic workers instead of one, each pacing itself at
`SEED_DEMO_INTERVAL_MS` — roughly `N`× the request rate, spread smoothly
rather than bursting in lockstep (each worker staggers its own first
tick). Turn this up when you actually want the dashboard's rate/latency
panels moving hard, e.g.:

```sh
SEED_DEMO_TRAFFIC=1 SEED_DEMO_CONCURRENCY=10 SEED_DEMO_INTERVAL_MS=200 cargo run --bin server
```

`SEED_DEMO_COMPANIES=N` (default 3) spreads the tickets over `N`
companies instead. Only a load test of the partitioned
`CompanyActiveTickets` needs this. That projection is keyed by company, so it
can use at most as many partitions as there are companies.

Start `server` itself with short deadlines
(`TRIAL_DURATION_DAYS=0 AUTO_CLOSE_AFTER_DAYS=0 cargo run --bin
server` — both env vars `ScheduleCompanyTrialConversion`/
`ScheduleTicketAutoClose` read directly, no separate process to start)
to see trial-conversion and auto-close traffic immediately too, instead
of after real days.

**All in one command**: `scripts/dev.sh` does steps 1-2 for you when
`OTEL=1` is set, and passes `SEED_DEMO_TRAFFIC` straight through:

```sh
OTEL=1 SEED_DEMO_TRAFFIC=1 scripts/dev.sh
```

Tear the observability stack down with
`docker compose -f observability/docker-compose.yml down -v` (or just
Ctrl+C `dev.sh`, if that's what started it) — nothing in it persists on
purpose.

## What's not built

Noted here, and in the relevant file's own doc comment, rather than
silently absent:

- **Real payment processing** — mocked on purpose; this is a showcase,
  not a billing product.
- **Ticket routing into tenants — Phase 4 is complete** — the spec calls for each
  company to be its own skilj tenant (bounded context), stamped from a template
  context), stamped from a template via
  `CreateBoundedContextFromTemplate`. That mechanism was first proven in
  isolation by `tests/multi_tenant_provisioning.rs` (stamps a real
  second bounded context from the shared `helpdesk` context as its
  template, grants a role access to it in the same call, runs
  `SignUpCompany`/`CreateTicket` against it independently, and confirms
  the result never leaks into the shared context's own projection
  state), and is a real production side effect: `SignUpCompany` emits
  `CompanySignedUp`, `src/bin/provisioner.rs` reacts to it by calling
  `createBoundedContextFromTemplate` for real, and reports the result
  back via `RecordCompanyTenant` so every company's own tenant is durably
  recorded — now also queryable as a routing lookup, via the new
  `TenantDirectory` projection.

  The blocker that used to be listed here is resolved. `CreateTicket` et
  al.'s `company_status` guard is a *same-context* DCB read, so a tenant
  — whose own history never saw the company's signup — used to reject
  every command `company_not_found`. `src/bin/lifecycle-replicator.rs`
  now mirrors each company's lifecycle into its own tenant
  (`RecordTenantLifecycle` -> `CompanyLifecycleMirrored`), which is what
  those guards read; `tests/tenant_lifecycle_mirroring.rs` proves the
  before/after against a real tenant. It does this the way it must: the
  fan-out can't be a `CrossContextRoute` (a route's `Target` bounded
  context is a compile-time const, and the route list is read once at
  startup, so a route can name a context but never the set created at
  runtime), so it runs in application code. Per-tenant `CommandToken`s
  are minted on demand through skilj's own `createCommandToken`, since
  no single token spans every tenant.

  Ticket traffic is now *routed* at tenants, end to end, with reads and
  writes moving together:
  - **Routing decision** (`src/routing.rs`): pure, unit-tested, opt-in
    via `TICKET_ROUTING=tenant`. Ticket traffic follows a company into
    its tenant; lifecycle commands never do; a company with no tenant
    falls back to the shared context.
  - **Access reconciliation** (`src/tenant_access.rs`, run on an
    interval by `server.rs`): projects each company's *company-scoped*
    `RoleAccessMapping`s from the shared context into its tenant, in both
    directions — a grant added later appears, and a grant revoked on the
    authority is revoked in the tenant too, so revoking someone actually
    takes effect. Without this, `submitCommand` at a tenant is refused
    `grant_not_active`, because it authorizes against the caller's
    mapping on the *named* context and never consults the shared one.
    `tests/tenant_access_reconciliation.rs` proves that end to end.
  - **Client-side resolution** (`frontend/src/routing.rs`): the frontend
    resolves `company_id -> tenant` once per session from the shared
    `TenantDirectory` projection, and every ticket call uses it — the four
    writes and both reads. Resolving once is the point: reads and writes
    have to agree, or a dashboard reads empty from one context while its
    tickets went to another. Starts as `Resolving`, and every ticket
    action is gated on it, so a write can't slip out against an unknown
    context.
  - **Server-side enforcement** (`src/routing_guard.rs`, mounted on the
    GraphQL router only when `TICKET_ROUTING=tenant`): the client half is
    not trusted, because `skilj-graphql` takes `boundedContext` as a
    caller-supplied argument. The guard *refuses* ticket traffic naming
    the shared context for a company that has a tenant, rather than
    rewriting it, with a `ticket_routing_error` code and a message that
    says it is a routing error and not an authorization one (the caller
    does hold a valid shared-context mapping). REST needs no equivalent —
    `skilj-rest` derives its destination from the command token, so a REST
    caller cannot name a context at all. `tests/ticket_routing_enforcement.rs`
     proves the refusal, and equally that correctly-routed traffic,
     lifecycle traffic, `TenantDirectory` itself, and a company with no
     tenant all still pass.

   Backend components are now multi-tenant aware end to end — every one of
   them discovers tenants from `CompanyTenantProvisioned` and mints its own
   per-tenant tokens via GraphQL, so it follows a company's traffic into its
   tenant rather than reading the shared context alone:
   - **Alerter** (`src/bin/alerter.rs`): watches each tenant's `UrgentTicketNeedsImmediateAttention`,
     `TicketEscalated`, `TicketResolved`, `TicketReopened`, `TicketClosed`,
     and `TicketsMerged` feeds, and submits `EscalateTicket` to the right
     tenant. `tests/alerter_multi_tenant.rs` proves the full loop against a
     real provisioned tenant.
   - **CSAT metrics loop** (`src/bin/server.rs`): discovers tenants, mints a
     per-tenant `TicketRated` event read token, and polls each tenant's own
     feed — so a `RateTicket` routed to a tenant still records its rating as
     `skilj_helpdesk_ticket_ratings_total`, not just in the shared context.
   - **Demo seed traffic** (`run_demo_seed_loop` in `src/bin/server.rs`): when
     `TICKET_ROUTING=tenant`, resolves each `SeedAction`'s `company_id`
     (`demo_seed::SeedAction::company_id`, backed by `SeedState::
     company_for_ticket`) and mints per-tenant REST `CreateTicket`/etc.
     tokens on demand, so the fake load exercises the same per-tenant path
     real traffic takes — instead of always hitting the shared context.

  Two deliberate properties of the guard, both tested:
  - It **fails open** on a body it cannot parse. A guard that
    intermittently refused valid traffic would be one people disable, and
    authorisation still applies per context either way — evading the
    extractor buys a misrouted write, never an unauthorised one.
  - `TicketInternalNotes` and `TicketSummary` are keyed by `ticket_id`, so
    a request for them names no company. They are refused rather than
    guessed at, which means a company with no tenant can still file
    tickets and read its company-keyed list, but not its ticket-keyed
    notes until it is provisioned. Rewriting instead of refusing would
    need a `ticket_id -> tenant` index that does not exist; the client
    already knows its tenant, so it can name it.

  Also fixed along the way, and worth calling out because it was silent:
  the provisioner derived tenant names as `company-{company_id}`, but a
  bounded context name must match `[a-z][a-z0-9_]{0,39}` and `company_id`
  is caller-supplied and unvalidated. A company id with a dash, an
  uppercase letter, or more than 32 characters produced a name skilj
  rejected — the provisioner logged the failure and moved on, and that
  company silently kept running in the shared context with no isolation
  and no error. `routing::tenant_name_for` now derives a legal, collision-
  resistant name instead; already-provisioned tenants are untouched
  because nothing re-derives a recorded tenant's name.

   Still open (limitation, not in-progress): `staff-lead`'s unrestricted
   (`scope: None`) cross-company visibility only works while everything
   shares one context — after a cutover it needs a mapping per tenant, and
   no single skilj query can span tenants. The reconciler deliberately
   leaves `scope: None` grants alone rather than quietly narrowing a
   cross-company staff grant to one company, so a staff-lead who resolves
   across all companies only sees tickets in the shared context post-cutover
   until an explicit per-tenant mapping is granted.
- **A backend-for-frontend / GraphQL schema beyond what's registered**
  — the frontend talks to skilj-graphql's own auto-generated schema
  directly; there's no hand-written GraphQL layer.

## Real bugs this project found (and fixed)

Verifying everything against a live stack — not just "it compiles" —
surfaced five real bugs, each confirmed failing first, then fixed:

1. **skilj-core**: JWT audience validation was never configured, so any
   spec-compliant OIDC token (which always carries `aud`) was rejected
   outright. Worked around first inside skilj-core itself
   (`validate_aud = false`, matching its own then-stated design), then
   superseded by the real fix in skilj 0.0.9: `IdpConfig::new` takes
   the deployment's own `audience`(s) and a token issued to any *other*
   application at the same IdP is refused instead. Adopted here in
   `src/bin/server.rs` (`DEX_AUDIENCE` for a real Dex,
   `TEST_AUDIENCE` for the local JWKS shortcut) and in the
   self-signed JWTs `src/bin/provisioner.rs` and
   `tests/support/mod.rs` mint.
2. **This project's `server.rs`**: re-seeded the two demo `Role`s on
   every restart, violating a uniqueness constraint on the second run.
   Fixed with a check-before-insert.
3. **This project's `server.rs`**: no CORS headers, so a real browser
   frontend was blocked outright. Fixed where the REST/GraphQL routers
   are merged (a per-deployment decision, correctly made here rather
   than baked into skilj itself).
4. **The frontend**: a double-unwrap bug parsing the projection query
   response — found by actually running it in a headless browser
   against the real stack, not by inspection.
5. **skilj-core/skilj-graphql**: `require_read_mapping` (the only gate on
   GraphQL's `projection`/event/command read surfaces) checked only "does
   the caller hold any active grant on this bounded context" — never
   whether the specific instance queried belonged to that caller. In a
   bounded context shared by several tenants (this project's own
   motivating case), any authenticated customer could read another
   company's tickets, or a staff-only projection like
   `TicketInternalNotes`, for any company. Found by a security review of
   this project, fixed in skilj itself (`RoleAccessMapping.scope` +
   `Projection`/`EventType`/`CommandType.OWNER_TAG_KEY` —
   `docs/architecture.md` §23–26 in the `skilj` repo), adopted here for
   `TicketSummary`/`CompanyTicketList` (since `CompanyActiveTickets`/`CustomerTickets`)/`TicketInternalNotes` and the demo
   customer Role — `tests/cross_company_projection_scoping.rs` proves the
   cross-company half live. The remaining role-type axis — a customer
   reading *their own* company's internal notes — stayed open until
   skilj 0.0.4's `Projection::TEAM_ONLY` (`docs/architecture.md` §31–32
   in the `skilj` repo, Codeberg issue #17) gave it a real gate instead
   of encryption infrastructure this project would otherwise have had to
   provision; adopted here as `TicketInternalNotes`'s own
   `TEAM_ONLY = Some("staff")` and the demo staff Role's `name` (see
   `TicketInternalNotes`'s own doc comment in `src/helpdesk.rs`) —
   `tests/cross_company_projection_scoping.rs` now proves this half live
   too.

## Running on skilj 0.0.9

- **Graceful shutdown.** `server` stops through skilj's own
  `Skilj::shutdown` on Ctrl-C/SIGTERM: every background loop it started
  — including the two deadline reactors the trial-conversion and
  ticket auto-close rules ride on — finishes the tick it is in and
  stops, then the pool closes, and the process prints which loops
  stopped cleanly and which, if any, were aborted at the timeout.
- **Telemetry on the matching OTel line.** The OpenTelemetry stack is
  pinned to what skilj 0.0.9 uses (0.33, with
  `tracing-opentelemetry` 0.34). `opentelemetry`'s globals are per
  crate version, so a provider installed on a different line receives
  none of skilj's spans or metrics — silently, with no error anywhere.
- **A required JWT audience.** `IdpConfig::new` now takes the
  deployment's own `audience`, so a token minted for any *other*
  application at the same IdP is refused. `server` passes Dex's own
  client id (`dex/config.yaml`'s static client) when
  `OIDC_ISSUER_URL` is set, and a fixed test audience for the local
  JWKS shortcut; `provisioner` and the test harness sign matching
  `aud` claims of their own.
- Two upstream correctness fixes land directly on this project's own
  deadline rules: a deadline could previously fire even though its
  cancelling event had already been committed, and a cancel could lose
  to the schedule that had not created the deadline yet
  (`docs/architecture.md` §130–131 in the `skilj` repo) — "a paid order
  still cancelled". Both are fixed upstream.

[`docs/load-test-report-2026-10-01.md`](docs/load-test-report-2026-10-01.md)
is the full 0.0.9 adoption pass and a load-ramp run on this build: no
regression at any step, ~50–63 accepted commands/s sustained (round
four measured ~45–60 on the same ramp), lock-hold time at zero across
the whole ramp, and slow-statement warnings down from 71 to 4. It also
flags the one open item every report since 09-18b has raised and none
has yet investigated: RSS still reaches ~4.6GB under sustained
1600/s offered load.
