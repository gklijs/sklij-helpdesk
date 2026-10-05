//! Server-side ticket-routing enforcement, end to end against a real
//! Postgres: `src/routing_guard.rs`'s own unit tests prove the decision,
//! and this proves the decision is actually reachable by a request.
//!
//! Why this needs a database at all: the guard's whole input for a
//! company-backed request is "does this company have a tenant", which is
//! a read of the `TenantDirectory` projection. A test that only exercised
//! the pure functions would prove nothing about whether the middleware is
//! mounted, whether the projection read finds a real entry, or whether
//! the body is correctly rewound and still reaches skilj afterwards - and
//! the last of those is the kind of bug that turns into "every GraphQL
//! request hangs" rather than an obvious failure.
//!
//! The two properties worth stating up front, because they are what the
//! tests below are really pinning:
//!
//!   - **Refusal is specific.** A misrouted request is refused; a
//!     correctly-routed one, and non-ticket traffic on the shared context,
//!     is not. A guard that refused everything would pass a
//!     "misrouted traffic is refused" test while breaking the app.
//!   - **It is a routing error, not an authorization one.** The caller
//!     holds a perfectly valid mapping on the shared context, so the
//!     message has to say so.

mod support;

use skilj_core::db;
use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use skilj_helpdesk::routing::RoutingMode;
use skilj_helpdesk::routing_guard::{enforce_graphql_routing, GuardState};
use std::sync::Arc;
use support::{
    graphql_request, mint_command_token, runtime, seed_superadmin, setup_graphql, sign_jwt,
    test_db, trigger, unique_name,
};

struct Fixture {
    company_id: String,
    tenant_name: String,
    /// A customer of the company with a ticket in the shared context, so
    /// a `CustomerTickets` row owned by the company exists there.
    requester_id: String,
}

/// A company with a real, recorded tenant and a real, created context.
///
/// Only the company/tenant half of `tenant_access_reconciliation.rs`'s
/// own fixture: these tests are about *routing*, so the roles and
/// mappings that reconciliation projects into the tenant belong to that
/// file's tests. What matters here is that `TenantDirectory` has a real
/// entry for the company, since that is the guard's only input.
async fn fixture() -> (axum::Router, db::Pool, Fixture) {
    let (skilj, pool, mapping, _jwt) = setup_graphql().await;
    let graphql = skilj.graphql_router().await.unwrap();
    let rest = skilj.rest_router();

    let sign_up = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
    let record_tenant =
        mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "RecordCompanyTenant").await;
    let create_ticket = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "CreateTicket").await;

    // No tenant yet for this one test, which is the fallback case: a
    // company that never signed up for isolation must keep working.
    let company_id = format!("acme-corp {}", unique_name("x"));
    let tenant_name = skilj_helpdesk::routing::tenant_name_for(&company_id);
    trigger(
        &rest,
        &sign_up,
        serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
    )
    .await;
    // Filed over REST before the tenant is recorded, as a pre-cutover
    // ticket would have been.
    let requester_id = unique_name("customer");
    let response = trigger(
        &rest,
        &create_ticket,
        serde_json::json!({
            "ticket_id": unique_name("ticket"), "company_id": company_id, "requester_id": requester_id,
            "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
        }),
    )
    .await;
    assert!(response["accepted"].as_bool() == Some(true), "{response:?}");
    let response = trigger(
        &rest,
        &record_tenant,
        serde_json::json!({ "company_id": company_id, "tenant_name": tenant_name }),
    )
    .await;
    assert!(response["accepted"].as_bool() == Some(true), "{response:?}");

    // Create the bounded context for real, so a request naming the tenant
    // is a request at a context that exists.
    let ops = seed_superadmin(&pool).await;
    let ops_jwt = sign_jwt(&ops.external_subject);
    let mutation = format!(
        r#"mutation {{
            createBoundedContextFromTemplate(
                template: {template:?} name: {tenant_name:?} roleId: {role_id:?}
                level: ADMIN canReadSensitive: true
            ) {{ name }}
        }}"#,
        template = BOUNDED_CONTEXT,
        tenant_name = tenant_name,
        role_id = ops.id,
    );
    let response = graphql_request(&graphql, &ops_jwt, &mutation).await;
    assert!(response.get("errors").is_none(), "{response:?}");

    (
        graphql,
        pool,
        Fixture {
            company_id,
            tenant_name,
            requester_id,
        },
    )
}

/// The same router with the guard mounted, as `server.rs` mounts it.
fn guarded(graphql: axum::Router, pool: db::Pool, mode: RoutingMode) -> axum::Router {
    graphql.layer(axum::middleware::from_fn_with_state(
        Arc::new(GuardState { pool, mode }),
        enforce_graphql_routing,
    ))
}

/// A `CreateTicket` mutation exactly as the frontend spells it.
fn create_ticket_query(company_id: &str, context: &str) -> String {
    let payload = serde_json::json!({
        "ticket_id": "tk-1",
        "company_id": company_id,
        "requester_id": "requester-1",
        "title": "help",
        "description": "please",
        "priority": "low",
    });
    let literal = serde_json::to_string(&serde_json::to_string(&payload).unwrap()).unwrap();
    format!(
        r#"mutation {{ submitCommand(boundedContext: {context:?}, commandTypeName: "CreateTicket", payload: {literal}) {{ accepted rejectionReason }} }}"#
    )
}

/// A `CompanyTicketQueue` read exactly as the frontend spells it.
fn ticket_list_query(company_id: &str, context: &str) -> String {
    format!(
        r#"query {{ projection(boundedContext: {context:?}, name: "CompanyTicketQueue", key: {company_id:?}) {{ ... on helpdesk_CompanyTicketQueue {{ tickets }} }} }}"#
    )
}

/// A `CustomerTickets` read exactly as the frontend spells it.
fn customer_tickets_query(requester_id: &str, context: &str) -> String {
    format!(
        r#"query {{ projection(boundedContext: {context:?}, name: "CustomerTickets", key: {requester_id:?}) {{ ... on helpdesk_CustomerTickets {{ tickets }} }} }}"#
    )
}

/// The guard's own error code, as a client would see it.
fn refusal_code(response: &serde_json::Value) -> Option<&str> {
    response["errors"][0]["extensions"]["code"].as_str()
}

#[test]
fn ticket_traffic_naming_the_shared_context_is_refused_once_the_cutover_is_on() {
    runtime().block_on(async {
        let Some(_db) = test_db().await else {
            eprintln!("skipping: no TEST_DATABASE_URL");
            return;
        };
        let (graphql, pool, fixture) = fixture().await;
        let router = guarded(graphql, pool, RoutingMode::Tenant);
        let jwt = sign_jwt("someone");

        // The exact bypass this guard exists to stop: a caller that
        // resolved no tenant and named the shared context anyway. It is
        // refused even though this caller is a superadmin and could
        // write there perfectly legally - the point is that this
        // company's tickets no longer belong there.
        let response = graphql_request(
            &router,
            &jwt,
            &create_ticket_query(&fixture.company_id, BOUNDED_CONTEXT),
        )
        .await;
        assert_eq!(
            refusal_code(&response),
            Some("ticket_routing_error"),
            "a misrouted write must be refused: {response:?}"
        );
        let message = response["errors"][0]["message"]
            .as_str()
            .unwrap_or_default();
        assert!(
            message.contains("routing error"),
            "the message must not read as an authorization failure: {message}"
        );
        assert!(
            message.contains(&fixture.tenant_name) || message.contains("tenant"),
            "the message should point at the tenant to use instead: {message}"
        );

        // And the same for the read side - otherwise a company with a
        // tenant would get a refusal on write and an empty shared
        // dashboard on read, which is the split this whole change exists
        // to prevent.
        let response = graphql_request(
            &router,
            &jwt,
            &ticket_list_query(&fixture.company_id, BOUNDED_CONTEXT),
        )
        .await;
        assert_eq!(
            refusal_code(&response),
            Some("ticket_routing_error"),
            "a misrouted read must be refused too: {response:?}"
        );

        // A customer-keyed read names no company, but its row's owner
        // does - so it is refused the same way.
        let response = graphql_request(
            &router,
            &jwt,
            &customer_tickets_query(&fixture.requester_id, BOUNDED_CONTEXT),
        )
        .await;
        assert_eq!(
            refusal_code(&response),
            Some("ticket_routing_error"),
            "a misrouted customer read must be refused: {response:?}"
        );
        // A row that doesn't exist holds nothing to split or leak.
        let response = graphql_request(
            &router,
            &jwt,
            &customer_tickets_query(&unique_name("no-tickets-yet"), BOUNDED_CONTEXT),
        )
        .await;
        // (skilj itself may still refuse this caller - just not the guard.)
        assert_ne!(
            refusal_code(&response),
            Some("ticket_routing_error"),
            "{response:?}"
        );
    });
}

#[test]
fn traffic_naming_the_companys_own_tenant_is_left_alone() {
    runtime().block_on(async {
        let Some(_db) = test_db().await else {
            eprintln!("skipping: no TEST_DATABASE_URL");
            return;
        };
        let (graphql, pool, fixture) = fixture().await;
        let router = guarded(graphql, pool, RoutingMode::Tenant);
        let jwt = sign_jwt("someone");

        // The guard must not refuse correctly-routed traffic. This
        // request does go on to fail authorisation - the tenant context
        // has no projection state for this key yet, and the caller holds
        // no mapping on the tenant - but it must fail *downstream* of the
        // guard, which is the whole point: a `grant_not_active` from
        // skilj means the guard let it through, whereas
        // `ticket_routing_error` would mean it did not.
        let response = graphql_request(
            &router,
            &jwt,
            &create_ticket_query(&fixture.company_id, &fixture.tenant_name),
        )
        .await;
        let code = refusal_code(&response);
        assert_ne!(
            code,
            Some("ticket_routing_error"),
            "correctly-routed traffic must not be refused by the guard: {response:?}"
        );
    });
}

#[test]
fn with_the_cutover_off_the_shared_context_still_serves_tickets() {
    runtime().block_on(async {
        let Some(_db) = test_db().await else {
            eprintln!("skipping: no TEST_DATABASE_URL");
            return;
        };
        let (graphql, pool, fixture) = fixture().await;
        // Same company, same tenant, same request as the first test -
        // only the mode differs. This is the property that makes the
        // cutover safe to leave off: nothing changes for a deployment
        // that has not opted in, even once it has tenants.
        let router = guarded(graphql, pool, RoutingMode::Shared);
        let jwt = sign_jwt("someone");
        let response = graphql_request(
            &router,
            &jwt,
            &create_ticket_query(&fixture.company_id, BOUNDED_CONTEXT),
        )
        .await;
        assert_ne!(
            refusal_code(&response),
            Some("ticket_routing_error"),
            "the cutover being off must not refuse anything: {response:?}"
        );
    });
}

#[test]
fn a_company_with_no_tenant_is_still_served_from_the_shared_context() {
    runtime().block_on(async {
        let Some(_db) = test_db().await else {
            eprintln!("skipping: no TEST_DATABASE_URL");
            return;
        };
        let (skilj, pool, mapping, _jwt) = setup_graphql().await;
        let graphql = skilj.graphql_router().await.unwrap();
        let rest = skilj.rest_router();
        let router = guarded(graphql, pool.clone(), RoutingMode::Tenant);
        let jwt = sign_jwt("someone");

        // Signed up, but no tenant recorded: the fallback case, and the
        // one that makes an incremental rollout possible at all. A
        // company in this state must keep filing tickets exactly as it
        // did before the cutover.
        let sign_up = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
        let company_id = format!("unprovisioned {}", unique_name("x"));
        trigger(
            &rest,
            &sign_up,
            serde_json::json!({ "company_id": company_id, "name": "New", "contact_email": "n@new.example" }),
        )
        .await;

        let response =
            graphql_request(&router, &jwt, &create_ticket_query(&company_id, BOUNDED_CONTEXT))
                .await;
        assert_ne!(
            refusal_code(&response),
            Some("ticket_routing_error"),
            "a company with no tenant must still be served from the shared context: {response:?}"
        );
    });
}

#[test]
fn non_ticket_traffic_on_the_shared_context_is_never_refused() {
    runtime().block_on(async {
        let Some(_db) = test_db().await else {
            eprintln!("skipping: no TEST_DATABASE_URL");
            return;
        };
        let (graphql, pool, fixture) = fixture().await;
        let router = guarded(graphql, pool, RoutingMode::Tenant);
        let jwt = sign_jwt("someone");

        // Lifecycle authority lives in the shared context on purpose.
        // Refusing it would break the invariant the cutover exists to
        // preserve, so this is checked explicitly rather than assumed.
        let query = format!(
            r#"mutation {{ submitCommand(boundedContext: {BOUNDED_CONTEXT:?}, commandTypeName: "SignUpCompany", payload: "{{}}") {{ accepted }} }}"#
        );
        let response = graphql_request(&router, &jwt, &query).await;
        assert_ne!(
            refusal_code(&response),
            Some("ticket_routing_error"),
            "lifecycle traffic on the shared context must not be refused: {response:?}"
        );

        // And the read a client makes precisely to find out where to
        // route - refusing this would make routing impossible.
        let query = format!(
            r#"query {{ projection(boundedContext: {BOUNDED_CONTEXT:?}, name: "TenantDirectory", key: {company:?}) {{ ... on helpdesk_TenantDirectory {{ tenantName }} }} }}"#,
            company = fixture.company_id,
        );
        let response = graphql_request(&router, &jwt, &query).await;
        assert_ne!(
            refusal_code(&response),
            Some("ticket_routing_error"),
            "the tenant directory read must not be refused: {response:?}"
        );
    });
}

#[test]
fn a_malformed_request_is_passed_through_rather_than_refused() {
    runtime().block_on(async {
        let Some(_db) = test_db().await else {
            eprintln!("skipping: no TEST_DATABASE_URL");
            return;
        };
        let (graphql, pool, _fixture) = fixture().await;
        let router = guarded(graphql, pool, RoutingMode::Tenant);
        let jwt = sign_jwt("someone");

        // Fails open on anything unreadable, deliberately: a parser gap
        // must not become an outage for traffic that is routed correctly.
        // skilj's own rejection is what a client sees here.
        let response = graphql_request(&router, &jwt, "this is not graphql").await;
        assert_ne!(
            refusal_code(&response),
            Some("ticket_routing_error"),
            "an unparseable request must not be refused by the guard: {response:?}"
        );
    });
}

#[test]
fn the_guard_actually_mounted_on_the_router_rewinds_the_body_it_buffers() {
    runtime().block_on(async {
        let Some(_db) = test_db().await else {
            eprintln!("skipping: no TEST_DATABASE_URL");
            return;
        };
        let (graphql, pool, fixture) = fixture().await;
        let router = guarded(graphql, pool, RoutingMode::Tenant);
        let jwt = sign_jwt("someone");

        // The body-buffering bug this pins: the guard reads the body to
        // find out what the request is, and has to hand the request on
        // intact. If it did not, this correctly-routed request would
        // reach skilj with an empty body and fail with a GraphQL parse
        // error instead of the authorization result it should get.
        let response = graphql_request(
            &router,
            &jwt,
            &create_ticket_query(&fixture.company_id, &fixture.tenant_name),
        )
        .await;
        let errors = response
            .get("errors")
            .and_then(|e| e.as_array())
            .map(|errors| {
                errors
                    .iter()
                    .filter_map(|e| e["message"].as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let joined = errors.join("; ");
        assert!(
            !joined.to_lowercase().contains("parse")
                && !joined.to_lowercase().contains("unexpected"),
            "the body must survive the guard intact: {joined:?}"
        );
    });
}
