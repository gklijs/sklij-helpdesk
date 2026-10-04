//! Shorthand constructors for the given events and expected `EventSpec`s
//! the other modules build their histories from.

use skilj_core::shared::{CommandDecision, EventSpec};
use skilj_helpdesk::helpdesk::*;

pub fn spec(event_type: &str, payload: serde_json::Value) -> EventSpec {
    EventSpec {
        event_type: event_type.into(),
        payload,
    }
}

/// For a `.then(...)` that only cares that the command went through, not
/// what it emitted - the emitted events are pinned elsewhere.
pub fn assert_accepted(decision: &CommandDecision) {
    assert!(
        matches!(decision, CommandDecision::Accepted { .. }),
        "expected Accepted, got {decision:?}"
    );
}

// --- company ---

pub fn signed_up(company_id: &str) -> HelpdeskEvent {
    HelpdeskEvent::CompanySignedUp(CompanySignedUpPayload {
        company_id: company_id.into(),
        name: "Acme".into(),
        contact_email: "ops@acme.example".into(),
    })
}

pub fn activated(company_id: &str) -> HelpdeskEvent {
    HelpdeskEvent::CompanyActivated(CompanyActivatedPayload {
        company_id: company_id.into(),
    })
}

pub fn expired(company_id: &str) -> HelpdeskEvent {
    HelpdeskEvent::CompanyExpired(CompanyExpiredPayload {
        company_id: company_id.into(),
    })
}

pub fn tenant_provisioned(company_id: &str, tenant_name: &str) -> HelpdeskEvent {
    HelpdeskEvent::CompanyTenantProvisioned(CompanyTenantProvisionedPayload {
        company_id: company_id.into(),
        tenant_name: tenant_name.into(),
    })
}

pub fn mirrored(company_id: &str, status: CompanyStatus) -> HelpdeskEvent {
    HelpdeskEvent::CompanyLifecycleMirrored(CompanyLifecycleMirroredPayload {
        company_id: company_id.into(),
        status,
        source_event_type: "test".into(),
    })
}

// --- ticket ---

pub fn created(ticket_id: &str, company_id: &str, priority: TicketPriority) -> HelpdeskEvent {
    HelpdeskEvent::TicketCreated(TicketCreatedPayload {
        ticket_id: ticket_id.into(),
        company_id: company_id.into(),
        requester_id: "customer-1".into(),
        logged_by_staff_id: None,
        title: format!("{ticket_id} title"),
        description: format!("{ticket_id} description"),
        priority,
    })
}

pub fn assigned(ticket_id: &str, company_id: &str, staff_id: &str) -> HelpdeskEvent {
    HelpdeskEvent::TicketAssigned(TicketAssignedPayload {
        ticket_id: ticket_id.into(),
        company_id: company_id.into(),
        staff_id: staff_id.into(),
    })
}

pub fn resolved(ticket_id: &str, company_id: &str, resolution: u32) -> HelpdeskEvent {
    HelpdeskEvent::TicketResolved(TicketResolvedPayload {
        ticket_id: ticket_id.into(),
        company_id: company_id.into(),
        resolution: Some(resolution),
    })
}

pub fn reopened(ticket_id: &str, company_id: &str, resolution: u32) -> HelpdeskEvent {
    HelpdeskEvent::TicketReopened(TicketReopenedPayload {
        ticket_id: ticket_id.into(),
        company_id: company_id.into(),
        resolution: Some(resolution),
    })
}

pub fn info_requested(ticket_id: &str, company_id: &str, message: &str) -> HelpdeskEvent {
    HelpdeskEvent::TicketInfoRequested(TicketInfoRequestedPayload {
        ticket_id: ticket_id.into(),
        company_id: company_id.into(),
        staff_id: "staff-1".into(),
        message: message.into(),
    })
}

pub fn customer_responded(ticket_id: &str, company_id: &str, message: &str) -> HelpdeskEvent {
    HelpdeskEvent::TicketCustomerResponded(TicketCustomerRespondedPayload {
        ticket_id: ticket_id.into(),
        company_id: company_id.into(),
        requester_id: "customer-1".into(),
        message: message.into(),
    })
}

pub fn closed(ticket_id: &str, company_id: &str) -> HelpdeskEvent {
    HelpdeskEvent::TicketClosed(TicketClosedPayload {
        ticket_id: ticket_id.into(),
        company_id: company_id.into(),
    })
}

pub fn escalated(
    ticket_id: &str,
    company_id: &str,
    previous_priority: TicketPriority,
    new_priority: TicketPriority,
) -> HelpdeskEvent {
    HelpdeskEvent::TicketEscalated(TicketEscalatedPayload {
        ticket_id: ticket_id.into(),
        company_id: company_id.into(),
        previous_priority,
        new_priority,
    })
}

pub fn merged(primary: &str, duplicate: &str, company_id: &str) -> HelpdeskEvent {
    HelpdeskEvent::TicketsMerged(TicketsMergedPayload {
        primary_ticket_id: primary.into(),
        duplicate_ticket_id: duplicate.into(),
        company_id: company_id.into(),
    })
}

pub fn rated(ticket_id: &str, company_id: &str, rating: u8) -> HelpdeskEvent {
    HelpdeskEvent::TicketRated(TicketRatedPayload {
        ticket_id: ticket_id.into(),
        company_id: company_id.into(),
        rating,
        comment: None,
    })
}

pub fn note_added(ticket_id: &str, company_id: &str, note: &str) -> HelpdeskEvent {
    HelpdeskEvent::TicketInternalNoteAdded(TicketInternalNoteAddedPayload {
        ticket_id: ticket_id.into(),
        company_id: company_id.into(),
        staff_id: "staff-1".into(),
        note: note.into(),
    })
}

// --- ticket histories, one per reachable status ---

pub fn open_ticket(ticket_id: &str, company_id: &str) -> Vec<HelpdeskEvent> {
    vec![created(ticket_id, company_id, TicketPriority::Medium)]
}

pub fn in_progress_ticket(ticket_id: &str, company_id: &str) -> Vec<HelpdeskEvent> {
    let mut events = open_ticket(ticket_id, company_id);
    events.push(assigned(ticket_id, company_id, "staff-1"));
    events
}

pub fn waiting_ticket(ticket_id: &str, company_id: &str) -> Vec<HelpdeskEvent> {
    let mut events = in_progress_ticket(ticket_id, company_id);
    events.push(info_requested(ticket_id, company_id, "which version?"));
    events
}

pub fn resolved_ticket(ticket_id: &str, company_id: &str) -> Vec<HelpdeskEvent> {
    let mut events = in_progress_ticket(ticket_id, company_id);
    events.push(resolved(ticket_id, company_id, 1));
    events
}

pub fn closed_ticket(ticket_id: &str, company_id: &str) -> Vec<HelpdeskEvent> {
    let mut events = resolved_ticket(ticket_id, company_id);
    events.push(closed(ticket_id, company_id));
    events
}
