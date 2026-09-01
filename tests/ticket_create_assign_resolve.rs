//! Integration tests for the core ticket lifecycle - `CreateTicket`,
//! `AssignTicket`, `ResolveTicket`, `ReopenTicket`. See
//! `tests/company.rs`'s own doc comment for why this is a separate
//! binary/file rather than one large `tests/helpdesk.rs`.

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

/// `specs/skilj-helpdesk.allium`'s `requires: company.status != expired`
/// (originally, and buggily, `= active` - see `helpdesk.rs`'s own doc
/// comment on `CreateTicket`).
#[test]
fn creating_a_ticket_for_an_unknown_company_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;

        let response = trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": unique_name("ticket"),
                "company_id": unique_name("company"), // never signed up
                "requester_id": unique_name("customer"),
                "logged_by_staff_id": null,
                "title": "Can't log in",
                "description": "Getting a 500 on the login page",
                "priority": "high",
            }),
        )
        .await;
        assert!(!accepted(&response), "should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "company_not_found");
    });
}

/// The happy path through the whole slice this pass implements: signup,
/// then a ticket through open → in_progress → resolved → in_progress
/// (reopened), checking `TicketSummary` after each step. Covers every
/// transition-graph edge `AssignTicket`/`ResolveTicket`/`ReopenTicket`
/// witness.
#[test]
fn ticket_lifecycle_create_assign_resolve_reopen() {
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
        let reopen_ticket = token(&pool, &mapping, "ReopenTicket").await;

        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");
        let staff_id = unique_name("staff");

        let response = trigger(
            &router,
            &sign_up,
            serde_json::json!({ "company_id": company_id, "name": "Acme Corp", "contact_email": "support@acme.example" }),
        )
        .await;
        assert!(accepted(&response));

        // open
        let response = trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id,
                "company_id": company_id,
                "requester_id": unique_name("customer"),
                "logged_by_staff_id": null,
                "title": "Can't log in",
                "description": "Getting a 500 on the login page",
                "priority": "urgent",
            }),
        )
        .await;
        assert!(accepted(&response), "ticket creation should be accepted: {response:?}");
        let state: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert_eq!(state.status.as_deref(), Some("open"));
        assert_eq!(state.assigned_staff_id, None);

        // open -> in_progress
        let response = trigger(
            &router,
            &assign_ticket,
            serde_json::json!({ "ticket_id": ticket_id, "staff_id": staff_id }),
        )
        .await;
        assert!(accepted(&response), "assignment should be accepted: {response:?}");
        let state: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert_eq!(state.status.as_deref(), Some("in_progress"));
        assert_eq!(state.assigned_staff_id.as_deref(), Some(staff_id.as_str()));

        // in_progress -> resolved
        let response = trigger(
            &router,
            &resolve_ticket,
            serde_json::json!({ "ticket_id": ticket_id }),
        )
        .await;
        assert!(accepted(&response), "resolution should be accepted: {response:?}");
        let state: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert_eq!(state.status.as_deref(), Some("resolved"));

        // resolved -> in_progress (reopened)
        let response = trigger(
            &router,
            &reopen_ticket,
            serde_json::json!({ "ticket_id": ticket_id }),
        )
        .await;
        assert!(accepted(&response), "reopening should be accepted: {response:?}");
        let state: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert_eq!(state.status.as_deref(), Some("in_progress"));
    });
}

#[test]
fn assigning_a_ticket_that_is_already_assigned_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let assign_ticket = token(&pool, &mapping, "AssignTicket").await;
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
        let response = trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") })).await;
        assert!(accepted(&response), "first assignment should succeed: {response:?}");

        let response = trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") })).await;
        assert!(!accepted(&response), "second assignment should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "ticket_not_open");
    });
}

#[test]
fn resolving_a_ticket_that_was_never_picked_up_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let resolve_ticket = token(&pool, &mapping, "ResolveTicket").await;
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

        let response = trigger(&router, &resolve_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;
        assert!(!accepted(&response), "resolving a never-assigned ticket should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "ticket_not_in_progress");
    });
}

#[test]
fn reopening_a_ticket_that_is_not_resolved_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let reopen_ticket = token(&pool, &mapping, "ReopenTicket").await;
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

        let response = trigger(&router, &reopen_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;
        assert!(!accepted(&response), "reopening an open (never-resolved) ticket should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "ticket_not_resolved");
    });
}

#[test]
fn acting_on_a_nonexistent_ticket_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let assign_ticket = token(&pool, &mapping, "AssignTicket").await;

        let response = trigger(
            &router,
            &assign_ticket,
            serde_json::json!({ "ticket_id": unique_name("ticket"), "staff_id": unique_name("staff") }),
        )
        .await;
        assert!(!accepted(&response), "assigning a nonexistent ticket should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "ticket_not_found");
    });
}
