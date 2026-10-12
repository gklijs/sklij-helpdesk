# Allium spec drift check - 2026-10-12

Issue #36. This compares `specs/skilj-helpdesk.allium`, `specs/activity.allium`, `specs/marketing.allium` and `specs/knowledge_base.allium` with the code, using the `allium:weed` skill, now that the app spans several bounded contexts: the shared `helpdesk` context, the per-company tenants, `activity`, `marketing` and `knowledge_base`.

There were two passes. The first covered the code from before the SLA (#42), macro (#43), auto-assignment (#44) and knowledge-base (#45) work. The second covered those features after they were merged.

The new state-machine and property tests in `tests/spec_state_machine.rs` (propagated with `allium:propagate`) now enforce the result.

## Code bugs (fixed)

### 1. Ticket ids were unique per company, but tickets are identified globally

- **Spec.** A ticket has one identity, and `closed`/`merged` are terminal (`entity Ticket`).
- **Code before the fix.** `CreateTicket` rejected `ticket_already_exists` only when the same company had already used the id. The cause was `CompanyFacts::apply` skipping other companies' `TicketCreated`, and the consistency query being tagged `ticket` and `company`. Everything downstream knows a ticket by its id alone: the `ticket` tag, `TicketSnapshot`, `TicketSummary`, `CustomerTickets` and every later command. So a second company's ticket under the same id landed on the first company's ticket. It reset the ticket to `open`, even out of `closed` or `merged`, which breaks the terminal states. The ticket also kept the first company's `company_id`. The frontend makes ids like `tk-<Date.now()>`, so two companies filing in the same millisecond is enough to trigger this in shared-context mode (the default). It doesn't need a malicious caller.
- **Fix.**
  - `src/helpdesk/company.rs` (`CompanyFacts::apply`): a used ticket id now counts whichever company filed it.
  - `src/helpdesk/ticket_commands.rs` (`CreateTicket::consistency_query`): the `TicketCreated` item is now tagged by `ticket` only, so another company's use of the id is both read and conflicts.
  - `tests/fixture/ticket.rs`: new test `a_ticket_id_is_used_once_across_companies`.
  - `tests/fixture/consistency_query.rs`: the baseline now includes the `ticket` tag.
  - The SLA work (#42) later added a third `CompanySlaPolicySet` item to the same query. The fix is unaffected: that item stays company-scoped.
  - `tests/golden/rest_graphql_conformance.json` was regenerated. The only diff is that the `company` tag is gone from the stored `TicketCreated` consistency-query item of `CreateTicket`, for both tickets in the transcript.
  - The property test `random_command_sequences_follow_the_spec` finds the old behaviour within a few cases. This was checked by temporarily reverting the fix.
- **Impact.** No migration is needed. Histories that already hold a colliding id stay as they are. From now on a colliding `CreateTicket` is rejected, and the frontend shows the rejection.

### 2. A reopened ticket could still breach its resolution SLA

- **Spec.** The resolution SLA "runs to the first resolution only: a reopen doesn't restart it" (the guidance on the SLA breach rules). But `TicketBreachesResolutionSla` required `ticket.resolved_at = null`, and a reopen clears `resolved_at`.
- **Code before the fix.** `BreachTicketSla` checked `first_responded` for the first-response SLA but nothing equivalent for the resolution SLA. Its own doc comment says that `decide()` still checks, "so a deadline that fires anyway ... can't escalate a ticket that met its target". In practice, a reopened ticket that had met its resolution target could still be marked as breached and escalated. That needs either a deadline that slipped past its cancel (a cancel lost to a rolling deploy) or a staff member submitting `BreachTicketSla` by hand.
- **Fix.**
  - `src/helpdesk/sla_policy.rs`: a resolution breach is now rejected `sla_already_met` once `TicketFacts::resolutions > 0`.
  - Spec: new field `Ticket.first_resolved_at`, set by `StaffResolvesTicket` and by a resolving macro and never cleared. The breach rule now requires it to be null.
  - `tests/fixture/sla.rs`: new test `a_reopened_ticket_has_already_met_its_resolution_sla`.
  - `tests/spec_state_machine.rs`: new test `a_reopened_ticket_has_met_its_resolution_sla`. The property test also finds this bug when the fix is reverted.

## Spec bugs (spec updated to match the code)

| # | Construct | Spec said | Code does | Resolution |
|---|---|---|---|---|
| 1 | `Company.tenant`, `rule CompanySignsUp` | The tenant is created inside the signup rule and is always present | `decide()` is pure. `src/bin/provisioner.rs` creates the tenant after `CompanySignedUp` and reports back through `RecordCompanyTenant`, which is rejected `company_not_found` / `tenant_already_provisioned` | `tenant` is now optional. Added `rule CompanyTenantIsRecorded`, plus actor `PlatformReactor` and surface `TenantProvisioning` |
| 2 | Tenant lifecycle mirror | Not modelled | `RecordTenantLifecycle` / `CompanyLifecycleMirrored` copies a company's status into its tenant, only forwards (trialing → expired → active), rejecting `lifecycle_already_mirrored` / `lifecycle_regression`. Inside a tenant, the ticket-creation guard reads this mirror | Added `Company.mirrored_status`, `rule TenantLifecycleIsMirrored` (its three `requires` are checked exhaustively in `the_lifecycle_mirror_only_moves_forward`) and surface `TenantLifecycleReplication`. `Company.status` now uses a named `enum CompanyStatus` |
| 3 | Bounded contexts | The Dependencies section described one tenant per company and nothing more | Lifecycle is decided only in the shared context. Ticket traffic can optionally be routed into tenants (`TICKET_ROUTING=tenant`, `src/routing.rs`, `src/routing_guard.rs`). `activity` and `marketing` read from `helpdesk` | Added a "Bounded contexts" paragraph to the scope header |
| 4 | Manual close | Only `TicketAutoCloses` (temporal) closes a ticket | `CloseTicket` is staff-only and sits behind the dashboard's Close button for a resolved ticket | Added `rule StaffClosesTicket`, and `CloseTicket` in `StaffTicketView` |
| 5 | `Ticket.rating` | `rating: Integer? when status = resolved \| closed`: cleared on reopen, and rating twice while resolved was explicitly allowed | One rating per ticket, ever (`already_rated` survives a reopen). The rating must be 1 to 5 (`invalid_rating`) and come from the ticket's own requester (`requester_mismatch`) | `rating` no longer has a `when` clause. `CustomerRatesTicket` takes the requester and requires `rating = null` and the `config.min_rating..max_rating` range. The surface hides the rating form once rated |
| 6 | `CustomerReplies` | No requester guard | Rejects `requester_mismatch` | Added `requires: requester = ticket.requester` |
| 7 | `Ticket.assigned_to` | Absent once `merged` | A merge doesn't unassign | `merged` added to its `when` set |
| 8 | `CustomerCreatesTicket` | Silent on duplicate ids and unknown companies | `ticket_already_exists` (now global, see the bug above), `company_not_found` | Documented on the rule |
| 9 | `CompanySignsUp` | No input validation | Rejects empty or overlong `company_id`/`name` and implausible emails | Documented on the rule, but left to the code |
| 10 | Stale references | `src/helpdesk.rs` (×4), `scheduler.rs`, "engagement-watcher not yet implemented" | The `src/helpdesk/` module, skilj native deadlines, and a built `src/bin/engagement-watcher.rs` | Paths and resolved notes updated in both specs |
| 11 | `activity`: "gone quiet" | Zero customer activity in the trailing window | The watcher only starts the window at the first customer activity it has seen, so a company with no customer activity at all is never flagged | Documented in the resolved note |
| 12 | `StaffCreatesMacro` (second pass) | `requires: not exists Macro{company, macro_id}`, which a deleted macro no longer blocks | `macro_already_exists` for any macro id ever used, deleted or not (as `Macro.macro_id`'s own comment says) | Documented on the rule |

Second pass, no divergence found:
- **SLA** (#42): creation stamps `target_for(sla_policy_for(company), priority)`; first-response breaches; escalation only if the ticket isn't escalated yet; `CompanySetsSlaPolicy`. The resolution-SLA bug is listed above.
- **Macros** (#43): `StaffAppliesMacro`'s guards and effects. A priority change event is only emitted when the priority actually differs, which has the same effect as the spec.
- **Auto-assignment** (#44): least loaded, with ties going to the first candidate listed. Load means in-progress or waiting tickets.
- **Knowledge base** (#45): every rule and both state machines match. Articles being shared across companies is the spec's own open question, not drift.

## Intentional gaps (documented, both sides left alone)

- **`TrialPeriodEnds` charge timing.** The mocked charge is decided at signup, by scheduling either `ConvertCompanyTrial` or `ExpireCompanyTrial`, not when the trial ends. The result is the same while the mock is deterministic. This is now noted as `@guidance` on the rule.
- **`CompanyResubscription` surface.** `ReactivateCompany` can only be reached over REST with a command token. The frontend has no resubscribe button yet. Noted on `CompanySubscribes`.
- **`StaffLogsTicketOnBehalf`.** `CreateTicket` is not role-gated, so `logged_by_staff_id` is whatever the caller sends and is never checked to be a staff member. The frontend never sets it. This is noted on the rule. It is worth an issue of its own if `logged_by` ever matters for authorisation.
- **`Ticket.resolved_at`.** The code has no such field. The auto-close deadline stands in for it, and it is cancelled on reopen by `ticket_resolution` tag. This is an abstraction, not a divergence.
- **`allium analyse` conflict.** `TicketReopened` and `TicketAutoCloses` can both apply while a ticket is resolved. This was already resolved in the spec's `@guidance`: reopen wins, the deadline is cancelled, and a stale deadline is rejected.

## Code comments corrected along the way

- `src/alerting.rs` said `TicketBecomesOverdue` was "not wired up". `src/bin/alerter.rs` does sweep for it and submits `EscalateTicket`.
- `src/marketing.rs` pointed at `specs/activity-marketing-event-model.md`. The file is in `docs/`.

Left for the owners of `src/helpdesk/mod.rs`, since other work is touching it: its module doc still says "no Ticket command or query is actually *routed* at a tenant yet". That is out of date since `src/routing.rs` / `src/routing_guard.rs` and `TICKET_ROUTING=tenant`.

## Generated tests: `tests/spec_state_machine.rs`

- **Transition graphs.** Every ticket and company status is reached through the real commands, and every command is tried in every status. Each accepted change is a declared edge, and each declared edge has a witnessing rule. Terminal states (`closed`, `merged`, company `active`) have no exit, and every non-terminal state has one. There are also reachability walks to both ticket terminal states.
- **Reference model, checked with proptest.** The spec's `requires`/`ensures` are transcribed into `Model`, including SLA policies and breaches, macros (create, update, delete, apply), auto-assignment and priority changes. 1024 random sequences of up to 80 commands, over 2 companies, 2 ticket ids, 2 requesters, 2 macro ids and 3 agents, run against the model and the real `decide()`/`decide_with()`. Ticket commands get their `TicketSnapshot` state (and `AutoAssignTicket` each candidate's `StaffWorkload`) as JSON through `Decision`, as skilj does. After every step the test checks that:
  - the model and the code agree on accept or reject and on the resulting state: status, priority, assignee, first response, and breaches;
  - the graph and terminal invariants hold;
  - each ticket is created once, escalated at most once and rated at most once;
  - each SLA kind is breached at most once per ticket;
  - merges stay within one company;
  - an escalation bumps the current priority by exactly one tier, and a breach only escalates a ticket that isn't escalated yet;
  - a macro emits a priority-change event only when the priority actually changes;
  - auto-assignment picks the least-loaded candidate, and every agent's `StaffWorkload` equals their in-progress or waiting tickets;
  - a new ticket's SLA comes from its company's latest policy;
  - an urgent ticket pages a lead at creation.
- **Macros and auto-assignment in the graph walks.** Both are among the commands tried in every status, and both appear as witnesses for their edges.
- **Tenant rules.** `CompanyTenantIsRecorded` is checked to apply once. `TenantLifecycleIsMirrored`'s guards are checked against the code exhaustively.
- **activity.** A property checks that activity is recorded once per company, person and day, and that an engagement decline is recorded once per company.
- **marketing.** The three `CrossContextRoute`s carry the company over, and `flagged_at` is passed through unchanged. `RecordTrialLapse`, `RecordTrialConversion` and `RecordEngagementDecline` accept repeats.
- **knowledge_base.** The resolution-to-suggestion route is tested. A model property covers the rest: one suggestion per ticket; drafting or dismissing only while pending; an article id used once; titles must not be blank; publishing only a draft or an edit; each publication is one revision on.
- **Config defaults.** `trial_duration` is 30 days and `auto_close_after` is 7 days. The 4-hour overdue boundary is inclusive.

Obligations from `allium plan` that this file doesn't cover:

- **Surface exposure and actor obligations** need GraphQL/REST and access control. They are already covered by the Postgres suites (`tests/graphql.rs`, `tests/private_field_team_gating.rs`, `tests/company_signals.rs`).
- **Temporal firing** (`temporal.*`) needs a controllable clock. skilj's deadlines stamp `fire_at` with `Utc::now()`, and the fire loop is exercised by `tests/native_deadlines.rs` and `tests/ticket_auto_close_cancel.rs`.
- **`config.unhandled_alert_after` / `quiet_after` defaults** live in the `alerter` and `engagement-watcher` binaries, not in the library.
