# Load test report — 2026-10-01, round five (skilj 0.0.9)

Adoption pass plus a fifth load-ramp test of `server`, run locally against
`skilj` **0.0.9** (tag `v0.0.9`, commit `7e9a7dd` "Prepare 0.0.9
release"), the release that landed the whole `skilj-bridge`/`skilj-retry`
extraction, a required JWT `aud`, several registration/lookup
optimisations, a deadline-retention sweep, and `Skilj::shutdown`. This
report answers three questions: what in 0.0.9 does this crate have to
adopt, what can it use that it isn't yet, and does anything regress
against the `docs/load-test-report-2026-09-19.md` (round four) numbers.

## Summary

- **No performance regression; a small, consistent improvement.**
  Accepted commands/s, same ramp, same box shape as round four:

  | step | offered (nominal) | round four (09-19) | this run (0.0.9) | delta |
  |---|---|---|---|---|
  | 2 (8w @ 500ms) | ~16/s | 12.59/s | **13.39/s** | +6.4% |
  | 3 (20w @ 250ms) | ~80/s | 45.44/s | **50.86/s** | +11.9% |
  | 4 (40w @ 100ms) | ~400/s | 60.42/s | **62.84/s** | +4.0% |
  | 5 (80w @ 50ms) | ~1600/s | 58.05/s | **62.50/s** | +7.7% |

  Every step is at or above round four. Read this as "no regression,
  probably a small gain", not as a clean measurement of 0.0.9's
  throughput work — see "Methodology notes" for the confounders (a
  different box, and a different rate-window convention).
- **Two real breakages had to be fixed, only one of them a compile
  error.** `IdpConfig::new` now takes a required JWT `aud`
  (`docs/architecture.md` §81) — a compile error, fixed here; and
  GraphQL `fetchCommands` now returns `[QueriedCommand!]!`
  (`{id, createdAt, payload}`) instead of a flat `[String!]!` of payload
  strings — **not** a compile error (it deserialises into `serde_json`
  dynamically), so it was caught only by running
  `tests/private_field_team_gating.rs`, which read `c.as_str()`. It
  would have panicked on `.unwrap()` at runtime.
- **One silent, invisible upgrade was required: the OpenTelemetry
  stack 0.32 → 0.33.** `opentelemetry`'s globals are per crate version,
  so leaving this crate on 0.32 compiles fine, runs fine, and silently
  receives none of `skilj`'s spans or metrics — this project's whole
  `observability/` stack would have gone quiet with no error anywhere.
  Bumped and verified with a real (collector-less) run: all three
  pipelines build, install and export, 0 panics.
- **Full regression pass is green.** `skilj-helpdesk`: 64 tests (15 unit
  + 49 DB-backed integration), **0 failures, 0 skips**, every DB test a
  real run (nonzero elapsed per file — the silent-skip trap is called
  out below). Scoped `skilj-core`/`skilj`/`skilj-rest`/`skilj-graphql`/
  `skilj-macros`, CI-style (each binary on its own fresh Postgres):
  **934 passed, 0 failed, 4 ignored** across 91 test binaries.
- **Lock-hold time went to zero.** Max observed `idle in transaction`
  age across all 68 `pg_stat_activity` samples of the ramp: **0.000s**
  (not one sample ever caught a connection in that state), against
  round four's 0.026s / 0.022s / 0.191s. Consistent with 0.0.9's §116/
  §117 work moving lock-holder reads onto the lock's own transaction.
- **Slow-statement warnings collapsed**: 4 in step 5, elapsed
  1.01–1.12s, versus round four's 71 at 1.03–1.66s (and 09-18c's 46 at
  1.0–2.4s). Same statement (`SELECT next_value FROM
  "bc_helpdesk".sequence FOR UPDATE`), same band — genuinely less
  queueing under the same offered load.
- **RSS is lower at every step but the absolute number is still the
  open item.** Step 5 peaked at **4.57GB** (round four: 4.79GB); see
  "RSS growth" — this is the fifth report in a row to raise it and the
  fifth not to root-cause it.
- **Graceful shutdown now actually happens.** `server` calls 0.0.9's
  `Skilj::shutdown` on Ctrl-C/SIGTERM; all four ramp steps and the
  step-1 baseline ended with
  `background loops stopped cleanly: [...10 loops...]; aborted at the
  timeout: []; pool closed: true`.

## What 0.0.9 forced (adopted)

1. **JWT `aud` is now required** — `IdpConfig::new(jwks, issuer,
   audience, algorithm)`. Three places in this crate sign or verify
   their own JWTs and all three now carry and declare an audience:
   - `src/bin/server.rs`: `DEX_AUDIENCE = "skilj-helpdesk-frontend"`,
     matching `dex/config.yaml`'s own static client id (Dex puts that
     id in `aud` for every token it issues for this deployment), and
     `TEST_AUDIENCE = "skilj-helpdesk-test-client"` for the local JWKS
     shortcut's `sign_jwt`.
   - `src/bin/provisioner.rs`: same `TEST_AUDIENCE` on the
     short-lived superadmin JWTs it signs, so they still verify against
     `server`'s local-shortcut `IdpConfig`.
   - `tests/support/mod.rs`: same, for every GraphQL test's JWT.

   This is a real security improvement over what it replaced — round
   four's `IdpConfig` had no audience check at all, so a token minted
   for *any other* application at the same IdP would have been accepted
   here as its user. Note the README's own "Real bugs this project
   found" §1 records the earlier fix in the opposite direction
   (`validate_aud = false`); 0.0.9 supersedes that workaround with the
   real check.
2. **OTel 0.32 → 0.33** (`opentelemetry`, `opentelemetry_sdk`,
   `opentelemetry-otlp`, `opentelemetry-appender-tracing`;
   `tracing-opentelemetry` 0.33 → 0.34), pinned in `Cargo.toml` to
   match `../skilj/Cargo.toml` exactly, as that manifest's own comment
   already insisted. No call-site changes in `src/telemetry.rs`.
3. **GraphQL `fetchCommands` wire shape** —
   `tests/private_field_team_gating.rs` now selects `{ payload }` and
   reads `c["payload"]`, the payload still a JSON string inside the
   new object. The test still proves the same thing (an admin-level
   Role off the staff team cannot read `AddInternalNote`'s `note` /
   `staff_id`; a Role named `staff` can).
4. **Graceful shutdown** — `src/bin/server.rs` now awaits
   `skilj.shutdown(10s)` after `axum::serve(...).with_graceful_shutdown`
   returns, and prints the `ShutdownReport`. It is placed *after* the
   server returns on purpose: the routers share the pool, and §123 is
   explicit that requests after the shutdown fail.

## What 0.0.9 offers that this crate deliberately did not adopt

- **`SkiljBuilder::application_version(u64)`** (§ the
  `registered_by_version` column). Real and cheap, and this project's
  own README argues for two versions running side by side — but that
  only bites when two `server` processes share one database during a
  rolling deploy, which nothing here does. Adopting an untested
  registration-stamping knob into a showcase to cover a deployment
  shape it doesn't have is the wrong trade; it is a one-line change
  when a deployment does.
- **`deadline_retention` / `keep_resolved_deadlines_forever`**
  (§163). The default (30 days) is already correct for this project's
  two deadlines per ticket (trial conversion at 30 days, auto-close at
  7). No override needed; only worth pinning if someone wants resolved
  deadlines kept forever for audit.
- **`idempotency_key_retention` / `keep_idempotency_keys_forever`**
  (§ the idempotency expiry change). The default one hour is well past
  anything that retries here, and this project mints a fresh token set
  per run anyway. Worth knowing: the `deadlines`, `idempotency_keys`
  and parked-delivery tables now get swept, which they never did
  before — that is a fix, not an option to weigh.
- **`max_events_per_read`** (default 1000, now capping REST
  `GET /v1/events`, `/v1/events/consume` and `queryEvents`). Nothing in
  this crate reads history directly: `alerter`, `provisioner` and
  `engagement-watcher` all use `mode=auto`, whose cursor advances
  server-side, so the cap simply means catching up over more poll
  cycles rather than one giant response. No change needed, and a
  smaller peak is a bonus.
- **`skilj_retry::run_with_retry` / `run_until_with_retry`,
  `run_outbound_until` / `run_inbound_until`, `--token-command` for
  `skilj-tui`, `skilj-bridge` itself.** All of these belong to the
  broker bridges, `skilj-tui` and `skilj-temporal`. This project uses
  none of those crates (and `skilj-kafka` still doesn't build in this
  sandbox — see below), so there is nothing here to wire them into.
- **0.0.9's deadline *correctness* fixes need no code here and are
  worth calling out anyway**, because they land directly on this
  project's own deadline rules (`ScheduleCompanyTrialConversion` /
  `ScheduleTicketAutoClose`, `src/helpdesk.rs`): a deadline could
  previously fire even though its cancelling event had already been
  committed (§131), and a cancel could lose to the schedule that had
  not created the deadline yet (§130) — "a paid order still
  cancelled". Both are now fixed upstream, so "convert the trial" and
  "auto-close the ticket" are strictly more correct on this build than
  on any previous one.

## Regression pass

- `cargo build --release --all-targets`: clean, no warnings.
- `cargo test --release` in `skilj-helpdesk` against a **real**
  Postgres 18.6 (`DATABASE_URL` pointed at a real cluster, not
  `postgresql_embedded`): **64 tests, 0 failures, 0 skips, 0 ignored**
  — 15 pure unit tests (the library's own decision logic: `alerting`,
  `demo_seed`, `activity_scheduling`) plus **49 DB-backed integration
  tests across 15 `tests/*.rs` files**. Every DB file's elapsed time is
  nonzero (0.28s–5.33s), which is the check that they really ran.
- `cargo test --release -p skilj-core -p skilj -p skilj-rest
  -p skilj-graphql -p skilj-macros --no-fail-fast`: **934 passed, 0
  failed, 4 ignored** across 91 test binaries.

### The silent-skip trap, and why the shared-`DATABASE_URL` run isn't the number to quote

The first scoped run pointed `DATABASE_URL` at one shared database, the
way a convenient local setup does. It reported **930 passed, 4 failed**.
All four failures were contamination of that one shared database, not
0.0.9 regressions:

| failing test | failure |
|---|---|
| `cross_instance_push_reaches_a_second_instance_sharing_one_database` | timed out waiting for a websocket message |
| `full_admin_console_lifecycle_end_to_end` | "no active superadmin exists yet on a freshly built Skilj" |
| `each_background_loop_records_the_tick_duration_histogram` | no `tick.duration` point for `task=async_projection` |
| `the_bootstrap_secret_ends_at_the_first_claim_and_claims_cannot_race` | `left: 15, right: 0` |

Every one of them asserts something about a *freshly built* `Skilj` (no
superadmin yet, a fresh bootstrap secret, no metrics recorded yet), and
every one of them passed when its own binary was given its own fresh
database — which is what CI does and what the 934/0 run above
reproduces. Two further details worth recording:

- The **first** attempt to verify that in isolation produced four
  *passes* that were not passes at all: with `DATABASE_URL` unset,
  `postgresql_embedded` silently failed to start (this sandbox ships
  libxml2 2.15 / `libxml2.so.16`; the embedded Postgres wants
  `libxml2.so.2`) and each test took the documented "skipping: …"
  early return, finishing in 0.01s. Fixing `LD_LIBRARY_PATH` (a private
  compat dir with a `libxml2.so.2` symlink, the same workaround the
  prior reports' Postgres setup used) turned those skips into real runs.
  A "passing" suite that finished in 0.00s per file is a skipped suite,
  and every prior report in this series checked elapsed time for exactly
  this reason — worth keeping that habit.
- Round four reported 6 `Database(PoolTimedOut)` flakes in this same
  suite. **Zero** occurred in either run this time. The most likely
  explanation is the opposite of "0.0.9 fixed them": round four ran
  with 21 leaked embedded-Postgres process trees of backlog to starve
  the box; this run started with a clean process table. Treat the
  `PoolTimedOut` class as *environment* noise, not a version signal —
  it is not evidence either way about 0.0.9.

## Load-test setup

Same shape as all five prior reports, deliberately, so the numbers are
comparable: `cargo build --release --bin server` against `skilj` 0.0.9;
Postgres 18.6 from `~/.theseus/postgresql/18.6.0` (`initdb`/`pg_ctl`
directly, long-lived cluster, database dropped and recreated between
every step); no Dex (the ramp drives REST `CommandToken`s, which needs
no IdP); `DATABASE_MAX_CONNECTIONS=90` /
`HTTP_MAX_IN_FLIGHT_REQUESTS=70` throughout; `SEED_DEMO_TRAFFIC=1` with
the same 8/20/40/80-worker ramp at 500/250/100/50ms (16/80/400/1600/s
nominal), 4 minutes of load per step; a fresh server process per step
(`SEED_DEMO_CONCURRENCY`/`SEED_DEMO_INTERVAL_MS` are read once at
startup); `pg_stat_activity` connection counts, `idle in transaction`
ages and server RSS sampled every 15s (17 samples per step). OTel and
Grafana skipped again, same reasoning as every prior report — except
that the OTel *wiring* was separately smoke-tested this round, because
this is the round that changed its versions (see below). Phase-timing
logs were collected at `RUST_LOG=info,skilj_core::db=debug`.

### Step 1 (manual baseline)

Five sequential `SignUpCompany` calls against a freshly migrated, idle
server: **4.5–10.0ms** each (`http=200`), against round four's 21–32ms
and earlier rounds' 11–25ms. No regression; the floor got lower.

## Step results

| step | fired | accepted | rejected | 503/transport failures | accepted/s (09-19) | mean batch | mean `decide_us`/command | RSS start → peak |
|---|---|---|---|---|---|---|---|---|
| 2 (8w) | 3825 | 3287 | 538 | 0 | **13.39** (12.59) | 1.1 | 0.85ms | 38MB → 148MB |
| 3 (20w) | 14505 | 12506 | 1999 | 7 | **50.86** (45.44) | 1.9 | 3.48ms | 51MB → 1439MB |
| 4 (40w) | 17874 | 15512 | 2362 | 88 | **62.84** (60.42) | 14.7 | 4.43ms | 257MB → 3531MB |
| 5 (80w) | 17946 | 15483 | 2463 | 43030 | **62.50** (58.05) | 34.5 | 4.71ms | 417MB → 4574MB |

Notes on reading that table:

- **Step 5's 43,030 failures are load-shed 503s, not errors.** They
  begin 4 seconds into the step (offered load is ~40x capacity) and are
  `503 Service Unavailable` from this project's own
  `tower::load_shed`, same mechanism and same order of magnitude as
  round four's 45,278. Only 337 of the 43,030 are transport errors, and
  those are at the shutdown boundary.
- **Steps 3 and 4's failures are shutdown artifacts.** Step 3: 7
  failures inside a 13ms window at `SIGINT`. Step 4: 88 inside a 334ms
  window at `SIGINT`. Round four saw the same shape (97 inside its final
  0.8s) and called it what it is — the client noticing the listener
  close — not a runtime fault. 0 panics, 0 `ERROR`-level lines, 0
  sequence gaps, no dedup false negatives, any step.
- **Mean batch size at step 5 is 34.5**, essentially identical to round
  four's 34.6 and to the `0d9c409` commit message's own claimed ~34.5.
  The self-tuning batcher is still landing on the same batch size at
  saturation; what changed is that each of those batches is cheaper, and
  the ceiling is correspondingly a little higher.
- **`decide_us` per command grows with batch size** (0.85ms at batch 1.1
  → 4.71ms at batch 34.5), the same shape round four reported
  (~2.9ms → ~5.3ms) — bigger batches amortise the lock but cost more
  per row. Absolute per-command cost at the top end is a little lower
  than round four's.
- **`persist_us` now dominates**: 276ms per batch at step 5 against
  162ms of `decide_us`, where round four's totals were dominated by
  `decide`. That is the shape you would expect from 0.0.9's §125
  (multi-event reads resolving command rows in one batch instead of
  several queries each) — the redispatch-check cost that round four
  spent four commits optimising is no longer where the time goes.

## RSS growth (still flagged — fifth round, still uninvestigated)

| step | nominal | RSS start → end (this run) | RSS start → end (round four) |
|---|---|---|---|
| 2 (8w) | 16/s | 38MB → 148MB | 60MB → 254MB |
| 3 (20w) | 80/s | 51MB → 1439MB | 262MB → 1586MB |
| 4 (40w) | 400/s | 257MB → 3531MB | 836MB → 4048MB |
| 5 (80w) | 1600/s | 417MB → 4574MB | 1288MB → 4788MB |

Every step ends *lower* than round four (−4% to −42%) while doing 4–12%
more work, so whatever 0.0.9 changed here, it did not make memory worse.
But the absolute number is unchanged in order of magnitude: **4.57GB**
sustained under 1600/s offered load on a 15GB box. Still no heap
profiler, still no root cause, still a real capacity-planning input
rather than a curiosity. This is now the fifth report to raise it and
the fifth to defer it; the next one should either profile it or say
plainly that this project has decided not to.

## OTel 0.33 smoke test

Because this round changed the telemetry stack, it was verified rather
than assumed: `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:4318` with
**no collector listening**, one real `SignUpCompany` over REST, 12s of
run, then SIGTERM. Result: all three pipelines (trace via
`tracing-opentelemetry`, logs via `opentelemetry-appender-tracing`,
metrics via the `PeriodicReader`) build, install and attempt export; the
only errors logged are `BatchSpanProcessor.ExportError` /
`BatchLogProcessor.ExportError` "HTTP export failed: network error" —
i.e. the exporter is genuinely trying, which is exactly the failure
mode that proves the version bump took. **0 panics**, clean
`ShutdownReport`, `http=200` on the command.

## Methodology notes

1. **Rate windows differ slightly from round four.** Round four divided
   by the wall time from its first to last fired command (260.9s,
   271.8s); this run does the same thing and gets 245.5–247.7s windows
   (it samples RSS every 15s, so the last sample sits up to 15s before
   the step ends). Same ramp, same duration, ~6% different divisor — so
   treat sub-10% deltas as noise and the +12% step-3 result as the only
   one clearly outside it.
2. **Not the same machine.** Round four's absolute numbers came from
   whatever this WSL box looked like on 2026-09-19; this run is
   22 cores / 15GB. There is no way to separate "0.0.9 is faster" from
   "the box is faster" with one run of each. The claim this report
   actually supports is the weak, honest one: **no regression against
   round four on any step**, plus the mechanism-level evidence (lock-hold
   time at zero, slow-statement warnings 71 → 4, `persist_us` overtaking
   `decide_us`) that points the same way.
3. **`skilj-kafka` still doesn't build in this sandbox**
   (`rdkafka-sys`'s vendored `librdkafka` needs `curl/curl.h`).
   Unchanged, unrelated, and `skilj-helpdesk` doesn't depend on it.
4. **Embedded Postgres needs a `libxml2.so.2` compat symlink here**
   (`LD_LIBRARY_PATH=/tmp/pgcompat`, symlink to the installed
   `libxml2.so.16.1.2`). Without it every DB-backed test silently skips
   in 0.01s and reports green — the single most misleading failure mode
   in this sandbox, and the reason this report quotes elapsed times.
5. **No leaked Postgres process trees** were found before or after the
   ramp this round (the 21-process backlog round four had to clean up
   was absent), which is also the most likely reason the
   `PoolTimedOut` class did not recur.
6. **`cargo fmt --check` did not pass on this crate before this
   round, and does now.** rustfmt wanted to reformat ~13 `tests/*.rs`
   files and parts of `server.rs`/`provisioner.rs` — pre-existing, none
   of it on the lines this round touched. Fixed afterwards the
   measurements above were taken, by running `cargo fmt --all` over the
   whole crate and the separate `frontend/` crate (which failed the same
   check in 4 files) and re-verifying: build clean, clippy `-D warnings`
   clean, **the same 64 tests, 0 failures, 0 skips**, and
   `cargo check --target wasm32-unknown-unknown` clean for the frontend
   (no `trunk` in this sandbox, so `trunk build`'s asset-bundling half
   is unverified — a pure-rustfmt diff cannot introduce those failures).
   CI still gates on clippy only; adding a `cargo fmt --check` step to
   `.github/workflows/ci.yml` is the natural follow-up now that the
   tree is clean, so this can't drift back.

## Recommendation

1. **skilj 0.0.9 is safe for this crate, and worth having.** No
   regression at any step of the ramp, a green regression pass on both
   crates (64/0 here, 934/0 in the scoped `skilj` suite), and two real
   correctness fixes landing directly on this project's deadline rules.
2. **Update capacity planning once more, upward and with an asterisk:**
   ~50–63 accepted commands/s sustained (round four said ~45–60), and
   treat the asterisk as "measured on one box against a moving
   baseline", not as a new hard ceiling.
3. **The next lever is still not inside `skilj-core`.** Unchanged from
   round four's own conclusion: `skilj-helpdesk`'s single shared
   `"helpdesk"` bounded context is an explicitly documented placeholder,
   and per-company tenants via `CreateBoundedContextFromTemplate` —
   already proven live by `src/bin/provisioner.rs` and
   `tests/multi_tenant_provisioning.rs` — is what removes the single
   shared sequence every `commit_command_batch` optimisation series has
   been squeezing. 0.0.9 makes that migration safer, not easier: it
   adds `application_version` precisely so two versions can run side by
   side while you do it.
4. **RSS: profile it or consciously decline it.** 4.57GB at 1600/s is
   the same order as the last four reports. Stop deferring.
5. **Commit the updated `Cargo.lock` — and consider gating on
   `cargo fmt` in CI.** CI builds `--locked`, and this round changes the
   lockfile (the OTel 0.32 → 0.33 bump moves five packages); a
   `--locked` build against the old lockfile fails outright, which is
   the correct outcome but an annoying one to discover in CI. Separately:
   the tree failed `cargo fmt --check` before this round (pre-existing,
   ~13 test files plus parts of two binaries, and the same in 4 frontend
   files) and is clean now — a one-line `cargo fmt --all -- --check`
   step in `.github/workflows/ci.yml` would stop it drifting back.
   Note that CI is *already* protected against the misleading-green
   problem this session ran into twice: it runs tests with
   `--nocapture` against a real service-container Postgres and has an
   explicit "Fail if any DB-dependent test silently skipped" step (its
   own comment records proving that step fires with `--nocapture` and
   doesn't without it). The exposure is **local** runs only — which is
   why every prior report in this series quotes per-file elapsed time,
   and why this one does too.

## Raw data

Server logs (`server-step{1..5}.log` plus ANSI-stripped `.clean.log`
copies, which is what the greps in this report run against —
`tracing`'s `fmt` layer colourises field names, so `grep "accepted=true"`
silently matches nothing on the raw log) and RSS/connection/
idle-transaction samples (`step{2..5}-rss.csv`) are in this session's
scratchpad (`/tmp/loadtest-0.0.9` inside the WSL VM) — not committed
here, same reasoning as every prior report.