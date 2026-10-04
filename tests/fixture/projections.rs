//! Every helpdesk projection's fold. `skilj-test-fixture` folds all given
//! events into one state under the key `""`, without applying `keys()` -
//! so each test gives only events for one instance (one ticket for
//! `TicketSummary`, one company for `CompanyTicketList`), and the one
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
        .event(resolved("t1", "acme"))
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
                events.push(reopened("t1", "acme"));
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

// --- CompanyTicketList ---

#[test]
fn company_ticket_list_keeps_every_ticket_with_its_conversation() {
    GivenEvents::<CompanyTicketList>::new()
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
            let t2 = &state.tickets["t2"];
            assert_eq!(t2.status, "open");
            assert_eq!(t2.title, "t2 title");
            assert_eq!(t2.rating, Some(3));
        });
}

#[test]
fn company_ticket_list_marks_the_duplicate_of_a_merge() {
    GivenEvents::<CompanyTicketList>::new()
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
fn internal_notes_never_reach_the_customer_visible_ticket_list() {
    GivenEvents::<CompanyTicketList>::new()
        .events(open_ticket("t1", "acme"))
        .event(note_added("t1", "acme", "staff eyes only"))
        .then(|state| {
            let rendered = serde_json::to_string(state).expect("state serializes");
            assert!(!rendered.contains("staff eyes only"), "{rendered}");
        });
}

#[test]
fn company_ticket_list_skips_events_for_tickets_it_never_saw_created() {
    GivenEvents::<CompanyTicketList>::new()
        .event(assigned("ghost", "acme", "staff-1"))
        .event(resolved("ghost", "acme"))
        .then(|state| assert!(state.tickets.is_empty()));
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
