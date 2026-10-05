//! Every helpdesk projection's fold. `skilj-test-fixture` folds all given
//! events into one state under the key `""`, without applying `keys()` -
//! so each test gives only events for one instance (one ticket for
//! `TicketSummary`, one company for `CompanyTicketQueue`, one customer for `CustomerTickets`), and the one
//! fold that reads its key (`TicketSummary` on `TicketsMerged`) is tested
//! through `Projection::project` directly.

use skilj::Projection;
use skilj_helpdesk::helpdesk::*;
use skilj_test_fixture::projection::GivenEvents;

use crate::events::*;

// --- TicketSummary ---

#[test]
fn ticket_summary_follows_a_ticket_through_its_whole_lifecycle() {
    GivenEvents::<TicketSummary>::new()
        .event(created("t1", "acme", TicketPriority::Low))
        .event(assigned("t1", "acme", "staff-1"))
        .event(info_requested("t1", "acme", "which version?"))
        .event(customer_responded("t1", "acme", "2.1"))
        .event(escalated(
            "t1",
            "acme",
            TicketPriority::Low,
            TicketPriority::Medium,
        ))
        .event(resolved("t1", "acme", 1))
        .event(rated("t1", "acme", 4))
        .event(closed("t1", "acme"))
        .then_state(TicketSummaryState {
            status: Some("closed".into()),
            priority: Some("medium".into()),
            assigned_staff_id: Some("staff-1".into()),
            escalated: true,
            rating: Some(4),
        });
}

#[test]
fn ticket_summary_shows_each_intermediate_status() {
    let cases: [(Vec<HelpdeskEvent>, &str); 5] = [
        (open_ticket("t1", "acme"), "open"),
        (in_progress_ticket("t1", "acme"), "in_progress"),
        (waiting_ticket("t1", "acme"), "waiting_on_customer"),
        (resolved_ticket("t1", "acme"), "resolved"),
        (
            {
                let mut events = resolved_ticket("t1", "acme");
                events.push(reopened("t1", "acme", 1));
                events
            },
            "in_progress",
        ),
    ];
    for (history, expected) in cases {
        GivenEvents::<TicketSummary>::new()
            .events(history)
            .then(|state| assert_eq!(state.status.as_deref(), Some(expected)));
    }
}

#[test]
fn ticket_summary_ignores_internal_notes_and_company_events() {
    GivenEvents::<TicketSummary>::new()
        .event(signed_up("acme"))
        .events(open_ticket("t1", "acme"))
        .event(note_added("t1", "acme", "staff eyes only"))
        .event(expired("acme"))
        .then_state(TicketSummaryState {
            status: Some("open".into()),
            priority: Some("medium".into()),
            assigned_staff_id: None,
            escalated: false,
            rating: None,
        });
}

#[test]
fn a_merge_marks_only_the_duplicate_ticket_as_merged() {
    let merge = merged("t1", "t2", "acme");
    assert_eq!(
        TicketSummary::keys(&merge),
        vec!["t1".to_string(), "t2".to_string()]
    );

    let mut primary = TicketSummaryState::default();
    TicketSummary::project(
        &mut primary,
        &created("t1", "acme", TicketPriority::Medium),
        "t1",
    );
    TicketSummary::project(&mut primary, &merge, "t1");
    assert_eq!(primary.status.as_deref(), Some("open"));

    let mut duplicate = TicketSummaryState::default();
    TicketSummary::project(
        &mut duplicate,
        &created("t2", "acme", TicketPriority::Medium),
        "t2",
    );
    TicketSummary::project(&mut duplicate, &merge, "t2");
    assert_eq!(duplicate.status.as_deref(), Some("merged"));
}

// --- CompanyTicketQueue ---

#[test]
fn company_ticket_queue_follows_every_ticket_of_the_company() {
    GivenEvents::<CompanyTicketQueue>::new()
        .events(open_ticket("t1", "acme"))
        .events(open_ticket("t2", "acme"))
        .event(assigned("t1", "acme", "staff-1"))
        .event(info_requested("t1", "acme", "which version?"))
        .event(customer_responded("t1", "acme", "2.1"))
        .event(rated("t2", "acme", 3))
        .then(|state| {
            assert_eq!(state.tickets.len(), 2);
            let t1 = &state.tickets["t1"];
            assert_eq!(t1.status, "in_progress");
            assert_eq!(t1.assigned_staff_id.as_deref(), Some("staff-1"));
            assert_eq!(t1.requester_id, "customer-1");
            let t2 = &state.tickets["t2"];
            assert_eq!(t2.status, "open");
            assert_eq!(t2.rating, Some(3));
        });
}

#[test]
fn company_ticket_queue_holds_nothing_a_customer_wrote() {
    // That text is encrypted under each customer's own key, which a
    // company-keyed row could never decrypt - see `CompanyTicketQueue`.
    GivenEvents::<CompanyTicketQueue>::new()
        .events(open_ticket("t1", "acme"))
        .event(assigned("t1", "acme", "staff-1"))
        .event(info_requested("t1", "acme", "which version?"))
        .event(customer_responded("t1", "acme", "2.1"))
        .event(resolved("t1", "acme", 1))
        .event(rated("t1", "acme", 3))
        .then(|state| {
            let rendered = serde_json::to_string(state).expect("state serializes");
            for text in [
                "t1 title",
                "t1 description",
                "which version?",
                "2.1",
                "t1 comment",
            ] {
                assert!(!rendered.contains(text), "{text:?} in {rendered}");
            }
        });
}

#[test]
fn company_ticket_queue_marks_the_duplicate_of_a_merge() {
    GivenEvents::<CompanyTicketQueue>::new()
        .events(open_ticket("t1", "acme"))
        .events(open_ticket("t2", "acme"))
        .event(escalated(
            "t1",
            "acme",
            TicketPriority::Medium,
            TicketPriority::High,
        ))
        .event(merged("t1", "t2", "acme"))
        .then(|state| {
            assert_eq!(state.tickets["t1"].status, "open");
            assert_eq!(state.tickets["t1"].priority, "high");
            assert!(state.tickets["t1"].escalated);
            assert_eq!(state.tickets["t2"].status, "merged");
        });
}

#[test]
fn company_ticket_queue_skips_events_for_tickets_it_never_saw_created() {
    GivenEvents::<CompanyTicketQueue>::new()
        .event(assigned("ghost", "acme", "staff-1"))
        .event(resolved("ghost", "acme", 1))
        .then(|state| assert!(state.tickets.is_empty()));
}

// --- CustomerTickets ---

#[test]
fn customer_tickets_keeps_each_ticket_with_its_conversation() {
    GivenEvents::<CustomerTickets>::new()
        .events(open_ticket("t1", "acme"))
        .events(open_ticket("t2", "acme"))
        .event(assigned("t1", "acme", "staff-1"))
        .event(info_requested("t1", "acme", "which version?"))
        .event(customer_responded("t1", "acme", "2.1"))
        .event(rated("t2", "acme", 3))
        .then(|state| {
            assert_eq!(state.tickets.len(), 2);
            let t1 = &state.tickets["t1"];
            assert_eq!(t1.title, "t1 title");
            assert_eq!(t1.description, "t1 description");
            let thread: Vec<_> = t1
                .messages
                .iter()
                .map(|m| (m.from_staff, m.author_id.as_str(), m.text.as_str()))
                .collect();
            assert_eq!(
                thread,
                vec![
                    (true, "staff-1", "which version?"),
                    (false, "customer-1", "2.1"),
                ]
            );
            assert_eq!(
                state.tickets["t2"].rating_comment.as_deref(),
                Some("t2 comment")
            );
        });
}

#[test]
fn customer_tickets_is_keyed_by_the_customer_the_text_is_encrypted_under() {
    for event in [
        created("t1", "acme", TicketPriority::Low),
        info_requested("t1", "acme", "?"),
        customer_responded("t1", "acme", "!"),
        rated("t1", "acme", 5),
    ] {
        assert_eq!(
            CustomerTickets::keys(&event),
            vec!["customer-1".to_string()]
        );
    }
    // Stored before these events named their requester: no row to put
    // them in.
    let HelpdeskEvent::TicketInfoRequested(mut legacy) = info_requested("t1", "acme", "?") else {
        unreachable!()
    };
    legacy.requester_id = None;
    assert!(CustomerTickets::keys(&HelpdeskEvent::TicketInfoRequested(legacy)).is_empty());
    assert!(CustomerTickets::keys(&assigned("t1", "acme", "staff-1")).is_empty());
}

#[test]
fn internal_notes_never_reach_a_customer_visible_projection() {
    let note = || note_added("t1", "acme", "staff eyes only");
    GivenEvents::<CompanyTicketQueue>::new()
        .events(open_ticket("t1", "acme"))
        .event(note())
        .then(|state| {
            let rendered = serde_json::to_string(state).expect("state serializes");
            assert!(!rendered.contains("staff eyes only"), "{rendered}");
        });
    GivenEvents::<CustomerTickets>::new()
        .events(open_ticket("t1", "acme"))
        .event(note())
        .then(|state| {
            let rendered = serde_json::to_string(state).expect("state serializes");
            assert!(!rendered.contains("staff eyes only"), "{rendered}");
        });
}

// --- TicketInternalNotes / TenantDirectory ---

#[test]
fn internal_notes_are_kept_in_order() {
    GivenEvents::<TicketInternalNotes>::new()
        .event(note_added("t1", "acme", "first"))
        .event(note_added("t1", "acme", "second"))
        .then(|state| {
            let notes: Vec<_> = state.notes.iter().map(|n| n.note.as_str()).collect();
            assert_eq!(notes, vec!["first", "second"]);
        });
}

#[test]
fn tenant_directory_records_the_provisioned_tenant() {
    GivenEvents::<TenantDirectory>::new()
        .event(signed_up("acme"))
        .event(tenant_provisioned("acme", "tenant-acme"))
        .then(|state| assert_eq!(state.tenant_name.as_deref(), Some("tenant-acme")));
}
