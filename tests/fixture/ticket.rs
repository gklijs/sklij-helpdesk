//! The ticket lifecycle: every command's accepted transition and every
//! rejection kind it can return.

use serde_json::json;
use skilj::ScheduleDeadline;
use skilj_core::shared::Tag;
use skilj_helpdesk::helpdesk::*;
use skilj_helpdesk::scheduling;
use skilj_test_fixture::command::GivenEvents;

use crate::events::*;

// --- CreateTicket ---

fn create(ticket_id: &str) -> CreateTicketPayload {
    CreateTicketPayload {
        ticket_id: ticket_id.into(),
        company_id: "acme".into(),
        requester_id: "customer-1".into(),
        logged_by_staff_id: None,
        title: "Login broken".into(),
        description: "Can't log in since this morning".into(),
        priority: TicketPriority::High,
    }
}

#[test]
fn a_trialing_company_can_create_tickets() {
    // The spec bug this project once shipped: the guard blocked tickets
    // for the whole free trial, not just after it expired.
    GivenEvents::<CreateTicket>::new()
        .event(signed_up("acme"))
        .when(create("t1"))
        .then_accepted(vec![spec(
            "TicketCreated",
            json!({
                "ticket_id": "t1",
                "company_id": "acme",
                "requester_id": "customer-1",
                "logged_by_staff_id": null,
                "title": "Login broken",
                "description": "Can't log in since this morning",
                "priority": "high",
            }),
        )]);
}

#[test]
fn active_and_reactivated_companies_can_create_tickets() {
    for history in [
        vec![signed_up("acme"), activated("acme")],
        vec![signed_up("acme"), expired("acme"), activated("acme")],
    ] {
        GivenEvents::<CreateTicket>::new()
            .events(history)
            .when(create("t1"))
            .then(assert_accepted);
    }
}

#[test]
fn an_expired_or_unknown_company_cannot_create_tickets() {
    GivenEvents::<CreateTicket>::new()
        .events([signed_up("acme"), expired("acme")])
        .when(create("t1"))
        .then_rejected("company_expired");
    GivenEvents::<CreateTicket>::new()
        .when(create("t1"))
        .then_rejected("company_not_found");
}

#[test]
fn a_mirrored_lifecycle_guards_ticket_creation_like_the_original() {
    // Inside a tenant, the mirror is the only lifecycle history there is.
    GivenEvents::<CreateTicket>::new()
        .event(mirrored("acme", CompanyStatus::Expired))
        .when(create("t1"))
        .then_rejected("company_expired");
    // And a later mirror wins over an earlier shared-context event.
    GivenEvents::<CreateTicket>::new()
        .events([
            signed_up("acme"),
            expired("acme"),
            mirrored("acme", CompanyStatus::Active),
        ])
        .when(create("t1"))
        .then(assert_accepted);
}

#[test]
fn a_ticket_id_can_only_be_used_once() {
    GivenEvents::<CreateTicket>::new()
        .event(signed_up("acme"))
        .events(open_ticket("t1", "acme"))
        .when(create("t1"))
        .then_rejected("ticket_already_exists");
}

// --- AssignTicket ---

#[test]
fn an_open_ticket_can_be_assigned() {
    GivenEvents::<AssignTicket>::new()
        .events(open_ticket("t1", "acme"))
        .when(AssignTicketPayload {
            ticket_id: "t1".into(),
            staff_id: "staff-2".into(),
        })
        .then_accepted(vec![spec(
            "TicketAssigned",
            json!({ "ticket_id": "t1", "company_id": "acme", "staff_id": "staff-2" }),
        )]);
}

#[test]
fn only_an_open_ticket_can_be_assigned() {
    for history in [
        in_progress_ticket("t1", "acme"),
        waiting_ticket("t1", "acme"),
        resolved_ticket("t1", "acme"),
        closed_ticket("t1", "acme"),
    ] {
        GivenEvents::<AssignTicket>::new()
            .events(history)
            .when(AssignTicketPayload {
                ticket_id: "t1".into(),
                staff_id: "staff-2".into(),
            })
            .then_rejected("ticket_not_open");
    }
}

#[test]
fn another_tickets_history_does_not_count() {
    GivenEvents::<AssignTicket>::new()
        .events(open_ticket("t2", "acme"))
        .when(AssignTicketPayload {
            ticket_id: "t1".into(),
            staff_id: "staff-1".into(),
        })
        .then_rejected("ticket_not_found");
}

// --- ResolveTicket / ReopenTicket / CloseTicket ---

#[test]
fn an_in_progress_ticket_resolves() {
    GivenEvents::<ResolveTicket>::new()
        .events(in_progress_ticket("t1", "acme"))
        .when(ResolveTicketPayload {
            ticket_id: "t1".into(),
        })
        .then_accepted(vec![spec(
            "TicketResolved",
            json!({ "ticket_id": "t1", "company_id": "acme" }),
        )]);
}

#[test]
fn a_ticket_not_in_progress_cannot_be_resolved() {
    for history in [
        open_ticket("t1", "acme"),
        waiting_ticket("t1", "acme"),
        resolved_ticket("t1", "acme"),
        closed_ticket("t1", "acme"),
    ] {
        GivenEvents::<ResolveTicket>::new()
            .events(history)
            .when(ResolveTicketPayload {
                ticket_id: "t1".into(),
            })
            .then_rejected("ticket_not_in_progress");
    }
}

#[test]
fn a_resolved_ticket_reopens_and_can_be_resolved_again() {
    GivenEvents::<ReopenTicket>::new()
        .events(resolved_ticket("t1", "acme"))
        .when(ReopenTicketPayload {
            ticket_id: "t1".into(),
        })
        .then_accepted(vec![spec(
            "TicketReopened",
            json!({ "ticket_id": "t1", "company_id": "acme" }),
        )]);
    let mut history = resolved_ticket("t1", "acme");
    history.push(reopened("t1", "acme"));
    GivenEvents::<ResolveTicket>::new()
        .events(history)
        .when(ResolveTicketPayload {
            ticket_id: "t1".into(),
        })
        .then(assert_accepted);
}

#[test]
fn only_a_resolved_ticket_reopens() {
    for history in [
        open_ticket("t1", "acme"),
        in_progress_ticket("t1", "acme"),
        closed_ticket("t1", "acme"),
    ] {
        GivenEvents::<ReopenTicket>::new()
            .events(history)
            .when(ReopenTicketPayload {
                ticket_id: "t1".into(),
            })
            .then_rejected("ticket_not_resolved");
    }
}

#[test]
fn a_resolved_ticket_closes() {
    GivenEvents::<CloseTicket>::new()
        .events(resolved_ticket("t1", "acme"))
        .when(CloseTicketPayload {
            ticket_id: "t1".into(),
        })
        .then_accepted(vec![spec(
            "TicketClosed",
            json!({ "ticket_id": "t1", "company_id": "acme" }),
        )]);
}

#[test]
fn an_auto_close_that_lost_the_race_to_a_reopen_is_rejected() {
    // ScheduleTicketAutoClose has no CancelDeadline - this guard is what
    // stops a reopened ticket from closing when its old deadline fires.
    let mut history = resolved_ticket("t1", "acme");
    history.push(reopened("t1", "acme"));
    GivenEvents::<CloseTicket>::new()
        .events(history)
        .when(CloseTicketPayload {
            ticket_id: "t1".into(),
        })
        .then_rejected("ticket_not_resolved");
    GivenEvents::<CloseTicket>::new()
        .events(closed_ticket("t1", "acme"))
        .when(CloseTicketPayload {
            ticket_id: "t1".into(),
        })
        .then_rejected("ticket_not_resolved");
}

#[test]
fn resolving_schedules_an_auto_close_for_that_ticket() {
    let before = chrono::Utc::now();
    let deadline = ScheduleTicketAutoClose::schedule(&TicketResolvedPayload {
        ticket_id: "t1".into(),
        company_id: "acme".into(),
    })
    .expect("every resolution schedules an auto-close");
    let after = chrono::Utc::now();
    assert_eq!(deadline.payload.ticket_id, "t1");
    assert_eq!(
        deadline.tags,
        vec![Tag {
            key: "ticket".into(),
            value: Some("t1".into())
        }]
    );
    let wait = scheduling::auto_close_after();
    assert!(deadline.fire_at >= before + wait && deadline.fire_at <= after + wait);
}

// --- RequestInfoFromCustomer / CustomerRespondsToTicket ---

#[test]
fn staff_ask_and_the_customer_answers() {
    GivenEvents::<RequestInfoFromCustomer>::new()
        .events(in_progress_ticket("t1", "acme"))
        .when(RequestInfoFromCustomerPayload {
            ticket_id: "t1".into(),
            staff_id: "staff-1".into(),
            message: "Which browser?".into(),
        })
        .then_accepted(vec![spec(
            "TicketInfoRequested",
            json!({ "ticket_id": "t1", "company_id": "acme", "staff_id": "staff-1", "message": "Which browser?" }),
        )]);
    GivenEvents::<CustomerRespondsToTicket>::new()
        .events(waiting_ticket("t1", "acme"))
        .when(CustomerRespondsToTicketPayload {
            ticket_id: "t1".into(),
            requester_id: "customer-1".into(),
            message: "Firefox".into(),
        })
        .then_accepted(vec![spec(
            "TicketCustomerResponded",
            json!({ "ticket_id": "t1", "company_id": "acme", "requester_id": "customer-1", "message": "Firefox" }),
        )]);
}

#[test]
fn info_can_only_be_requested_on_an_in_progress_ticket() {
    for history in [
        open_ticket("t1", "acme"),
        waiting_ticket("t1", "acme"),
        resolved_ticket("t1", "acme"),
    ] {
        GivenEvents::<RequestInfoFromCustomer>::new()
            .events(history)
            .when(RequestInfoFromCustomerPayload {
                ticket_id: "t1".into(),
                staff_id: "staff-1".into(),
                message: "?".into(),
            })
            .then_rejected("ticket_not_in_progress");
    }
}

#[test]
fn a_customer_can_only_respond_when_asked() {
    let mut answered = waiting_ticket("t1", "acme");
    answered.push(customer_responded("t1", "acme", "Firefox"));
    for history in [
        open_ticket("t1", "acme"),
        in_progress_ticket("t1", "acme"),
        answered,
    ] {
        GivenEvents::<CustomerRespondsToTicket>::new()
            .events(history)
            .when(CustomerRespondsToTicketPayload {
                ticket_id: "t1".into(),
                requester_id: "customer-1".into(),
                message: "again".into(),
            })
            .then_rejected("ticket_not_waiting_on_customer");
    }
}

// --- EscalateTicket ---

fn escalation_of(priority: TicketPriority) -> (TicketPriority, TicketPriority) {
    let decision = GivenEvents::<EscalateTicket>::new()
        .event(created("t1", "acme", priority))
        .when(EscalateTicketPayload {
            ticket_id: "t1".into(),
        })
        .into_decision();
    let skilj_core::shared::CommandDecision::Accepted { events } = decision else {
        panic!("expected an escalation of a {priority:?} ticket to be accepted, got {decision:?}");
    };
    let payload: TicketEscalatedPayload =
        serde_json::from_value(events[0].payload.clone()).expect("a TicketEscalated payload");
    assert_eq!(payload.company_id, "acme");
    (payload.previous_priority, payload.new_priority)
}

#[test]
fn escalation_bumps_priority_one_step_capped_at_urgent() {
    use TicketPriority::*;
    assert_eq!(escalation_of(Low), (Low, Medium));
    assert_eq!(escalation_of(Medium), (Medium, High));
    assert_eq!(escalation_of(High), (High, Urgent));
    assert_eq!(escalation_of(Urgent), (Urgent, Urgent));
}

#[test]
fn any_unhandled_ticket_can_be_escalated_once() {
    for history in [
        open_ticket("t1", "acme"),
        in_progress_ticket("t1", "acme"),
        waiting_ticket("t1", "acme"),
    ] {
        GivenEvents::<EscalateTicket>::new()
            .events(history)
            .when(EscalateTicketPayload {
                ticket_id: "t1".into(),
            })
            .then(assert_accepted);
    }
    let mut history = open_ticket("t1", "acme");
    history.push(escalated(
        "t1",
        "acme",
        TicketPriority::Medium,
        TicketPriority::High,
    ));
    GivenEvents::<EscalateTicket>::new()
        .events(history)
        .when(EscalateTicketPayload {
            ticket_id: "t1".into(),
        })
        .then_rejected("already_escalated");
}

#[test]
fn a_handled_ticket_is_not_escalated() {
    let mut merged_away = open_ticket("t1", "acme");
    merged_away.extend(open_ticket("t0", "acme"));
    merged_away.push(merged("t0", "t1", "acme"));
    for history in [
        resolved_ticket("t1", "acme"),
        closed_ticket("t1", "acme"),
        merged_away,
    ] {
        GivenEvents::<EscalateTicket>::new()
            .events(history)
            .when(EscalateTicketPayload {
                ticket_id: "t1".into(),
            })
            .then_rejected("ticket_not_unhandled");
    }
}

// --- MergeTickets ---

fn merge(primary: &str, duplicate: &str) -> MergeTicketsPayload {
    MergeTicketsPayload {
        primary_ticket_id: primary.into(),
        duplicate_ticket_id: duplicate.into(),
    }
}

#[test]
fn two_tickets_of_one_company_merge() {
    GivenEvents::<MergeTickets>::new()
        .events(resolved_ticket("t1", "acme"))
        .events(open_ticket("t2", "acme"))
        .when(merge("t1", "t2"))
        .then_accepted(vec![spec(
            "TicketsMerged",
            json!({ "primary_ticket_id": "t1", "duplicate_ticket_id": "t2", "company_id": "acme" }),
        )]);
}

#[test]
fn merge_rejections_name_the_side_that_failed() {
    GivenEvents::<MergeTickets>::new()
        .events(open_ticket("t1", "acme"))
        .when(merge("t1", "t1"))
        .then_rejected("cannot_merge_ticket_into_itself");
    GivenEvents::<MergeTickets>::new()
        .events(open_ticket("t2", "acme"))
        .when(merge("t1", "t2"))
        .then_rejected("primary_ticket_not_found");
    GivenEvents::<MergeTickets>::new()
        .events(open_ticket("t1", "acme"))
        .when(merge("t1", "t2"))
        .then_rejected("duplicate_ticket_not_found");
    GivenEvents::<MergeTickets>::new()
        .events(closed_ticket("t1", "acme"))
        .events(open_ticket("t2", "acme"))
        .when(merge("t1", "t2"))
        .then_rejected("primary_ticket_not_mergeable");
    GivenEvents::<MergeTickets>::new()
        .events(open_ticket("t1", "acme"))
        .events(closed_ticket("t2", "acme"))
        .when(merge("t1", "t2"))
        .then_rejected("duplicate_ticket_not_mergeable");
}

#[test]
fn a_ticket_merged_away_cannot_be_merged_again_from_either_side() {
    let mut history = open_ticket("t1", "acme");
    history.extend(open_ticket("t2", "acme"));
    history.extend(open_ticket("t3", "acme"));
    history.push(merged("t1", "t2", "acme"));
    GivenEvents::<MergeTickets>::new()
        .events(history)
        .when(merge("t2", "t3"))
        .then_rejected("primary_ticket_not_mergeable");
    let mut history = open_ticket("t1", "acme");
    history.extend(open_ticket("t2", "acme"));
    history.extend(open_ticket("t3", "acme"));
    history.push(merged("t1", "t2", "acme"));
    GivenEvents::<MergeTickets>::new()
        .events(history)
        .when(merge("t3", "t2"))
        .then_rejected("duplicate_ticket_not_mergeable");
}

#[test]
fn tickets_of_different_companies_never_merge() {
    GivenEvents::<MergeTickets>::new()
        .events(open_ticket("t1", "acme"))
        .events(open_ticket("t2", "globex"))
        .when(merge("t1", "t2"))
        .then_rejected("tickets_belong_to_different_companies");
}

// --- RateTicket ---

fn rate(rating: u8) -> RateTicketPayload {
    RateTicketPayload {
        ticket_id: "t1".into(),
        rating,
        comment: Some("quick fix".into()),
    }
}

#[test]
fn a_resolved_or_closed_ticket_can_be_rated() {
    for history in [resolved_ticket("t1", "acme"), closed_ticket("t1", "acme")] {
        GivenEvents::<RateTicket>::new()
            .events(history)
            .when(rate(5))
            .then_accepted(vec![spec(
                "TicketRated",
                json!({ "ticket_id": "t1", "company_id": "acme", "rating": 5, "comment": "quick fix" }),
            )]);
    }
}

#[test]
fn ratings_outside_one_to_five_are_rejected() {
    for rating in [0, 6, 255] {
        GivenEvents::<RateTicket>::new()
            .events(resolved_ticket("t1", "acme"))
            .when(rate(rating))
            .then_rejected("invalid_rating");
    }
}

#[test]
fn a_ticket_is_rated_once_and_only_once_handled() {
    let mut history = resolved_ticket("t1", "acme");
    history.push(rated("t1", "acme", 4));
    GivenEvents::<RateTicket>::new()
        .events(history)
        .when(rate(5))
        .then_rejected("already_rated");
    for history in [
        open_ticket("t1", "acme"),
        in_progress_ticket("t1", "acme"),
        waiting_ticket("t1", "acme"),
    ] {
        GivenEvents::<RateTicket>::new()
            .events(history)
            .when(rate(5))
            .then_rejected("ticket_not_ratable");
    }
    GivenEvents::<RateTicket>::new()
        .when(rate(5))
        .then_rejected("ticket_not_found");
}

// --- AddInternalNote ---

#[test]
fn staff_can_note_a_ticket_in_any_status() {
    for history in [
        open_ticket("t1", "acme"),
        waiting_ticket("t1", "acme"),
        closed_ticket("t1", "acme"),
    ] {
        GivenEvents::<AddInternalNote>::new()
            .events(history)
            .when(AddInternalNotePayload {
                ticket_id: "t1".into(),
                staff_id: "staff-1".into(),
                note: "customer is a VIP".into(),
            })
            .then_accepted(vec![spec(
                "TicketInternalNoteAdded",
                json!({ "ticket_id": "t1", "company_id": "acme", "staff_id": "staff-1", "note": "customer is a VIP" }),
            )]);
    }
    GivenEvents::<AddInternalNote>::new()
        .when(AddInternalNotePayload {
            ticket_id: "t1".into(),
            staff_id: "staff-1".into(),
            note: "?".into(),
        })
        .then_rejected("ticket_not_found");
}

// --- every ticket command on a ticket that doesn't exist ---

#[test]
fn every_ticket_command_rejects_an_unknown_ticket() {
    GivenEvents::<ResolveTicket>::new()
        .when(ResolveTicketPayload {
            ticket_id: "t1".into(),
        })
        .then_rejected("ticket_not_found");
    GivenEvents::<ReopenTicket>::new()
        .when(ReopenTicketPayload {
            ticket_id: "t1".into(),
        })
        .then_rejected("ticket_not_found");
    GivenEvents::<CloseTicket>::new()
        .when(CloseTicketPayload {
            ticket_id: "t1".into(),
        })
        .then_rejected("ticket_not_found");
    GivenEvents::<RequestInfoFromCustomer>::new()
        .when(RequestInfoFromCustomerPayload {
            ticket_id: "t1".into(),
            staff_id: "staff-1".into(),
            message: "?".into(),
        })
        .then_rejected("ticket_not_found");
    GivenEvents::<CustomerRespondsToTicket>::new()
        .when(CustomerRespondsToTicketPayload {
            ticket_id: "t1".into(),
            requester_id: "customer-1".into(),
            message: "?".into(),
        })
        .then_rejected("ticket_not_found");
    GivenEvents::<EscalateTicket>::new()
        .when(EscalateTicketPayload {
            ticket_id: "t1".into(),
        })
        .then_rejected("ticket_not_found");
}
