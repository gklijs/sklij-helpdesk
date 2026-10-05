//! `TicketSnapshot`: every single-ticket command must reach the same
//! decision from a stored snapshot plus the events since as `decide()`
//! reaches from the full history - skilj's own correctness obligation on
//! `decide_from_snapshot` (issue #14).
//!
//! The snapshot here is folded the way skilj's catch-up folds it - from
//! the default state, through `TicketSnapshot::fold`, with no ticket id
//! handed in - and split at every point of each history, including "no
//! snapshot yet" (split at 0, which is also what an older-`VERSION` row
//! is treated as).

use skilj::{CommandType, Snapshot};
use skilj_core::shared::CommandDecision;
use skilj_helpdesk::helpdesk::*;

use crate::events::*;

/// Histories for ticket `t1`, each only the events tagged `ticket: t1` -
/// what `matching_events` holds for a single-ticket command.
fn histories() -> Vec<Vec<HelpdeskEvent>> {
    let mut long = vec![
        created("t1", "acme", TicketPriority::Medium),
        assigned("t1", "acme", "staff-1"),
    ];
    for round in 0..5 {
        long.push(info_requested("t1", "acme", &format!("question {round}")));
        long.push(customer_responded("t1", "acme", &format!("answer {round}")));
        long.push(note_added("t1", "acme", &format!("note {round}")));
    }
    long.push(escalated(
        "t1",
        "acme",
        TicketPriority::Medium,
        TicketPriority::High,
    ));
    long.push(resolved("t1", "acme", 1));
    long.push(reopened("t1", "acme", 1));
    long.push(resolved("t1", "acme", 2));
    long.push(rated("t1", "acme", 4));
    long.push(closed("t1", "acme"));

    vec![
        vec![],
        open_ticket("t1", "acme"),
        in_progress_ticket("t1", "acme"),
        waiting_ticket("t1", "acme"),
        resolved_ticket("t1", "acme"),
        closed_ticket("t1", "acme"),
        long,
        // `t1` as the primary of a merge stays as it was...
        then(open_ticket("t1", "acme"), merged("t1", "t2", "acme")),
        // ...and as the duplicate becomes merged.
        then(open_ticket("t1", "acme"), merged("t0", "t1", "acme")),
    ]
}

fn then(mut events: Vec<HelpdeskEvent>, event: HelpdeskEvent) -> Vec<HelpdeskEvent> {
    events.push(event);
    events
}

/// Folds `events` the way skilj's snapshot catch-up does.
fn snapshot_of(events: &[HelpdeskEvent]) -> String {
    let mut state = TicketFacts::default();
    for event in events {
        TicketSnapshot::fold(&mut state, event);
    }
    serde_json::to_string(&state).unwrap()
}

/// `CommandDecision` has no `PartialEq`; its `Debug` output covers every
/// field, event payloads included.
fn same(a: &CommandDecision, b: &CommandDecision) -> bool {
    format!("{a:?}") == format!("{b:?}")
}

fn assert_snapshot_agrees<C: CommandType<Event = HelpdeskEvent>>(payload: C::Payload) {
    assert_eq!(C::snapshot(), Some(TicketSnapshot::NAME), "{}", C::NAME);
    for history in histories() {
        let full = C::decide(&payload, &history);
        for split in 0..=history.len() {
            let (before, after) = history.split_at(split);
            let resumed = C::decide_from_snapshot(&payload, &snapshot_of(before), after);
            assert!(
                same(&full, &resumed),
                "{} split at {split}/{}:\n  decide():               {full:?}\n  decide_from_snapshot(): {resumed:?}",
                C::NAME,
                history.len(),
            );
        }
    }
}

#[test]
fn every_single_ticket_command_decides_the_same_from_a_snapshot() {
    let requester = || Some("customer-1".to_string());
    assert_snapshot_agrees::<AssignTicket>(AssignTicketPayload {
        ticket_id: "t1".into(),
        staff_id: "staff-2".into(),
    });
    assert_snapshot_agrees::<ResolveTicket>(ResolveTicketPayload {
        ticket_id: "t1".into(),
    });
    assert_snapshot_agrees::<ReopenTicket>(ReopenTicketPayload {
        ticket_id: "t1".into(),
    });
    for requester_id in [requester(), Some("someone-else".into()), None] {
        assert_snapshot_agrees::<RequestInfoFromCustomer>(RequestInfoFromCustomerPayload {
            ticket_id: "t1".into(),
            staff_id: "staff-1".into(),
            message: "more?".into(),
            requester_id: requester_id.clone(),
        });
        assert_snapshot_agrees::<RateTicket>(RateTicketPayload {
            ticket_id: "t1".into(),
            rating: 5,
            comment: None,
            requester_id,
        });
    }
    for requester_id in ["customer-1", "someone-else"] {
        assert_snapshot_agrees::<CustomerRespondsToTicket>(CustomerRespondsToTicketPayload {
            ticket_id: "t1".into(),
            requester_id: requester_id.into(),
            message: "here".into(),
        });
    }
    assert_snapshot_agrees::<CloseTicket>(CloseTicketPayload {
        ticket_id: "t1".into(),
        requester_id: requester(),
        requester_email: None,
    });
    assert_snapshot_agrees::<EscalateTicket>(EscalateTicketPayload {
        ticket_id: "t1".into(),
    });
    assert_snapshot_agrees::<AddInternalNote>(AddInternalNotePayload {
        ticket_id: "t1".into(),
        staff_id: "staff-1".into(),
        note: "n".into(),
    });
}

#[test]
fn the_snapshot_folds_a_ticket_without_being_told_which_one() {
    // skilj's catch-up hands `fold` no key; the ticket's own id comes from
    // its `TicketCreated`, which is what tells the two sides of a merge
    // apart afterwards.
    let history = histories().remove(6);
    let mut state = TicketFacts::default();
    for event in &history {
        TicketSnapshot::fold(&mut state, event);
    }
    assert_eq!(state, TicketFacts::of(&history, "t1"));
    assert_eq!(state.ticket_id, "t1");
    assert_eq!(state.status, Some(TicketStatus::Closed));
    assert_eq!(state.resolutions, 2);
    assert!(state.escalated && state.rated);
    assert_eq!(state.created_priority, Some(TicketPriority::Medium));
}

#[test]
fn an_unreadable_snapshot_is_rejected_rather_than_trusted() {
    let decision = AssignTicket::decide_from_snapshot(
        &AssignTicketPayload {
            ticket_id: "t1".into(),
            staff_id: "staff-1".into(),
        },
        r#"{"status": 42}"#,
        &[],
    );
    assert!(
        matches!(&decision, CommandDecision::Rejected { kind, .. } if kind == "snapshot_unreadable"),
        "{decision:?}"
    );
}
