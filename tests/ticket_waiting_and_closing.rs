//! Integration tests for the rest of the ticket lifecycle -
//! `StaffLogsTicketOnBehalf` (via `CreateTicket`'s own
//! `logged_by_staff_id`), the `waiting_on_customer` round trip
//! (`RequestInfoFromCustomer`/`CustomerRespondsToTicket`), and
//! `CloseTicket`. See `tests/company.rs`'s own doc comment for why this
//! is a separate binary/file rather than one large `tests/helpdesk.rs`.

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

/// `logged_by_staff_id` set (not null) is `StaffLogsTicketOnBehalf`'s
/// own shape - same `CreateTicket` command, the field is what tells the
/// two cases apart (see `helpdesk.rs`'s own doc comment).
#[test]
fn staff_can_log_a_ticket_on_a_customers_behalf() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let company_id = unique_name("company");
        let staff_id = unique_name("staff");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;

        let response = trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": unique_name("ticket"),
                "company_id": company_id,
                "requester_id": unique_name("customer"),
                "logged_by_staff_id": staff_id,
                "title": "Called in about billing",
                "description": "Customer called, couldn't use the portal themselves",
                "priority": "medium",
            }),
        )
        .await;
        assert!(accepted(&response), "staff-logged ticket should be accepted: {response:?}");
    });
}

#[test]
fn ticket_can_go_to_waiting_on_customer_and_back() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let assign_ticket = token(&pool, &mapping, "AssignTicket").await;
        let request_info = token(&pool, &mapping, "RequestInfoFromCustomer").await;
        let customer_responds = token(&pool, &mapping, "CustomerRespondsToTicket").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");
        let staff_id = unique_name("staff");
        let requester_id = unique_name("customer");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": requester_id,
                "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
            }),
        )
        .await;
        trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": ticket_id, "staff_id": staff_id })).await;

        // in_progress -> waiting_on_customer
        let response = trigger(&router, &request_info, serde_json::json!({ "ticket_id": ticket_id, "staff_id": staff_id, "message": "Which browser are you using?" })).await;
        assert!(accepted(&response), "requesting info should be accepted: {response:?}");
        let state: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert_eq!(state.status.as_deref(), Some("waiting_on_customer"));

        // waiting_on_customer -> in_progress
        let response = trigger(&router, &customer_responds, serde_json::json!({ "ticket_id": ticket_id, "requester_id": requester_id, "message": "Firefox 120" })).await;
        assert!(accepted(&response), "customer's response should be accepted: {response:?}");
        let state: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert_eq!(state.status.as_deref(), Some("in_progress"));
    });
}

#[test]
fn requesting_info_on_a_ticket_that_is_not_in_progress_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let request_info = token(&pool, &mapping, "RequestInfoFromCustomer").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
            }),
        )
        .await;

        // still open - never assigned
        let response = trigger(&router, &request_info, serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff"), "message": "?" })).await;
        assert!(!accepted(&response), "requesting info on an open ticket should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "ticket_not_in_progress");
    });
}

#[test]
fn customer_responding_when_nothing_was_asked_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let assign_ticket = token(&pool, &mapping, "AssignTicket").await;
        let customer_responds = token(&pool, &mapping, "CustomerRespondsToTicket").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");
        let requester_id = unique_name("customer");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": requester_id,
                "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
            }),
        )
        .await;
        trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") })).await;

        // in_progress, but nobody asked for anything
        let response = trigger(&router, &customer_responds, serde_json::json!({ "ticket_id": ticket_id, "requester_id": requester_id, "message": "?" })).await;
        assert!(!accepted(&response), "an unprompted response should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "ticket_not_waiting_on_customer");
    });
}

// --- rule TicketAutoCloses, submitted the way src/bin/scheduler.rs really submits it ---

#[test]
fn closing_a_resolved_ticket_succeeds_and_closing_twice_is_rejected() {
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
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
            }),
        )
        .await;
        trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") })).await;
        trigger(&router, &resolve_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;

        let response = trigger(&router, &close_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;
        assert!(accepted(&response), "closing a resolved ticket should succeed: {response:?}");
        let state: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert_eq!(state.status.as_deref(), Some("closed"));

        let response = trigger(&router, &close_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;
        assert!(!accepted(&response), "closing an already-closed ticket should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "ticket_not_resolved");
    });
}
