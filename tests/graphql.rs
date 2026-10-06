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

use skilj_core::access_control::AccessLevel;
use skilj_helpdesk::helpdesk::{BOUNDED_CONTEXT, STAFF_TEAM};
use support::{
    graphql_accepted, graphql_request, runtime, seed_role, seed_scoped_mapping, setup_graphql,
    sign_jwt, submit_command_mutation, test_db, unique_name,
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

/// `#[requires_role("staff")]` on `AssignTicket` (and the other
/// staff-only command types in `helpdesk.rs`): over GraphQL, a Role
/// whose `name` isn't `STAFF_TEAM` is refused before `decide()` runs,
/// however much access its mapping grants. REST triggering is not
/// covered by this gate - a `CommandToken` is its own per-command grant.
#[test]
fn graphql_refuses_a_staff_only_command_from_a_non_staff_role() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, _mapping, admin_jwt) = setup_graphql().await;
        let router = skilj.graphql_router().await.unwrap();
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        for (command, payload) in [
            (
                "SignUpCompany",
                serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
            ),
            (
                "CreateTicket",
                serde_json::json!({
                    "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                    "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
                }),
            ),
        ] {
            let response =
                graphql_request(&router, &admin_jwt, &submit_command_mutation(BOUNDED_CONTEXT, command, &payload)).await;
            assert!(graphql_accepted(&response), "{command} should be accepted: {response:?}");
        }

        let customer = seed_role(&pool, "customer").await;
        seed_scoped_mapping(&pool, &customer, AccessLevel::Write, Some(company_id.clone())).await;
        let customer_jwt = sign_jwt(&customer.external_subject);

        let staff = seed_role(&pool, STAFF_TEAM).await;
        seed_scoped_mapping(&pool, &staff, AccessLevel::Write, None).await;
        let staff_jwt = sign_jwt(&staff.external_subject);

        let assign = submit_command_mutation(
            BOUNDED_CONTEXT,
            "AssignTicket",
            &serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") }),
        );

        // The company's own customer, and the unscoped "Test Admin" Role
        // `setup_graphql` seeds: neither is named `STAFF_TEAM`.
        for (who, jwt) in [("customer", &customer_jwt), ("admin", &admin_jwt)] {
            let response = graphql_request(&router, jwt, &assign).await;
            assert_eq!(
                response["errors"][0]["extensions"]["code"], "insufficient_role",
                "AssignTicket from the {who} Role must be refused by the role gate: {response:?}"
            );
        }

        let response = graphql_request(&router, &staff_jwt, &assign).await;
        assert!(graphql_accepted(&response), "AssignTicket from the staff Role should be accepted: {response:?}");

        let query = format!(
            r#"query {{ projection(boundedContext: {BOUNDED_CONTEXT:?}, name: "TicketSummary", key: {ticket_id:?}) {{ ... on helpdesk_TicketSummary {{ status }} }} }}"#
        );
        let response = graphql_request(&router, &admin_jwt, &query).await;
        assert_eq!(
            response["data"]["projection"]["status"], "in_progress",
            "only the staff Role's AssignTicket should have taken effect: {response:?}"
        );
    });
}

/// What `frontend/`'s dashboard relies on after every write:
/// `CompanyActiveTickets` is async, so a plain read right after
/// `CreateTicket` can miss the ticket for up to one catch-up tick. Passing
/// the command's own `triggeredEventSequences` back as `waitForSequence`
/// makes the read return only once the projection has folded it.
#[test]
fn a_read_waiting_for_the_writes_own_sequence_sees_the_write_in_an_async_projection() {
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
        assert!(graphql_accepted(&response), "{response:?}");

        let payload = serde_json::json!({
            "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
            "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
        });
        let response = graphql_request(
            &router,
            &jwt,
            &submit_command_mutation(BOUNDED_CONTEXT, "CreateTicket", &payload),
        )
        .await;
        assert!(graphql_accepted(&response), "{response:?}");
        let written = response["data"]["submitCommand"]["triggeredEventSequences"]
            .as_array()
            .expect("an accepted command lists the sequences it triggered")
            .iter()
            .filter_map(serde_json::Value::as_i64)
            .max()
            .expect("CreateTicket triggers one event");

        let query = format!(
            r#"query {{ projection(boundedContext: {BOUNDED_CONTEXT:?}, name: "CompanyActiveTickets", key: {company_id:?}, waitForSequence: {written}) {{ ... on helpdesk_CompanyActiveTickets {{ tickets }} }} }}"#
        );
        let response = graphql_request(&router, &jwt, &query).await;
        assert!(response.get("errors").is_none(), "{response:?}");
        let tickets: serde_json::Value = serde_json::from_str(
            response["data"]["projection"]["tickets"]
                .as_str()
                .expect("tickets comes back JSON-encoded"),
        )
        .unwrap();
        assert_eq!(tickets[&ticket_id]["status"], "open", "{tickets}");
    });
}
