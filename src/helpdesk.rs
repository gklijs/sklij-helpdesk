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
//! real production side effect of every signup. What's still deferred:
//! the new tenant is provisioned and recorded, but Ticket commands
//!/queries for that company still run against this shared context, not
//! the tenant just created for it - routing Ticket traffic into
//! per-company tenants needs Company's own guard reads
//! (`company_status` below, read by `CreateTicket` et al. via a single
//! same-context DCB query) to work across two bounded contexts instead
//! of one, which is real cross-context query work, not a follow-up
//! detail - see `RecordCompanyTenant`'s own doc comment.
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
//! deliberately kept out of `CompanyTicketList`/`TicketSummary` below -
//! that projection's own doc comment claims "nothing here is actually
//! customer-only data," which is only true because an internal note
//! never enters it; folding one in and relying on the frontend to filter
//! it back out (not touched this pass - no wasm toolchain to verify
//! against in this sandbox) would make that claim false with no
//! server-side enforcement behind it.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skilj::{auto_register, CommandType, EventType, Projection, ScheduleDeadline};
use skilj_core::event_store::Event;
use skilj_core::plugin::{BoundedContextEvent, DeadlinePollStartFrom, DeadlineSpec};
use skilj_core::shared::{
    CommandDecision, EventSpec, PrivateField, PrivateFieldKind, Tag, TagMapping,
};

use crate::scheduling;

pub const BOUNDED_CONTEXT: &str = "helpdesk";

/// The one team name `TicketInternalNotes`'s own `TEAM_ONLY` and
/// `AddInternalNote`/`TicketInternalNoteAdded`'s own `private_fields()`
/// all compare `Role.name` against (see `TicketInternalNotes`'s own doc
/// comment) - a shared constant rather than five independent string
/// literals (this file, `server.rs`, both test files) so a future edit
/// to one can't silently desync from the others and reopen exactly one
/// of the two gates while the other still looks closed.
pub const STAFF_TEAM: &str = "staff";

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
    pub message: String,
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
    pub comment: Option<String>,
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
/// folded into `CompanyTicketList`/`TicketSummary` (see this file's own
/// module doc comment on why keeping it structurally separate, rather
/// than tagging entries "internal" for the frontend to filter, is what
/// actually keeps `CompanyTicketList`'s own "nothing here is customer-
/// only data" claim true).
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
    /// alone isn't enough here, since `TicketSummary`/`CompanyTicketList`
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
            "TicketInfoRequested" => Some(
                serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketInfoRequested),
            ),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TicketStatus {
    Open,
    InProgress,
    WaitingOnCustomer,
    Resolved,
    Closed,
    Merged,
}

/// Folds this ticket's own status from its slice of `matching_events` -
/// same technique as `banking.rs`'s `balance_of`/`courses.rs`'s roster
/// folds. `None` means the ticket doesn't exist (no `TicketCreated`
/// found).
fn ticket_status(matching_events: &[HelpdeskEvent], ticket_id: &str) -> Option<TicketStatus> {
    let mut status = None;
    for event in matching_events {
        match event {
            HelpdeskEvent::TicketCreated(p) if p.ticket_id == ticket_id => {
                status = Some(TicketStatus::Open);
            }
            HelpdeskEvent::TicketAssigned(p) if p.ticket_id == ticket_id => {
                status = Some(TicketStatus::InProgress);
            }
            HelpdeskEvent::TicketResolved(p) if p.ticket_id == ticket_id => {
                status = Some(TicketStatus::Resolved);
            }
            HelpdeskEvent::TicketReopened(p) if p.ticket_id == ticket_id => {
                status = Some(TicketStatus::InProgress);
            }
            HelpdeskEvent::TicketInfoRequested(p) if p.ticket_id == ticket_id => {
                status = Some(TicketStatus::WaitingOnCustomer);
            }
            HelpdeskEvent::TicketCustomerResponded(p) if p.ticket_id == ticket_id => {
                status = Some(TicketStatus::InProgress);
            }
            HelpdeskEvent::TicketClosed(p) if p.ticket_id == ticket_id => {
                status = Some(TicketStatus::Closed);
            }
            // Only the *duplicate* side becomes Merged - the primary's
            // own status is untouched by a merge (see `MergeTickets`'s
            // own doc comment), so this only ever matches
            // `duplicate_ticket_id`, never `primary_ticket_id`.
            HelpdeskEvent::TicketsMerged(p) if p.duplicate_ticket_id == ticket_id => {
                status = Some(TicketStatus::Merged);
            }
            _ => {}
        }
    }
    status
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompanyStatus {
    Trialing,
    Active,
    Expired,
}

/// Folds this company's own status - same technique as `ticket_status`
/// above. `None` means the company doesn't exist (no `CompanySignedUp`
/// found).
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
            _ => {}
        }
    }
    status
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

/// The company a ticket belongs to, read off its own `TicketCreated`
/// (always present in `matching_events` for any ticket-tagged command:
/// `TicketCreated` carries both the "ticket" and "company" tags - see
/// its own `tag_mappings` doc comment). Every ticket-lifecycle command
/// past creation itself uses this to stamp `company_id` onto the event
/// it emits, which is what lets `CompanyTicketList` below fold every
/// ticket event for one ticket into the correct per-company projection
/// instance - `AssignTicket`/`ResolveTicket`/etc.'s own payloads never
/// carried `company_id` as caller input (there's no reason to trust a
/// caller-supplied one when the real answer is already in the ticket's
/// own history).
fn company_id_for_ticket(matching_events: &[HelpdeskEvent], ticket_id: &str) -> Option<String> {
    matching_events.iter().find_map(|event| match event {
        HelpdeskEvent::TicketCreated(p) if p.ticket_id == ticket_id => Some(p.company_id.clone()),
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
/// against this shared context, not the tenant just created for it -
/// promoting those too needs `CreateTicket` et al.'s own `company_status`
/// read (currently one same-context DCB query) to reach across two
/// bounded contexts instead, which is real cross-context query design
/// work this pass doesn't do.
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
/// No `CancelDeadline` counterpart, deliberately - see
/// `ScheduleTicketAutoClose`'s own doc comment for why a stale pending
/// row is a non-issue here too: `ConvertCompanyTrial`/`ExpireCompanyTrial`
/// both already reject gracefully ("not trialing") against a company
/// that converted, expired, or reactivated by some other path before its
/// own deadline came due.
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
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match company_status(matching_events, &payload.company_id) {
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
        if ticket_status(matching_events, &payload.ticket_id).is_some() {
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
                }),
            }],
        }
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
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match ticket_status(matching_events, &payload.ticket_id) {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Open) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketAssigned".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": company_id_for_ticket(matching_events, &payload.ticket_id)
                            .expect("a ticket with any status has a TicketCreated in its own history"),
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
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match ticket_status(matching_events, &payload.ticket_id) {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::InProgress) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketResolved".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": company_id_for_ticket(matching_events, &payload.ticket_id)
                            .expect("a ticket with any status has a TicketCreated in its own history"),
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
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match ticket_status(matching_events, &payload.ticket_id) {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Resolved) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketReopened".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": company_id_for_ticket(matching_events, &payload.ticket_id)
                            .expect("a ticket with any status has a TicketCreated in its own history"),
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
    pub message: String,
}

pub struct RequestInfoFromCustomer;

/// `specs/skilj-helpdesk.allium`'s `rule StaffRequestsInfo`: `requires:
/// ticket.status = in_progress`.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RequestInfoFromCustomer {
    type Payload = RequestInfoFromCustomerPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "RequestInfoFromCustomer";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match ticket_status(matching_events, &payload.ticket_id) {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::InProgress) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketInfoRequested".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": company_id_for_ticket(matching_events, &payload.ticket_id)
                            .expect("a ticket with any status has a TicketCreated in its own history"),
                        "staff_id": payload.staff_id,
                        "message": payload.message,
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
    pub requester_id: String,
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
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match ticket_status(matching_events, &payload.ticket_id) {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::WaitingOnCustomer) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketCustomerResponded".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": company_id_for_ticket(matching_events, &payload.ticket_id)
                            .expect("a ticket with any status has a TicketCreated in its own history"),
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
}

pub struct CloseTicket;

/// `specs/skilj-helpdesk.allium`'s `rule TicketAutoCloses`: `requires:
/// ticket.status = resolved`. Submitted by `ScheduleTicketAutoClose`
/// below - see `TicketClosed`'s own doc comment.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for CloseTicket {
    type Payload = CloseTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CloseTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match ticket_status(matching_events, &payload.ticket_id) {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Resolved) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketClosed".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": company_id_for_ticket(matching_events, &payload.ticket_id)
                            .expect("a ticket with any status has a TicketCreated in its own history"),
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
/// No `CancelDeadline` counterpart, deliberately: a resolved-then-
/// reopened (or closed, or merged-away) ticket whose deadline still
/// fires just gets rejected by `CloseTicket::decide()` itself ("not
/// resolved - only a resolved ticket auto-closes") - a legitimate
/// one-shot outcome, not a bug to guard against (docs/architecture.md
/// §46: "a `Target` command that ... gets rejected by its own `decide()`
/// is marked `fired` too"). The old `scheduler.rs`'s own
/// `resolved_tickets` bookkeeping had to actively remove entries on
/// `TicketReopened`/`TicketClosed`/`TicketsMerged` to stop *every
/// 30-second poll tick* from resubmitting `CloseTicket` for the same
/// stale ticket forever - a real bug that file found and fixed. That
/// failure mode can't happen here: each deadline row fires exactly once,
/// ever.
pub struct ScheduleTicketAutoClose;

impl ScheduleDeadline for ScheduleTicketAutoClose {
    type Source = TicketResolved;
    type Target = CloseTicket;
    const NAME: &'static str = "ScheduleTicketAutoClose";
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn schedule(
        source_payload: &TicketResolvedPayload,
    ) -> Option<DeadlineSpec<CloseTicketPayload>> {
        Some(DeadlineSpec {
            fire_at: chrono::Utc::now() + scheduling::auto_close_after(),
            tags: vec![ticket_tag_value(&source_payload.ticket_id)],
            payload: CloseTicketPayload {
                ticket_id: source_payload.ticket_id.clone(),
            },
        })
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
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match ticket_status(matching_events, &payload.ticket_id) {
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
                let already_escalated = matching_events.iter().any(|e| {
                    matches!(e, HelpdeskEvent::TicketEscalated(p) if p.ticket_id == payload.ticket_id)
                });
                if already_escalated {
                    return CommandDecision::Rejected {
                        reason: format!("ticket {} has already been escalated", payload.ticket_id),
                        kind: "already_escalated".into(),
                    };
                }
                let previous_priority = matching_events
                    .iter()
                    .find_map(|e| match e {
                        HelpdeskEvent::TicketCreated(p) if p.ticket_id == payload.ticket_id => {
                            Some(p.priority)
                        }
                        _ => None,
                    })
                    .expect("a ticket with any status has a TicketCreated in its own history");
                let new_priority = escalate_priority(previous_priority);
                CommandDecision::Accepted {
                    events: vec![EventSpec {
                        event_type: "TicketEscalated".into(),
                        payload: serde_json::json!({
                            "ticket_id": payload.ticket_id,
                            "company_id": company_id_for_ticket(matching_events, &payload.ticket_id)
                                .expect("a ticket with any status has a TicketCreated in its own history"),
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
        let primary_status = ticket_status(matching_events, &payload.primary_ticket_id);
        let duplicate_status = ticket_status(matching_events, &payload.duplicate_ticket_id);
        match (primary_status, duplicate_status) {
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
                let primary_company =
                    company_id_for_ticket(matching_events, &payload.primary_ticket_id)
                        .expect("a ticket with any status has a TicketCreated in its own history");
                let duplicate_company =
                    company_id_for_ticket(matching_events, &payload.duplicate_ticket_id)
                        .expect("a ticket with any status has a TicketCreated in its own history");
                if primary_company != duplicate_company {
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
    pub comment: Option<String>,
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
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match ticket_status(matching_events, &payload.ticket_id) {
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
                let already_rated = matching_events.iter().any(|e| {
                    matches!(e, HelpdeskEvent::TicketRated(p) if p.ticket_id == payload.ticket_id)
                });
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
                            "company_id": company_id_for_ticket(matching_events, &payload.ticket_id)
                                .expect("a ticket with any status has a TicketCreated in its own history"),
                            "rating": payload.rating,
                            "comment": payload.comment,
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
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match ticket_status(matching_events, &payload.ticket_id) {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(_) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketInternalNoteAdded".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": company_id_for_ticket(matching_events, &payload.ticket_id)
                            .expect("a ticket with any status has a TicketCreated in its own history"),
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
/// here directly since `closed` never occurs in this pass
/// (`TicketAutoCloses` is deferred); a caller derives it from `status`
/// the same way.
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
            HelpdeskEvent::TicketInfoRequested(_) => {
                state.status = Some("waiting_on_customer".into());
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
            // own instance is untouched, matching `ticket_status`'s own
            // treatment in this file's command-decision helpers.
            HelpdeskEvent::TicketsMerged(p) => {
                if key == p.duplicate_ticket_id {
                    state.status = Some("merged".into());
                }
            }
        }
    }
}

// --- company-wide ticket list, for the frontend ---

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

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TicketListEntry {
    pub ticket_id: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub priority: String,
    pub requester_id: String,
    pub assigned_staff_id: Option<String>,
    pub messages: Vec<TicketMessage>,
    /// See `TicketSummaryState::escalated`/`::rating`'s own doc comments -
    /// identical reasoning, mirrored here since `frontend/` reads this
    /// projection, not `TicketSummary`.
    pub escalated: bool,
    pub rating: Option<u8>,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct CompanyTicketListState {
    pub tickets: std::collections::HashMap<String, TicketListEntry>,
}

/// Keyed by `company_id`. What `frontend/` actually queries to render
/// both sides of the app: the staff dashboard shows every entry, the
/// customer view filters client-side to `requester_id = self` (the
/// `StaffTicketQueue`/`CustomerPortal` surfaces `specs/skilj-helpdesk.allium`
/// describes at the domain level - this is their real implementation,
/// merged into one projection since nothing here is actually customer-
/// only data; the split is presentation, not access control).
pub struct CompanyTicketList;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for CompanyTicketList {
    type State = CompanyTicketListState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CompanyTicketList";
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
    fn sync() -> bool {
        true
    }
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
            | HelpdeskEvent::TicketInternalNoteAdded(_) => vec![],
            HelpdeskEvent::TicketCreated(p) => vec![p.company_id.clone()],
            // Every ticket-lifecycle event past creation now carries its
            // own `company_id` too (stamped by each command's own
            // `decide()` via `company_id_for_ticket` - see that
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
        match event {
            HelpdeskEvent::CompanySignedUp(_)
            | HelpdeskEvent::CompanyActivated(_)
            | HelpdeskEvent::CompanyExpired(_)
            | HelpdeskEvent::CompanyTenantProvisioned(_)
            | HelpdeskEvent::TicketInternalNoteAdded(_) => {}
            HelpdeskEvent::TicketCreated(p) => {
                state.tickets.insert(
                    p.ticket_id.clone(),
                    TicketListEntry {
                        ticket_id: p.ticket_id.clone(),
                        title: p.title.clone(),
                        description: p.description.clone(),
                        status: "open".into(),
                        priority: priority_str(p.priority).to_string(),
                        requester_id: p.requester_id.clone(),
                        assigned_staff_id: None,
                        messages: Vec::new(),
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
            HelpdeskEvent::TicketResolved(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.status = "resolved".into();
                }
            }
            HelpdeskEvent::TicketReopened(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.status = "in_progress".into();
                }
            }
            HelpdeskEvent::TicketInfoRequested(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.status = "waiting_on_customer".into();
                    entry.messages.push(TicketMessage {
                        author_id: p.staff_id.clone(),
                        from_staff: true,
                        text: p.message.clone(),
                    });
                }
            }
            HelpdeskEvent::TicketCustomerResponded(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.status = "in_progress".into();
                    entry.messages.push(TicketMessage {
                        author_id: p.requester_id.clone(),
                        from_staff: false,
                        text: p.message.clone(),
                    });
                }
            }
            HelpdeskEvent::TicketClosed(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.status = "closed".into();
                }
            }
            HelpdeskEvent::TicketEscalated(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.priority = priority_str(p.new_priority).to_string();
                    entry.escalated = true;
                }
            }
            HelpdeskEvent::TicketRated(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.rating = Some(p.rating);
                }
            }
            HelpdeskEvent::TicketsMerged(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.duplicate_ticket_id) {
                    entry.status = "merged".into();
                }
            }
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
/// `CompanyTicketList`, on purpose: those two are what `frontend/`'s
/// single "fetch everything" call returns to render a ticket at all, so
/// keeping `TicketInternalNoteAdded` out of both is what actually keeps
/// `CompanyTicketList`'s own "nothing here is customer-only data" claim
/// true (see that event's own doc comment). This projection exists
/// purely so staff have something to fetch on demand (`frontend/`'s own
/// "Notes" toggle, a second, separate query - not folded into the
/// eager one).
///
/// **Both the cross-company and the same-company staff-vs-customer gaps
/// are now closed.** A security review found this projection (and
/// `TicketSummary`/`CompanyTicketList`) readable by any Role with *any*
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
