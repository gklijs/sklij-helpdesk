//! Proof of concept for real per-company multi-tenancy, first written
//! when `README.md`'s own "What's not built" section still named this
//! its biggest gap. It no longer is: `src/bin/provisioner.rs` now runs
//! the exact mechanism this file proves - `createBoundedContextFromTemplate`
//! from the shared `helpdesk` context as the template - for real, as a
//! production reaction to every `SignUpCompany`. This file stays, kept
//! deliberately independent of that binary: it drives the same GraphQL
//! mutation directly and synchronously, in-process, so this crate's
//! test suite can assert on the mechanism itself (isolation, the
//! access-grant side effect) without spinning up a real HTTP server and
//! polling an async reactor for it to notice a REST event feed - see
//! `alerter.rs`'s own module doc comment for why that same tradeoff
//! already keeps *that* binary out of `cargo test`.
//!
//! What's still open, deliberately left that way rather than glossed
//! over (see `helpdesk.rs`'s own module doc comment and
//! `RecordCompanyTenant`'s own doc comment for the same list from the
//! production side):
//!   - Nothing yet routes a `CreateTicket` (or any other Ticket command/
//!     query) for a signed-up company into the tenant `provisioner.rs`
//!     just provisioned for it - every company's tickets still run in
//!     the shared `helpdesk` context. `CreateTicket` et al.'s own
//!     `company_status` guard read is one same-context DCB query today;
//!     making it reach across two bounded contexts instead is real
//!     cross-context query design work, not a detail.
//!   - `alerter.rs` reads *one* set of `EventReadToken`s today, each
//!     scoped to one bounded context. Watching every tenant's own event
//!     feed, or paging a lead the moment a *new* tenant is provisioned,
//!     has no answer here: N tenants means N token sets, minted and
//!     rotated somehow, which is a real design question of its own, not
//!     a detail. The trial/auto-close deadline reactors
//!     (`helpdesk.rs`'s own `ScheduleDeadline`s) sidestep the token half
//!     of this, reading through skilj's internal poller rather than a
//!     minted `EventReadToken`, but registering a plugin trait per
//!     bounded context still means N tenants means N registrations,
//!     the same open question in a different shape.
//!   - The frontend's `DEMO_COMPANY_ID` (a key inside one shared
//!     context) would become "which tenant does this login belong to,"
//!     an actual routing decision, not a constant.
//!
//! Kept in its own file rather than folded into `tests/company.rs` or
//! `tests/graphql.rs` for the same reason every other split in this
//! crate is: this is a fundamentally different kind of proof (bounded-
//! context creation and cross-context isolation) than either of those
//! covers.

mod support;

use skilj_helpdesk::helpdesk::{TicketSummaryState, BOUNDED_CONTEXT};
use support::{
    accepted, graphql_request, mint_command_token, projection_state, runtime, seed_role, seed_superadmin, setup_graphql,
    sign_jwt, test_db, trigger, unique_name,
};

#[test]
fn a_new_tenant_provisioned_from_the_shared_context_works_independently_and_in_isolation() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        // `setup_graphql()` also ensures the shared `helpdesk` context
        // exists with every `EventType`/`CommandType`/`Projection`
        // `helpdesk.rs` declares already registered on it - exactly the
        // "current registrations" `createBoundedContextFromTemplate`
        // copies onto the new tenant below.
        let (skilj, pool, _mapping, _admin_jwt) = setup_graphql().await;
        let router = skilj.graphql_router().await.unwrap();

        // Two separate identities, on purpose: `superadmin` is a
        // platform-ops caller (the only kind `CreateBoundedContextFromTemplate`
        // accepts), `tenant_role` is the new tenant's own staff -
        // exactly the "one ops action grants the tenant's own admin
        // access in the same call" shape `@guarantee
        // AccessGrantedWithCreation` describes, not the same identity
        // wearing two hats.
        let superadmin = seed_superadmin(&pool).await;
        let superadmin_jwt = sign_jwt(&superadmin.external_subject);
        let tenant_role = seed_role(&pool, "tenant-staff-lead").await;

        let tenant_name = unique_name("tenant");
        // `BoundedContext`'s own GraphQL shape (skilj-graphql's
        // `bounded_context_object()`) has no `template` field to
        // sub-select - the template relationship is proven below
        // instead, by the new tenant actually running `helpdesk.rs`'s
        // own command types the same way `helpdesk` itself does.
        // `accessMappings` is what's exposed instead, and doubles as
        // proof of `@guarantee AccessGrantedWithCreation`: `tenant_role`
        // already has an ADMIN mapping on this tenant, in the very same
        // response, without a second call.
        let mutation = format!(
            r#"mutation {{
                createBoundedContextFromTemplate(
                    template: {template:?}
                    name: {tenant_name:?}
                    roleId: {role_id:?}
                    level: ADMIN
                    canReadSensitive: true
                ) {{
                    name
                    status
                    accessMappings {{ role {{ id }} level status }}
                }}
            }}"#,
            template = BOUNDED_CONTEXT,
            tenant_name = tenant_name,
            role_id = tenant_role.id,
        );
        let response = graphql_request(&router, &superadmin_jwt, &mutation).await;
        assert!(
            response.get("errors").is_none(),
            "createBoundedContextFromTemplate should succeed: {response:?}"
        );
        let created = &response["data"]["createBoundedContextFromTemplate"];
        assert_eq!(created["name"], tenant_name);
        assert_eq!(created["status"], "ACTIVE");
        let mappings = created["accessMappings"].as_array().unwrap();
        assert!(
            mappings
                .iter()
                .any(|m| m["role"]["id"] == tenant_role.id && m["level"] == "ADMIN" && m["status"] == "ACTIVE"),
            "tenant_role should already have an active ADMIN mapping on the new tenant, granted in the same call: {mappings:?}"
        );

        // --- the real proof: independent, isolated function, not just
        // a row in a BoundedContexts table ---

        // The `RoleAccessMapping` the mutation above granted `tenant_role`
        // on the new tenant, in the same call - fetched back rather than
        // reconstructed by hand, so minting tokens below is against what
        // is actually durable, not what this test assumes should be there.
        let tenant_mapping = skilj_core::db::get_active_role_access_mapping(&pool, &tenant_role.id, &tenant_name)
            .await
            .unwrap()
            .expect("createBoundedContextFromTemplate should have granted tenant_role access to its own new tenant");

        let sign_up = mint_command_token(&pool, &tenant_mapping, &tenant_name, "SignUpCompany").await;
        let create_ticket = mint_command_token(&pool, &tenant_mapping, &tenant_name, "CreateTicket").await;

        let rest_router = skilj.rest_router();
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");
        let response = trigger(
            &rest_router,
            &sign_up,
            serde_json::json!({ "company_id": company_id, "name": "Tenant Co", "contact_email": "a@tenant.example" }),
        )
        .await;
        assert!(
            accepted(&response),
            "SignUpCompany, copied over from the template, should work the same way inside the new tenant: {response:?}"
        );
        let response = trigger(
            &rest_router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "proves real tenant isolation", "description": "d", "priority": "low",
            }),
        )
        .await;
        assert!(
            accepted(&response),
            "CreateTicket should work the same way inside the new tenant too: {response:?}"
        );

        // Isolation, not just success: the same business logic just ran
        // against a *different* bounded context - this ticket exists in
        // the new tenant's own projection state...
        let in_tenant: TicketSummaryState = projection_state(&pool, &tenant_name, "TicketSummary", &ticket_id).await;
        assert!(
            in_tenant.status.is_some(),
            "the ticket should be visible in the new tenant's own TicketSummary projection"
        );

        // ...but never touched the shared `helpdesk` context at all -
        // same ticket_id, a genuinely separate bounded context, not
        // just a differently-filtered view over one shared table.
        let in_shared: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert!(
            in_shared.status.is_none(),
            "a new tenant's own ticket must never leak into the shared helpdesk context's projection state: {in_shared:?}"
        );
    });
}
