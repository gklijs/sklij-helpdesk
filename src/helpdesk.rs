//! The "helpdesk" bounded context: company signup and the core ticket
//! lifecycle. Implements a slice of `specs/skilj-helpdesk.allium` - see
//! that file for the full domain spec, and this crate's `Cargo.toml`
//! doc comment for exactly what this pass covers vs. defers.
//!
//! One bounded context still runs the real decide() logic for both
//! Company and Ticket - not the two the spec's Dependencies section
//! implies (a shared "billing" context for Company, a per-company
//! tenant "helpdesk" context for Ticket). What's no longer deferred:
//! `SignUpCompany` now has a real per-company tenant provisioned for it
//! (`RecordCompanyTenant`/`CompanyTenantProvisioned` below,
//! `src/bin/provisioner.rs` the reactor that calls skilj's own
//! `CreateBoundedContextFromTemplate` and reports back), proven first as
//! a standalone mechanism in `tests/multi_tenant_provisioning.rs`, now a
//! real production side effect of every signup.
//!
//! The cross-context guard read that used to block routing Ticket
//! traffic into those tenants is now solved too, and how is worth
//! recording because the obvious answers don't work: `company_status`
//! below is a *same-context* DCB query, so a tenant cannot simply ask
//! the shared context what its company's status is, and skilj's own
//! `CrossContextRoute` cannot fan one event out to a runtime-created set
//! of contexts (a route's `Target` bounded context is a compile-time
//! const, and the route list is read once at startup - see
//! `CompanyLifecycleMirrored`'s own doc comment for the full
//! reasoning). So the shared context stays the single authority for
//! lifecycle, and `src/bin/lifecycle-replicator.rs` *mirrors* each
//! company's status into its own tenant
//! (`RecordTenantLifecycle`/`CompanyLifecycleMirrored`), which is what
//! those guards then read. `company_status` folds both sources in one
//! pass, so a mixed history resolves by recency with no special-casing.
//!
//! Still deferred: no Ticket command or query is actually *routed* at a
//! tenant yet - every company's tickets still run in this shared
//! context, so none of the above is load-bearing yet. Cutting over is
//! the remaining step, and the open questions that come with it (per-
//! tenant vs. segment sharding, `staff-lead`'s cross-company access
//! after the split) are called out in this crate's `README.md`.
//!
//! Every id (`company_id`, `ticket_id`) is caller-supplied, same
//! convention as `skilj-demo`'s own `account_id`/`course_id` - never
//! generated inside `decide()`, which stays pure and I/O-free by
//! contract (`CommandType::decide`'s own doc comment).
//!
//! Four flows beyond the original spec (`TicketEscalated`/`EscalateTicket`,
//! `TicketsMerged`/`MergeTickets`, `TicketRated`/`RateTicket`,
//! `TicketInternalNoteAdded`/`AddInternalNote` - each type's own doc
//! comment explains its own reasoning) - added to give the telemetry/
//! dashboard work (see `src/telemetry.rs`, `observability/`) genuinely
//! varied traffic to show, grounded in how real helpdesk tools work
//! (SLA-breach escalation, ticket merging, CSAT, internal notes), not
//! invented for their own sake. `TicketInternalNoteAdded` is the one
//! deliberately kept out of `CompanyActiveTickets`/`CustomerTickets`/
//! `TicketSummary` below - both of the frontend's eager reads are visible
//! to customers, so an internal note in either, relying on the frontend
//! to filter it back out, would reach them with no server-side
//! enforcement behind it.
//!
//! Customer data is GDPR-erasable: everything a customer wrote, and
//! their contact details, is a skilj `sensitive_field` encrypted under
//! their own key - see `CUSTOMER_SUBJECT` and `CustomerTickets`.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skilj::{
    auto_register, requires_role, CancelDeadline, CommandType, EventType, Projection,
    ScheduleDeadline, Snapshot,
};
use skilj_core::event_store::Event;
use skilj_core::plugin::{BoundedContextEvent, DeadlinePollStartFrom, DeadlineSpec};
use skilj_core::shared::{
    CommandDecision, EventSpec, PrivateField, PrivateFieldKind, SensitiveField, Tag, TagMapping,
};

use crate::scheduling;

pub const BOUNDED_CONTEXT: &str = "helpdesk";

/// The one team name `TicketInternalNotes`'s own `TEAM_ONLY` and
/// `AddInternalNote`/`TicketInternalNoteAdded`'s own `private_fields()`
/// all compare `Role.name` against (see `TicketInternalNotes`'s own doc
/// comment), as does `#[requires_role("staff")]` on the staff-only
/// command types (that attribute only accepts a literal, so
/// `staff_only_commands_require_the_staff_role` below pins it to this
/// constant instead) - a shared constant rather than five independent string
/// literals (this file, `server.rs`, both test files) so a future edit
/// to one can't silently desync from the others and reopen exactly one
/// of the two gates while the other still looks closed.
pub const STAFF_TEAM: &str = "staff";

/// `RecordTenantLifecycle::NAME`, as a plain `const` rather than reached
/// for through the trait.
///
/// Only `src/bin/lifecycle-replicator.rs`'s own GraphQL token mint needs
/// the name as a *string* (a `CommandToken` is minted by type name, not
/// by Rust type), and it reaches this over a process boundary where
/// `<RecordTenantLifecycle as CommandType>::NAME` can't be written
/// without importing the trait there. One place the literal lives, so
/// the mint can't drift from the registration the way a second inline
/// `"RecordTenantLifecycle"` in that binary could - the same reasoning
/// `STAFF_TEAM` above exists for.
pub const RECORD_TENANT_LIFECYCLE_COMMAND: &str = "RecordTenantLifecycle";

/// The `subject_key` a customer's personal data is encrypted under, with
/// `requester_id` as the subject value - what an admin passes to skilj's
/// own `forgetSubject(boundedContext: "helpdesk", subjectKey: "customer",
/// subjectValue: <requester_id>)` to erase one customer (GDPR art. 17).
/// That destroys the customer's key, so every field below that was
/// written under it reads back as ciphertext from then on, and resolves
/// any pending deadline naming them as `forgotten` (see `CloseTicket`).
pub const CUSTOMER_SUBJECT: &str = "customer";

/// `fields` of a payload, each encrypted under the customer named by the
/// payload's own `requester_id`. That field itself stays plaintext - it's
/// the subject the key is found by.
///
/// Readable afterwards only to a caller with `can_read_sensitive` (staff)
/// or whose IdP subject is that `requester_id` (the customer). Projections
/// fold the ciphertext as is, and skilj decrypts a projection row only
/// against its own key, so every one of these fields lives in
/// `CustomerTickets`, keyed by `requester_id`, and none in the
/// company-keyed `CompanyActiveTickets`.
fn customer_fields(fields: &[&str]) -> Vec<SensitiveField> {
    fields
        .iter()
        .map(|field| SensitiveField {
            field: (*field).into(),
            subject_key: CUSTOMER_SUBJECT.into(),
            subject_field: "requester_id".into(),
        })
        .collect()
}

/// Everything a ticket's creation says about its customer: what they wrote
/// and how to reach them.
fn ticket_created_customer_fields() -> Vec<SensitiveField> {
    customer_fields(&["title", "description", "requester_name", "requester_email"])
}

fn company_tag() -> Vec<TagMapping> {
    vec![TagMapping {
        key: "company".into(),
        field: "company_id".into(),
    }]
}

fn ticket_tag() -> Vec<TagMapping> {
    vec![TagMapping {
        key: "ticket".into(),
        field: "ticket_id".into(),
    }]
}

/// `company_tag()`'s own counterpart for `ScheduleDeadline`/
/// `CancelDeadline` below, which tag a `DeadlineSpec` with real `Tag`
/// values (a key *and* the entity's own id) rather than a `TagMapping`
/// (a key and which payload field to read it from) - the same
/// consistency-boundary tag, computed the other direction.
fn company_tag_value(company_id: &str) -> Tag {
    Tag {
        key: "company".into(),
        value: Some(company_id.to_string()),
    }
}

/// `ticket_tag()`'s own counterpart - see `company_tag_value`'s doc
/// comment.
fn ticket_tag_value(ticket_id: &str) -> Tag {
    Tag {
        key: "ticket".into(),
        value: Some(ticket_id.to_string()),
    }
}

/// One resolution of one ticket, as a deadline tag - see
/// `CancelTicketAutoCloseOnReopen` for why the auto-close deadline needs
/// a tag narrower than `ticket_tag_value`. Only ever a deadline tag, never
/// an event tag: no `TagMapping` produces it.
fn ticket_resolution_tag_value(ticket_id: &str, resolution: u32) -> Tag {
    Tag {
        key: "ticket_resolution".into(),
        value: Some(format!("{ticket_id}#{resolution}")),
    }
}

// --- events ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanySignedUpPayload {
    pub company_id: String,
    pub name: String,
    pub contact_email: String,
}

pub struct CompanySignedUp;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanySignedUp {
    type Payload = CompanySignedUpPayload;
    const NAME: &'static str = "CompanySignedUp";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    /// `src/bin/provisioner.rs` reads this via an `EventReadToken` to
    /// provision each new company's own tenant - see
    /// `TicketCreated::event_read_allowed`'s own doc comment for why
    /// this override exists at all.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanyTenantProvisionedPayload {
    pub company_id: String,
    pub tenant_name: String,
}

pub struct CompanyTenantProvisioned;

/// `RecordCompanyTenant`'s own outcome - see that command's doc comment.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanyTenantProvisioned {
    type Payload = CompanyTenantProvisionedPayload;
    const NAME: &'static str = "CompanyTenantProvisioned";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    /// Phase 4 multi-tenant alerter discovers tenants by reading this
    /// event off the shared context's own REST feed - the alerter has no
    /// other way to enumerate which companies have been provisioned a
    /// tenant at runtime, since `TenantDirectory` is keyed by company_id
    /// and `CrossContextRoute` can't fan out to a runtime-created set of
    /// contexts. The event carries nothing `company_id`-scoped beyond what
    /// skilj's own `event_read_allowed` gate exists to control: it carries
    /// a `tenant_name`, not a `ticket_id` or any per-company data.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanyActivatedPayload {
    pub company_id: String,
}

pub struct CompanyActivated;

/// One event for both `specs/skilj-helpdesk.allium`'s `rule
/// TrialPeriodEnds`'s success branch (`trialing -> active`) and `rule
/// CompanySubscribes`'s success branch (`expired -> active`) - the spec
/// keeps them as two rules because they're two different triggers (a
/// trial deadline firing vs. a company choosing to pay), but the
/// resulting domain fact is identical ("this company is now active"), so
/// one event type covers both here, the same simplification
/// `CreateTicket` already makes for its own two triggering rules.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanyActivated {
    type Payload = CompanyActivatedPayload;
    const NAME: &'static str = "CompanyActivated";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanyExpiredPayload {
    pub company_id: String,
}

pub struct CompanyExpired;

/// `specs/skilj-helpdesk.allium`'s `rule TrialPeriodEnds`'s failure
/// branch: `trialing -> expired`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanyExpired {
    type Payload = CompanyExpiredPayload;
    const NAME: &'static str = "CompanyExpired";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    /// `src/bin/lifecycle-replicator.rs` reads this to mirror a
    /// company's `trialing -> expired` transition into its own tenant -
    /// see `CompanySignedUp::event_read_allowed`'s own doc comment for
    /// why the override exists at all.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanyLifecycleMirroredPayload {
    pub company_id: String,
    /// `CompanyStatus` needs the serde/schemars derives `TicketPriority`
    /// above already carries (and `rename_all = "snake_case"`, so the
    /// wire values are `"trialing"`/`"active"`/`"expired"` rather than
    /// Rust's default PascalCase) - this payload is what makes it a
    /// value that can cross a process boundary at all. Before this pass
    /// `CompanyStatus` was a pure in-process `decide()` fold result and
    /// needed no derives; that stopped being true the moment a tenant's
    /// own history has to record one.
    pub status: CompanyStatus,
    /// The name of the shared-context event this fact was derived from
    /// (`"CompanySignedUp"`/`"CompanyActivated"`/`"CompanyExpired"`),
    /// recorded rather than inferred so an operator reading a tenant's
    /// own event log can tell a replayed lifecycle fact from a locally
    /// originated one. Never a routing input - `company_status` below
    /// folds `status` only, so this field is provenance, not behaviour.
    pub source_event_type: String,
}

pub struct CompanyLifecycleMirrored;

/// A company's lifecycle state, **mirrored** into its own tenant - not
/// a lifecycle transition that happened there.
///
/// **Why this event exists at all.** `CreateTicket` and every other
/// Ticket command opens with a `company_status` guard read, and that
/// read is one *same-context* DCB query: skilj hands `decide()` only
/// the events carrying the command's own tags, read from that bounded
/// context's own `bc_<name>` schema. A tenant's own history therefore
/// contains nothing about its company's lifecycle, because signup
/// happens in the shared `helpdesk` context, so a `CreateTicket` routed
/// at a tenant would be rejected `company_not_found` for a company that
/// demonstrably exists.
///
/// **The alternative, and why it isn't this.** skilj's own
/// `CrossContextRoute` cannot express "fan this one event out to every
/// tenant": `Target: CommandType` is a single type whose
/// `BOUNDED_CONTEXT` is a compile-time const
/// (`skilj_core::plugin::CrossContextRoute`'s own doc comment), and the
/// background route loop reads its route list *once at startup*
/// (`skilj::Skilj`'s `cross_context_routes` loop), with no runtime
/// registration surface for a newly-created tenant. Deadlines have the
/// dynamic fan-out `routes` lack (`deadline_fire_tick` re-lists bounded
/// contexts every tick), but no `ScheduleDeadline` fires off another
/// bounded context's events either - both source types are static consts
/// too.
///
/// So the fan-out has to happen in application code, which is what
/// `src/bin/lifecycle-replicator.rs` is: it reads lifecycle events off
/// the REST event feed and submits `RecordTenantLifecycle` below into
/// the right tenant, following the same "I/O happens outside `decide()`
/// in its own binary reacting to the event feed" split
/// `provisioner.rs`'s own module doc comment describes for tenant
/// creation. This event is what such a submission commits.
///
/// **Why it is safe to trust.** It is not a general "write anything you
/// like into a tenant" door: `RecordTenantLifecycle::decide` below
/// accepts only the three real lifecycle states, and (more importantly)
/// the *shared* `helpdesk` context stays the single authority for
/// lifecycle transitions - nothing in a tenant can move a company
/// between states on its own, so a compromised or buggy tenant client
/// can at worst make its own (already-authoritative-shared) view stale,
/// which `RecordTenantLifecycle`'s own guard below bounds to "never
/// regress to an earlier state".
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanyLifecycleMirrored {
    type Payload = CompanyLifecycleMirroredPayload;
    const NAME: &'static str = "CompanyLifecycleMirrored";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketPriority {
    Low,
    Medium,
    High,
    Urgent,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketCreatedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub requester_id: String,
    pub logged_by_staff_id: Option<String>,
    pub title: String,
    pub description: String,
    pub priority: TicketPriority,
    /// Encrypted under the requester's key, like `title`/`description` -
    /// see `customer_fields`.
    pub requester_name: Option<String>,
    /// Encrypted under the requester's key - see `customer_fields`.
    pub requester_email: Option<String>,
}

pub struct TicketCreated;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketCreated {
    type Payload = TicketCreatedPayload;
    const NAME: &'static str = "TicketCreated";
    /// Tagged on both, same technique as `skilj-demo`'s own
    /// `StudentEnrolled` (`courses.rs`): `AssignTicket`/`ResolveTicket`/
    /// `ReopenTicket` only ever need the "ticket" tag, but `CreateTicket`
    /// itself needs to see this company's own signup history too, so
    /// the creating event carries both tags up front.
    fn tag_mappings() -> Vec<TagMapping> {
        vec![
            TagMapping {
                key: "ticket".into(),
                field: "ticket_id".into(),
            },
            TagMapping {
                key: "company".into(),
                field: "company_id".into(),
            },
        ]
    }
    /// `src/bin/alerter.rs` reads this via an `EventReadToken` to track
    /// unhandled tickets - `EventType`'s own default (`false`) would
    /// 403 that read (docs/architecture.md §7.5): a type must opt in
    /// explicitly. Found the hard way, over a real REST request, once a
    /// real embedded Postgres was available in this sandbox to run
    /// against - see `tests/alerting_feed.rs`.
    fn event_read_allowed() -> bool {
        true
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        ticket_created_customer_fields()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketAssignedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub staff_id: String,
}

pub struct TicketAssigned;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketAssigned {
    type Payload = TicketAssignedPayload;
    const NAME: &'static str = "TicketAssigned";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketResolvedPayload {
    pub ticket_id: String,
    pub company_id: String,
    /// Which resolution of this ticket this is: 1 for the first, 2 after
    /// one reopen, and so on (see `TicketFacts::resolutions`). It tags the
    /// auto-close deadline this schedules, so `CancelTicketAutoCloseOnReopen`
    /// can cancel exactly this resolution's deadline and no later one.
    /// `None` on events stored before the field existed.
    pub resolution: Option<u32>,
    /// The ticket's requester, copied from its `TicketCreated` so the
    /// auto-close deadline this schedules can name them (see
    /// `CloseTicketPayload::requester_id`). `None` on events stored
    /// before the field existed.
    pub requester_id: Option<String>,
}

pub struct TicketResolved;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketResolved {
    type Payload = TicketResolvedPayload;
    const NAME: &'static str = "TicketResolved";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    /// `src/bin/alerter.rs` reads this too - see
    /// `TicketCreated::event_read_allowed`'s own doc comment.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketReopenedPayload {
    pub ticket_id: String,
    pub company_id: String,
    /// The `TicketResolved::resolution` this reopen undoes - what
    /// `CancelTicketAutoCloseOnReopen` cancels by. `None` on events
    /// stored before the field existed.
    pub resolution: Option<u32>,
}

pub struct TicketReopened;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketReopened {
    type Payload = TicketReopenedPayload;
    const NAME: &'static str = "TicketReopened";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    /// `src/bin/alerter.rs` reads this too - see
    /// `TicketCreated::event_read_allowed`'s own doc comment.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketInfoRequestedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub staff_id: String,
    /// Encrypted under the requester's key: a question to the customer
    /// is part of their ticket's conversation.
    pub message: String,
    /// The ticket's requester, whose key `message` is encrypted under and
    /// whose `CustomerTickets` row it lands in. `None` on events stored
    /// before the field existed, whose message stays plaintext and is in
    /// no `CustomerTickets` row.
    pub requester_id: Option<String>,
}

pub struct TicketInfoRequested;

/// `specs/skilj-helpdesk.allium`'s `rule StaffRequestsInfo`'s own
/// outcome: `in_progress -> waiting_on_customer`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketInfoRequested {
    type Payload = TicketInfoRequestedPayload;
    const NAME: &'static str = "TicketInfoRequested";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["message"])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketCustomerRespondedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub requester_id: String,
    pub message: String,
}

pub struct TicketCustomerResponded;

/// `specs/skilj-helpdesk.allium`'s `rule CustomerReplies`'s own outcome:
/// `waiting_on_customer -> in_progress`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketCustomerResponded {
    type Payload = TicketCustomerRespondedPayload;
    const NAME: &'static str = "TicketCustomerResponded";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["message"])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketClosedPayload {
    pub ticket_id: String,
    pub company_id: String,
}

pub struct TicketClosed;

/// `specs/skilj-helpdesk.allium`'s `rule TicketAutoCloses`: `resolved ->
/// closed`. "Auto" in the spec's own name refers to *who* decides
/// (nobody - a sweep, not a person), not to *how* the resulting state
/// change reaches skilj: `CloseTicket` below is an ordinary command, the
/// same as every other mutation in this file, submitted by skilj's own
/// native per-entity deadline mechanism (`ScheduleTicketAutoClose`
/// below, docs/architecture.md §46) rather than a customer or staff
/// member. See that reactor's own doc comment for why.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketClosed {
    type Payload = TicketClosedPayload;
    const NAME: &'static str = "TicketClosed";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    /// `src/bin/alerter.rs` reads this too - see
    /// `TicketCreated::event_read_allowed`'s own doc comment.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketEscalatedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub previous_priority: TicketPriority,
    pub new_priority: TicketPriority,
}

pub struct TicketEscalated;

/// A deliberate, documented extension of `specs/skilj-helpdesk.allium`'s
/// `rule TicketBecomesOverdue` - see that rule's own updated `@guidance`
/// note for why this pass turns "page a lead" into a real persisted
/// priority bump, not just a console alert. Submitted by
/// `src/bin/alerter.rs`'s own overdue sweep, the same "a background
/// binary submits an ordinary command" treatment `TicketClosed` gets
/// from `ScheduleTicketAutoClose` (below) and `CompanyActivated` gets
/// from `ScheduleCompanyTrialConversion`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketEscalated {
    type Payload = TicketEscalatedPayload;
    const NAME: &'static str = "TicketEscalated";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    /// `src/bin/alerter.rs` reads this itself (own output, consumed back)
    /// to stop re-submitting `EscalateTicket` for a ticket it (or another
    /// alerter instance) already escalated - see
    /// `TicketCreated::event_read_allowed`'s own doc comment for the
    /// general pattern.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketsMergedPayload {
    pub primary_ticket_id: String,
    pub duplicate_ticket_id: String,
    pub company_id: String,
}

pub struct TicketsMerged;

/// A showcase of skilj's own DCB model, not in the original spec: two
/// tickets, one event, no aggregate boundary needed - see
/// `MergeTickets::tag_mappings` below for the command side of the same
/// trick. Tagged on *both* ticket ids (two `TagMapping` entries under
/// the same `"ticket"` key), so any later command against either ticket
/// sees this in its own `matching_events`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketsMerged {
    type Payload = TicketsMergedPayload;
    const NAME: &'static str = "TicketsMerged";
    fn tag_mappings() -> Vec<TagMapping> {
        vec![
            TagMapping {
                key: "ticket".into(),
                field: "primary_ticket_id".into(),
            },
            TagMapping {
                key: "ticket".into(),
                field: "duplicate_ticket_id".into(),
            },
        ]
    }
    /// `src/bin/alerter.rs` reads this to stop tracking the duplicate
    /// ticket as unhandled once merged away - see
    /// `TicketCreated::event_read_allowed`'s own doc comment.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketRatedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub rating: u8,
    /// Encrypted under the requester's key.
    pub comment: Option<String>,
    /// The customer who rated, whose key `comment` is encrypted under -
    /// see `TicketInfoRequestedPayload::requester_id`.
    pub requester_id: Option<String>,
}

pub struct TicketRated;

/// Not in the original spec - a CSAT survey response, standard practice
/// once a ticket is resolved (Zendesk, Freshdesk).
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketRated {
    type Payload = TicketRatedPayload;
    const NAME: &'static str = "TicketRated";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["comment"])
    }
    /// `src/csat_metrics.rs` reads this via an `EventReadToken` to
    /// record the rating *value* as a real metric - see
    /// `TicketCreated::event_read_allowed`'s own doc comment for why
    /// this default needs an explicit override. Everything else about
    /// a rating (who gave it, the comment) still only ever goes through
    /// GraphQL/`get_projection_state`, same as before this existed.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketInternalNoteAddedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub staff_id: String,
    pub note: String,
}

pub struct TicketInternalNoteAdded;

/// Not in the original spec - a staff-only note. Deliberately never
/// folded into `CompanyActiveTickets`/`CustomerTickets`/`TicketSummary`
/// (see this file's own module doc comment on why keeping it structurally
/// separate, rather than tagging entries "internal" for the frontend to
/// filter, is what keeps it from customers).
///
/// `private_fields()` below closes the same admin-tooling surface
/// `AddInternalNote`'s own doc comment describes for the command side -
/// skilj's generic `queryEvents`/`countEvents`/`inspectEvent`
/// (`AdminAccess`-gated, `skilj-core::event_store::query_events`/
/// `inspect_event`) check only `access_mapping.level == Admin`, never
/// `event_read_allowed` (that flag only gates the *REST* feed's
/// `EventReadToken` path - `fetch_events`/`consume_events` - a
/// different function entirely). So even though this event type has no
/// `event_read_allowed() = true` override and is therefore unreachable
/// over REST, it was still fully readable in cleartext through the
/// GraphQL admin surface without this declaration - found in review,
/// not assumed from the REST-side block alone.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketInternalNoteAdded {
    type Payload = TicketInternalNoteAddedPayload;
    const NAME: &'static str = "TicketInternalNoteAdded";
    /// Tagged on both, same reasoning as `TicketCreated`'s own doc
    /// comment: `TicketInternalNotes`'s own `OWNER_TAG_KEY` (see that
    /// projection's own doc comment) needs a "company" tag on *some*
    /// consuming event to derive an owner from, and this is the only
    /// one it consumes at all - `TicketCreated`'s own "company" tag
    /// alone isn't enough here, since `TicketSummary`/`CompanyActiveTickets`
    /// consume `TicketCreated` but `TicketInternalNotes` deliberately
    /// doesn't (see this file's own module doc comment on why).
    fn tag_mappings() -> Vec<TagMapping> {
        vec![
            TagMapping {
                key: "ticket".into(),
                field: "ticket_id".into(),
            },
            TagMapping {
                key: "company".into(),
                field: "company_id".into(),
            },
        ]
    }
    fn private_fields() -> Vec<PrivateField> {
        vec![
            PrivateField {
                field: "staff_id".into(),
                kind: PrivateFieldKind::Team,
                team: Some(STAFF_TEAM.into()),
                addressee_field: None,
            },
            PrivateField {
                field: "note".into(),
                kind: PrivateFieldKind::Team,
                team: Some(STAFF_TEAM.into()),
                addressee_field: None,
            },
        ]
    }
}

/// This bounded context's own hand-written event enum - docs/
/// architecture.md §1.4/§1.6, same technique as skilj-demo's
/// `BankingEvent`/`CoursesEvent`.
pub enum HelpdeskEvent {
    CompanySignedUp(CompanySignedUpPayload),
    CompanyActivated(CompanyActivatedPayload),
    CompanyExpired(CompanyExpiredPayload),
    CompanyTenantProvisioned(CompanyTenantProvisionedPayload),
    CompanyLifecycleMirrored(CompanyLifecycleMirroredPayload),
    TicketCreated(TicketCreatedPayload),
    TicketAssigned(TicketAssignedPayload),
    TicketResolved(TicketResolvedPayload),
    TicketReopened(TicketReopenedPayload),
    TicketInfoRequested(TicketInfoRequestedPayload),
    TicketCustomerResponded(TicketCustomerRespondedPayload),
    TicketClosed(TicketClosedPayload),
    TicketEscalated(TicketEscalatedPayload),
    TicketsMerged(TicketsMergedPayload),
    TicketRated(TicketRatedPayload),
    TicketInternalNoteAdded(TicketInternalNoteAddedPayload),
}

impl BoundedContextEvent for HelpdeskEvent {
    fn try_from_event(event: &Event) -> Option<Result<Self, serde_json::Error>> {
        match event.event_type.name.as_str() {
            "CompanySignedUp" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::CompanySignedUp))
            }
            "CompanyActivated" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::CompanyActivated))
            }
            "CompanyExpired" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::CompanyExpired))
            }
            "CompanyTenantProvisioned" => Some(
                serde_json::from_str(&event.payload).map(HelpdeskEvent::CompanyTenantProvisioned),
            ),
            "CompanyLifecycleMirrored" => Some(
                serde_json::from_str(&event.payload).map(HelpdeskEvent::CompanyLifecycleMirrored),
            ),
            "TicketCreated" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketCreated))
            }
            "TicketAssigned" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketAssigned))
            }
            "TicketResolved" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketResolved))
            }
            "TicketReopened" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketReopened))
            }
            "TicketInfoRequested" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketInfoRequested))
            }
            "TicketCustomerResponded" => Some(
                serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketCustomerResponded),
            ),
            "TicketClosed" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketClosed))
            }
            "TicketEscalated" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketEscalated))
            }
            "TicketsMerged" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketsMerged))
            }
            "TicketRated" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketRated))
            }
            "TicketInternalNoteAdded" => Some(
                serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketInternalNoteAdded),
            ),
            _ => None,
        }
    }
}

/// `specs/skilj-helpdesk.allium`'s `Ticket.status`, plus `Merged` - not
/// in the original spec, `MergeTickets`'s own outcome for the duplicate
/// side of a merge (see that command's doc comment). Every *other*
/// command's own status match already ends on a catch-all `Some(other)
/// => Rejected{..}` arm, so adding this variant needed no changes
/// anywhere else - verified by reading each one, not assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    Open,
    InProgress,
    WaitingOnCustomer,
    Resolved,
    Closed,
    Merged,
}

/// Everything a ticket command's `decide()` reads from one ticket's
/// history, folded in one pass - same technique as `banking.rs`'s
/// `balance_of`/`courses.rs`'s roster folds, gathered into one struct so
/// it can also be `TicketSnapshot`'s stored state (issue #14). Every
/// single-ticket command decides from this alone, whether it was folded
/// from the full `matching_events` (`decide()`) or resumed from a stored
/// snapshot plus the events since (`decide_from_snapshot()`), so the two
/// paths can't drift apart.
///
/// Changing what this folds, or its shape, means bumping
/// `TicketSnapshot::VERSION`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TicketFacts {
    /// Which ticket this is - set up front by `of`/`resume`, or by the
    /// first `TicketCreated` when `TicketSnapshot` folds from nothing
    /// (its `fold` gets no key). Needed for `TicketsMerged`, the one
    /// event tagged with two tickets.
    pub ticket_id: String,
    /// `None` means the ticket doesn't exist (no `TicketCreated` found).
    pub status: Option<TicketStatus>,
    /// Off its `TicketCreated` - see `company_id`.
    pub company_id: Option<String>,
    /// Off its `TicketCreated`. Safe to copy out of history, unlike
    /// `requester_name`/`requester_email`: those are stored, and so seen
    /// here, as ciphertext, and copying them into a new sensitive field
    /// would encrypt them a second time.
    pub requester_id: Option<String>,
    /// The priority it was created with - what `EscalateTicket` bumps.
    pub created_priority: Option<TicketPriority>,
    /// How many times it has been resolved so far. Counts the events
    /// rather than reading `TicketResolved::resolution`, so a history that
    /// predates that field still numbers its next resolution correctly.
    pub resolutions: u32,
    pub escalated: bool,
    pub rated: bool,
}

impl TicketFacts {
    /// Folded from `ticket_id`'s slice of `matching_events`.
    pub fn of(matching_events: &[HelpdeskEvent], ticket_id: &str) -> Self {
        let mut facts = Self {
            ticket_id: ticket_id.to_string(),
            ..Self::default()
        };
        for event in matching_events {
            facts.apply(event);
        }
        facts
    }

    /// A stored `TicketSnapshot` state brought up to date with the events
    /// stored after it. `state_json` is the default state when there is
    /// no usable snapshot yet, and then `events_since` is the full
    /// history - see `CommandType::decide_from_snapshot`.
    pub fn resume(
        state_json: &str,
        events_since: &[HelpdeskEvent],
        ticket_id: &str,
    ) -> Result<Self, serde_json::Error> {
        let mut facts: Self = serde_json::from_str(state_json)?;
        if facts.ticket_id.is_empty() {
            facts.ticket_id = ticket_id.to_string();
        }
        for event in events_since {
            facts.apply(event);
        }
        Ok(facts)
    }

    fn apply(&mut self, event: &HelpdeskEvent) {
        match event {
            HelpdeskEvent::TicketCreated(p)
                if self.ticket_id.is_empty() || p.ticket_id == self.ticket_id =>
            {
                self.ticket_id = p.ticket_id.clone();
                self.status = Some(TicketStatus::Open);
                self.company_id.get_or_insert_with(|| p.company_id.clone());
                self.requester_id
                    .get_or_insert_with(|| p.requester_id.clone());
                self.created_priority.get_or_insert(p.priority);
            }
            HelpdeskEvent::TicketAssigned(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::InProgress);
            }
            HelpdeskEvent::TicketResolved(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::Resolved);
                self.resolutions += 1;
            }
            HelpdeskEvent::TicketReopened(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::InProgress);
            }
            HelpdeskEvent::TicketInfoRequested(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::WaitingOnCustomer);
            }
            HelpdeskEvent::TicketCustomerResponded(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::InProgress);
            }
            HelpdeskEvent::TicketClosed(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::Closed);
            }
            // Only the *duplicate* side becomes Merged - the primary's
            // own status is untouched by a merge (see `MergeTickets`'s
            // own doc comment), so this only ever matches
            // `duplicate_ticket_id`, never `primary_ticket_id`.
            HelpdeskEvent::TicketsMerged(p) if p.duplicate_ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::Merged);
            }
            HelpdeskEvent::TicketEscalated(p) if p.ticket_id == self.ticket_id => {
                self.escalated = true;
            }
            HelpdeskEvent::TicketRated(p) if p.ticket_id == self.ticket_id => {
                self.rated = true;
            }
            _ => {}
        }
    }

    /// The company a ticket belongs to. Every ticket-lifecycle command
    /// past creation uses this to stamp `company_id` onto the event it
    /// emits, which is what lets `CompanyActiveTickets` below fold every
    /// ticket event for one ticket into the correct per-company projection
    /// instance - `AssignTicket`/`ResolveTicket`/etc.'s own payloads never
    /// carried `company_id` as caller input (there's no reason to trust a
    /// caller-supplied one when the real answer is already in the ticket's
    /// own history). Only called once `status` is known to be `Some`.
    fn company_id(&self) -> &str {
        self.company_id
            .as_deref()
            .expect("a ticket with any status has a TicketCreated in its own history")
    }

    /// Rejects a command whose `requester_id` isn't this ticket's
    /// requester.
    ///
    /// The command's free text is encrypted under the key its own
    /// `requester_id` names, so a wrong one would file the text under
    /// another customer: readable to them, and out of reach of the real
    /// customer's erasure. Only meaningful once the ticket is known to
    /// exist.
    fn reject_unless_requester(&self, requester_id: Option<&str>) -> Option<CommandDecision> {
        if requester_id.is_some() && requester_id == self.requester_id.as_deref() {
            return None;
        }
        Some(CommandDecision::Rejected {
            reason: format!(
                "requester_id {requester_id:?} is not ticket {}'s requester",
                self.ticket_id
            ),
            kind: "requester_mismatch".into(),
        })
    }
}

/// `TicketFacts`, kept per ticket - lets every single-ticket command
/// decide from a stored state plus the events since, instead of reading
/// the ticket's whole history each time (issue #14). skilj only uses it
/// for a command whose one derived tag is `ticket`, so `MergeTickets`
/// (two tickets) keeps plain `decide()`, and `CreateTicket` (tagged by
/// company) uses `CompanySnapshot`. See
/// `docs/ticket-snapshot-report-2026-10-05.md`.
pub struct TicketSnapshot;

#[auto_register(BOUNDED_CONTEXT)]
impl Snapshot for TicketSnapshot {
    type State = TicketFacts;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "TicketSnapshot";
    const TAG_KEY: &'static str = "ticket";
    /// Same owner every ticket projection derives - a company-scoped
    /// Role can only `inspectSnapshot` its own company's tickets.
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    const VERSION: u64 = 1;
    /// Keyed by ticket, so unlike `CompanyActiveTickets` the work spreads
    /// over every partition - see that projection's `PARTITION_COUNT`
    /// and `docs/partitioned-projection-report-2026-10-05.md`.
    const PARTITION_COUNT: u32 = 4;
    fn fold(state: &mut Self::State, event: &Self::Event) {
        state.apply(event);
    }
}

/// The `decide_from_snapshot()` every single-ticket command shares: the
/// stored state brought up to date, then the same decision `decide()`
/// makes from the full history.
fn decide_from_ticket_snapshot(
    ticket_id: &str,
    state_json: &str,
    events_since: &[HelpdeskEvent],
    decide: impl FnOnce(&TicketFacts) -> CommandDecision,
) -> CommandDecision {
    match TicketFacts::resume(state_json, events_since, ticket_id) {
        Ok(facts) => decide(&facts),
        // Only written by `TicketSnapshot::fold` itself, and an
        // older-`VERSION` row is never handed over - so this is a bug,
        // reported as a rejection rather than a wrong decision.
        Err(e) => CommandDecision::Rejected {
            reason: format!("stored TicketSnapshot for {ticket_id} is unreadable: {e}"),
            kind: "snapshot_unreadable".into(),
        },
    }
}

/// The one-tier priority bump `EscalateTicket` applies - covered by that
/// command's own integration test (this file has no unit-test module of
/// its own; every other pure fold here is proven the same way, through
/// the REST surface). Clamped at `Urgent` rather than wrapping or
/// erroring: escalating an already-urgent ticket a second time is
/// rejected before this is ever called (see `EscalateTicket`'s own
/// `already_escalated` guard), but clamping here too means this
/// function is total and never needs to fail on its own account.
fn escalate_priority(priority: TicketPriority) -> TicketPriority {
    match priority {
        TicketPriority::Low => TicketPriority::Medium,
        TicketPriority::Medium => TicketPriority::High,
        TicketPriority::High | TicketPriority::Urgent => TicketPriority::Urgent,
    }
}

/// `specs/skilj-helpdesk.allium`'s `Company.status`, in full.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CompanyStatus {
    Trialing,
    Active,
    Expired,
}

/// Folds this company's own status - same technique as `TicketFacts`
/// above. `None` means the company doesn't exist (no `CompanySignedUp`
/// found).
///
/// **Two sources, one answer.** In the shared `helpdesk` context the
/// status comes from the real `CompanySignedUp`/`CompanyActivated`/
/// `CompanyExpired` events. In a tenant it comes from
/// `CompanyLifecycleMirrored` instead, because a tenant's own history
/// holds none of the three - see that event's own doc comment for why
/// the mirror exists. Both are folded here, in the same pass, in stored
/// sequence order, so a mixed history (a tenant that also happened to
/// sign the company up locally, as `tests/multi_tenant_provisioning.rs`
/// does) resolves by recency exactly the way a single-source history
/// would - no special-casing, no "prefer the mirror" branch to get
/// wrong.
fn company_status(matching_events: &[HelpdeskEvent], company_id: &str) -> Option<CompanyStatus> {
    let mut status = None;
    for event in matching_events {
        match event {
            HelpdeskEvent::CompanySignedUp(p) if p.company_id == company_id => {
                status = Some(CompanyStatus::Trialing);
            }
            HelpdeskEvent::CompanyActivated(p) if p.company_id == company_id => {
                status = Some(CompanyStatus::Active);
            }
            HelpdeskEvent::CompanyExpired(p) if p.company_id == company_id => {
                status = Some(CompanyStatus::Expired);
            }
            // The mirror, folded identically to the event it stands in
            // for. Deliberately *not* matched on `source_event_type`:
            // that field is provenance for a human reading the log, and
            // trusting it here would mean a payload that disagrees with
            // its own `status` field silently decides a Ticket command's
            // guard. `status` is the single field this reads.
            HelpdeskEvent::CompanyLifecycleMirrored(p) if p.company_id == company_id => {
                status = Some(p.status);
            }
            _ => {}
        }
    }
    status
}

/// What `CreateTicket` decides from: the company's status and which
/// ticket ids it has already created, folded in one pass. Kept per
/// company by `CompanySnapshot`, so a ticket's creation reads that row
/// plus the events since, instead of the company's whole history: every
/// `TicketCreated` (and `TicketInternalNoteAdded`) carries the "company"
/// tag, so that history grows with every ticket the company ever filed
/// (docs/create-ticket-latency-report-2026-10-09.md). Same split as
/// `TicketFacts`, so `decide()` and `decide_from_snapshot()` can't drift
/// apart.
///
/// Changing what this folds, or its shape, means bumping
/// `CompanySnapshot::VERSION`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct CompanyFacts {
    /// Which company this is - set up front by `of`/`resume`, or by the
    /// first event when `CompanySnapshot` folds from nothing (its `fold`
    /// gets no key).
    pub company_id: String,
    /// `None` means the company doesn't exist - see `company_status`.
    pub status: Option<CompanyStatus>,
    /// Every ticket id a `TicketCreated` of this company used.
    pub ticket_ids: std::collections::BTreeSet<String>,
}

impl CompanyFacts {
    /// Folded from `company_id`'s slice of `matching_events`.
    pub fn of(matching_events: &[HelpdeskEvent], company_id: &str) -> Self {
        let mut facts = Self {
            company_id: company_id.to_string(),
            ..Self::default()
        };
        for event in matching_events {
            facts.apply(event);
        }
        facts
    }

    /// A stored `CompanySnapshot` state brought up to date with the
    /// events stored after it - see `TicketFacts::resume`.
    pub fn resume(
        state_json: &str,
        events_since: &[HelpdeskEvent],
        company_id: &str,
    ) -> Result<Self, serde_json::Error> {
        let mut facts: Self = serde_json::from_str(state_json)?;
        if facts.company_id.is_empty() {
            facts.company_id = company_id.to_string();
        }
        for event in events_since {
            facts.apply(event);
        }
        Ok(facts)
    }

    fn apply(&mut self, event: &HelpdeskEvent) {
        let company_id = match event {
            HelpdeskEvent::CompanySignedUp(p) => &p.company_id,
            HelpdeskEvent::CompanyActivated(p) => &p.company_id,
            HelpdeskEvent::CompanyExpired(p) => &p.company_id,
            HelpdeskEvent::CompanyLifecycleMirrored(p) => &p.company_id,
            HelpdeskEvent::TicketCreated(p) => &p.company_id,
            _ => return,
        };
        if self.company_id.is_empty() {
            self.company_id = company_id.clone();
        } else if *company_id != self.company_id {
            return;
        }
        if let HelpdeskEvent::TicketCreated(p) = event {
            self.ticket_ids.insert(p.ticket_id.clone());
        } else {
            self.status = company_status(std::slice::from_ref(event), company_id).or(self.status);
        }
    }
}

/// `CompanyFacts`, kept per company - see its doc comment. skilj uses it
/// for `CreateTicket`, the one command whose only derived tag is
/// "company" and that runs on every new ticket. The company lifecycle
/// commands keep plain `decide()`: they run a few times per company.
pub struct CompanySnapshot;

#[auto_register(BOUNDED_CONTEXT)]
impl Snapshot for CompanySnapshot {
    type State = CompanyFacts;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CompanySnapshot";
    const TAG_KEY: &'static str = "company";
    /// The company is its own owner, as for every company-keyed
    /// projection.
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    const VERSION: u64 = 1;
    /// Keyed by company, so it spreads over as many partitions as there
    /// are companies - same as `CompanyActiveTickets`.
    const PARTITION_COUNT: u32 = 4;
    fn fold(state: &mut Self::State, event: &Self::Event) {
        state.apply(event);
    }
}

/// This company's own provisioned tenant name, if `RecordCompanyTenant`
/// has already recorded one - `None` covers both "hasn't signed up yet"
/// and "signed up but the provisioner hasn't reacted yet", same
/// approach as `company_status`'s own single `Option` for two not-yet
/// states; `RecordCompanyTenant::decide` below tells them apart itself
/// via `company_status`.
fn company_tenant(matching_events: &[HelpdeskEvent], company_id: &str) -> Option<String> {
    matching_events.iter().find_map(|event| match event {
        HelpdeskEvent::CompanyTenantProvisioned(p) if p.company_id == company_id => {
            Some(p.tenant_name.clone())
        }
        _ => None,
    })
}

// --- commands ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SignUpCompanyPayload {
    pub company_id: String,
    pub name: String,
    pub contact_email: String,
}

pub struct SignUpCompany;

/// `specs/skilj-helpdesk.allium`'s `rule CompanySignsUp`. What the spec
/// also does here - provisioning the company's own skilj tenant via
/// `CreateBoundedContextFromTemplate` - stays out of `decide()` (pure
/// and I/O-free by contract), but is no longer skipped: it now happens
/// as a real side effect, driven by `src/bin/provisioner.rs` reacting to
/// the `CompanySignedUp` this emits and reporting back via
/// `RecordCompanyTenant` below.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for SignUpCompany {
    type Payload = SignUpCompanyPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "SignUpCompany";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        if payload.company_id.is_empty() {
            return CommandDecision::Rejected {
                reason: "company_id must not be empty".into(),
                kind: "empty_company_id".into(),
            };
        }
        if payload.company_id.len() > 256 {
            return CommandDecision::Rejected {
                reason: "company_id is too long (max 256 characters)".into(),
                kind: "company_id_too_long".into(),
            };
        }
        if payload.name.trim().is_empty() {
            return CommandDecision::Rejected {
                reason: "company name must not be empty".into(),
                kind: "empty_company_name".into(),
            };
        }
        if payload.name.len() > 256 {
            return CommandDecision::Rejected {
                reason: "company name is too long (max 256 characters)".into(),
                kind: "company_name_too_long".into(),
            };
        }
        let valid_email = match payload.contact_email.split_once('@') {
            Some((local, domain)) => {
                !local.is_empty()
                    && !domain.is_empty()
                    && !domain.contains('@')
                    && domain.contains('.')
                    && !domain.starts_with('.')
                    && !domain.ends_with('.')
                    && !payload.contact_email.chars().any(char::is_whitespace)
            }
            None => false,
        };
        if !valid_email {
            return CommandDecision::Rejected {
                reason: format!(
                    "contact_email {:?} is not a valid email address",
                    payload.contact_email
                ),
                kind: "invalid_email".into(),
            };
        }
        if company_status(matching_events, &payload.company_id).is_some() {
            return CommandDecision::Rejected {
                reason: format!("company {} has already signed up", payload.company_id),
                kind: "already_signed_up".into(),
            };
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "CompanySignedUp".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "name": payload.name,
                    "contact_email": payload.contact_email,
                }),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordCompanyTenantPayload {
    pub company_id: String,
    pub tenant_name: String,
}

pub struct RecordCompanyTenant;

/// The real side effect `SignUpCompany`'s own doc comment above
/// describes, reported back: `src/bin/provisioner.rs` reads
/// `CompanySignedUp` off skilj's REST event feed, calls skilj's own
/// `createBoundedContextFromTemplate` GraphQL mutation (superadmin-gated
/// - see `tests/multi_tenant_provisioning.rs`, which proves this exact
/// mechanism first) to stamp a brand-new tenant from this `helpdesk`
/// context as the template, then submits this command to make the
/// result durable - the same "I/O happens outside decide(), a pure
/// command records the outcome" split `ScheduleCompanyTrialConversion`'s
/// own mocked `PaymentGateway.charge` uses, just with a real skilj call
/// in place of a mock.
///
/// This is one company's tenant coming into real existence, not a
/// no-op: `provisioner`'s own doc comment and this crate's `README.md`
/// are explicit that Ticket commands/queries for that company still run
/// against this shared context, not the tenant just created for it.
/// The `company_status` read those commands' guards depend on used to be
/// the reason that couldn't change; it no longer is, and the mechanism
/// is `CompanyLifecycleMirrored` above plus
/// `src/bin/lifecycle-replicator.rs`. The cutover itself - actually
/// routing those commands - is still not done, and is called out in this
/// crate's `README.md`.
///
/// Idempotent by construction, same shape as `EscalateTicket`'s own
/// `already_escalated` guard: a redelivered `CompanySignedUp` (the REST
/// feed's own accepted "occasional missed/redelivered events" tradeoff -
/// see `specs/skilj-helpdesk.allium`'s resolved alerting design note,
/// which `provisioner.rs` accepts for the identical reason) would
/// otherwise record two conflicting tenant names for one company.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RecordCompanyTenant {
    type Payload = RecordCompanyTenantPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "RecordCompanyTenant";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        if company_status(matching_events, &payload.company_id).is_none() {
            return CommandDecision::Rejected {
                reason: format!("company {} hasn't signed up", payload.company_id),
                kind: "company_not_found".into(),
            };
        }
        if let Some(existing) = company_tenant(matching_events, &payload.company_id) {
            return CommandDecision::Rejected {
                reason: format!(
                    "company {} already has tenant {existing:?} recorded",
                    payload.company_id
                ),
                kind: "tenant_already_provisioned".into(),
            };
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "CompanyTenantProvisioned".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "tenant_name": payload.tenant_name,
                }),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordTenantLifecyclePayload {
    pub company_id: String,
    pub status: CompanyStatus,
    /// See `CompanyLifecycleMirroredPayload::source_event_type` - the
    /// same provenance string, carried through untouched.
    pub source_event_type: String,
}

pub struct RecordTenantLifecycle;

/// Writes a `CompanyLifecycleMirrored` fact into **this** bounded
/// context's own history - the write half of
/// `CompanyLifecycleMirrored`'s doc comment above, submitted by
/// `src/bin/lifecycle-replicator.rs` into the tenant named by
/// `RecordCompanyTenant`'s own recorded mapping.
///
/// **Not a lifecycle transition, and deliberately unidirectional.** The
/// shared `helpdesk` context remains the only place a company's status
/// actually changes; this command cannot move one, it can only record
/// what the shared context already decided. That asymmetry is the whole
/// safety argument, and it is why the guard below rejects a regression
/// rather than trying to validate a transition table of its own: a
/// tenant client that can only ever write the state it was last told,
/// and can never move backwards, cannot disagree with the authority in
/// a way that widens access.
///
/// **Why `status` is taken as already-resolved, not recomputed.** The
/// replicator derives it from the shared event's *type*
/// (`CompanySignedUp` -> `Trialing`), and this command trusts that
/// derivation rather than re-deriving it from a source event it cannot
/// read - the tenant has no copy of the shared event to re-read. The
/// compensating control is not a re-derivation but the caller shape:
/// this is a `rest_trigger_allowed` command reachable only with a
/// `CommandToken` minted against *this* tenant, which in this
/// deployment only `lifecycle-replicator.rs` (and tests) hold - see
/// `server.rs`'s own doc comment on how those tokens are minted.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RecordTenantLifecycle {
    type Payload = RecordTenantLifecyclePayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = RECORD_TENANT_LIFECYCLE_COMMAND;
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        // The shared context is authoritative, but a tenant's own mirror
        // history is not *ordered* by anything the shared context
        // guarantees: the replicator reads three separate REST event
        // feeds (`CompanySignedUp`/`CompanyActivated`/`CompanyExpired`,
        // each its own `EventReadToken` and therefore its own cursor), so
        // a company that converts and expires close together can deliver
        // `Expired` before `Activated`. Folding by sequence alone would
        // then leave the tenant permanently reporting the older state.
        //
        // So rank the states and refuse to move backwards, which makes
        // the fold order-independent for every ordering the three feeds
        // can actually produce. `Active` and `Expired` are both
        // terminal-ish ranks above `Trialing`, and `Active` outranks
        // `Expired` only because `ReactivateCompany`'s own
        // `expired -> active` transition makes "active" the later real
        // outcome of that pair; a company that genuinely expires *after*
        // activating is not a reachable sequence (the trial deadline is
        // cancelled on conversion - see `ScheduleCompanyTrialConversion`).
        let current = company_status(matching_events, &payload.company_id);
        if let Some(current) = current {
            if current == payload.status {
                return CommandDecision::Rejected {
                    reason: format!(
                        "company {} is already mirrored as {:?}",
                        payload.company_id, payload.status
                    ),
                    // Idempotency, same shape as `EscalateTicket`'s own
                    // `already_escalated` guard: the REST feed's
                    // `mode=auto` cursor can redeliver on a crash
                    // mid-tick (skilj-rest's own architecture docs §7.4),
                    // and `lifecycle-replicator.rs` logs a rejection and
                    // moves on rather than treating it as a failure.
                    kind: "lifecycle_already_mirrored".into(),
                };
            }
            if lifecycle_rank(payload.status) < lifecycle_rank(current) {
                return CommandDecision::Rejected {
                    reason: format!(
                        "refusing to mirror company {} backwards from {:?} to {:?}",
                        payload.company_id, current, payload.status
                    ),
                    kind: "lifecycle_regression".into(),
                };
            }
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "CompanyLifecycleMirrored".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "status": payload.status,
                    "source_event_type": payload.source_event_type,
                }),
            }],
        }
    }
}

/// Total order over the lifecycle states, for
/// `RecordTenantLifecycle`'s own regression guard above - deliberately
/// hand-written rather than derived from the enum's declaration order,
/// so reordering the variants can't silently change what "backwards"
/// means. `Trialing` is first (everything starts there); `Active` above
/// `Expired` for the reason given at the guard.
fn lifecycle_rank(status: CompanyStatus) -> u8 {
    match status {
        CompanyStatus::Trialing => 0,
        CompanyStatus::Expired => 1,
        CompanyStatus::Active => 2,
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ConvertCompanyTrialPayload {
    pub company_id: String,
}

pub struct ConvertCompanyTrial;

/// `specs/skilj-helpdesk.allium`'s `rule TrialPeriodEnds`'s success
/// branch: `trialing -> active`. Submitted by skilj's own native
/// per-entity deadline mechanism (`ScheduleCompanyTrialConversion`
/// below, docs/architecture.md §46), not a person - see that reactor's
/// own doc comment for why this pass implements the *state change* as
/// an ordinary command rather than skilj's `system_triggered`
/// scheduling (a global-cron mechanism, not suited to a per-company
/// deadline like this one).
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for ConvertCompanyTrial {
    type Payload = ConvertCompanyTrialPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "ConvertCompanyTrial";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match company_status(matching_events, &payload.company_id) {
            Some(CompanyStatus::Trialing) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "CompanyActivated".into(),
                    payload: serde_json::json!({ "company_id": payload.company_id }),
                }],
            },
            None => CommandDecision::Rejected {
                reason: format!("company {} does not exist", payload.company_id),
                kind: "company_not_found".into(),
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "company {} is {other:?}, not trialing - nothing to convert",
                    payload.company_id
                ),
                kind: "company_not_trialing".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ExpireCompanyTrialPayload {
    pub company_id: String,
}

pub struct ExpireCompanyTrial;

/// `specs/skilj-helpdesk.allium`'s `rule TrialPeriodEnds`'s failure
/// branch: `trialing -> expired`. Same submitter and reasoning as
/// `ConvertCompanyTrial` above - `ScheduleCompanyTrialExpiry` below
/// schedules this one instead, whenever the mocked charge outcome says
/// to.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for ExpireCompanyTrial {
    type Payload = ExpireCompanyTrialPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "ExpireCompanyTrial";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match company_status(matching_events, &payload.company_id) {
            Some(CompanyStatus::Trialing) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "CompanyExpired".into(),
                    payload: serde_json::json!({ "company_id": payload.company_id }),
                }],
            },
            None => CommandDecision::Rejected {
                reason: format!("company {} does not exist", payload.company_id),
                kind: "company_not_found".into(),
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "company {} is {other:?}, not trialing - nothing to expire",
                    payload.company_id
                ),
                kind: "company_not_trialing".into(),
            },
        }
    }
}

/// `specs/skilj-helpdesk.allium`'s `rule TrialPeriodEnds`, ported off
/// `src/bin/scheduler.rs`'s own hand-rolled polling loop onto skilj
/// 0.0.7's native per-entity deadline (`ScheduleDeadline`/
/// `CancelDeadline`, docs/architecture.md §46) - exactly the gap that
/// binary's own doc comment named before this feature existed ("neither
/// of which skilj's own `system_triggered` scheduling fits ... a
/// per-company deadline like this one").
///
/// The mocked `PaymentGateway.charge` outcome (`scheduling::mock_charge_succeeds`)
/// is decided here, at schedule time (when `CompanySignedUp` commits),
/// rather than at fire time inside `ConvertCompanyTrial::decide()` -
/// `ScheduleDeadline::Target` is one fixed command per schedule, so the
/// two outcomes need two independently-deciding schedules (this one and
/// `ScheduleCompanyTrialExpiry` below) rather than one branching inside
/// `decide()`. Harmless since the mock is deterministic (always `true` -
/// see its own doc comment); a real gateway integration would need the
/// charge attempt moved to fire time instead, inside the target
/// command's own `decide()` - exactly what docs/architecture.md §46
/// means by "whether a deadline is still relevant is a decision
/// `Target::decide()` gets to make against *current* state at fire
/// time, not one baked in back when the timer was scheduled."
///
/// `fire_at` uses `Utc::now()` rather than `CompanySignedUp`'s own
/// commit time, since `schedule()` only ever receives the event's
/// payload, never its metadata (unlike old `scheduler.rs`, which read
/// `metadata.createdAt` off the REST feed directly). Accurate as long
/// as this runs close to when the event actually committed - true for
/// real-time operation (background poll default 500ms) and every test
/// in this suite (each signs a company up and waits on its own next
/// tick) - which is also why `START_FROM` is `Latest`, not the
/// `Beginning` default: backfilling pre-existing `CompanySignedUp`
/// history on a fresh deploy would stamp every older signup with
/// today's date instead of its real one, worse than not scheduling it
/// at all.
///
/// No `CancelDeadline` counterpart, deliberately: a stale pending row
/// fires once and `ConvertCompanyTrial`/`ExpireCompanyTrial` reject it
/// ("not trialing") against a company that converted, expired, or
/// reactivated by some other path before its own deadline came due.
/// Unlike a ticket (see `CancelTicketAutoCloseOnReopen`), a company
/// never returns to `trialing`, so an old deadline can't land on a newer
/// trial.
pub struct ScheduleCompanyTrialConversion;

impl ScheduleDeadline for ScheduleCompanyTrialConversion {
    type Source = CompanySignedUp;
    type Target = ConvertCompanyTrial;
    const NAME: &'static str = "ScheduleCompanyTrialConversion";
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn schedule(
        source_payload: &CompanySignedUpPayload,
    ) -> Option<DeadlineSpec<ConvertCompanyTrialPayload>> {
        if !scheduling::mock_charge_succeeds() {
            return None;
        }
        Some(DeadlineSpec {
            fire_at: chrono::Utc::now() + scheduling::trial_duration(),
            tags: vec![company_tag_value(&source_payload.company_id)],
            payload: ConvertCompanyTrialPayload {
                company_id: source_payload.company_id.clone(),
            },
        })
    }
}

/// `rule TrialPeriodEnds`'s failure branch - see
/// `ScheduleCompanyTrialConversion`'s own doc comment for why this is a
/// second, independently-deciding schedule rather than a branch inside
/// one. Never actually fires in this showcase (`mock_charge_succeeds`
/// always returns `true`) - kept anyway so `ExpireCompanyTrial` stays
/// reachable the way it always has, both directly
/// (`rest_trigger_allowed`, used by this crate's own tests to force a
/// company into `expired` without waiting on a deadline) and as the
/// intended path once a real gateway integration flips the mock.
pub struct ScheduleCompanyTrialExpiry;

impl ScheduleDeadline for ScheduleCompanyTrialExpiry {
    type Source = CompanySignedUp;
    type Target = ExpireCompanyTrial;
    const NAME: &'static str = "ScheduleCompanyTrialExpiry";
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn schedule(
        source_payload: &CompanySignedUpPayload,
    ) -> Option<DeadlineSpec<ExpireCompanyTrialPayload>> {
        if scheduling::mock_charge_succeeds() {
            return None;
        }
        Some(DeadlineSpec {
            fire_at: chrono::Utc::now() + scheduling::trial_duration(),
            tags: vec![company_tag_value(&source_payload.company_id)],
            payload: ExpireCompanyTrialPayload {
                company_id: source_payload.company_id.clone(),
            },
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReactivateCompanyPayload {
    pub company_id: String,
}

pub struct ReactivateCompany;

/// `specs/skilj-helpdesk.allium`'s `rule CompanySubscribes`: `expired ->
/// active`. Unlike `ConvertCompanyTrial`/`ExpireCompanyTrial`, this one
/// really is person-submitted (an expired company choosing to pay) -
/// still a mocked `PaymentGateway.charge`, but the caller is a real
/// customer-facing surface, not a deadline reactor. Kept unconditionally
/// successful here (no `charged.succeeded` branch) since a real payment
/// retry-on-failure UX is presentation-level, out of this pass's scope.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for ReactivateCompany {
    type Payload = ReactivateCompanyPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "ReactivateCompany";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match company_status(matching_events, &payload.company_id) {
            Some(CompanyStatus::Expired) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "CompanyActivated".into(),
                    payload: serde_json::json!({ "company_id": payload.company_id }),
                }],
            },
            None => CommandDecision::Rejected {
                reason: format!("company {} does not exist", payload.company_id),
                kind: "company_not_found".into(),
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "company {} is {other:?}, not expired - nothing to reactivate",
                    payload.company_id
                ),
                kind: "company_not_expired".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreateTicketPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub requester_id: String,
    pub logged_by_staff_id: Option<String>,
    pub title: String,
    pub description: String,
    pub priority: TicketPriority,
    /// See `TicketCreatedPayload::requester_name`.
    pub requester_name: Option<String>,
    /// See `TicketCreatedPayload::requester_email`.
    pub requester_email: Option<String>,
}

pub struct CreateTicket;

/// `specs/skilj-helpdesk.allium`'s `rule CustomerCreatesTicket`/
/// `StaffLogsTicketOnBehalf`, merged into one command
/// (`logged_by_staff_id` tells the two cases apart) - the spec keeps
/// them as two triggers because they're two different surfaces
/// (`CustomerPortal` vs. `StaffTicketQueue`); at the `decide()` level
/// they're the same decision, so one `CommandType` covers both, the
/// same way the spec's own `logged_by: StaffMember?` already unifies
/// them on the `Ticket` entity.
///
/// `requires: company.status != expired` - implemented in full now that
/// `company_status` tracks the real lifecycle (this was originally
/// written against `specs/skilj-helpdesk.allium`'s own
/// `requires: company.status = active`, which turned out to be a bug in
/// the spec itself, caught while wiring this up for real: it would have
/// blocked ticket creation during the free trial entirely, which
/// contradicts the whole point of a trial - fixed in the spec alongside
/// this code, not worked around here).
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for CreateTicket {
    type Payload = CreateTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CreateTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    /// The stored command holds the same customer data as the event it
    /// produces, so it needs the same protection.
    fn sensitive_fields() -> Vec<SensitiveField> {
        ticket_created_customer_fields()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        create_ticket(
            payload,
            &CompanyFacts::of(matching_events, &payload.company_id),
        )
    }
    fn snapshot() -> Option<&'static str> {
        Some(CompanySnapshot::NAME)
    }
    fn decide_from_snapshot(
        payload: &Self::Payload,
        snapshot_state_json: &str,
        events_since_snapshot: &[Self::Event],
    ) -> CommandDecision {
        match CompanyFacts::resume(
            snapshot_state_json,
            events_since_snapshot,
            &payload.company_id,
        ) {
            Ok(facts) => create_ticket(payload, &facts),
            // See `decide_from_ticket_snapshot`.
            Err(e) => CommandDecision::Rejected {
                reason: format!(
                    "stored CompanySnapshot for {} is unreadable: {e}",
                    payload.company_id
                ),
                kind: "snapshot_unreadable".into(),
            },
        }
    }
}

/// `CreateTicket`'s decision, from either path.
fn create_ticket(payload: &CreateTicketPayload, company: &CompanyFacts) -> CommandDecision {
    match company.status {
        None => {
            return CommandDecision::Rejected {
                reason: format!("company {} has not signed up", payload.company_id),
                kind: "company_not_found".into(),
            };
        }
        Some(CompanyStatus::Expired) => {
            return CommandDecision::Rejected {
                reason: format!(
                    "company {} is expired - subscribe to keep creating tickets",
                    payload.company_id
                ),
                kind: "company_expired".into(),
            };
        }
        Some(CompanyStatus::Trialing | CompanyStatus::Active) => {}
    }
    if company.ticket_ids.contains(&payload.ticket_id) {
        return CommandDecision::Rejected {
            reason: format!("ticket {} already exists", payload.ticket_id),
            kind: "ticket_already_exists".into(),
        };
    }
    CommandDecision::Accepted {
        events: vec![EventSpec {
            event_type: "TicketCreated".into(),
            payload: serde_json::json!({
                "ticket_id": payload.ticket_id,
                "company_id": payload.company_id,
                "requester_id": payload.requester_id,
                "logged_by_staff_id": payload.logged_by_staff_id,
                "title": payload.title,
                "description": payload.description,
                "priority": payload.priority,
                "requester_name": payload.requester_name,
                "requester_email": payload.requester_email,
            }),
        }],
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AssignTicketPayload {
    pub ticket_id: String,
    pub staff_id: String,
}

pub struct AssignTicket;

/// `specs/skilj-helpdesk.allium`'s `rule StaffPicksUpTicket`:
/// `requires: ticket.status = open`.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for AssignTicket {
    type Payload = AssignTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "AssignTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn snapshot() -> Option<&'static str> {
        Some(TicketSnapshot::NAME)
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        Self::decide_with(
            payload,
            &TicketFacts::of(matching_events, &payload.ticket_id),
        )
    }
    fn decide_from_snapshot(
        payload: &Self::Payload,
        snapshot_state_json: &str,
        events_since_snapshot: &[Self::Event],
    ) -> CommandDecision {
        decide_from_ticket_snapshot(
            &payload.ticket_id,
            snapshot_state_json,
            events_since_snapshot,
            |facts| Self::decide_with(payload, facts),
        )
    }
}

impl AssignTicket {
    fn decide_with(payload: &AssignTicketPayload, facts: &TicketFacts) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Open) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketAssigned".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "staff_id": payload.staff_id,
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not open - only an open ticket can be picked up",
                    payload.ticket_id
                ),
                kind: "ticket_not_open".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResolveTicketPayload {
    pub ticket_id: String,
}

pub struct ResolveTicket;

/// `specs/skilj-helpdesk.allium`'s `rule StaffResolvesTicket`:
/// `requires: ticket.status = in_progress`.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for ResolveTicket {
    type Payload = ResolveTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "ResolveTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn snapshot() -> Option<&'static str> {
        Some(TicketSnapshot::NAME)
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        Self::decide_with(
            payload,
            &TicketFacts::of(matching_events, &payload.ticket_id),
        )
    }
    fn decide_from_snapshot(
        payload: &Self::Payload,
        snapshot_state_json: &str,
        events_since_snapshot: &[Self::Event],
    ) -> CommandDecision {
        decide_from_ticket_snapshot(
            &payload.ticket_id,
            snapshot_state_json,
            events_since_snapshot,
            |facts| Self::decide_with(payload, facts),
        )
    }
}

impl ResolveTicket {
    fn decide_with(payload: &ResolveTicketPayload, facts: &TicketFacts) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::InProgress) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketResolved".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "resolution": facts.resolutions + 1,
                        "requester_id": facts.requester_id,
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not in progress - only a picked-up ticket can be resolved",
                    payload.ticket_id
                ),
                kind: "ticket_not_in_progress".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReopenTicketPayload {
    pub ticket_id: String,
}

pub struct ReopenTicket;

/// `specs/skilj-helpdesk.allium`'s `rule TicketReopened`: `requires:
/// ticket.status = resolved`.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for ReopenTicket {
    type Payload = ReopenTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "ReopenTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn snapshot() -> Option<&'static str> {
        Some(TicketSnapshot::NAME)
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        Self::decide_with(
            payload,
            &TicketFacts::of(matching_events, &payload.ticket_id),
        )
    }
    fn decide_from_snapshot(
        payload: &Self::Payload,
        snapshot_state_json: &str,
        events_since_snapshot: &[Self::Event],
    ) -> CommandDecision {
        decide_from_ticket_snapshot(
            &payload.ticket_id,
            snapshot_state_json,
            events_since_snapshot,
            |facts| Self::decide_with(payload, facts),
        )
    }
}

impl ReopenTicket {
    fn decide_with(payload: &ReopenTicketPayload, facts: &TicketFacts) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Resolved) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketReopened".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "resolution": facts.resolutions,
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not resolved - only a resolved ticket can be reopened",
                    payload.ticket_id
                ),
                kind: "ticket_not_resolved".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RequestInfoFromCustomerPayload {
    pub ticket_id: String,
    pub staff_id: String,
    /// Encrypted under the requester's key, here and in the event.
    pub message: String,
    /// The ticket's requester. Required, though optional in the schema
    /// (a field added later must be): without it the stored command would
    /// hold `message` in plaintext. See `TicketFacts::reject_unless_requester`.
    pub requester_id: Option<String>,
}

pub struct RequestInfoFromCustomer;

/// `specs/skilj-helpdesk.allium`'s `rule StaffRequestsInfo`: `requires:
/// ticket.status = in_progress`.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for RequestInfoFromCustomer {
    type Payload = RequestInfoFromCustomerPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "RequestInfoFromCustomer";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["message"])
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn snapshot() -> Option<&'static str> {
        Some(TicketSnapshot::NAME)
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        Self::decide_with(
            payload,
            &TicketFacts::of(matching_events, &payload.ticket_id),
        )
    }
    fn decide_from_snapshot(
        payload: &Self::Payload,
        snapshot_state_json: &str,
        events_since_snapshot: &[Self::Event],
    ) -> CommandDecision {
        decide_from_ticket_snapshot(
            &payload.ticket_id,
            snapshot_state_json,
            events_since_snapshot,
            |facts| Self::decide_with(payload, facts),
        )
    }
}

impl RequestInfoFromCustomer {
    fn decide_with(
        payload: &RequestInfoFromCustomerPayload,
        facts: &TicketFacts,
    ) -> CommandDecision {
        let status = facts.status;
        if status.is_some() {
            if let Some(rejected) = facts.reject_unless_requester(payload.requester_id.as_deref()) {
                return rejected;
            }
        }
        match status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::InProgress) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketInfoRequested".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "staff_id": payload.staff_id,
                        "message": payload.message,
                        "requester_id": payload.requester_id,
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not in progress - can only ask a picked-up ticket's customer for more information",
                    payload.ticket_id
                ),
                kind: "ticket_not_in_progress".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CustomerRespondsToTicketPayload {
    pub ticket_id: String,
    /// Must be the ticket's requester - see `TicketFacts::reject_unless_requester`.
    pub requester_id: String,
    /// Encrypted under the requester's key, here and in the event.
    pub message: String,
}

pub struct CustomerRespondsToTicket;

/// `specs/skilj-helpdesk.allium`'s `rule CustomerReplies`: `requires:
/// ticket.status = waiting_on_customer`.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for CustomerRespondsToTicket {
    type Payload = CustomerRespondsToTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CustomerRespondsToTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["message"])
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn snapshot() -> Option<&'static str> {
        Some(TicketSnapshot::NAME)
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        Self::decide_with(
            payload,
            &TicketFacts::of(matching_events, &payload.ticket_id),
        )
    }
    fn decide_from_snapshot(
        payload: &Self::Payload,
        snapshot_state_json: &str,
        events_since_snapshot: &[Self::Event],
    ) -> CommandDecision {
        decide_from_ticket_snapshot(
            &payload.ticket_id,
            snapshot_state_json,
            events_since_snapshot,
            |facts| Self::decide_with(payload, facts),
        )
    }
}

impl CustomerRespondsToTicket {
    fn decide_with(
        payload: &CustomerRespondsToTicketPayload,
        facts: &TicketFacts,
    ) -> CommandDecision {
        let status = facts.status;
        if status.is_some() {
            if let Some(rejected) = facts.reject_unless_requester(Some(&payload.requester_id)) {
                return rejected;
            }
        }
        match status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::WaitingOnCustomer) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketCustomerResponded".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "requester_id": payload.requester_id,
                        "message": payload.message,
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not waiting on the customer - nothing to respond to",
                    payload.ticket_id
                ),
                kind: "ticket_not_waiting_on_customer".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CloseTicketPayload {
    pub ticket_id: String,
    /// Whose ticket this is - set by `ScheduleTicketAutoClose` from
    /// `TicketResolvedPayload::requester_id`, so a pending auto-close
    /// deadline names its customer. skilj's `forgetSubject` finds the
    /// deadlines to resolve as `forgotten` through the target command's
    /// `sensitive_fields` subjects, which is why `CloseTicket` declares
    /// `requester_email` below.
    pub requester_id: Option<String>,
    /// Where a closure notice would go, encrypted under the requester's
    /// key. The auto-close deadline leaves it `None`: the only email it
    /// could copy is `TicketCreated`'s, which it sees as ciphertext (see
    /// `TicketFacts::requester_id`). Declaring it is what ties a
    /// `CloseTicket` payload to its customer for `forgetSubject` -
    /// skilj has no way to declare a subject without a sensitive field.
    pub requester_email: Option<String>,
}

pub struct CloseTicket;

/// `specs/skilj-helpdesk.allium`'s `rule TicketAutoCloses`: `requires:
/// ticket.status = resolved`. Submitted by `ScheduleTicketAutoClose`
/// below - see `TicketClosed`'s own doc comment.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for CloseTicket {
    type Payload = CloseTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CloseTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        vec![SensitiveField {
            field: "requester_email".into(),
            subject_key: CUSTOMER_SUBJECT.into(),
            subject_field: "requester_id".into(),
        }]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn snapshot() -> Option<&'static str> {
        Some(TicketSnapshot::NAME)
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        Self::decide_with(
            payload,
            &TicketFacts::of(matching_events, &payload.ticket_id),
        )
    }
    fn decide_from_snapshot(
        payload: &Self::Payload,
        snapshot_state_json: &str,
        events_since_snapshot: &[Self::Event],
    ) -> CommandDecision {
        decide_from_ticket_snapshot(
            &payload.ticket_id,
            snapshot_state_json,
            events_since_snapshot,
            |facts| Self::decide_with(payload, facts),
        )
    }
}

impl CloseTicket {
    fn decide_with(payload: &CloseTicketPayload, facts: &TicketFacts) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Resolved) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketClosed".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not resolved - only a resolved ticket auto-closes",
                    payload.ticket_id
                ),
                kind: "ticket_not_resolved".into(),
            },
        }
    }
}

/// `specs/skilj-helpdesk.allium`'s `rule TicketAutoCloses`, ported the
/// same way `ScheduleCompanyTrialConversion` above is - see that one's
/// doc comment for the `fire_at`/`START_FROM` reasoning, identical here.
///
/// A reopen cancels the deadline - see `CancelTicketAutoCloseOnReopen`.
/// A ticket that is closed by hand or merged away while its deadline is
/// pending needs no cancel: the deadline fires once and
/// `CloseTicket::decide()` rejects it ("not resolved"), which
/// docs/architecture.md §46 treats as a normal outcome ("a `Target`
/// command that ... gets rejected by its own `decide()` is marked
/// `fired` too").
///
/// Tagged twice: `ticket` like every other ticket deadline, and
/// `ticket_resolution` for this one resolution, which is the only tag
/// the cancel names. Events stored before `TicketResolved::resolution`
/// existed get the `ticket` tag only, so a reopen never cancels them and
/// `decide()` is their only guard, as before.
pub struct ScheduleTicketAutoClose;

impl ScheduleDeadline for ScheduleTicketAutoClose {
    type Source = TicketResolved;
    type Target = CloseTicket;
    const NAME: &'static str = "ScheduleTicketAutoClose";
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn schedule(
        source_payload: &TicketResolvedPayload,
    ) -> Option<DeadlineSpec<CloseTicketPayload>> {
        let mut tags = vec![ticket_tag_value(&source_payload.ticket_id)];
        if let Some(resolution) = source_payload.resolution {
            tags.push(ticket_resolution_tag_value(
                &source_payload.ticket_id,
                resolution,
            ));
        }
        Some(DeadlineSpec {
            fire_at: chrono::Utc::now() + scheduling::auto_close_after(),
            tags,
            payload: CloseTicketPayload {
                ticket_id: source_payload.ticket_id.clone(),
                requester_id: source_payload.requester_id.clone(),
                requester_email: None,
            },
        })
    }
}

/// Cancels a resolved ticket's pending auto-close when it is reopened -
/// `specs/skilj-helpdesk.allium`'s `rule TicketReopened` ends the
/// resolution `rule TicketAutoCloses` was waiting on.
///
/// `CloseTicket::decide()` alone is not enough here. Resolve, reopen and
/// resolve again within `auto_close_after`, and the first resolution's
/// deadline comes due while the ticket is `resolved` again: `decide()`
/// accepts, and the ticket closes days before its second resolution's
/// wait is up.
///
/// It cancels by `ticket_resolution` rather than by `ticket` because
/// skilj's schedule and cancel loops keep separate cursors, and nothing
/// stops the schedule loop running ahead (docs/architecture.md §130 only
/// holds a cancel back until its schedule catches up, not the reverse).
/// If the schedule loop has already turned the second `TicketResolved`
/// into a deadline when this cancel reaches the reopen before it, a
/// `ticket` tag would cancel that one too, and the ticket would never
/// auto-close. The resolution number makes the reopen name only the
/// deadline it actually ends. The opposite race - the deadline coming
/// due before this cancel has run - is skilj's own to hold back (§131).
pub struct CancelTicketAutoCloseOnReopen;

impl CancelDeadline for CancelTicketAutoCloseOnReopen {
    type Source = TicketReopened;
    type Deadline = ScheduleTicketAutoClose;
    const NAME: &'static str = "CancelTicketAutoCloseOnReopen";
    /// Matches `ScheduleTicketAutoClose::START_FROM`: a reopen from
    /// before this deploy has no deadline of this shape to cancel.
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn cancel_tags(source_payload: &TicketReopenedPayload) -> Option<Vec<Tag>> {
        let resolution = source_payload.resolution?;
        Some(vec![ticket_resolution_tag_value(
            &source_payload.ticket_id,
            resolution,
        )])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EscalateTicketPayload {
    pub ticket_id: String,
}

pub struct EscalateTicket;

/// Not in the original spec - see `TicketEscalated`'s own doc comment,
/// and `specs/skilj-helpdesk.allium`'s updated `rule
/// TicketBecomesOverdue`. Submitted by `src/bin/alerter.rs`'s own
/// overdue sweep, never by a person - same "a background reactor submits
/// an ordinary command" treatment `CloseTicket` gets from
/// `ScheduleTicketAutoClose` and `ConvertCompanyTrial` gets from
/// `ScheduleCompanyTrialConversion`.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for EscalateTicket {
    type Payload = EscalateTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "EscalateTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn snapshot() -> Option<&'static str> {
        Some(TicketSnapshot::NAME)
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        Self::decide_with(
            payload,
            &TicketFacts::of(matching_events, &payload.ticket_id),
        )
    }
    fn decide_from_snapshot(
        payload: &Self::Payload,
        snapshot_state_json: &str,
        events_since_snapshot: &[Self::Event],
    ) -> CommandDecision {
        decide_from_ticket_snapshot(
            &payload.ticket_id,
            snapshot_state_json,
            events_since_snapshot,
            |facts| Self::decide_with(payload, facts),
        )
    }
}

impl EscalateTicket {
    fn decide_with(payload: &EscalateTicketPayload, facts: &TicketFacts) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Resolved | TicketStatus::Closed | TicketStatus::Merged) => {
                CommandDecision::Rejected {
                    reason: format!(
                        "ticket {} is already handled - nothing to escalate",
                        payload.ticket_id
                    ),
                    kind: "ticket_not_unhandled".into(),
                }
            }
            Some(
                TicketStatus::Open | TicketStatus::InProgress | TicketStatus::WaitingOnCustomer,
            ) => {
                let already_escalated = facts.escalated;
                if already_escalated {
                    return CommandDecision::Rejected {
                        reason: format!("ticket {} has already been escalated", payload.ticket_id),
                        kind: "already_escalated".into(),
                    };
                }
                let previous_priority = facts
                    .created_priority
                    .expect("a ticket with any status has a TicketCreated in its own history");
                let new_priority = escalate_priority(previous_priority);
                CommandDecision::Accepted {
                    events: vec![EventSpec {
                        event_type: "TicketEscalated".into(),
                        payload: serde_json::json!({
                            "ticket_id": payload.ticket_id,
                            "company_id": facts.company_id(),
                            "previous_priority": previous_priority,
                            "new_priority": new_priority,
                        }),
                    }],
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MergeTicketsPayload {
    pub primary_ticket_id: String,
    pub duplicate_ticket_id: String,
}

pub struct MergeTickets;

/// Not in the original spec - the showcase of skilj's own DCB model this
/// crate hadn't yet demonstrated: `tag_mappings` below declares *two*
/// `"ticket"` tags (one per payload field), so `matching_events` is the
/// union of both tickets' own histories - no classic aggregate boundary,
/// no two-phase commit, one ordinary `decide()` reasoning about two
/// entities' consistency at once. See `TicketsMerged`'s own doc comment
/// for the event side of the same trick.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for MergeTickets {
    type Payload = MergeTicketsPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "MergeTickets";
    fn tag_mappings() -> Vec<TagMapping> {
        vec![
            TagMapping {
                key: "ticket".into(),
                field: "primary_ticket_id".into(),
            },
            TagMapping {
                key: "ticket".into(),
                field: "duplicate_ticket_id".into(),
            },
        ]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        if payload.primary_ticket_id == payload.duplicate_ticket_id {
            return CommandDecision::Rejected {
                reason: "a ticket cannot be merged into itself".into(),
                kind: "cannot_merge_ticket_into_itself".into(),
            };
        }
        let primary = TicketFacts::of(matching_events, &payload.primary_ticket_id);
        let duplicate = TicketFacts::of(matching_events, &payload.duplicate_ticket_id);
        match (primary.status, duplicate.status) {
            (None, _) => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.primary_ticket_id),
                kind: "primary_ticket_not_found".into(),
            },
            (_, None) => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.duplicate_ticket_id),
                kind: "duplicate_ticket_not_found".into(),
            },
            (Some(TicketStatus::Closed | TicketStatus::Merged), _) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is already closed or merged - not mergeable",
                    payload.primary_ticket_id
                ),
                kind: "primary_ticket_not_mergeable".into(),
            },
            (_, Some(TicketStatus::Closed | TicketStatus::Merged)) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is already closed or merged - not mergeable",
                    payload.duplicate_ticket_id
                ),
                kind: "duplicate_ticket_not_mergeable".into(),
            },
            (Some(_), Some(_)) => {
                let primary_company = primary.company_id();
                if primary_company != duplicate.company_id() {
                    return CommandDecision::Rejected {
                        reason: "the two tickets belong to different companies".into(),
                        kind: "tickets_belong_to_different_companies".into(),
                    };
                }
                CommandDecision::Accepted {
                    events: vec![EventSpec {
                        event_type: "TicketsMerged".into(),
                        payload: serde_json::json!({
                            "primary_ticket_id": payload.primary_ticket_id,
                            "duplicate_ticket_id": payload.duplicate_ticket_id,
                            "company_id": primary_company,
                        }),
                    }],
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RateTicketPayload {
    pub ticket_id: String,
    pub rating: u8,
    /// Encrypted under the requester's key, here and in the event.
    pub comment: Option<String>,
    /// The ticket's requester - required, see
    /// `RequestInfoFromCustomerPayload::requester_id`.
    pub requester_id: Option<String>,
}

pub struct RateTicket;

/// Not in the original spec - a CSAT survey response (see `TicketRated`'s
/// own doc comment). Requires `resolved` *or* `closed`, not just
/// `closed`: real tools survey right after resolution, not after
/// `config.auto_close_after`'s own multi-day wait.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RateTicket {
    type Payload = RateTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "RateTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["comment"])
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn snapshot() -> Option<&'static str> {
        Some(TicketSnapshot::NAME)
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        Self::decide_with(
            payload,
            &TicketFacts::of(matching_events, &payload.ticket_id),
        )
    }
    fn decide_from_snapshot(
        payload: &Self::Payload,
        snapshot_state_json: &str,
        events_since_snapshot: &[Self::Event],
    ) -> CommandDecision {
        decide_from_ticket_snapshot(
            &payload.ticket_id,
            snapshot_state_json,
            events_since_snapshot,
            |facts| Self::decide_with(payload, facts),
        )
    }
}

impl RateTicket {
    fn decide_with(payload: &RateTicketPayload, facts: &TicketFacts) -> CommandDecision {
        let status = facts.status;
        if status.is_some() {
            if let Some(rejected) = facts.reject_unless_requester(payload.requester_id.as_deref()) {
                return rejected;
            }
        }
        match status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Resolved | TicketStatus::Closed) => {
                if !(1..=5).contains(&payload.rating) {
                    return CommandDecision::Rejected {
                        reason: format!("rating {} is not between 1 and 5", payload.rating),
                        kind: "invalid_rating".into(),
                    };
                }
                let already_rated = facts.rated;
                if already_rated {
                    return CommandDecision::Rejected {
                        reason: format!("ticket {} has already been rated", payload.ticket_id),
                        kind: "already_rated".into(),
                    };
                }
                CommandDecision::Accepted {
                    events: vec![EventSpec {
                        event_type: "TicketRated".into(),
                        payload: serde_json::json!({
                            "ticket_id": payload.ticket_id,
                            "company_id": facts.company_id(),
                            "rating": payload.rating,
                            "comment": payload.comment,
                            "requester_id": payload.requester_id,
                        }),
                    }],
                }
            }
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not resolved or closed - nothing to rate yet",
                    payload.ticket_id
                ),
                kind: "ticket_not_ratable".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddInternalNotePayload {
    pub ticket_id: String,
    pub staff_id: String,
    pub note: String,
}

pub struct AddInternalNote;

/// Not in the original spec - a staff-only note (see
/// `TicketInternalNoteAdded`'s own doc comment for why it's kept out of
/// the customer-facing projections below). Allowed at any ticket status,
/// including after close - a real audit trail doesn't stop just because
/// the ticket did.
///
/// `private_fields()` below is one of two places `TEAM_ONLY =
/// Some(STAFF_TEAM)` on `TicketInternalNotes` (see that projection's
/// own doc comment) doesn't reach - the other is `TicketInternalNoteAdded`'s
/// own identical `private_fields()`, the event this command's own
/// `decide()` emits. Both exist because skilj's generic
/// `CommandQuery`/`EventQuery` (`fetchCommands`/`queryEvents`/
/// `countEvents`/`inspectEvent` - GraphQL, `AdminAccess`-gated,
/// skilj-inspector/skilj-tui's own read path, not anything this crate
/// builds itself) can show any command or event's raw payload to a
/// superadmin-mapped Role regardless of `TEAM_ONLY`, which only gates
/// `ProjectionQuery` - confirmed by tracing `query_events`/
/// `count_events`/`inspect_event` in the sibling `skilj` repo: none of
/// them check `event_read_allowed` (only the REST feed's
/// `fetch_events`/`consume_events` do), so `TicketInternalNoteAdded`
/// having no `event_read_allowed() = true` override blocks the REST
/// path but not this one - missed in the first pass, caught in review.
/// `staff_id`/`note` as `PrivateFieldKind::Team(STAFF_TEAM)` on both
/// closes it, on the same terms - no superadmin bypass, `Role.name`
/// must literally be `STAFF_TEAM` (`docs/architecture.md`'s own
/// private-field writeup in the sibling `skilj` repo). A deliberate
/// choice, not a mechanical default: it means even this project's own
/// platform operators can't read a ticket's internal notes through
/// generic admin tooling without also holding a staff Role - accepted
/// here since "staff-only" is this feature's entire point, not a
/// boundary meant to stop only customers.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for AddInternalNote {
    type Payload = AddInternalNotePayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "AddInternalNote";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn private_fields() -> Vec<PrivateField> {
        vec![
            PrivateField {
                field: "staff_id".into(),
                kind: PrivateFieldKind::Team,
                team: Some(STAFF_TEAM.into()),
                addressee_field: None,
            },
            PrivateField {
                field: "note".into(),
                kind: PrivateFieldKind::Team,
                team: Some(STAFF_TEAM.into()),
                addressee_field: None,
            },
        ]
    }
    fn snapshot() -> Option<&'static str> {
        Some(TicketSnapshot::NAME)
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        Self::decide_with(
            payload,
            &TicketFacts::of(matching_events, &payload.ticket_id),
        )
    }
    fn decide_from_snapshot(
        payload: &Self::Payload,
        snapshot_state_json: &str,
        events_since_snapshot: &[Self::Event],
    ) -> CommandDecision {
        decide_from_ticket_snapshot(
            &payload.ticket_id,
            snapshot_state_json,
            events_since_snapshot,
            |facts| Self::decide_with(payload, facts),
        )
    }
}

impl AddInternalNote {
    fn decide_with(payload: &AddInternalNotePayload, facts: &TicketFacts) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(_) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketInternalNoteAdded".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "staff_id": payload.staff_id,
                        "note": payload.note,
                    }),
                }],
            },
        }
    }
}

// --- projection ---

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct TicketSummaryState {
    pub status: Option<String>,
    /// A plain string, not `Option<TicketPriority>` - found the hard
    /// way, over a real GraphQL request (`tests/graphql.rs`): a
    /// `schemars`-derived enum's JSON Schema shape doesn't match what
    /// `skilj-graphql`'s mapper recognises as a scalar, so it falls
    /// back to its own documented behaviour (docs/architecture.md
    /// §5.1) - an opaque, *double*-JSON-encoded string
    /// (`"\"urgent\""`, not `"urgent"`). Not a bug to work around at
    /// the GraphQL layer; a plain string field here is simply the
    /// right shape for a read-model a GraphQL client will actually
    /// query.
    pub priority: Option<String>,
    pub assigned_staff_id: Option<String>,
    /// Set once by `TicketEscalated` (see that event's own doc comment),
    /// never cleared - same "once escalated, stays escalated" treatment
    /// `EscalateTicket`'s own `already_escalated` guard already gives it.
    pub escalated: bool,
    /// Set once by `TicketRated` - unlike `TicketInternalNoteAdded`
    /// (deliberately kept out of every projection, see that event's own
    /// doc comment), a CSAT rating has no customer-visibility concern:
    /// the customer who left it, and any staff member, both already see
    /// this fine either way, so folding it in here is a real UX need
    /// (the frontend needs to know a ticket's already been rated so it
    /// doesn't keep showing the rating form), not scope creep.
    pub rating: Option<u8>,
    /// The staff member whose `TicketInfoRequested` was the first reply
    /// the customer got - set once, by the first one, and never moved by
    /// a later round of the back-and-forth. Who answered first is not
    /// when: `project` sees an event's payload, not when it was stored,
    /// so a first-response *time* has nothing here to be derived from.
    ///
    /// Added after tickets already existed, which is what a rebuild is
    /// for (README's "Changing a projection: zero-downtime rebuilds"):
    /// until one is run, a ticket answered before the deploy stays at
    /// `None`, or names whoever answered *after* it instead of first.
    /// `tests/projection_rebuild.rs` walks both through to the switch-over.
    pub first_responder_staff_id: Option<String>,
}

fn priority_str(priority: TicketPriority) -> &'static str {
    match priority {
        TicketPriority::Low => "low",
        TicketPriority::Medium => "medium",
        TicketPriority::High => "high",
        TicketPriority::Urgent => "urgent",
    }
}

/// Keyed by `ticket_id`. `specs/skilj-helpdesk.allium`'s `unhandled`
/// derived field is `status not in {resolved, closed}` - not stored
/// here directly; a caller derives it from `status`.
pub struct TicketSummary;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for TicketSummary {
    type State = TicketSummaryState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "TicketSummary";
    /// See `TicketInternalNotes`'s own doc comment for the vulnerability
    /// this closes and what it doesn't - `TicketCreated`'s own "company"
    /// tag (already there for `CreateTicket`'s own consistency check) is
    /// what an instance's owner derives from here; every other event
    /// this projection consumes only tags "ticket", so an owner once
    /// established at creation is never touched again by anything else,
    /// exactly the "an event lacking the tag leaves it untouched"
    /// behaviour `docs/architecture.md` §23 (in the sibling `skilj`
    /// repo) describes.
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    fn consumed_event_types() -> Vec<&'static str> {
        vec![
            "TicketCreated",
            "TicketAssigned",
            "TicketResolved",
            "TicketReopened",
            "TicketInfoRequested",
            "TicketCustomerResponded",
            "TicketClosed",
            "TicketEscalated",
            "TicketsMerged",
            "TicketRated",
        ]
    }
    fn sync() -> bool {
        true
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        match event {
            HelpdeskEvent::CompanySignedUp(_)
            | HelpdeskEvent::CompanyActivated(_)
            | HelpdeskEvent::CompanyExpired(_)
            | HelpdeskEvent::CompanyTenantProvisioned(_)
            // Deliberately absent from `consumed_event_types()` above
            // (see that event's own doc comment) - listed here only
            // because `keys`/`project` take `&HelpdeskEvent`
            // unconditionally, so the match has to stay exhaustive over
            // every variant even ones this projection never actually
            // gets invoked for.
            | HelpdeskEvent::CompanyLifecycleMirrored(_)
            | HelpdeskEvent::TicketInternalNoteAdded(_) => vec![],
            HelpdeskEvent::TicketCreated(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketAssigned(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketResolved(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketReopened(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketInfoRequested(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketCustomerResponded(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketClosed(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketEscalated(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketRated(p) => vec![p.ticket_id.clone()],
            // Fans out to *both* tickets - unlike every other event here,
            // one `TicketsMerged` updates two projection instances. See
            // `project`'s own handling of the two keys below.
            HelpdeskEvent::TicketsMerged(p) => {
                vec![p.primary_ticket_id.clone(), p.duplicate_ticket_id.clone()]
            }
        }
    }
    fn project(state: &mut Self::State, event: &Self::Event, key: &str) {
        match event {
            HelpdeskEvent::CompanySignedUp(_)
            | HelpdeskEvent::CompanyActivated(_)
            | HelpdeskEvent::CompanyExpired(_)
            | HelpdeskEvent::CompanyTenantProvisioned(_)
            // A lifecycle mirror, not a ticket fact: this projection is
            // keyed by `ticket_id` and a mirror carries no ticket at all,
            // so there is nothing here for it to contribute. Listed
            // alongside the other company-lifecycle events above for the
            // same reason they are - exhaustiveness, not intent.
            | HelpdeskEvent::CompanyLifecycleMirrored(_)
            | HelpdeskEvent::TicketInternalNoteAdded(_) => {}
            HelpdeskEvent::TicketCreated(p) => {
                state.status = Some("open".into());
                state.priority = Some(priority_str(p.priority).to_string());
            }
            HelpdeskEvent::TicketAssigned(p) => {
                state.status = Some("in_progress".into());
                state.assigned_staff_id = Some(p.staff_id.clone());
            }
            HelpdeskEvent::TicketResolved(_) => {
                state.status = Some("resolved".into());
            }
            HelpdeskEvent::TicketReopened(_) => {
                state.status = Some("in_progress".into());
            }
            HelpdeskEvent::TicketInfoRequested(p) => {
                state.status = Some("waiting_on_customer".into());
                state
                    .first_responder_staff_id
                    .get_or_insert_with(|| p.staff_id.clone());
            }
            HelpdeskEvent::TicketCustomerResponded(_) => {
                state.status = Some("in_progress".into());
            }
            HelpdeskEvent::TicketClosed(_) => {
                state.status = Some("closed".into());
            }
            HelpdeskEvent::TicketEscalated(p) => {
                state.priority = Some(priority_str(p.new_priority).to_string());
                state.escalated = true;
            }
            HelpdeskEvent::TicketRated(p) => {
                state.rating = Some(p.rating);
            }
            // Only the duplicate's own instance (this projection is
            // keyed per-ticket, so `key` tells the two fanned-out calls
            // apart - see `keys` above) becomes "merged"; the primary's
            // own instance is untouched, matching `TicketFacts`'s own
            // treatment in this file's command-decision helpers.
            HelpdeskEvent::TicketsMerged(p) => {
                if key == p.duplicate_ticket_id {
                    state.status = Some("merged".into());
                }
            }
        }
    }
}

// --- the frontend's two ticket reads ---

/// One turn of the `StaffRequestsInfo`/`CustomerReplies` back-and-forth
/// (`rule StaffRequestsInfo`/`CustomerReplies` in the spec) - the actual
/// conversation those two rules previously carried no content for.
/// Nothing stops the cycle repeating (assign → ask → reply → ask again),
/// so this accumulates across as many rounds as actually happen, not
/// just one.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketMessage {
    pub author_id: String,
    pub from_staff: bool,
    pub text: String,
}

/// A ticket's lifecycle, with nothing its customer wrote - see
/// `CompanyActiveTickets`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TicketQueueEntry {
    pub ticket_id: String,
    pub status: String,
    pub priority: String,
    pub requester_id: String,
    pub assigned_staff_id: Option<String>,
    /// See `TicketSummaryState::escalated`/`::rating`'s own doc comments -
    /// identical reasoning, mirrored here since `frontend/` reads this
    /// projection, not `TicketSummary`.
    pub escalated: bool,
    pub rating: Option<u8>,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct CompanyActiveTicketsState {
    pub tickets: std::collections::HashMap<String, TicketQueueEntry>,
}

/// Keyed by `company_id`: every ticket of one company that can still
/// change, for the staff queue and for the customer view's status column.
///
/// A ticket leaves once nothing can happen to it any more: a merged
/// duplicate right away, a closed ticket once it carries a rating
/// (`CustomerRatesTicket` still accepts a closed, unrated ticket, and
/// this is the read the customer rates from). Without that, one row held
/// every ticket a company ever had, and skilj rewrites the whole row on
/// every fold - see `docs/partitioned-projection-report-2026-10-05.md`.
/// A closed ticket nobody rates still stays.
///
/// Holds no customer-written text, on purpose. That text is encrypted
/// under each customer's own key (`customer_fields`), and skilj decrypts
/// a projection row only against the row's own key - a company here - so
/// any of it folded in would read as ciphertext to everyone. It lives in
/// `CustomerTickets` instead; the frontend joins the two by `ticket_id`.
///
/// Replaces `CompanyTicketList`, which held that text in plaintext. A
/// registered projection can't lose a field (skilj's schema compatibility
/// rule), so this is a new projection rather than a slimmed-down old one;
/// the old one's stored state is no longer registered or updated. It was
/// `CompanyTicketQueue` before tickets started leaving it, renamed rather
/// than kept so a fresh fold from history drops the finished tickets
/// already stored - the schema didn't change, so skilj would not have
/// rebuilt it on its own.
///
/// What a customer still sees through this: the ids, statuses and
/// requester ids of their company's other tickets, never their content.
///
/// Async, unlike every other projection here (issue #15): it's the one
/// read whose key (a company) every ticket event of that company
/// contends on - the obvious candidate for `PARTITION_COUNT`. The cost
/// is read-your-writes: a ticket just created can be missing from this
/// queue for up to one `async_projection_poll_interval`, which is why
/// the frontend's dashboard reads it with `waitForSequence` set to its
/// own last write's sequence (`AfterWrite` in `dashboard.rs`). Issue #12
/// measured the trade: ~7% more accepted commands/s at saturation, for
/// a median lag of ~20 events - see
/// `docs/async-projection-report-2026-10-06.md`.
pub struct CompanyActiveTickets;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for CompanyActiveTickets {
    type State = CompanyActiveTicketsState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CompanyActiveTickets";
    /// See `TicketSummary`'s own doc comment - identical fix, identical
    /// reasoning. Keyed by `company_id` itself here (unlike
    /// `TicketSummary`'s `ticket_id`), so the derived owner ends up
    /// equal to the key for every instance - a degenerate but correct
    /// case of the same general mechanism, not special-cased.
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    fn consumed_event_types() -> Vec<&'static str> {
        vec![
            "TicketCreated",
            "TicketAssigned",
            "TicketResolved",
            "TicketReopened",
            "TicketInfoRequested",
            "TicketCustomerResponded",
            "TicketClosed",
            "TicketEscalated",
            "TicketsMerged",
            "TicketRated",
        ]
    }
    /// Lets up to this many `server` instances sharing one database
    /// fold disjoint slices of the queue concurrently, instead of each
    /// redoing all of it. Only helps with more than one instance, and
    /// only up to the number of companies in a context: keys are
    /// companies, so a per-company tenant context (one key) always
    /// lands in a single partition. Rebuilds stay single-instance
    /// regardless. See `docs/partitioned-projection-report-2026-10-05.md`.
    const PARTITION_COUNT: u32 = 4;
    fn keys(event: &Self::Event) -> Vec<String> {
        match event {
            HelpdeskEvent::CompanySignedUp(_)
            | HelpdeskEvent::CompanyActivated(_)
            | HelpdeskEvent::CompanyExpired(_)
            | HelpdeskEvent::CompanyTenantProvisioned(_)
            // Deliberately absent from `consumed_event_types()` above -
            // see that event's own doc comment, and `TicketSummary::keys`'s
            // own identical comment for why the match still needs this
            // arm regardless.
            //
            // Unlike `TicketSummary::keys`, this projection *could*
            // return a meaningful key here (a mirror carries the
            // `company_id` this projection is keyed by). It still must
            // not: `keys` only decides which instances to touch, and
            // `consumed_event_types()` is what decides whether `project`
            // runs at all - returning a key for an unconsumed event
            // would fold a lifecycle fact into a ticket queue under the
            // "an event lacking the tag leaves it untouched" contract
            // this projection otherwise keeps.
            | HelpdeskEvent::CompanyLifecycleMirrored(_)
            | HelpdeskEvent::TicketInternalNoteAdded(_) => vec![],
            HelpdeskEvent::TicketCreated(p) => vec![p.company_id.clone()],
            // Every ticket-lifecycle event past creation now carries its
            // own `company_id` too (stamped by each command's own
            // `decide()` via `TicketFacts::company_id` - see that
            // function's doc comment) precisely so this projection's
            // instance key is always the real company, not a stand-in.
            HelpdeskEvent::TicketAssigned(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketResolved(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketReopened(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketInfoRequested(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketCustomerResponded(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketClosed(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketEscalated(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketRated(p) => vec![p.company_id.clone()],
            // Unlike `TicketSummary` (keyed per-ticket, so it fans this
            // out to two instances), this projection is keyed per
            // *company* - both tickets already belong to the same one
            // (`MergeTickets`'s own `tickets_belong_to_different_companies`
            // guard), so one key here is correct; `project` below reaches
            // into `state.tickets` for the specific duplicate ticket_id.
            HelpdeskEvent::TicketsMerged(p) => vec![p.company_id.clone()],
        }
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        let mut set_status = |ticket_id: &str, status: &str| {
            if let Some(entry) = state.tickets.get_mut(ticket_id) {
                entry.status = status.into();
            }
        };
        match event {
            HelpdeskEvent::CompanySignedUp(_)
            | HelpdeskEvent::CompanyActivated(_)
            | HelpdeskEvent::CompanyExpired(_)
            | HelpdeskEvent::CompanyTenantProvisioned(_)
            // A lifecycle mirror carries no ticket - see
            // `CompanyActiveTickets::keys`'s own comment on the same event.
            | HelpdeskEvent::CompanyLifecycleMirrored(_)
            | HelpdeskEvent::TicketInternalNoteAdded(_) => {}
            HelpdeskEvent::TicketCreated(p) => {
                state.tickets.insert(
                    p.ticket_id.clone(),
                    TicketQueueEntry {
                        ticket_id: p.ticket_id.clone(),
                        status: "open".into(),
                        priority: priority_str(p.priority).to_string(),
                        requester_id: p.requester_id.clone(),
                        assigned_staff_id: None,
                        escalated: false,
                        rating: None,
                    },
                );
            }
            HelpdeskEvent::TicketAssigned(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.status = "in_progress".into();
                    entry.assigned_staff_id = Some(p.staff_id.clone());
                }
            }
            HelpdeskEvent::TicketResolved(p) => set_status(&p.ticket_id, "resolved"),
            HelpdeskEvent::TicketReopened(p) => set_status(&p.ticket_id, "in_progress"),
            HelpdeskEvent::TicketInfoRequested(p) => {
                set_status(&p.ticket_id, "waiting_on_customer")
            }
            HelpdeskEvent::TicketCustomerResponded(p) => set_status(&p.ticket_id, "in_progress"),
            HelpdeskEvent::TicketClosed(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    if entry.rating.is_some() {
                        state.tickets.remove(&p.ticket_id);
                    } else {
                        entry.status = "closed".into();
                    }
                }
            }
            HelpdeskEvent::TicketsMerged(p) => {
                state.tickets.remove(&p.duplicate_ticket_id);
            }
            HelpdeskEvent::TicketEscalated(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.priority = priority_str(p.new_priority).to_string();
                    entry.escalated = true;
                }
            }
            HelpdeskEvent::TicketRated(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    if entry.status == "closed" {
                        state.tickets.remove(&p.ticket_id);
                    } else {
                        entry.rating = Some(p.rating);
                    }
                }
            }
        }
    }
}

/// What one customer wrote, or was asked, on one ticket - see
/// `CustomerTickets`. Every string here is held encrypted under that
/// customer's key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CustomerTicketContent {
    pub ticket_id: String,
    pub title: String,
    pub description: String,
    pub messages: Vec<TicketMessage>,
    pub rating_comment: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct CustomerTicketsState {
    pub tickets: std::collections::HashMap<String, CustomerTicketContent>,
}

/// Keyed by `requester_id`: the content of every ticket one customer
/// filed - title, description, the conversation and their rating comment.
///
/// Keyed by the customer because that's what makes it readable. Each
/// field is folded in as the ciphertext it's stored as (`customer_fields`),
/// and skilj decrypts a projection row only when the caller may read the
/// row's own key's data: with `can_read_sensitive` (staff, see
/// `server.rs`), or as the subject itself, which the customer is - their
/// IdP subject is their `requester_id`. Another customer of the same
/// company can read the row (`OWNER_TAG_KEY` scopes it to the company, no
/// further) but only ever sees ciphertext. And once the customer is
/// forgotten (`CUSTOMER_SUBJECT`), nobody can decrypt it any more.
///
/// Only the four events that carry customer text are consumed; status and
/// the rest come from `CompanyActiveTickets`. Events stored before
/// `TicketInfoRequested`/`TicketRated` carried a `requester_id` are
/// skipped - their plaintext isn't erasable either way.
pub struct CustomerTickets;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for CustomerTickets {
    type State = CustomerTicketsState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CustomerTickets";
    /// `TicketCreated`'s "company" tag sets the owner, as for
    /// `TicketSummary`. A customer belongs to one company
    /// (`specs/skilj-helpdesk.allium`'s `entity Customer`), so their row
    /// has one owner.
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    fn consumed_event_types() -> Vec<&'static str> {
        vec![
            "TicketCreated",
            "TicketInfoRequested",
            "TicketCustomerResponded",
            "TicketRated",
        ]
    }
    fn sync() -> bool {
        true
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        match event {
            HelpdeskEvent::TicketCreated(p) => vec![p.requester_id.clone()],
            HelpdeskEvent::TicketCustomerResponded(p) => vec![p.requester_id.clone()],
            HelpdeskEvent::TicketInfoRequested(p) => p.requester_id.iter().cloned().collect(),
            HelpdeskEvent::TicketRated(p) => p.requester_id.iter().cloned().collect(),
            // Not consumed - see `TicketSummary::keys` on why the match
            // still has to name them.
            HelpdeskEvent::CompanySignedUp(_)
            | HelpdeskEvent::CompanyActivated(_)
            | HelpdeskEvent::CompanyExpired(_)
            | HelpdeskEvent::CompanyTenantProvisioned(_)
            | HelpdeskEvent::CompanyLifecycleMirrored(_)
            | HelpdeskEvent::TicketAssigned(_)
            | HelpdeskEvent::TicketResolved(_)
            | HelpdeskEvent::TicketReopened(_)
            | HelpdeskEvent::TicketClosed(_)
            | HelpdeskEvent::TicketEscalated(_)
            | HelpdeskEvent::TicketsMerged(_)
            | HelpdeskEvent::TicketInternalNoteAdded(_) => vec![],
        }
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        match event {
            HelpdeskEvent::TicketCreated(p) => {
                state.tickets.insert(
                    p.ticket_id.clone(),
                    CustomerTicketContent {
                        ticket_id: p.ticket_id.clone(),
                        title: p.title.clone(),
                        description: p.description.clone(),
                        messages: Vec::new(),
                        rating_comment: None,
                    },
                );
            }
            HelpdeskEvent::TicketInfoRequested(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.messages.push(TicketMessage {
                        author_id: p.staff_id.clone(),
                        from_staff: true,
                        text: p.message.clone(),
                    });
                }
            }
            HelpdeskEvent::TicketCustomerResponded(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.messages.push(TicketMessage {
                        author_id: p.requester_id.clone(),
                        from_staff: false,
                        text: p.message.clone(),
                    });
                }
            }
            HelpdeskEvent::TicketRated(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.rating_comment = p.comment.clone();
                }
            }
            // Not consumed - see `keys`.
            _ => {}
        }
    }
}

// --- internal notes, staff-only by access control (TEAM_ONLY below) ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketInternalNote {
    pub staff_id: String,
    pub note: String,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct TicketInternalNotesState {
    pub notes: Vec<TicketInternalNote>,
}

/// Keyed by `ticket_id`. A *separate* projection from `TicketSummary`/
/// `CompanyActiveTickets`/`CustomerTickets`, on purpose: those are what
/// `frontend/` fetches eagerly to render a ticket at all, for customers
/// too, so keeping `TicketInternalNoteAdded` out of them is what keeps
/// it from customers (see that event's own doc comment). This projection exists
/// purely so staff have something to fetch on demand (`frontend/`'s own
/// "Notes" toggle, a second, separate query - not folded into the
/// eager one).
///
/// **Both the cross-company and the same-company staff-vs-customer gaps
/// are now closed.** A security review found this projection (and
/// `TicketSummary`/`CompanyTicketList`, since replaced by
/// `CompanyActiveTickets`/`CustomerTickets`) readable by any Role with *any*
/// mapping on the bounded context, regardless of which company the
/// queried key actually belonged to - `skilj-graphql`'s
/// `require_read_mapping` checked only that. skilj's own fix
/// (`docs/architecture.md` §23 in the sibling `skilj` repo) added
/// `OWNER_TAG_KEY`/`RoleAccessMapping.scope`, adopted here (this
/// projection's `OWNER_TAG_KEY` below, `TicketInternalNoteAdded`'s own
/// "company" tag, and `server.rs`'s demo customer Role scoped to its
/// own company) - closing the cross-company half for all three
/// projections, `tests/cross_company_projection_scoping.rs` proves it
/// live.
///
/// `OWNER_TAG_KEY` alone left a second, different-axis gap open: a
/// customer scoped to their *own* company could still read this
/// projection for their own tickets, seeing staff-only notes the
/// feature was built to keep from them regardless of company - a role
/// dimension (staff or not), not a tenancy one, which `scope` was never
/// built to express. skilj 0.0.4 closes exactly that with
/// `Projection::TEAM_ONLY` (`docs/architecture.md` §31-32 in the
/// sibling `skilj` repo, Codeberg issue #17): a whole-projection gate,
/// independent of and composed with `OWNER_TAG_KEY` rather than a
/// refinement of it - a query must satisfy both, each failing on its
/// own terms (`GrantScopeMismatch` vs. `NotOnRequiredTeam`). Adopted
/// here as `TEAM_ONLY = Some(STAFF_TEAM)` below, matched against
/// `server.rs`'s staff Role(s), whose `Role.name` is literally
/// `STAFF_TEAM` for exactly this reason (no separate "team" field
/// exists on `Role` - `name` doubles as the team identifier `TEAM_ONLY`
/// compares against). `STAFF_TEAM` (this file's own top-level const) is
/// the one place that string lives - `AddInternalNote`/
/// `TicketInternalNoteAdded`'s own `private_fields()` and `server.rs`'s
/// seeding both reference it rather than repeating the literal, found
/// worth doing in review after the two independent gates almost drifted
/// apart under separate literals.
/// No superadmin bypass, deliberately - see `Projection::TEAM_ONLY`'s
/// own doc comment. `tests/cross_company_projection_scoping.rs` now
/// proves this half live too: a company-A customer reading company A's
/// *own* internal notes is rejected, not just company B's.
pub struct TicketInternalNotes;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for TicketInternalNotes {
    type State = TicketInternalNotesState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "TicketInternalNotes";
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    const TEAM_ONLY: Option<&'static str> = Some(STAFF_TEAM);
    fn consumed_event_types() -> Vec<&'static str> {
        vec!["TicketInternalNoteAdded"]
    }
    fn sync() -> bool {
        true
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        match event {
            HelpdeskEvent::TicketInternalNoteAdded(p) => vec![p.ticket_id.clone()],
            _ => vec![],
        }
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        if let HelpdeskEvent::TicketInternalNoteAdded(p) = event {
            state.notes.push(TicketInternalNote {
                staff_id: p.staff_id.clone(),
                note: p.note.clone(),
            });
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TenantDirectoryState {
    /// The tenant bounded context provisioned for this company, if
    /// `RecordCompanyTenant` has committed one. `None` covers both
    /// "hasn't signed up yet" and "signed up, provisioner hasn't reacted
    /// yet" - the same conflation `company_tenant`'s own `Option` makes
    /// on the event-history side, kept here so the two agree on what
    /// absence means.
    pub tenant_name: Option<String>,
}

/// Keyed by `company_id`.
pub struct TenantDirectory;

/// `company_id -> tenant_name`, as a queryable read model - the lookup
/// every piece of per-tenant routing needs, and the reason this pass
/// doesn't need a bespoke "resolve tenant" query bolted onto the API
/// layer.
///
/// **Why a Projection and not a `company_tenant`-style history fold.**
/// `company_tenant` above reads the answer off a command's
/// `matching_events`, which is the right shape for a `decide()` guard
/// but the wrong shape for a router: routing happens *before* any
/// command is dispatched, so there is no `matching_events` to read. A
/// caller would otherwise have to page the shared context's own event
/// history itself, re-implementing tag filtering and sequence ordering
/// that skilj already does correctly. This projection is that same
/// answer, maintained incrementally and reachable through the existing
/// `projection(boundedContext, name, key)` GraphQL query.
///
/// **`sync()`, deliberately.** Every other projection in this file is
/// `sync()` and so is this one, which for this particular projection is
/// a correctness requirement rather than a latency preference: a router
/// that reads a `stale` (or absent) entry for a company whose tenant was
/// provisioned seconds ago would route that company's first real command
/// to the shared context while a later one went to the tenant - splitting
/// one company's ticket history across two bounded contexts with nothing
/// recording that it happened. `sync()` bounds that window to "not yet
/// committed", which `RecordTenantLifecycle`'s own guard downstream is
/// built to tolerate. Async would buy nothing back either (issue #12): it
/// folds only `CompanyTenantProvisioned`, once per company, so it adds
/// nothing to an ordinary command's commit.
///
/// **Not an access-control boundary.** It carries no `OWNER_TAG_KEY`,
/// deliberately: this is an infrastructure mapping, and unlike
/// `CompanyActiveTickets`/`TicketSummary` there is no company data in it to
/// protect - it says which bounded context holds a company, not anything
/// about the company. It is readable by anyone with an Admin mapping on
/// the shared `helpdesk` context, which is the same audience that can
/// already call `listBoundedContexts` and mint tokens against any
/// context (`createCommandToken`'s own resolver requires exactly that
/// mapping), so gating it more tightly would add a check without closing
/// a real path.
#[auto_register(BOUNDED_CONTEXT)]
impl Projection for TenantDirectory {
    type State = TenantDirectoryState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "TenantDirectory";
    fn consumed_event_types() -> Vec<&'static str> {
        vec!["CompanyTenantProvisioned"]
    }
    fn sync() -> bool {
        true
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        match event {
            HelpdeskEvent::CompanyTenantProvisioned(p) => vec![p.company_id.clone()],
            _ => vec![],
        }
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        if let HelpdeskEvent::CompanyTenantProvisioned(p) = event {
            state.tenant_name = Some(p.tenant_name.clone());
        }
    }
}

#[cfg(test)]
mod sign_up_validation_tests {
    use super::*;

    fn decide(company_id: &str, name: &str, contact_email: &str) -> CommandDecision {
        SignUpCompany::decide(
            &SignUpCompanyPayload {
                company_id: company_id.into(),
                name: name.into(),
                contact_email: contact_email.into(),
            },
            &[],
        )
    }

    fn rejection_kind(decision: CommandDecision) -> Option<String> {
        match decision {
            CommandDecision::Rejected { kind, .. } => Some(kind),
            _ => None,
        }
    }

    #[test]
    fn a_well_formed_sign_up_is_not_rejected() {
        assert_eq!(
            rejection_kind(decide("acme", "Acme", "a@acme.example")),
            None
        );
    }

    #[test]
    fn malformed_contact_emails_are_rejected() {
        for email in [
            "",
            "acme.example",
            "@acme.example",
            "a@",
            "a@localhost",
            "a@b@acme.example",
            "a@.acme",
            "a@acme.",
            "a b@acme.example",
        ] {
            assert_eq!(
                rejection_kind(decide("acme", "Acme", email)).as_deref(),
                Some("invalid_email"),
                "{email:?} should be rejected"
            );
        }
    }

    #[test]
    fn empty_ids_and_names_are_rejected() {
        assert_eq!(
            rejection_kind(decide("", "Acme", "a@acme.example")).as_deref(),
            Some("empty_company_id")
        );
        assert_eq!(
            rejection_kind(decide("acme", "  ", "a@acme.example")).as_deref(),
            Some("empty_company_name")
        );
    }
}

#[cfg(test)]
mod required_role_tests {
    use super::*;

    /// `#[requires_role(...)]` only takes a string literal, so the seven
    /// staff-only commands spell out `"staff"` rather than `STAFF_TEAM`.
    /// This pins them to the constant, and pins the customer-facing and
    /// background-only commands to no role gate at all.
    #[test]
    fn staff_only_commands_require_the_staff_role() {
        let staff_only = [
            AssignTicket::required_role(),
            ResolveTicket::required_role(),
            RequestInfoFromCustomer::required_role(),
            CloseTicket::required_role(),
            EscalateTicket::required_role(),
            MergeTickets::required_role(),
            AddInternalNote::required_role(),
        ];
        for required in staff_only {
            assert_eq!(required, Some(STAFF_TEAM));
        }

        let ungated = [
            CreateTicket::required_role(),
            ReopenTicket::required_role(),
            CustomerRespondsToTicket::required_role(),
            RateTicket::required_role(),
            SignUpCompany::required_role(),
        ];
        for required in ungated {
            assert_eq!(required, None);
        }
    }
}
