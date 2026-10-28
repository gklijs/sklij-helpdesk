//! `TicketSnapshot` as the single-ticket commands and `MergeTickets` read
//! it: a snapshot item in their consistency query (skilj
//! docs/architecture.md §202). skilj folds the state itself, with
//! `TicketSnapshot::fold_for`, once for every ticket an event carries, so
//! what is checked here is that fold: from the default state, with only
//! the tag value to say which ticket it is, it must reach the
//! `TicketFacts` the ticket's history gives (`TicketFacts::of`), both
//! sides of a merge included.
//!
//! `GivenTicket` is the given/when/then the ticket tests use: it folds
//! the given events into each ticket's state the same way, and hands
//! `decide_with()` those states.

use std::marker::PhantomData;

use skilj::{CommandType, Snapshot};
use skilj_core::plugin::{Decision, DecisionContext, SnapshotState};
use skilj_core::shared::CommandDecision;
use skilj_helpdesk::helpdesk::*;
use skilj_test_fixture::command::{CommandOutcome, GivenEvents};

use crate::consistency_query::{derive, selected_events, Described};
use crate::events::*;

/// `ticket`'s state, folded as skilj folds it: every event carrying the
/// tag `ticket: <ticket>`, from the default state.
pub(crate) fn ticket_state(history: &[HelpdeskEvent], ticket: &str) -> TicketFacts {
    let tag = ("ticket".to_string(), ticket.to_string());
    let mut state = TicketFacts::default();
    for event in history {
        let (event_type, payload) = event.describe();
        if derive(&HelpdeskEvent::tag_mappings(event_type), &payload).contains(&tag) {
            TicketSnapshot::fold_for(&mut state, ticket, event);
        }
    }
    state
}

/// `GivenEvents` for a command whose query is `TicketSnapshot` items:
/// the events given are folded into the state of every ticket the
/// payload names, which is what `decide_with()` is handed.
pub(crate) struct GivenTicket<C> {
    events: Vec<HelpdeskEvent>,
    /// `Some(scheduled_at)` makes the decision a deadline firing.
    deadline: Option<chrono::DateTime<chrono::Utc>>,
    command: PhantomData<C>,
}

impl<C: CommandType<Event = HelpdeskEvent>> GivenTicket<C> {
    pub(crate) fn new() -> Self {
        Self {
            events: Vec::new(),
            deadline: None,
            command: PhantomData,
        }
    }

    /// Decides as skilj's deadline loop firing a deadline due at
    /// `scheduled_at` (`Decision::deadline`, skilj 0.2.0).
    pub(crate) fn deadline(mut self, scheduled_at: chrono::DateTime<chrono::Utc>) -> Self {
        self.deadline = Some(scheduled_at);
        self
    }

    pub(crate) fn event(mut self, event: HelpdeskEvent) -> Self {
        self.events.push(event);
        self
    }

    pub(crate) fn events(mut self, events: impl IntoIterator<Item = HelpdeskEvent>) -> Self {
        self.events.extend(events);
        self
    }

    pub(crate) fn when(self, payload: C::Payload) -> CommandOutcome {
        let json = serde_json::to_value(&payload).unwrap();
        // Beside its snapshot states, a command whose query also has
        // plain items (`ApplyMacro`'s macro) is handed the events those
        // select.
        let mut given = GivenEvents::<C>::new().events(selected_events::<C>(&self.events, &json));
        if let Some(scheduled_at) = self.deadline {
            given = given.now(scheduled_at).deadline(scheduled_at, None);
        }
        for item in C::consistency_query() {
            if item.snapshot.is_none() {
                continue;
            }
            assert_eq!(
                item.snapshot.as_deref(),
                Some(TicketSnapshot::NAME),
                "{}",
                C::NAME
            );
            for (_, ticket) in derive(&item.tag_mappings, &json) {
                given =
                    given.snapshot::<TicketSnapshot>(&ticket, ticket_state(&self.events, &ticket));
            }
        }
        given.when(payload)
    }
}

/// Histories for ticket `t1`, each with the events tagged `ticket: t1`.
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
    long.push(sla_breached(
        "t1",
        "acme",
        skilj_helpdesk::sla::SlaKind::Resolution,
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
        // A macro's reply leaves the status, its priority change moves
        // the priority off the created one, and an escalation after it
        // bumps from there.
        {
            let mut events = in_progress_ticket("t1", "acme");
            events.push(staff_replied("t1", "acme", "on it"));
            events.push(priority_changed(
                "t1",
                "acme",
                TicketPriority::Medium,
                TicketPriority::Low,
            ));
            events.push(escalated(
                "t1",
                "acme",
                TicketPriority::Low,
                TicketPriority::Medium,
            ));
            events
        },
    ]
}

fn then(mut events: Vec<HelpdeskEvent>, event: HelpdeskEvent) -> Vec<HelpdeskEvent> {
    events.push(event);
    events
}

#[test]
fn the_snapshot_folds_a_ticket_from_its_tag_value_alone() {
    for (i, history) in histories().into_iter().enumerate() {
        let state = ticket_state(&history, "t1");
        let mut expected = TicketFacts::of(&history, "t1");
        if history.is_empty() {
            // Nothing folded: the default state, whose id
            // `decide_on_ticket` fills in.
            expected.ticket_id.clear();
        }
        assert_eq!(state, expected, "history {i}");
    }
    let long = ticket_state(&histories()[6], "t1");
    assert_eq!(long.status, Some(TicketStatus::Closed));
    assert_eq!(long.resolutions, 2);
    assert!(long.escalated && long.rated);
    assert!(long.first_responded && long.resolution_breached && !long.first_response_breached);
    assert_eq!(long.created_priority, Some(TicketPriority::Medium));
    assert_eq!(long.current_priority(), Some(TicketPriority::High));

    let changed = ticket_state(&histories()[9], "t1");
    assert_eq!(changed.status, Some(TicketStatus::InProgress));
    assert_eq!(changed.created_priority, Some(TicketPriority::Medium));
    assert_eq!(changed.current_priority(), Some(TicketPriority::Medium));
    assert!(changed.escalated);
}

#[test]
fn a_merge_is_folded_into_both_tickets_and_changes_only_the_duplicate() {
    let mut history = open_ticket("t1", "acme");
    history.extend(in_progress_ticket("t2", "acme"));
    history.push(merged("t1", "t2", "acme"));
    assert_eq!(
        ticket_state(&history, "t1").status,
        Some(TicketStatus::Open)
    );
    assert_eq!(
        ticket_state(&history, "t2").status,
        Some(TicketStatus::Merged)
    );
}

fn assign() -> AssignTicketPayload {
    AssignTicketPayload {
        ticket_id: "t1".into(),
        staff_id: "staff-1".into(),
    }
}

#[test]
fn every_ticket_command_reads_its_ticket_through_the_snapshot() {
    fn snapshot_items<C: CommandType>() -> Vec<String> {
        C::consistency_query()
            .into_iter()
            .map(|item| {
                assert_eq!(
                    item.snapshot.as_deref(),
                    Some(TicketSnapshot::NAME),
                    "{}",
                    C::NAME
                );
                assert!(item.event_types.is_empty() && item.latest.is_none());
                item.tag_mappings.iter().map(|m| m.field.clone()).collect()
            })
            .collect()
    }
    let single = ["ticket_id".to_string()];
    assert_eq!(snapshot_items::<AssignTicket>(), single);
    assert_eq!(snapshot_items::<ResolveTicket>(), single);
    assert_eq!(snapshot_items::<ReopenTicket>(), single);
    assert_eq!(snapshot_items::<RequestInfoFromCustomer>(), single);
    assert_eq!(snapshot_items::<CustomerRespondsToTicket>(), single);
    assert_eq!(snapshot_items::<CloseTicket>(), single);
    assert_eq!(snapshot_items::<EscalateTicket>(), single);
    assert_eq!(snapshot_items::<RateTicket>(), single);
    assert_eq!(snapshot_items::<AddInternalNote>(), single);
    assert_eq!(snapshot_items::<BreachTicketSla>(), single);
    assert_eq!(
        snapshot_items::<MergeTickets>(),
        ["primary_ticket_id", "duplicate_ticket_id"]
    );

    // The ticket through the snapshot, the macro as its latest event.
    let items = ApplyMacro::consistency_query();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].snapshot.as_deref(), Some(TicketSnapshot::NAME));
    assert_eq!(items[0].tag_mappings[0].field, "ticket_id");
    assert!(items[1].snapshot.is_none());
    assert_eq!(items[1].latest, Some(1));
}

#[test]
fn the_events_beside_the_snapshot_are_not_what_decides() {
    // Only the state counts: an open ticket's state with no events is
    // assigned, events saying otherwise are ignored.
    let state = ticket_state(&open_ticket("t1", "acme"), "t1");
    GivenEvents::<AssignTicket>::new()
        .events(closed_ticket("t1", "acme"))
        .snapshot::<TicketSnapshot>("t1", state)
        .when(assign())
        .then(|decision| {
            assert!(
                matches!(decision, CommandDecision::Accepted { .. }),
                "{decision:?}"
            )
        });
}

#[test]
fn a_missing_or_unreadable_state_is_rejected_rather_than_trusted() {
    let unreadable = [SnapshotState {
        snapshot: TicketSnapshot::NAME.into(),
        tag_value: "t1".into(),
        state_json: r#"{"status": 42}"#.into(),
        folded_through: -1,
    }];
    for states in [&unreadable[..], &[]] {
        let decision = AssignTicket::decide_with(
            &assign(),
            &Decision::new(&[], states, &DecisionContext::at(chrono::Utc::now())),
        );
        assert!(
            matches!(&decision, CommandDecision::Rejected { kind, .. } if kind == "snapshot_unreadable"),
            "{decision:?}"
        );
    }
}
