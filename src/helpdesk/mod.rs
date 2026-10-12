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
//! Macros (canned staff replies, `macros.rs`) live here too rather than
//! in a context of their own, so `ApplyMacro` can read the macro and the
//! ticket in one consistency query - see that file's doc comment.
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
use skilj_core::plugin::{BoundedContextEvent, DeadlinePollStartFrom, DeadlineSpec, Decision};
use skilj_core::shared::{
    CommandDecision, EventSpec, PrivateField, PrivateFieldKind, QueryItemMapping, SensitiveField,
    Tag, TagMapping,
};

use crate::scheduling;

mod auto_assign;
mod company;
mod macros;
mod read_models;
mod sla_policy;
mod ticket;
mod ticket_commands;

pub use auto_assign::*;
pub use company::*;
pub use macros::*;
pub use read_models::*;
pub use sla_policy::*;
pub use ticket::*;
pub use ticket_commands::*;

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

/// The agent a payload's `field` names - what `StaffWorkload` is keyed
/// by (issue #44). A list-valued `field` (`AutoAssignTicket`'s
/// candidates) derives one tag per agent.
fn staff_tag(field: &str) -> Vec<TagMapping> {
    vec![TagMapping {
        key: STAFF_TAG_KEY.into(),
        field: field.into(),
    }]
}

/// The four events `company_status` folds.
const COMPANY_LIFECYCLE_EVENTS: [&str; 4] = [
    "CompanySignedUp",
    "CompanyActivated",
    "CompanyExpired",
    "CompanyLifecycleMirrored",
];

/// What `company_status` needs as a consistency query (skilj
/// docs/architecture.md §198): its last event wins, so only the
/// company's latest lifecycle event - not the company's tickets, which
/// carry its tag too.
fn company_status_query() -> QueryItemMapping {
    QueryItemMapping::types(&COMPANY_LIFECYCLE_EVENTS)
        .tagged(company_tag())
        .latest()
}

/// The consistency query of every single-ticket command: the ticket's
/// `TicketSnapshot` state, as a snapshot item (skilj docs/architecture.md
/// §202). The command reads the stored row and the ticket's events since
/// it, skilj folds those in, and `decide_with()` gets the `TicketFacts`.
/// Every event of the ticket still conflicts with it.
fn ticket_snapshot_query() -> Vec<QueryItemMapping> {
    vec![QueryItemMapping::snapshot::<TicketSnapshot>("ticket_id")]
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
    MacroCreated(MacroDefinitionPayload),
    MacroUpdated(MacroDefinitionPayload),
    MacroDeleted(MacroDeletedPayload),
    TicketStaffReplied(TicketStaffRepliedPayload),
    TicketPriorityChanged(TicketPriorityChangedPayload),
    CompanySlaPolicySet(CompanySlaPolicySetPayload),
    TicketSlaBreached(TicketSlaBreachedPayload),
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
            "MacroCreated" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::MacroCreated))
            }
            "MacroUpdated" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::MacroUpdated))
            }
            "MacroDeleted" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::MacroDeleted))
            }
            "TicketStaffReplied" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketStaffReplied))
            }
            "TicketPriorityChanged" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketPriorityChanged))
            }
            "CompanySlaPolicySet" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::CompanySlaPolicySet))
            }
            "TicketSlaBreached" => {
                Some(serde_json::from_str(&event.payload).map(HelpdeskEvent::TicketSlaBreached))
            }
            _ => None,
        }
    }
}

#[cfg(test)]
mod required_role_tests {
    use super::*;

    /// `#[requires_role(...)]` only takes a string literal, so the fourteen
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
            CreateMacro::required_role(),
            UpdateMacro::required_role(),
            DeleteMacro::required_role(),
            ApplyMacro::required_role(),
            SetCompanySlaPolicy::required_role(),
            BreachTicketSla::required_role(),
            AutoAssignTicket::required_role(),
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
