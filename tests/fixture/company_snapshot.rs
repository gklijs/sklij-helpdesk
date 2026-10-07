//! `CompanySnapshot`: `CreateTicket` must reach the same decision from a
//! stored snapshot plus the events since as `decide()` reaches from the
//! company's full history - skilj's correctness obligation on
//! `decide_from_snapshot`. Folded and split the same way as
//! `ticket_snapshot.rs`.

use skilj::{CommandType, Snapshot};
use skilj_core::shared::CommandDecision;
use skilj_helpdesk::helpdesk::*;

use crate::events::*;

/// Histories for company `acme`, each only the events tagged
/// `company: acme` - what `matching_events` holds for `CreateTicket`.
fn histories() -> Vec<Vec<HelpdeskEvent>> {
    let mut busy = vec![signed_up("acme")];
    for n in 0..5 {
        busy.push(created(&format!("t{n}"), "acme", TicketPriority::Low));
        busy.push(note_added(&format!("t{n}"), "acme", "n"));
    }
    busy.push(activated("acme"));
    vec![
        vec![],
        vec![signed_up("acme")],
        vec![
            signed_up("acme"),
            created("t1", "acme", TicketPriority::Low),
        ],
        vec![signed_up("acme"), activated("acme")],
        vec![signed_up("acme"), expired("acme")],
        // Reactivated after expiring.
        vec![signed_up("acme"), expired("acme"), activated("acme")],
        // A tenant's history: only the mirror.
        vec![mirrored("acme", CompanyStatus::Active)],
        vec![
            mirrored("acme", CompanyStatus::Trialing),
            created("t1", "acme", TicketPriority::Low),
            mirrored("acme", CompanyStatus::Expired),
        ],
        // Tickets before the signup is known, as a tenant can see them.
        vec![
            created("t1", "acme", TicketPriority::Low),
            mirrored("acme", CompanyStatus::Active),
        ],
        busy,
    ]
}

/// Folds `events` the way skilj's snapshot catch-up does: no company id
/// handed in.
fn snapshot_of(events: &[HelpdeskEvent]) -> String {
    let mut state = CompanyFacts::default();
    for event in events {
        CompanySnapshot::fold(&mut state, event);
    }
    serde_json::to_string(&state).unwrap()
}

fn same(a: &CommandDecision, b: &CommandDecision) -> bool {
    format!("{a:?}") == format!("{b:?}")
}

fn payload(ticket_id: &str) -> CreateTicketPayload {
    CreateTicketPayload {
        ticket_id: ticket_id.into(),
        company_id: "acme".into(),
        requester_id: "customer-1".into(),
        logged_by_staff_id: None,
        title: "t".into(),
        description: "d".into(),
        priority: TicketPriority::Low,
        requester_name: None,
        requester_email: None,
    }
}

#[test]
fn create_ticket_decides_the_same_from_a_snapshot() {
    assert_eq!(CreateTicket::snapshot(), Some(CompanySnapshot::NAME));
    // A new ticket id, and one every non-empty history already created.
    for ticket_id in ["t9", "t1"] {
        let payload = payload(ticket_id);
        for history in histories() {
            let full = CreateTicket::decide(&payload, &history);
            for split in 0..=history.len() {
                let (before, after) = history.split_at(split);
                let resumed =
                    CreateTicket::decide_from_snapshot(&payload, &snapshot_of(before), after);
                assert!(
                    same(&full, &resumed),
                    "{ticket_id} split at {split}/{}:\n  decide():               {full:?}\n  decide_from_snapshot(): {resumed:?}",
                    history.len(),
                );
            }
        }
    }
}

#[test]
fn the_snapshot_folds_a_company_without_being_told_which_one() {
    let history = histories().pop().unwrap();
    let mut state = CompanyFacts::default();
    for event in &history {
        CompanySnapshot::fold(&mut state, event);
    }
    assert_eq!(state, CompanyFacts::of(&history, "acme"));
    assert_eq!(state.company_id, "acme");
    assert_eq!(state.status, Some(CompanyStatus::Active));
    assert_eq!(state.ticket_ids.len(), 5);
}

#[test]
fn an_unreadable_company_snapshot_is_rejected_rather_than_trusted() {
    let decision = CreateTicket::decide_from_snapshot(&payload("t9"), r#"{"ticket_ids": 42}"#, &[]);
    assert!(
        matches!(&decision, CommandDecision::Rejected { kind, .. } if kind == "snapshot_unreadable"),
        "{decision:?}"
    );
}
