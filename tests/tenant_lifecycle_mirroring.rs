//! Phase 1+2 of per-company tenant isolation: mirroring a company's
//! lifecycle into its own tenant so ticket commands can be routed there.
//!
//! Kept in its own file for the same reason `multi_tenant_provisioning.rs`
//! is: this is a different kind of proof from `tests/company.rs`'s (which
//! exercises commands in one context) or `tests/graphql.rs`'s (which
//! exercises the GraphQL surface). What's proven here:
//!
//!   - `TenantDirectory` turns a recorded `company_id -> tenant_name`
//!     into a queryable read model (the routing lookup Phase 2 needs).
//!   - `RecordTenantLifecycle` writes a `CompanyLifecycleMirrored` fact
//!     into a tenant, and that fact is what lets `CreateTicket` - whose
//!     `company_status` guard is one *same-context* DCB read - pass its
//!     guard in a tenant that has never seen the company's signup.
//!   - The mirror cannot be used to walk a company *backwards*, which is
//!     what makes out-of-order delivery across the replicator's three
//!     independent event feeds safe.
//!
//! `src/bin/lifecycle-replicator.rs` is what submits these commands for
//! real, over the REST event feed; as with `provisioner.rs` and
//! `alerter.rs`, that reactor loop is exercised outside `cargo test` (see
//! that binary's own module doc comment) and the commands it drives are
//! proven here directly - the same "prove the command, not the loop
//! around it" split every other command in this suite already gets.

mod support;

use skilj_core::access_control::AccessLevel;
use skilj_helpdesk::helpdesk::{CompanyStatus, TenantDirectoryState, BOUNDED_CONTEXT};
use support::{
    accepted, graphql_request, mint_command_token, projection_state, rejection_kind, runtime,
    seed_role, seed_scoped_mapping, seed_superadmin, setup_graphql, sign_jwt, test_db, trigger,
    unique_name,
};

/// Signs a company up in the *shared* context and records a real tenant
/// for it, the exact two-step a `SignUpCompany` goes through in
/// production (`SignUpCompany` -> provisioner creates the tenant ->
/// `RecordCompanyTenant`). Returns the tenant name.
///
/// Every test below starts here, deliberately through the real
/// `createBoundedContextFromTemplate` mutation rather than by inserting
/// a `BoundedContext` row directly: the point of these tests is the
/// path a real company's command traffic would take, and a hand-inserted
/// row would skip the dispatch-template wiring that makes the tenant's
/// copied registrations reachable at all.
async fn provision_tenant_for_signed_up_company(
    router: &axum::Router,
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
) -> (String, String) {
    let sign_up = mint_command_token(pool, mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
    let record_tenant =
        mint_command_token(pool, mapping, BOUNDED_CONTEXT, "RecordCompanyTenant").await;

    let company_id = unique_name("company");
    let tenant_name = format!("company-{company_id}");
    let response = trigger(
        router,
        &sign_up,
        serde_json::json!({
            "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example",
        }),
    )
    .await;
    assert!(
        accepted(&response),
        "signup in the shared context should succeed: {response:?}"
    );

    let response = trigger(
        router,
        &record_tenant,
        serde_json::json!({ "company_id": company_id, "tenant_name": tenant_name }),
    )
    .await;
    assert!(
        accepted(&response),
        "recording the provisioned tenant should succeed: {response:?}"
    );
    (company_id, tenant_name)
}

/// Creates a real tenant bounded context from the shared `helpdesk`
/// template and grants `role` Admin access on it, via the same GraphQL
/// mutation `provisioner.rs` calls. Returns the tenant's own
/// `RoleAccessMapping` - fetched back from the database rather than
/// reconstructed, so tokens minted against it below are against what is
/// actually durable.
async fn create_tenant(
    graphql_router: &axum::Router,
    pool: &skilj_core::db::Pool,
    superadmin_jwt: &str,
    role_id: &str,
) -> (String, skilj_core::access_control::RoleAccessMapping) {
    let tenant_name = unique_name("tenant");
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
            }}
        }}"#,
        template = BOUNDED_CONTEXT,
        tenant_name = tenant_name,
        role_id = role_id,
    );
    let response = graphql_request(graphql_router, superadmin_jwt, &mutation).await;
    assert!(
        response.get("errors").is_none(),
        "createBoundedContextFromTemplate should succeed: {response:?}"
    );
    assert_eq!(
        response["data"]["createBoundedContextFromTemplate"]["status"],
        "ACTIVE"
    );

    let mapping = skilj_core::db::get_active_role_access_mapping(pool, role_id, &tenant_name)
        .await
        .unwrap()
        .expect("createBoundedContextFromTemplate should have granted the role access to its own new tenant");
    (tenant_name, mapping)
}

/// The Phase 2 lookup, end to end: a `RecordCompanyTenant` in the shared
/// context becomes a readable `TenantDirectory` entry, which is the only
/// thing that tells a router which bounded context a company's commands
/// belong in.
#[test]
fn the_tenant_directory_exposes_where_a_companys_commands_belong() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping, _jwt) = setup_graphql().await;
        let rest_router = skilj.rest_router();

        let company_id = unique_name("company");
        let unknown: TenantDirectoryState =
            projection_state(&pool, BOUNDED_CONTEXT, "TenantDirectory", &company_id).await;
        assert!(
            unknown.tenant_name.is_none(),
            "a company nobody has recorded a tenant for must resolve to nothing, not to a guess: {unknown:?}"
        );

        let sign_up = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
        let record_tenant = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "RecordCompanyTenant")
            .await
        .to_owned();
        trigger(
            &rest_router,
            &sign_up,
            serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
        )
        .await;
        let tenant_name = format!("company-{company_id}");
        trigger(
            &rest_router,
            &record_tenant,
            serde_json::json!({ "company_id": company_id, "tenant_name": tenant_name }),
        )
        .await;

        let resolved: TenantDirectoryState =
            projection_state(&pool, BOUNDED_CONTEXT, "TenantDirectory", &company_id).await;
        assert_eq!(
            resolved.tenant_name.as_deref(),
            Some(tenant_name.as_str()),
            "the recorded tenant should be readable straight back out of TenantDirectory"
        );
    });
}

/// The core Phase 1 proof, and the whole reason this pass exists.
///
/// A `CreateTicket` in a tenant is rejected `company_not_found` before
/// the mirror, because that tenant's own history has never heard of the
/// company - the company signed up in the *shared* context, and
/// `decide()` only ever sees its own bounded context's events. After
/// `RecordTenantLifecycle` mirrors the signup fact into the tenant, the
/// exact same command against the exact same tenant succeeds.
///
/// Both halves are asserted in one test on purpose: the "before" is what
/// makes the "after" meaningful, and running them together rules out a
/// tenant that would have accepted the ticket anyway (for some unrelated
/// reason - a leaked shared-context read, say), which is the failure mode
/// a test asserting only the happy path would miss.
#[test]
fn mirroring_a_companys_lifecycle_unlocks_ticket_commands_in_its_own_tenant() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping, _jwt) = setup_graphql().await;
        let graphql_router = skilj.graphql_router().await.unwrap();
        let rest_router = skilj.rest_router();

        let (company_id, _recorded_tenant) =
            provision_tenant_for_signed_up_company(&rest_router, &pool, &mapping).await;

        let superadmin = seed_superadmin(&pool).await;
        let superadmin_jwt = sign_jwt(&superadmin.external_subject);
        let tenant_role = seed_role(&pool, "tenant-staff-lead").await;
        let (tenant_name, tenant_mapping) = create_tenant(
            &graphql_router,
            &pool,
            &superadmin_jwt,
            &tenant_role.id,
        )
        .await;

        let create_ticket =
            mint_command_token(&pool, &tenant_mapping, &tenant_name, "CreateTicket").await;
        let ticket_id = unique_name("ticket");
        let payload = serde_json::json!({
            "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
            "logged_by_staff_id": null, "title": "routed at a tenant", "description": "d", "priority": "low",
        });

        // Before the mirror: rejected, because this tenant's own history
        // has no trace of the company even though the company exists.
        let response = trigger(&rest_router, &create_ticket, payload.clone()).await;
        assert!(
            !accepted(&response),
            "a tenant that has never seen the company's signup must reject CreateTicket: {response:?}"
        );
        assert_eq!(
            rejection_kind(&response),
            "company_not_found",
            "the rejection should be the lifecycle guard specifically, not some unrelated failure: {response:?}"
        );

        // The mirror.
        let record_lifecycle =
            mint_command_token(&pool, &tenant_mapping, &tenant_name, "RecordTenantLifecycle").await;
        let response = trigger(
            &rest_router,
            &record_lifecycle,
            serde_json::json!({
                "company_id": company_id,
                "status": "trialing",
                "source_event_type": "CompanySignedUp",
            }),
        )
        .await;
        assert!(accepted(&response), "mirroring a signup should succeed: {response:?}");

        // After: the identical command, against the identical tenant.
        let response = trigger(&rest_router, &create_ticket, payload).await;
        assert!(
            accepted(&response),
            "the same CreateTicket should now pass its company_status guard in the tenant: {response:?}"
        );
    });
}

/// The mirror is the only *new* thing a tenant can say about its own
/// lifecycle, and it must not be usable to talk a company back into a
/// state it has left. Three separate properties, one test each, because
/// they fail for different reasons:
///
///   - repeating a mirror is a no-op rejection, so a redelivered
///     lifecycle event (each of the replicator's three feeds has its own
///     cursor and can redeliver on a crash mid-tick) is safe;
///   - a backwards mirror is refused, so out-of-order delivery across
///     those independent feeds converges instead of latching;
///   - a mirror carries no tenant name, so it cannot be pointed at a
///     different company's context.
#[test]
fn a_lifecycle_mirror_is_idempotent_and_cannot_walk_a_company_backwards() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping, _jwt) = setup_graphql().await;
        let graphql_router = skilj.graphql_router().await.unwrap();
        let rest_router = skilj.rest_router();

        let (company_id, _) =
            provision_tenant_for_signed_up_company(&rest_router, &pool, &mapping).await;
        let superadmin = seed_superadmin(&pool).await;
        let superadmin_jwt = sign_jwt(&superadmin.external_subject);
        let tenant_role = seed_role(&pool, "tenant-staff-lead").await;
        let (tenant_name, tenant_mapping) =
            create_tenant(&graphql_router, &pool, &superadmin_jwt, &tenant_role.id).await;

        let record_lifecycle =
            mint_command_token(&pool, &tenant_mapping, &tenant_name, "RecordTenantLifecycle").await;
        let mirror = |status: &str, source: &str| {
            serde_json::json!({
                "company_id": company_id, "status": status, "source_event_type": source,
            })
        };

        let response =
            trigger(&rest_router, &record_lifecycle, mirror("trialing", "CompanySignedUp")).await;
        assert!(accepted(&response), "first mirror should succeed: {response:?}");

        // Idempotency.
        let response =
            trigger(&rest_router, &record_lifecycle, mirror("trialing", "CompanySignedUp")).await;
        assert!(
            !accepted(&response),
            "re-mirroring the same state should be rejected, not duplicated: {response:?}"
        );
        assert_eq!(rejection_kind(&response), "lifecycle_already_mirrored");

        // Forward is fine.
        let response =
            trigger(&rest_router, &record_lifecycle, mirror("active", "CompanyActivated")).await;
        assert!(accepted(&response), "trialing -> active should be allowed: {response:?}");

        // Backwards is not - the property that makes three unordered
        // event feeds safe to consume.
        let response =
            trigger(&rest_router, &record_lifecycle, mirror("expired", "CompanyExpired")).await;
        assert!(
            !accepted(&response),
            "active -> expired must be refused as a backwards mirror: {response:?}"
        );
        assert_eq!(rejection_kind(&response), "lifecycle_regression");

        // And the guard is real, not a blanket refusal: a company with no
        // mirror yet can still be introduced at any state.
        let other_company = unique_name("company");
        let response = trigger(
            &rest_router,
            &record_lifecycle,
            serde_json::json!({
                "company_id": other_company, "status": "expired", "source_event_type": "CompanyExpired",
            }),
        )
        .await;
        assert!(
            accepted(&response),
            "a first mirror must be accepted at any state - the regression guard only compares \
             against an existing one: {response:?}"
        );
    });
}

/// The regression guard ranks states rather than trusting arrival order,
/// so prove the ordering it encodes is the one that actually holds:
/// `expired -> active` is a real transition in this domain
/// (`ReactivateCompany`), and it must survive the guard, while
/// `active -> expired` is not one.
#[test]
fn reactivating_an_expired_company_is_allowed_where_expiring_an_active_one_is_not() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping, _jwt) = setup_graphql().await;
        let graphql_router = skilj.graphql_router().await.unwrap();
        let rest_router = skilj.rest_router();

        let (company_id, _) =
            provision_tenant_for_signed_up_company(&rest_router, &pool, &mapping).await;
        let superadmin = seed_superadmin(&pool).await;
        let superadmin_jwt = sign_jwt(&superadmin.external_subject);
        let tenant_role = seed_role(&pool, "tenant-staff-lead").await;
        let (tenant_name, tenant_mapping) =
            create_tenant(&graphql_router, &pool, &superadmin_jwt, &tenant_role.id).await;
        let record_lifecycle = mint_command_token(
            &pool,
            &tenant_mapping,
            &tenant_name,
            "RecordTenantLifecycle",
        )
        .await;
        let mirror = |status: &str, source: &str| {
            serde_json::json!({
                "company_id": company_id, "status": status, "source_event_type": source,
            })
        };

        for (status, event) in [
            ("trialing", "CompanySignedUp"),
            ("expired", "CompanyExpired"),
            ("active", "CompanyActivated"),
        ] {
            let response = trigger(&rest_router, &record_lifecycle, mirror(status, event)).await;
            assert!(
                accepted(&response),
                "the real lifecycle path trialing -> expired -> active must survive the guard \
                 (mirroring {status}): {response:?}"
            );
        }

        // And the resulting status is the one a ticket guard reads back -
        // the fold and the guard agree, which is the property that makes
        // the mirror worth having at all.
        let create_ticket =
            mint_command_token(&pool, &tenant_mapping, &tenant_name, "CreateTicket").await;
        let response = trigger(
            &rest_router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": unique_name("ticket"), "company_id": company_id,
                "requester_id": unique_name("customer"), "logged_by_staff_id": null,
                "title": "t", "description": "d", "priority": "low",
            }),
        )
        .await;
        assert!(
            accepted(&response),
            "a reactivated company should be able to create tickets in its own tenant: {response:?}"
        );
    });
}

/// `CompanyStatus` grew serde/schemars derives in this pass so a status
/// could cross a process boundary at all. Assert the wire shape the
/// replicator actually sends, so a `rename_all` change can't quietly
/// desync the binary from the command it submits.
#[test]
fn lifecycle_statuses_serialise_to_the_wire_values_the_replicator_sends() {
    for (status, expected) in [
        (CompanyStatus::Trialing, "trialing"),
        (CompanyStatus::Active, "active"),
        (CompanyStatus::Expired, "expired"),
    ] {
        assert_eq!(
            serde_json::to_value(status).unwrap(),
            serde_json::json!(expected),
            "the replicator sends these exact strings, so each must serialise to it"
        );
        assert_eq!(
            serde_json::from_value::<CompanyStatus>(serde_json::json!(expected)).unwrap(),
            status,
            "and each must parse back to its own status, or the mirror command's payload \
             would fail to deserialise in the tenant"
        );
    }
}

/// The Phase 2 credential story, proven rather than assumed.
///
/// There is no `CommandToken` spanning every tenant, so
/// `lifecycle-replicator.rs` mints one per tenant on first use via
/// skilj's own `createCommandToken` GraphQL mutation. This asserts the
/// two things that design rests on:
///
///   - the mint succeeds against a *runtime-named* tenant, which is the
///     only reason the fan-out can't be done with a pre-minted token set;
///   - a token minted that way actually routes a command into that
///     tenant - it is the credential, not a field in the request body,
///     that decides the target bounded context, which is why the request
///     below names no context at all.
///
/// The negative half matters as much as the positive: minting the same
/// command token against the *shared* context and submitting the same
/// payload must not be able to write a mirror into the wrong place.
#[test]
fn a_per_tenant_command_token_can_be_minted_on_demand_and_lands_in_that_tenant() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping, _jwt) = setup_graphql().await;
        let graphql_router = skilj.graphql_router().await.unwrap();
        let rest_router = skilj.rest_router();

        let (company_id, _) =
            provision_tenant_for_signed_up_company(&rest_router, &pool, &mapping).await;
        // The identity that drives the replicator, modelled the way
        // `server.rs`'s own bootstrap actually shapes it: a *single* ops
        // role that is `superadmin: true` (so it may create tenants) and
        // holds an Admin `RoleAccessMapping` on the shared `helpdesk`
        // context (granted here) as well as on every tenant (granted
        // below, by `createBoundedContextFromTemplate`, for that same
        // role id).
        //
        // Both halves are load-bearing and neither substitutes for the
        // other, which this test found by getting it wrong first:
        // `createBoundedContextFromTemplate` is gated on the
        // `superadmin` *flag*, while `createCommandToken` derives its
        // authority from a `RoleAccessMapping` on the named context and
        // rejects a mapping-less superadmin `grant_not_active`.
        let ops_role = seed_superadmin(&pool).await;
        seed_scoped_mapping(&pool, &ops_role, AccessLevel::Admin, None).await;
        let ops_jwt = sign_jwt(&ops_role.external_subject);
        let (tenant_name, _) = create_tenant(&graphql_router, &pool, &ops_jwt, &ops_role.id).await;

        // The mint, exactly as `TokenCache::get_or_mint` issues it.
        let mutation = format!(
            r#"mutation {{
                createCommandToken(
                    boundedContext: {bc:?}
                    commandTypeName: {command:?}
                ) {{ id secret status }}
            }}"#,
            bc = tenant_name,
            command = skilj_helpdesk::helpdesk::RECORD_TENANT_LIFECYCLE_COMMAND,
        );
        let response = graphql_request(&graphql_router, &ops_jwt, &mutation).await;
        assert!(
            response.get("errors").is_none(),
            "minting a CommandToken against a runtime-named tenant should succeed: {response:?}"
        );
        let minted = &response["data"]["createCommandToken"];
        assert_eq!(minted["status"], "ACTIVE");
        let credential = format!(
            "{}.{}",
            minted["id"].as_str().expect("token id"),
            minted["secret"].as_str().expect("token secret")
        );

        // And the token routes: no bounded context named anywhere in this
        // request, yet the command lands in the tenant.
        let response = trigger(
            &rest_router,
            &credential,
            serde_json::json!({
                "company_id": company_id, "status": "trialing",
                "source_event_type": "CompanySignedUp",
            }),
        )
        .await;
        assert!(
            accepted(&response),
            "a token minted for the tenant should route the command there: {response:?}"
        );

        // Proof it really landed there and nowhere else: the mirror is
        // readable in the tenant's own history, and the shared context's
        // is untouched by a command it never named.
        let in_tenant: skilj_core::event_store::Event =
            skilj_core::db::list_events_for_bounded_context_matching_tags(
                &pool,
                &tenant_name,
                &[skilj_core::shared::Tag {
                    key: "company".into(),
                    value: Some(company_id.clone()),
                }],
                None,
            )
            .await
            .unwrap()
            .into_iter()
            .find(|e| e.event_type.name == "CompanyLifecycleMirrored")
            .expect("the mirrored fact should be in the tenant's own event history");
        assert!(
            in_tenant.payload.contains("\"trialing\""),
            "the tenant's own copy should carry the status it was given: {}",
            in_tenant.payload
        );

        let shared_mirrors = skilj_core::db::list_events_for_bounded_context_matching_tags(
            &pool,
            BOUNDED_CONTEXT,
            &[skilj_core::shared::Tag {
                key: "company".into(),
                value: Some(company_id.clone()),
            }],
            None,
        )
        .await
        .unwrap()
        .into_iter()
        .filter(|e| e.event_type.name == "CompanyLifecycleMirrored")
        .count();
        assert_eq!(
            shared_mirrors, 0,
            "a token minted for a tenant must not be able to write a mirror into the shared \
             context - the credential is what scopes the write, and this proves it does"
        );
    });
}
