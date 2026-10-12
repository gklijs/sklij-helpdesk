//! `AutoAssignTicket` and the `StaffWorkload` snapshot it decides from
//! (issue #44).
//!
//! skilj folds `StaffWorkload` itself, from every event tagged
//! `staff: <id>`, so what is checked here is that fold: a history built
//! by running the real ticket commands, and at every step each agent's
//! state, folded from their tag alone, must be exactly the tickets the
//! full history says they have open (`TicketFacts::of` per ticket). That
//! is this snapshot's consistency-query parity, the same check
//! `ticket_snapshot.rs` makes for `TicketSnapshot`.

use std::collections::BTreeSet;

use serde_json::{json, Value};
use skilj::{CommandType, Snapshot};
use skilj_core::shared::CommandDecision;
use skilj_helpdesk::helpdesk::*;
use skilj_test_fixture::command::GivenEvents;

use crate::consistency_query::{derive, Described};
use crate::events::*;
use crate::ticket_snapshot::ticket_state;

const AGENTS: [&str; 3] = ["staff-a", "staff-b", "staff-c"];

/// `staff`'s state, folded as skilj folds it: every event carrying the
/// tag `staff: <staff>`, from the default state.
fn staff_state(history: &[HelpdeskEvent], staff: &str) -> StaffLoad {
    let tag = (STAFF_TAG_KEY.to_string(), staff.to_string());
    let mut state = StaffLoad::default();
    for event in history {
        let (event_type, payload) = event.describe();
        if derive(&HelpdeskEvent::tag_mappings(event_type), &payload).contains(&tag) {
            StaffWorkload::fold_for(&mut state, staff, event);
        }
    }
    state
}

/// What the whole history says `staff` has open: every ticket assigned to
/// them that is in progress or waiting on its customer.
fn open_per_history(history: &[HelpdeskEvent], staff: &str) -> BTreeSet<String> {
    history
        .iter()
        .filter_map(|event| match event {
            HelpdeskEvent::TicketCreated(p) => Some(p.ticket_id.clone()),
            _ => None,
        })
        .filter(|ticket| {
            let facts = TicketFacts::of(history, ticket);
            facts.assignee_id.as_deref() == Some(staff)
                && matches!(
                    facts.status,
                    Some(TicketStatus::InProgress | TicketStatus::WaitingOnCustomer)
                )
        })
        .collect()
}

/// The values a payload field derives as tags: one for a string, one per
/// element for a list.
fn values(payload: &Value, field: &str) -> Vec<String> {
    match &payload[field] {
        Value::String(s) => vec![s.clone()],
        Value::Array(items) => items
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

/// `payload` decided as skilj decides it: every snapshot item the
/// command's query derives, folded from `history`.
fn decide<C: CommandType<Event = HelpdeskEvent>>(
    history: &[HelpdeskEvent],
    payload: C::Payload,
) -> CommandDecision {
    let json = serde_json::to_value(&payload).unwrap();
    let mut given = GivenEvents::<C>::new();
    for item in C::consistency_query() {
        let field = &item.tag_mappings[0].field;
        for value in values(&json, field) {
            given = match item.snapshot.as_deref() {
                Some(TicketSnapshot::NAME) => {
                    given.snapshot::<TicketSnapshot>(&value, ticket_state(history, &value))
                }
                Some(StaffWorkload::NAME) => {
                    given.snapshot::<StaffWorkload>(&value, staff_state(history, &value))
                }
                other => panic!("{}: unexpected item {other:?}", C::NAME),
            };
        }
    }
    given.when(payload).into_decision()
}

/// Runs `payload` against `history`, appends what it emits, and checks
/// every agent's snapshot against the history.
fn apply<C: CommandType<Event = HelpdeskEvent>>(
    history: &mut Vec<HelpdeskEvent>,
    payload: C::Payload,
) {
    let CommandDecision::Accepted { events } = decide::<C>(history, payload) else {
        panic!("{} was rejected", C::NAME);
    };
    for event in events {
        history.push(HelpdeskEvent::rebuild(&event.event_type, &event.payload));
    }
    for agent in AGENTS {
        assert_eq!(
            staff_state(history, agent).open_ticket_ids,
            open_per_history(history, agent),
            "{agent} after {}",
            C::NAME
        );
    }
}

fn auto(ticket: &str) -> AutoAssignTicketPayload {
    AutoAssignTicketPayload {
        ticket_id: ticket.into(),
        candidate_staff_ids: AGENTS.iter().map(|a| a.to_string()).collect(),
    }
}

fn assignee(history: &[HelpdeskEvent], ticket: &str) -> Option<String> {
    TicketFacts::of(history, ticket).assignee_id
}

#[test]
fn the_workload_folded_per_agent_matches_the_history_and_drives_the_pick() {
    let mut h: Vec<HelpdeskEvent> = Vec::new();
    for t in ["t1", "t2", "t3", "t4", "t5", "t6", "t7"] {
        h.push(created(t, "acme", TicketPriority::Medium));
    }
    // All idle: the first listed, then round the list.
    apply::<AutoAssignTicket>(&mut h, auto("t1"));
    apply::<AutoAssignTicket>(&mut h, auto("t2"));
    apply::<AutoAssignTicket>(&mut h, auto("t3"));
    assert_eq!(
        ["t1", "t2", "t3"].map(|t| assignee(&h, t)),
        AGENTS.map(|a| Some(a.to_string()))
    );
    // A manual assignment counts too: b now has two.
    apply::<AssignTicket>(
        &mut h,
        AssignTicketPayload {
            ticket_id: "t4".into(),
            staff_id: "staff-b".into(),
        },
    );
    // Waiting on the customer is still b's ticket.
    apply::<RequestInfoFromCustomer>(
        &mut h,
        RequestInfoFromCustomerPayload {
            ticket_id: "t2".into(),
            staff_id: "staff-b".into(),
            message: "which version?".into(),
            requester_id: Some("customer-1".into()),
        },
    );
    // a resolves theirs: a 0, b 2, c 1.
    apply::<ResolveTicket>(
        &mut h,
        ResolveTicketPayload {
            ticket_id: "t1".into(),
        },
    );
    apply::<AutoAssignTicket>(&mut h, auto("t5"));
    assert_eq!(assignee(&h, "t5").as_deref(), Some("staff-a"));
    // Reopened, t1 is a's again: a 2, b 2, c 1.
    apply::<ReopenTicket>(
        &mut h,
        ReopenTicketPayload {
            ticket_id: "t1".into(),
        },
    );
    apply::<AutoAssignTicket>(&mut h, auto("t6"));
    assert_eq!(assignee(&h, "t6").as_deref(), Some("staff-c"));
    // c's t3 merged into b's t4 is off c's load: a 2, b 2, c 1.
    apply::<MergeTickets>(
        &mut h,
        MergeTicketsPayload {
            primary_ticket_id: "t4".into(),
            duplicate_ticket_id: "t3".into(),
        },
    );
    assert_eq!(staff_state(&h, "staff-c").open_tickets(), 1);
    apply::<AutoAssignTicket>(&mut h, auto("t7"));
    assert_eq!(assignee(&h, "t7").as_deref(), Some("staff-c"));
}

/// The payload of the one event an accepted `decision` emits.
fn emitted(decision: CommandDecision) -> Value {
    match decision {
        CommandDecision::Accepted { events } => events[0].payload.clone(),
        other => panic!("{other:?}"),
    }
}

#[test]
fn the_events_that_change_a_load_name_the_agent() {
    let mut h = in_progress_ticket("t1", "acme");
    h.extend(open_ticket("t2", "acme"));
    let resolved = emitted(decide::<ResolveTicket>(
        &h,
        ResolveTicketPayload {
            ticket_id: "t1".into(),
        },
    ));
    assert_eq!(resolved["assignee_id"], "staff-1");
    let merged = emitted(decide::<MergeTickets>(
        &h,
        MergeTicketsPayload {
            primary_ticket_id: "t2".into(),
            duplicate_ticket_id: "t1".into(),
        },
    ));
    assert_eq!(merged["duplicate_assignee_id"], "staff-1");
    // An unassigned duplicate names nobody, and is tagged with no agent.
    let merged = emitted(decide::<MergeTickets>(
        &h,
        MergeTicketsPayload {
            primary_ticket_id: "t1".into(),
            duplicate_ticket_id: "t2".into(),
        },
    ));
    assert_eq!(merged["duplicate_assignee_id"], Value::Null);
    let tags = derive(&HelpdeskEvent::tag_mappings("TicketsMerged"), &merged);
    assert!(tags.iter().all(|(key, _)| key != STAFF_TAG_KEY), "{tags:?}");
}

#[test]
fn events_stored_before_the_agent_was_named_leave_the_workload_alone() {
    // A resolution with no `assignee_id` carries no staff tag, and
    // resolving a ticket the set never held is a no-op, not a negative.
    let legacy = HelpdeskEvent::TicketResolved(TicketResolvedPayload {
        ticket_id: "t9".into(),
        company_id: "acme".into(),
        resolution: Some(1),
        requester_id: None,
        assignee_id: None,
    });
    let mut h = in_progress_ticket("t1", "acme");
    h.push(legacy);
    assert_eq!(staff_state(&h, "staff-1").open_tickets(), 1);
    let mut state = staff_state(&h, "staff-1");
    StaffWorkload::fold_for(&mut state, "staff-1", &resolved("t9", "acme", 1));
    assert_eq!(state.open_tickets(), 1);
}

#[test]
fn two_racing_auto_assignments_conflict_on_the_agent_they_both_read() {
    let mut h = open_ticket("t1", "acme");
    h.extend(open_ticket("t2", "acme"));
    let pair = |ticket: &str| AutoAssignTicketPayload {
        ticket_id: ticket.into(),
        candidate_staff_ids: vec!["staff-a".into(), "staff-b".into()],
    };
    // Both read the same idle agents, so on its own each picks a.
    let first = decide::<AutoAssignTicket>(&h, pair("t1"));
    let stale = decide::<AutoAssignTicket>(&h, pair("t2"));
    let picked = |d: &CommandDecision| match d {
        CommandDecision::Accepted { events } => events[0].payload["staff_id"].clone(),
        other => panic!("{other:?}"),
    };
    assert_eq!(
        (picked(&first), picked(&stale)),
        (json!("staff-a"), json!("staff-a"))
    );

    // The first one's event carries a tag the second one's query reads -
    // `staff: staff-a` - which is what makes it a conflict for skilj:
    // instead of appending its stale decision, the second is decided
    // again with the first folded in, and picks b.
    let CommandDecision::Accepted { events } = first else {
        unreachable!()
    };
    let appended = &events[0];
    let appended_tags = derive(
        &HelpdeskEvent::tag_mappings(&appended.event_type),
        &appended.payload,
    );
    let read: Vec<(String, String)> = ["staff-a", "staff-b"]
        .map(|a| (STAFF_TAG_KEY.to_string(), a.to_string()))
        .into();
    assert!(
        appended_tags.iter().any(|t| read.contains(t)),
        "{appended_tags:?}"
    );
    h.push(HelpdeskEvent::rebuild(
        &appended.event_type,
        &appended.payload,
    ));
    let redecided = decide::<AutoAssignTicket>(&h, pair("t2"));
    assert_eq!(picked(&redecided), json!("staff-b"));
}

#[test]
fn auto_assignment_rejects_like_assignment_does() {
    let candidates = || vec!["staff-a".to_string()];
    for (history, kind) in [
        (Vec::new(), "ticket_not_found"),
        (in_progress_ticket("t1", "acme"), "ticket_not_open"),
        (closed_ticket("t1", "acme"), "ticket_not_open"),
    ] {
        let decision = decide::<AutoAssignTicket>(
            &history,
            AutoAssignTicketPayload {
                ticket_id: "t1".into(),
                candidate_staff_ids: candidates(),
            },
        );
        assert!(
            matches!(&decision, CommandDecision::Rejected { kind: k, .. } if k == kind),
            "{decision:?}"
        );
    }
    let decision = decide::<AutoAssignTicket>(
        &open_ticket("t1", "acme"),
        AutoAssignTicketPayload {
            ticket_id: "t1".into(),
            candidate_staff_ids: Vec::new(),
        },
    );
    assert!(
        matches!(&decision, CommandDecision::Rejected { kind, .. } if kind == "no_candidates"),
        "{decision:?}"
    );
}

#[test]
fn it_emits_the_same_assignment_a_person_would() {
    let decision = decide::<AutoAssignTicket>(&open_ticket("t1", "acme"), auto("t1"));
    let CommandDecision::Accepted { events } = decision else {
        panic!("{decision:?}");
    };
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].event_type, "TicketAssigned");
    assert_eq!(
        events[0].payload,
        json!({ "ticket_id": "t1", "company_id": "acme", "staff_id": "staff-a" })
    );
}

#[test]
fn its_query_is_the_ticket_and_one_workload_per_candidate() {
    let items: Vec<(Option<String>, String)> = AutoAssignTicket::consistency_query()
        .into_iter()
        .map(|item| {
            assert!(item.event_types.is_empty() && item.latest.is_none());
            (item.snapshot, item.tag_mappings[0].field.clone())
        })
        .collect();
    assert_eq!(
        items,
        [
            (Some(TicketSnapshot::NAME.into()), "ticket_id".into()),
            (
                Some(StaffWorkload::NAME.into()),
                "candidate_staff_ids".into()
            ),
        ]
    );
}
