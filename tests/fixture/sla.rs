//! SLA policies (issue #42): `SetCompanySlaPolicy`, the SLA a new ticket
//! is stamped with, `BreachTicketSla`, and the deadlines that schedule and
//! cancel it. Whether skilj really schedules, cancels and fires them is
//! `tests/sla_policies.rs`'s.

use serde_json::json;
use skilj::{CancelDeadline, ScheduleDeadline};
use skilj_core::shared::Tag;
use skilj_helpdesk::helpdesk::*;
use skilj_helpdesk::sla::{add_business_minutes, BusinessHours, SlaKind, SlaPolicy};
use skilj_test_fixture::command::GivenEvents;

use crate::events::*;
use crate::ticket_snapshot::GivenTicket;
use skilj_core::shared::CommandDecision;

fn round_the_clock() -> SlaPolicy {
    SlaPolicy {
        business_hours: BusinessHours {
            open_hour: 0,
            close_hour: 24,
            working_days: (1..=7).collect(),
        },
        ..SlaPolicy::default()
    }
}

fn set(company_id: &str, policy: SlaPolicy) -> SetCompanySlaPolicyPayload {
    SetCompanySlaPolicyPayload {
        company_id: company_id.into(),
        policy,
    }
}

// --- SetCompanySlaPolicy ---

#[test]
fn a_company_sets_its_sla_policy() {
    GivenEvents::<SetCompanySlaPolicy>::new()
        .event(signed_up("acme"))
        .when(set("acme", round_the_clock()))
        .then_accepted(vec![spec(
            "CompanySlaPolicySet",
            json!({ "company_id": "acme", "policy": round_the_clock() }),
        )]);
}

#[test]
fn an_unknown_company_or_an_unusable_policy_is_rejected() {
    GivenEvents::<SetCompanySlaPolicy>::new()
        .when(set("acme", SlaPolicy::default()))
        .then_rejected("company_not_found");
    let mut policy = SlaPolicy::default();
    policy.business_hours.working_days.clear();
    GivenEvents::<SetCompanySlaPolicy>::new()
        .event(signed_up("acme"))
        .when(set("acme", policy))
        .then_rejected("invalid_sla_policy");
}

// --- CreateTicket stamps the SLA ---

fn create(company_id: &str, priority: TicketPriority) -> CreateTicketPayload {
    CreateTicketPayload {
        ticket_id: "t1".into(),
        company_id: company_id.into(),
        requester_id: "customer-1".into(),
        logged_by_staff_id: None,
        title: "t".into(),
        description: "d".into(),
        priority,
        requester_name: None,
        requester_email: None,
    }
}

fn stamped_sla(decision: &skilj_core::shared::CommandDecision) -> serde_json::Value {
    match decision {
        skilj_core::shared::CommandDecision::Accepted { events } => {
            events[0].payload["sla"].clone()
        }
        other => panic!("expected Accepted, got {other:?}"),
    }
}

#[test]
fn a_new_ticket_runs_under_its_companys_latest_policy_for_its_priority() {
    let mut first = round_the_clock();
    first.urgent.first_response_minutes = 5;
    let mut latest = round_the_clock();
    latest.urgent.first_response_minutes = 15;
    GivenEvents::<CreateTicket>::new()
        .events([
            signed_up("acme"),
            sla_policy_set("acme", first),
            sla_policy_set("acme", latest.clone()),
            // Another company's policy is not this one's.
            sla_policy_set("globex", SlaPolicy::default()),
        ])
        .when(create("acme", TicketPriority::Urgent))
        .then(|decision| {
            assert_eq!(
                stamped_sla(decision),
                json!(latest.for_priority(TicketPriority::Urgent))
            );
        });
}

#[test]
fn without_a_policy_a_ticket_gets_the_default() {
    GivenEvents::<CreateTicket>::new()
        .event(signed_up("acme"))
        .when(create("acme", TicketPriority::Low))
        .then(|decision| {
            assert_eq!(
                stamped_sla(decision),
                json!(SlaPolicy::default().for_priority(TicketPriority::Low))
            );
        });
}

// --- BreachTicketSla ---

fn breach(sla: SlaKind) -> BreachTicketSlaPayload {
    BreachTicketSlaPayload {
        ticket_id: "t1".into(),
        sla,
    }
}

#[test]
fn a_breach_is_recorded_and_escalates_the_ticket() {
    for (history, sla) in [
        (open_ticket("t1", "acme"), SlaKind::FirstResponse),
        (in_progress_ticket("t1", "acme"), SlaKind::FirstResponse),
        (waiting_ticket("t1", "acme"), SlaKind::Resolution),
    ] {
        GivenTicket::<BreachTicketSla>::new()
            .events(history)
            .when(breach(sla))
            .then_accepted(vec![
                spec(
                    "TicketSlaBreached",
                    json!({ "ticket_id": "t1", "company_id": "acme", "sla": sla }),
                ),
                spec(
                    "TicketEscalated",
                    json!({
                        "ticket_id": "t1", "company_id": "acme",
                        "previous_priority": "medium", "new_priority": "high",
                    }),
                ),
            ]);
    }
}

#[test]
fn a_breach_fired_by_its_deadline_records_when_it_was_due() {
    // The deadline's `scheduled_at`, not when the loop got to it.
    let due: chrono::DateTime<chrono::Utc> = "2026-10-28T09:30:00Z".parse().unwrap();
    GivenTicket::<BreachTicketSla>::new()
        .events(open_ticket("t1", "acme"))
        .deadline(due)
        .when(breach(SlaKind::FirstResponse))
        .then(|decision| {
            let CommandDecision::Accepted { events } = decision else {
                panic!("rejected: {decision:?}")
            };
            assert_eq!(events[0].event_type, "TicketSlaBreached");
            assert_eq!(events[0].payload["due_at"], "2026-10-28T09:30:00+00:00");
        });
}

#[test]
fn an_already_escalated_ticket_only_records_the_breach() {
    let mut history = open_ticket("t1", "acme");
    history.push(escalated(
        "t1",
        "acme",
        TicketPriority::Medium,
        TicketPriority::High,
    ));
    GivenTicket::<BreachTicketSla>::new()
        .events(history)
        .when(breach(SlaKind::Resolution))
        .then_accepted(vec![spec(
            "TicketSlaBreached",
            json!({ "ticket_id": "t1", "company_id": "acme", "sla": "resolution" }),
        )]);
}

#[test]
fn a_met_or_already_breached_sla_is_not_breached_again() {
    // A staff reply met the first-response SLA, even once the customer
    // answered and the ticket is back in progress.
    let mut replied = waiting_ticket("t1", "acme");
    replied.push(customer_responded("t1", "acme", "v2"));
    GivenTicket::<BreachTicketSla>::new()
        .events(replied)
        .when(breach(SlaKind::FirstResponse))
        .then_rejected("sla_already_met");
    for history in [resolved_ticket("t1", "acme"), closed_ticket("t1", "acme")] {
        for sla in [SlaKind::FirstResponse, SlaKind::Resolution] {
            GivenTicket::<BreachTicketSla>::new()
                .events(history.iter().map(clone_event))
                .when(breach(sla))
                .then_rejected("ticket_not_unhandled");
        }
    }
    let mut breached = open_ticket("t1", "acme");
    breached.push(sla_breached("t1", "acme", SlaKind::Resolution));
    GivenTicket::<BreachTicketSla>::new()
        .events(breached)
        .when(breach(SlaKind::Resolution))
        .then_rejected("sla_already_breached");
}

#[test]
fn a_reopened_ticket_has_already_met_its_resolution_sla() {
    // The resolution SLA runs to the first resolution only: a reopen
    // makes the ticket unhandled again, not late.
    let mut reopened_ticket = resolved_ticket("t1", "acme");
    reopened_ticket.push(reopened("t1", "acme", 1));
    GivenTicket::<BreachTicketSla>::new()
        .events(reopened_ticket)
        .when(breach(SlaKind::Resolution))
        .then_rejected("sla_already_met");
}

/// `HelpdeskEvent` isn't `Clone`; a round trip through its payload is.
fn clone_event(event: &HelpdeskEvent) -> HelpdeskEvent {
    use crate::consistency_query::Described;
    let (event_type, payload) = event.describe();
    HelpdeskEvent::rebuild(event_type, &payload)
}

// --- the deadlines ---

fn created_with_sla(policy: &SlaPolicy) -> TicketCreatedPayload {
    TicketCreatedPayload {
        ticket_id: "t1".into(),
        company_id: "acme".into(),
        requester_id: "customer-1".into(),
        logged_by_staff_id: None,
        title: "t".into(),
        description: "d".into(),
        priority: TicketPriority::High,
        requester_name: None,
        requester_email: None,
        sla: Some(policy.for_priority(TicketPriority::High)),
    }
}

fn ticket_tag(ticket_id: &str) -> Vec<Tag> {
    vec![Tag {
        key: "ticket".into(),
        value: Some(ticket_id.into()),
    }]
}

#[test]
fn creating_a_ticket_schedules_both_sla_deadlines_in_business_time() {
    let policy = SlaPolicy::default();
    let source = created_with_sla(&policy);
    let before = chrono::Utc::now();
    let first = ScheduleFirstResponseSla::schedule(&source).expect("a ticket with an SLA");
    let resolution = ScheduleResolutionSla::schedule(&source).expect("a ticket with an SLA");
    let after = chrono::Utc::now();

    for (deadline, kind, minutes) in [
        (
            first,
            SlaKind::FirstResponse,
            policy.high.first_response_minutes,
        ),
        (
            resolution,
            SlaKind::Resolution,
            policy.high.resolution_minutes,
        ),
    ] {
        assert_eq!(deadline.payload.ticket_id, "t1");
        assert_eq!(deadline.payload.sla, kind);
        assert_eq!(deadline.tags, ticket_tag("t1"));
        // add_business_minutes is monotonic in its start.
        let earliest = add_business_minutes(before, minutes, &policy.business_hours);
        let latest = add_business_minutes(after, minutes, &policy.business_hours);
        assert!(deadline.fire_at >= earliest && deadline.fire_at <= latest);
    }

    let legacy = TicketCreatedPayload {
        sla: None,
        ..created_with_sla(&policy)
    };
    assert!(ScheduleFirstResponseSla::schedule(&legacy).is_none());
    assert!(ScheduleResolutionSla::schedule(&legacy).is_none());
}

#[test]
fn a_staff_reply_or_a_resolution_cancels_by_ticket() {
    let reply = TicketInfoRequestedPayload {
        ticket_id: "t1".into(),
        company_id: "acme".into(),
        staff_id: "staff-1".into(),
        message: "?".into(),
        requester_id: Some("customer-1".into()),
    };
    assert_eq!(
        CancelFirstResponseSlaOnStaffReply::cancel_tags(&reply),
        Some(ticket_tag("t1"))
    );
    let resolution = TicketResolvedPayload {
        ticket_id: "t1".into(),
        company_id: "acme".into(),
        resolution: Some(1),
        requester_id: Some("customer-1".into()),
        assignee_id: None,
    };
    assert_eq!(
        CancelFirstResponseSlaOnResolve::cancel_tags(&resolution),
        Some(ticket_tag("t1"))
    );
    assert_eq!(
        CancelResolutionSlaOnResolve::cancel_tags(&resolution),
        Some(ticket_tag("t1"))
    );
}
