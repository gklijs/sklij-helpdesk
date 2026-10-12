//! Integration tests for `AutoAssignTicket` (issue #44) over the real REST
//! surface and Postgres: the pick follows each agent's open tickets as
//! skilj folds them into `StaffWorkload`, and concurrent auto-assignments
//! over the same agents conflict on their `staff` tags and are decided
//! again, so the load stays balanced. The decision itself is covered in
//! `tests/fixture/auto_assign.rs`.

mod support;

use std::collections::BTreeMap;

use skilj_helpdesk::helpdesk::{TicketSummaryState, BOUNDED_CONTEXT};
use support::{
    accepted, mint_command_token, projection_state, rejection_kind, runtime, setup, test_db,
    trigger, unique_name,
};

async fn token(
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    command_type_name: &str,
) -> String {
    mint_command_token(pool, mapping, BOUNDED_CONTEXT, command_type_name).await
}

fn create_ticket_payload(ticket_id: &str, company_id: &str) -> serde_json::Value {
    serde_json::json!({
        "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
        "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
    })
}

async fn assignee(pool: &skilj_core::db::Pool, ticket_id: &str) -> Option<String> {
    let state: TicketSummaryState =
        projection_state(pool, BOUNDED_CONTEXT, "TicketSummary", ticket_id).await;
    state.assigned_staff_id
}

#[test]
fn the_least_loaded_candidate_gets_the_ticket_and_a_resolution_frees_one_up() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create = token(&pool, &mapping, "CreateTicket").await;
        let assign = token(&pool, &mapping, "AssignTicket").await;
        let resolve = token(&pool, &mapping, "ResolveTicket").await;
        let auto = token(&pool, &mapping, "AutoAssignTicket").await;
        let company_id = unique_name("company");
        let [a, b, c] = ["a", "b", "c"].map(|n| unique_name(&format!("staff_{n}")));
        let tickets: Vec<String> = (0..6).map(|_| unique_name("ticket")).collect();

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        for ticket in &tickets {
            assert!(accepted(&trigger(&router, &create, create_ticket_payload(ticket, &company_id)).await));
        }
        // a has two open, b one, c none.
        for (ticket, staff) in [(&tickets[0], &a), (&tickets[1], &a), (&tickets[2], &b)] {
            let response = trigger(&router, &assign, serde_json::json!({ "ticket_id": ticket, "staff_id": staff })).await;
            assert!(accepted(&response), "{response:?}");
        }

        let response = trigger(&router, &auto, serde_json::json!({ "ticket_id": tickets[3], "candidate_staff_ids": [a, b, c] })).await;
        assert!(accepted(&response), "{response:?}");
        assert_eq!(assignee(&pool, &tickets[3]).await.as_ref(), Some(&c));

        // a 2, b 1, c 1: b is listed before c, so b.
        let response = trigger(&router, &auto, serde_json::json!({ "ticket_id": tickets[4], "candidate_staff_ids": [a, b, c] })).await;
        assert!(accepted(&response), "{response:?}");
        assert_eq!(assignee(&pool, &tickets[4]).await.as_ref(), Some(&b));

        // a resolves both: a 0, b 2, c 1.
        for ticket in &tickets[..2] {
            assert!(accepted(&trigger(&router, &resolve, serde_json::json!({ "ticket_id": ticket })).await));
        }
        let response = trigger(&router, &auto, serde_json::json!({ "ticket_id": tickets[5], "candidate_staff_ids": [b, c, a] })).await;
        assert!(accepted(&response), "{response:?}");
        assert_eq!(assignee(&pool, &tickets[5]).await.as_ref(), Some(&a));

        // And an assigned ticket can't be auto-assigned again.
        let response = trigger(&router, &auto, serde_json::json!({ "ticket_id": tickets[5], "candidate_staff_ids": [b] })).await;
        assert!(!accepted(&response));
        assert_eq!(rejection_kind(&response), "ticket_not_open");
        let response = trigger(&router, &auto, serde_json::json!({ "ticket_id": unique_name("ticket"), "candidate_staff_ids": [] })).await;
        assert_eq!(rejection_kind(&response), "no_candidates");
    });
}

/// The DCB point of the feature. Every request reads the same idle agents,
/// so decided on its own read each would pick the first listed. They are
/// sent at once, so their reads overlap; each one that commits after
/// another's `TicketAssigned` for an agent it read finds a conflict on
/// that agent's `staff` tag, and skilj decides it again with that
/// assignment folded in. Only then does every agent end up with the same
/// number.
#[test]
fn concurrent_auto_assignments_over_the_same_agents_stay_balanced() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create = token(&pool, &mapping, "CreateTicket").await;
        let auto = token(&pool, &mapping, "AutoAssignTicket").await;
        let company_id = unique_name("company");
        let agents: Vec<String> = ["a", "b", "c"].iter().map(|n| unique_name(&format!("staff_{n}"))).collect();
        const PER_AGENT: usize = 3;
        let tickets: Vec<String> = (0..agents.len() * PER_AGENT).map(|_| unique_name("ticket")).collect();

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        for ticket in &tickets {
            assert!(accepted(&trigger(&router, &create, create_ticket_payload(ticket, &company_id)).await));
        }

        let mut racing = tokio::task::JoinSet::new();
        for ticket in tickets.clone() {
            let (router, auto, agents) = (router.clone(), auto.clone(), agents.clone());
            racing.spawn(async move {
                trigger(&router, &auto, serde_json::json!({ "ticket_id": ticket, "candidate_staff_ids": agents })).await
            });
        }
        while let Some(response) = racing.join_next().await {
            let response = response.unwrap();
            assert!(accepted(&response), "{response:?}");
        }

        let mut per_agent: BTreeMap<String, usize> = BTreeMap::new();
        for ticket in &tickets {
            let staff = assignee(&pool, ticket).await.expect("every ticket was assigned");
            *per_agent.entry(staff).or_default() += 1;
        }
        let expected: BTreeMap<String, usize> = agents.iter().map(|a| (a.clone(), PER_AGENT)).collect();
        assert_eq!(per_agent, expected);
    });
}
