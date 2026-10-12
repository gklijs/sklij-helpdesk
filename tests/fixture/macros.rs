//! Macros (issue #43): creating, editing and deleting them, `ApplyMacro`'s
//! combined decision, and the `CompanyMacros` library.

use serde_json::json;
use skilj_helpdesk::helpdesk::*;
use skilj_test_fixture::command::GivenEvents;
use skilj_test_fixture::projection::GivenEvents as GivenProjectionEvents;

use crate::events::*;
use crate::ticket_snapshot::GivenTicket;

// --- CreateMacro / UpdateMacro / DeleteMacro ---

fn delete(macro_id: &str) -> MacroDeletedPayload {
    MacroDeletedPayload {
        company_id: "acme".into(),
        macro_id: macro_id.into(),
        staff_id: "staff-1".into(),
    }
}

#[test]
fn a_signed_up_company_creates_a_macro() {
    let definition = macro_definition(
        "m1",
        "acme",
        Some(MacroStatus::Resolved),
        Some(TicketPriority::Low),
    );
    GivenEvents::<CreateMacro>::new()
        .event(signed_up("acme"))
        .when(definition.clone())
        .then_accepted(vec![spec(
            "MacroCreated",
            json!({
                "company_id": "acme",
                "macro_id": "m1",
                "staff_id": "staff-1",
                "name": "m1 name",
                "body": "m1: about {{ticket_id}}, from {{staff_id}}",
                "status": "resolved",
                "priority": "low",
            }),
        )]);
    // Expiry only stops new tickets.
    GivenEvents::<CreateMacro>::new()
        .events([signed_up("acme"), expired("acme")])
        .when(definition)
        .then(assert_accepted);
}

#[test]
fn creating_a_macro_is_rejected_for_an_unknown_company_a_used_id_or_an_empty_definition() {
    GivenEvents::<CreateMacro>::new()
        .when(macro_definition("m1", "acme", None, None))
        .then_rejected("company_not_found");
    // A deleted macro's id stays used.
    for history in [
        vec![signed_up("acme"), macro_created("m1", "acme", None, None)],
        vec![
            signed_up("acme"),
            macro_created("m1", "acme", None, None),
            macro_deleted("m1", "acme"),
        ],
    ] {
        GivenEvents::<CreateMacro>::new()
            .events(history)
            .when(macro_definition("m1", "acme", None, None))
            .then_rejected("macro_already_exists");
    }
    // Another company's macro of the same id is no collision.
    GivenEvents::<CreateMacro>::new()
        .events([signed_up("acme"), macro_created("m1", "globex", None, None)])
        .when(macro_definition("m1", "acme", None, None))
        .then(assert_accepted);
    let mut blank = macro_definition("m1", "acme", None, None);
    blank.body = "  ".into();
    GivenEvents::<CreateMacro>::new()
        .event(signed_up("acme"))
        .when(blank)
        .then_rejected("invalid_macro");
}

#[test]
fn an_existing_macro_is_updated_and_deleted() {
    let mut edited = macro_definition("m1", "acme", None, Some(TicketPriority::Urgent));
    edited.body = "New text".into();
    GivenEvents::<UpdateMacro>::new()
        .event(macro_created("m1", "acme", None, None))
        .when(edited)
        .then_accepted(vec![spec(
            "MacroUpdated",
            json!({
                "company_id": "acme",
                "macro_id": "m1",
                "staff_id": "staff-1",
                "name": "m1 name",
                "body": "New text",
                "status": null,
                "priority": "urgent",
            }),
        )]);
    GivenEvents::<DeleteMacro>::new()
        .events([
            macro_created("m1", "acme", None, None),
            macro_updated("m1", "acme", None, None),
        ])
        .when(delete("m1"))
        .then_accepted(vec![spec(
            "MacroDeleted",
            json!({ "company_id": "acme", "macro_id": "m1", "staff_id": "staff-1" }),
        )]);
}

#[test]
fn a_missing_or_deleted_macro_cannot_be_updated_or_deleted() {
    for history in [
        vec![],
        vec![macro_created("m1", "globex", None, None)],
        vec![
            macro_created("m1", "acme", None, None),
            macro_deleted("m1", "acme"),
        ],
    ] {
        let replay = || {
            history
                .iter()
                .map(|e| match e {
                    HelpdeskEvent::MacroCreated(p) => HelpdeskEvent::MacroCreated(p.clone()),
                    HelpdeskEvent::MacroDeleted(p) => HelpdeskEvent::MacroDeleted(p.clone()),
                    _ => unreachable!(),
                })
                .collect::<Vec<_>>()
        };
        GivenEvents::<UpdateMacro>::new()
            .events(replay())
            .when(macro_definition("m1", "acme", None, None))
            .then_rejected("macro_not_found");
        GivenEvents::<DeleteMacro>::new()
            .events(replay())
            .when(delete("m1"))
            .then_rejected("macro_not_found");
    }
}

// --- ApplyMacro ---

fn apply(macro_id: &str) -> ApplyMacroPayload {
    ApplyMacroPayload {
        ticket_id: "t1".into(),
        company_id: "acme".into(),
        macro_id: macro_id.into(),
        staff_id: "staff-2".into(),
    }
}

/// `history`, then the macro `m1` defined as given.
fn with_macro(
    mut history: Vec<HelpdeskEvent>,
    status: Option<MacroStatus>,
    priority: Option<TicketPriority>,
) -> Vec<HelpdeskEvent> {
    history.push(macro_created("m1", "acme", status, priority));
    history
}

fn reply(message: &str) -> skilj_core::shared::EventSpec {
    spec(
        "TicketStaffReplied",
        json!({
            "ticket_id": "t1",
            "company_id": "acme",
            "staff_id": "staff-2",
            "message": message,
            "requester_id": "customer-1",
            "macro_id": "m1",
        }),
    )
}

const RENDERED: &str = "m1: about t1, from staff-2";

#[test]
fn a_plain_macro_is_one_rendered_reply() {
    for history in [
        open_ticket("t1", "acme"),
        in_progress_ticket("t1", "acme"),
        waiting_ticket("t1", "acme"),
        resolved_ticket("t1", "acme"),
    ] {
        GivenTicket::<ApplyMacro>::new()
            .events(with_macro(history, None, None))
            .when(apply("m1"))
            .then_accepted(vec![reply(RENDERED)]);
    }
}

#[test]
fn a_macro_replies_changes_the_priority_and_resolves_in_one_decision() {
    GivenTicket::<ApplyMacro>::new()
        .events(with_macro(
            in_progress_ticket("t1", "acme"),
            Some(MacroStatus::Resolved),
            Some(TicketPriority::Low),
        ))
        .when(apply("m1"))
        .then_accepted(vec![
            spec(
                "TicketPriorityChanged",
                json!({
                    "ticket_id": "t1",
                    "company_id": "acme",
                    "staff_id": "staff-2",
                    "previous_priority": "medium",
                    "new_priority": "low",
                    "macro_id": "m1",
                }),
            ),
            reply(RENDERED),
            spec(
                "TicketResolved",
                json!({
                    "ticket_id": "t1",
                    "company_id": "acme",
                    "resolution": 1,
                    "requester_id": "customer-1",
                    // Tagged `staff`, so `StaffWorkload` drops the agent's load.
                    "assignee_id": "staff-1",
                }),
            ),
        ]);
}

#[test]
fn a_waiting_on_customer_macro_asks_the_question_with_its_reply() {
    GivenTicket::<ApplyMacro>::new()
        .events(with_macro(
            in_progress_ticket("t1", "acme"),
            Some(MacroStatus::WaitingOnCustomer),
            None,
        ))
        .when(apply("m1"))
        .then_accepted(vec![spec(
            "TicketInfoRequested",
            json!({
                "ticket_id": "t1",
                "company_id": "acme",
                "staff_id": "staff-2",
                "message": RENDERED,
                "requester_id": "customer-1",
            }),
        )]);
}

#[test]
fn a_priority_the_ticket_already_has_is_not_changed_again() {
    // Created medium, escalated to high: a macro setting high only replies.
    let mut history = in_progress_ticket("t1", "acme");
    history.push(escalated(
        "t1",
        "acme",
        TicketPriority::Medium,
        TicketPriority::High,
    ));
    GivenTicket::<ApplyMacro>::new()
        .events(with_macro(history, None, Some(TicketPriority::High)))
        .when(apply("m1"))
        .then_accepted(vec![reply(RENDERED)]);
}

#[test]
fn a_status_change_keeps_its_own_commands_rule_and_rejects_the_whole_macro() {
    // Both need `in_progress`, like `ResolveTicket`/`RequestInfoFromCustomer`;
    // the priority change and reply go nowhere either.
    for (status, kind) in [
        (MacroStatus::Resolved, "ticket_not_in_progress"),
        (MacroStatus::WaitingOnCustomer, "ticket_not_in_progress"),
    ] {
        for history in [
            open_ticket("t1", "acme"),
            waiting_ticket("t1", "acme"),
            resolved_ticket("t1", "acme"),
        ] {
            GivenTicket::<ApplyMacro>::new()
                .events(with_macro(
                    history,
                    Some(status),
                    Some(TicketPriority::Urgent),
                ))
                .when(apply("m1"))
                .then_rejected(kind);
        }
    }
}

#[test]
fn a_closed_or_merged_ticket_takes_no_macro() {
    for history in [closed_ticket("t1", "acme"), {
        let mut events = open_ticket("t1", "acme");
        events.push(merged("t0", "t1", "acme"));
        events
    }] {
        GivenTicket::<ApplyMacro>::new()
            .events(with_macro(history, None, Some(TicketPriority::Low)))
            .when(apply("m1"))
            .then_rejected("ticket_not_replyable");
    }
}

#[test]
fn the_ticket_and_the_macro_must_exist_and_belong_together() {
    GivenTicket::<ApplyMacro>::new()
        .events(with_macro(vec![], None, None))
        .when(apply("m1"))
        .then_rejected("ticket_not_found");
    GivenTicket::<ApplyMacro>::new()
        .events(open_ticket("t1", "acme"))
        .when(apply("m1"))
        .then_rejected("macro_not_found");
    let mut deleted = with_macro(open_ticket("t1", "acme"), None, None);
    deleted.push(macro_deleted("m1", "acme"));
    GivenTicket::<ApplyMacro>::new()
        .events(deleted)
        .when(apply("m1"))
        .then_rejected("macro_not_found");
    // Another company's macro, on another company's ticket.
    let mut foreign = open_ticket("t1", "globex");
    foreign.push(macro_created("m1", "acme", None, None));
    GivenTicket::<ApplyMacro>::new()
        .events(foreign)
        .when(apply("m1"))
        .then_rejected("ticket_belongs_to_another_company");
}

#[test]
fn the_latest_edit_of_a_macro_is_what_gets_applied() {
    let mut history = with_macro(in_progress_ticket("t1", "acme"), None, None);
    history.push(HelpdeskEvent::MacroUpdated(MacroDefinitionPayload {
        body: "Edited for {{ticket_id}}".into(),
        ..macro_definition("m1", "acme", None, None)
    }));
    GivenTicket::<ApplyMacro>::new()
        .events(history)
        .when(apply("m1"))
        .then_accepted(vec![reply("Edited for t1")]);
}

#[test]
fn escalation_bumps_the_priority_a_macro_set() {
    let mut history = in_progress_ticket("t1", "acme");
    history.push(priority_changed(
        "t1",
        "acme",
        TicketPriority::Medium,
        TicketPriority::Low,
    ));
    GivenTicket::<EscalateTicket>::new()
        .events(history)
        .when(EscalateTicketPayload {
            ticket_id: "t1".into(),
        })
        .then_accepted(vec![spec(
            "TicketEscalated",
            json!({
                "ticket_id": "t1",
                "company_id": "acme",
                "previous_priority": "low",
                "new_priority": "medium",
            }),
        )]);
}

// --- projections ---

#[test]
fn company_macros_lists_the_current_macros() {
    GivenProjectionEvents::<CompanyMacros>::new()
        .event(macro_created("m1", "acme", None, None))
        .event(macro_created(
            "m2",
            "acme",
            Some(MacroStatus::WaitingOnCustomer),
            Some(TicketPriority::High),
        ))
        .event(macro_updated(
            "m1",
            "acme",
            Some(MacroStatus::Resolved),
            None,
        ))
        .event(macro_created("m3", "acme", None, None))
        .event(macro_deleted("m3", "acme"))
        .then(|state| {
            let listed: Vec<_> = state.macros.values().cloned().collect();
            assert_eq!(
                listed,
                vec![
                    MacroEntry {
                        macro_id: "m1".into(),
                        name: "m1 name".into(),
                        body: "m1: about {{ticket_id}}, from {{staff_id}}".into(),
                        status: Some("resolved".into()),
                        priority: None,
                    },
                    MacroEntry {
                        macro_id: "m2".into(),
                        name: "m2 name".into(),
                        body: "m2: about {{ticket_id}}, from {{staff_id}}".into(),
                        status: Some("waiting_on_customer".into()),
                        priority: Some("high".into()),
                    },
                ]
            );
        });
}

#[test]
fn a_macros_reply_and_priority_reach_the_ticket_read_models() {
    GivenProjectionEvents::<TicketSummary>::new()
        .events(in_progress_ticket("t1", "acme"))
        .event(staff_replied("t1", "acme", "on it"))
        .event(priority_changed(
            "t1",
            "acme",
            TicketPriority::Medium,
            TicketPriority::Urgent,
        ))
        .then(|state| {
            assert_eq!(state.priority.as_deref(), Some("urgent"));
            assert_eq!(state.status.as_deref(), Some("in_progress"));
            assert_eq!(state.first_responder_staff_id.as_deref(), Some("staff-1"));
        });
    GivenProjectionEvents::<CompanyActiveTickets>::new()
        .events(open_ticket("t1", "acme"))
        .event(priority_changed(
            "t1",
            "acme",
            TicketPriority::Medium,
            TicketPriority::Low,
        ))
        .then(|state| assert_eq!(state.tickets["t1"].priority, "low"));
    GivenProjectionEvents::<CustomerTickets>::new()
        .events(open_ticket("t1", "acme"))
        .event(staff_replied("t1", "acme", "on it"))
        .then(|state| {
            let messages = &state.tickets["t1"].messages;
            assert_eq!(messages.len(), 1);
            assert!(messages[0].from_staff);
            assert_eq!(messages[0].text, "on it");
        });
}

// --- SLA (issue #42) ---

#[test]
fn a_macro_reply_is_a_first_response() {
    use skilj::CancelDeadline;
    use skilj_helpdesk::sla::SlaKind;

    let mut history = open_ticket("t1", "acme");
    history.push(staff_replied("t1", "acme", "on it"));
    assert!(crate::ticket_snapshot::ticket_state(&history, "t1").first_responded);
    // A first-response deadline firing after it finds the SLA met...
    GivenTicket::<BreachTicketSla>::new()
        .events(history)
        .when(BreachTicketSlaPayload {
            ticket_id: "t1".into(),
            sla: SlaKind::FirstResponse,
        })
        .then_rejected("sla_already_met");
    // ...and the reply cancels that deadline in the first place.
    let HelpdeskEvent::TicketStaffReplied(reply) = staff_replied("t1", "acme", "on it") else {
        unreachable!()
    };
    let tags = CancelFirstResponseSlaOnMacroReply::cancel_tags(&reply).unwrap();
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].key, "ticket");
    assert_eq!(tags[0].value.as_deref(), Some("t1"));
}

#[test]
fn an_sla_breach_escalates_from_the_priority_a_macro_set() {
    use skilj_helpdesk::sla::SlaKind;

    let mut history = in_progress_ticket("t1", "acme");
    history.push(priority_changed(
        "t1",
        "acme",
        TicketPriority::Medium,
        TicketPriority::High,
    ));
    GivenTicket::<BreachTicketSla>::new()
        .events(history)
        .when(BreachTicketSlaPayload {
            ticket_id: "t1".into(),
            sla: SlaKind::Resolution,
        })
        .then_accepted(vec![
            spec(
                "TicketSlaBreached",
                json!({ "ticket_id": "t1", "company_id": "acme", "sla": "resolution" }),
            ),
            spec(
                "TicketEscalated",
                json!({
                    "ticket_id": "t1",
                    "company_id": "acme",
                    "previous_priority": "high",
                    "new_priority": "urgent",
                }),
            ),
        ]);
}
