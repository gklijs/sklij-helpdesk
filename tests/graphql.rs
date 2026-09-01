//! Proves the claim `Cargo.toml`'s own doc comment makes: skilj-graphql
//! auto-builds a real, usable GraphQL surface from whatever's
//! registered in `helpdesk.rs`, with zero GraphQL-specific code written
//! in this crate. Real HTTP requests through `Skilj::graphql_router()`,
//! authenticated with a real JWT against a real local JWKS server -
//! adapted from `skilj-demo/tests/graphql_auth.rs`, which proves the
//! identical authentication path for `skilj-demo`'s own bounded
//! contexts.
//!
//! Deliberately narrower than `tests/helpdesk.rs`: this file exists to
//! prove the GraphQL surface itself works, not to re-prove every
//! `decide()` branch already covered there over REST.

mod support;

use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use support::{
    graphql_accepted, graphql_request, runtime, setup_graphql, submit_command_mutation, test_db,
    unique_name,
};

#[test]
fn graphql_can_submit_commands_and_query_the_result_back() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, _pool, _mapping, jwt) = setup_graphql().await;
        let router = skilj.graphql_router().await.unwrap();
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        let response = graphql_request(
            &router,
            &jwt,
            &submit_command_mutation(
                BOUNDED_CONTEXT,
                "SignUpCompany",
                &serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
            ),
        )
        .await;
        assert!(graphql_accepted(&response), "signup should be accepted: {response:?}");

        let response = graphql_request(
            &router,
            &jwt,
            &submit_command_mutation(
                BOUNDED_CONTEXT,
                "CreateTicket",
                &serde_json::json!({
                    "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                    "logged_by_staff_id": null, "title": "Can't log in", "description": "500s everywhere", "priority": "urgent",
                }),
            ),
        )
        .await;
        assert!(graphql_accepted(&response), "ticket creation should be accepted: {response:?}");

        // The whole point of this test: TicketSummary - an ordinary
        // Projection impl in helpdesk.rs, nothing GraphQL-specific about
        // it - is queryable as a real GraphQL type
        // (`{boundedContext}_{projectionName}`, skilj-graphql's own
        // naming convention) with zero extra code.
        let query = format!(
            r#"query {{ projection(boundedContext: {BOUNDED_CONTEXT:?}, name: "TicketSummary", key: {ticket_id:?}) {{ ... on helpdesk_TicketSummary {{ status priority }} }} }}"#
        );
        let response = graphql_request(&router, &jwt, &query).await;
        assert!(response.get("errors").is_none(), "expected no GraphQL errors, got {response:?}");
        assert_eq!(response["data"]["projection"]["status"], "open");
        assert_eq!(response["data"]["projection"]["priority"], "urgent");
    });
}

#[test]
fn graphql_surfaces_a_business_rejection_as_typed_data_not_a_graphql_error() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, _pool, _mapping, jwt) = setup_graphql().await;
        let router = skilj.graphql_router().await.unwrap();

        let response = graphql_request(
            &router,
            &jwt,
            &submit_command_mutation(
                BOUNDED_CONTEXT,
                "CreateTicket",
                &serde_json::json!({
                    "ticket_id": unique_name("ticket"), "company_id": unique_name("company"), "requester_id": unique_name("customer"),
                    "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
                }),
            ),
        )
        .await;

        assert!(response.get("errors").is_none(), "a business rejection is not a GraphQL error: {response:?}");
        assert_eq!(response["data"]["submitCommand"]["accepted"], false);
        assert_eq!(response["data"]["submitCommand"]["rejectionKind"], "company_not_found");
    });
}
