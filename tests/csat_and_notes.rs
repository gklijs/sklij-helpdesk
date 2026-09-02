//! Integration tests for `RateTicket`/`AddInternalNote` - see
//! `tests/escalation_and_merge.rs`'s own doc comment for why this is a
//! separate file/binary from `EscalateTicket`/`MergeTickets`'s own
//! tests, rather than one file covering all four new commands.

mod support;

use skilj_helpdesk::helpdesk::{TicketInternalNotesState, TicketSummaryState, BOUNDED_CONTEXT};
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

// --- RateTicket ---

#[test]
fn rating_a_resolved_ticket_is_accepted() {
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
        let rate_ticket = token(&pool, &mapping, "RateTicket").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&ticket_id, &company_id, "low")).await;
        trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") })).await;
        trigger(&router, &resolve_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;

        let response = trigger(&router, &rate_ticket, serde_json::json!({ "ticket_id": ticket_id, "rating": 5, "comment": "Great support!" })).await;
        assert!(accepted(&response), "rating a resolved ticket should be accepted: {response:?}");
        let state: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert_eq!(state.rating, Some(5), "the frontend needs this to stop re-showing the rating form");
    });
}

#[test]
fn rating_a_ticket_that_is_still_open_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let rate_ticket = token(&pool, &mapping, "RateTicket").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&ticket_id, &company_id, "low")).await;

        let response = trigger(&router, &rate_ticket, serde_json::json!({ "ticket_id": ticket_id, "rating": 5, "comment": null })).await;
        assert!(!accepted(&response), "should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "ticket_not_ratable");
    });
}

#[test]
fn rating_a_resolved_ticket_out_of_range_is_rejected() {
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
        let rate_ticket = token(&pool, &mapping, "RateTicket").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&ticket_id, &company_id, "low")).await;
        trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") })).await;
        trigger(&router, &resolve_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;

        let response = trigger(&router, &rate_ticket, serde_json::json!({ "ticket_id": ticket_id, "rating": 0, "comment": null })).await;
        assert!(!accepted(&response), "should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "invalid_rating");
    });
}

#[test]
fn rating_the_same_ticket_twice_is_rejected() {
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
        let rate_ticket = token(&pool, &mapping, "RateTicket").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&ticket_id, &company_id, "low")).await;
        trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") })).await;
        trigger(&router, &resolve_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;
        trigger(&router, &rate_ticket, serde_json::json!({ "ticket_id": ticket_id, "rating": 4, "comment": null })).await;

        let response = trigger(&router, &rate_ticket, serde_json::json!({ "ticket_id": ticket_id, "rating": 2, "comment": null })).await;
        assert!(!accepted(&response), "should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "already_rated");
    });
}

// --- AddInternalNote ---

#[test]
fn adding_an_internal_note_to_an_open_ticket_is_accepted() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let add_internal_note = token(&pool, &mapping, "AddInternalNote").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");
        let staff_id = unique_name("staff");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, create_ticket_payload(&ticket_id, &company_id, "low")).await;

        let response = trigger(
            &router,
            &add_internal_note,
            serde_json::json!({ "ticket_id": ticket_id, "staff_id": staff_id, "note": "customer sounds frustrated, prioritise" }),
        )
        .await;
        assert!(accepted(&response), "should be accepted: {response:?}");

        // The one place this note is actually readable back - never
        // folded into TicketSummary/CompanyTicketList, see
        // TicketInternalNoteAdded's own doc comment.
        let notes: TicketInternalNotesState =
            projection_state(&pool, BOUNDED_CONTEXT, "TicketInternalNotes", &ticket_id).await;
        assert_eq!(notes.notes.len(), 1);
        assert_eq!(notes.notes[0].staff_id, staff_id);
        assert_eq!(notes.notes[0].note, "customer sounds frustrated, prioritise");
    });
}

#[test]
fn adding_an_internal_note_to_a_nonexistent_ticket_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let add_internal_note = token(&pool, &mapping, "AddInternalNote").await;

        let response = trigger(
            &router,
            &add_internal_note,
            serde_json::json!({ "ticket_id": unique_name("ticket"), "staff_id": unique_name("staff"), "note": "n" }),
        )
        .await;
        assert!(!accepted(&response), "should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "ticket_not_found");
    });
}
