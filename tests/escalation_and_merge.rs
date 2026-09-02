//! Integration tests for `EscalateTicket`/`MergeTickets` - two of the
//! four flows added beyond `specs/skilj-helpdesk.allium`'s original
//! scope (each type's own doc comment in `src/helpdesk.rs` explains
//! why it exists). Split from `RateTicket`/`AddInternalNote`'s own
//! `tests/csat_and_notes.rs` for the same reason every other test file
//! here is split by concern - see `tests/company.rs`'s own doc comment:
//! each `tests/*.rs` is its own binary with its own `Skilj`/pool
//! instances, and enough of them sharing one Postgres instance
//! eventually exhausts its connection pool. Verified the hard way: one
//! file covering all four commands' tests (13 of them) reliably hit
//! `Database(PoolTimedOut)` under the test harness's default
//! parallelism.

mod support;

use skilj_helpdesk::helpdesk::{TicketSummaryState, BOUNDED_CONTEXT};
use support::{accepted, mint_command_token, projection_state, rejection_kind, runtime, setup, test_db, trigger, unique_name};

async fn token(
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    command_type_name: &str,
) -> String {
    mint_command_token(pool, mapping, BOUNDED_CONTEXT, command_type_name).await
}

fn create_ticket_payload(ticket_id: &str, company_id: &str, priority: &str) -> serde_json::Value {
    serde_json::json!({
        "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
        "logged_by_staff_id": null, "title": "t", "description": "d", "priority": priority,
    })
}

// --- EscalateTicket ---

#[test]
fn escalating_an_open_ticket_bumps_its_priority_and_is_reflected_in_the_projection() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let escalate_ticket = token(&pool, &mapping, "EscalateTicket").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&ticket_id, &company_id, "low")).await;

        let response = trigger(&router, &escalate_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;
        assert!(accepted(&response), "escalation should be accepted: {response:?}");
        let state: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert_eq!(state.priority.as_deref(), Some("medium"), "low escalates one tier to medium");
        assert!(state.escalated, "the frontend needs this to show an 'Escalated' badge");
    });
}

#[test]
fn escalating_the_same_ticket_twice_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let escalate_ticket = token(&pool, &mapping, "EscalateTicket").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&ticket_id, &company_id, "low")).await;
        trigger(&router, &escalate_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;

        let response = trigger(&router, &escalate_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;
        assert!(!accepted(&response), "escalating twice should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "already_escalated");
    });
}

#[test]
fn escalating_a_resolved_ticket_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let assign_ticket = token(&pool, &mapping, "AssignTicket").await;
        let resolve_ticket = token(&pool, &mapping, "ResolveTicket").await;
        let escalate_ticket = token(&pool, &mapping, "EscalateTicket").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&ticket_id, &company_id, "low")).await;
        trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") })).await;
        trigger(&router, &resolve_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;

        let response = trigger(&router, &escalate_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;
        assert!(!accepted(&response), "a resolved ticket is already handled - nothing to escalate: {response:?}");
        assert_eq!(rejection_kind(&response), "ticket_not_unhandled");
    });
}

// --- MergeTickets ---

#[test]
fn merging_two_open_tickets_marks_the_duplicate_merged_and_leaves_the_primary_alone() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let merge_tickets = token(&pool, &mapping, "MergeTickets").await;
        let company_id = unique_name("company");
        let primary_id = unique_name("ticket");
        let duplicate_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&primary_id, &company_id, "low")).await;
        trigger(&router, &create_ticket, create_ticket_payload(&duplicate_id, &company_id, "low")).await;

        let response = trigger(
            &router,
            &merge_tickets,
            serde_json::json!({ "primary_ticket_id": primary_id, "duplicate_ticket_id": duplicate_id }),
        )
        .await;
        assert!(accepted(&response), "merge should be accepted: {response:?}");

        let duplicate_state: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &duplicate_id).await;
        assert_eq!(duplicate_state.status.as_deref(), Some("merged"));
        let primary_state: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &primary_id).await;
        assert_eq!(primary_state.status.as_deref(), Some("open"), "the primary is untouched by a merge");
    });
}

#[test]
fn merging_a_ticket_into_itself_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let merge_tickets = token(&pool, &mapping, "MergeTickets").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&ticket_id, &company_id, "low")).await;

        let response = trigger(
            &router,
            &merge_tickets,
            serde_json::json!({ "primary_ticket_id": ticket_id, "duplicate_ticket_id": ticket_id }),
        )
        .await;
        assert!(!accepted(&response), "should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "cannot_merge_ticket_into_itself");
    });
}

#[test]
fn merging_an_already_closed_ticket_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let assign_ticket = token(&pool, &mapping, "AssignTicket").await;
        let resolve_ticket = token(&pool, &mapping, "ResolveTicket").await;
        let close_ticket = token(&pool, &mapping, "CloseTicket").await;
        let merge_tickets = token(&pool, &mapping, "MergeTickets").await;
        let company_id = unique_name("company");
        let primary_id = unique_name("ticket");
        let duplicate_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&primary_id, &company_id, "low")).await;
        trigger(&router, &create_ticket, create_ticket_payload(&duplicate_id, &company_id, "low")).await;
        trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": primary_id, "staff_id": unique_name("staff") })).await;
        trigger(&router, &resolve_ticket, serde_json::json!({ "ticket_id": primary_id })).await;
        trigger(&router, &close_ticket, serde_json::json!({ "ticket_id": primary_id })).await;

        let response = trigger(
            &router,
            &merge_tickets,
            serde_json::json!({ "primary_ticket_id": primary_id, "duplicate_ticket_id": duplicate_id }),
        )
        .await;
        assert!(!accepted(&response), "a closed primary isn't mergeable: {response:?}");
        assert_eq!(rejection_kind(&response), "primary_ticket_not_mergeable");
    });
}

#[test]
fn merging_tickets_from_different_companies_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let merge_tickets = token(&pool, &mapping, "MergeTickets").await;
        let company_a = unique_name("company");
        let company_b = unique_name("company");
        let ticket_a = unique_name("ticket");
        let ticket_b = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_a, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_b, "name": "Globex", "contact_email": "b@globex.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&ticket_a, &company_a, "low")).await;
        trigger(&router, &create_ticket, create_ticket_payload(&ticket_b, &company_b, "low")).await;

        let response = trigger(
            &router,
            &merge_tickets,
            serde_json::json!({ "primary_ticket_id": ticket_a, "duplicate_ticket_id": ticket_b }),
        )
        .await;
        assert!(!accepted(&response), "should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "tickets_belong_to_different_companies");
    });
}
