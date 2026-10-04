//! The tenant-access reconciler, driven end to end against a real
//! Postgres: `src/tenant_access.rs`'s own `sync_plan` unit tests prove the
//! decision, and this proves the decision reaches the database.
//!
//! What that buys, and why it needs its own file: `skilj-graphql`'s
//! `submitCommand` authorizes against the caller's `RoleAccessMapping` on
//! the *named* bounded context, never against the shared one. So a
//! tenant with no grant rejects every command aimed at it, and "the
//! reconciler correctly computed that a grant was needed" is not the same
//! claim as "a customer can now actually submit a ticket to their tenant".
//! The last test in this file asserts exactly that, end to end.
//!
//! ## Why every test here takes `exclusive()`
//!
//! One test in this file calls `reconcile_all_tenants`, which is a
//! **sweep over every recorded company/tenant pair in the database** -
//! that is the whole point of it, and it is exactly what `server.rs` runs
//! on an interval in production. But the database is shared by every test
//! in this binary, so that sweep also reaches *this file's other
//! fixtures*, not just one.
//!
//! Each other test's fixture is a live company/tenant pair by the time the
//! sweep runs, and each asserts something about the *unreconciled* state of
//! that pair. So the sweep corrupts all of them, not just the last one, and
//! it does so in two different ways:
//!
//! - `after_reconciliation_a_customer_can_submit_a_ticket_command_to_their_tenant`
//!   asserts a "before" state over GraphQL - the customer has no grant on
//!   their tenant *yet* - and a concurrent sweep grants it out from under
//!   the assertion, so the refusal it expects comes back `accepted: true`.
//!   This is the one CI actually caught: it failed on the first CI run of
//!   PR #9.
//! - `a_customers_shared_grant_is_projected_into_their_own_tenant` and
//!   `reconciling_twice_changes_nothing_the_second_time` assert the exact
//!   *shape* of what a reconciliation does - "this pass granted exactly this
//!   Role, and skipped nothing". A sweep that gets there first leaves the
//!   grant already in place, so the pass under test skips it instead
//!   (`already holds an active RoleAccessMapping`, or a unique-constraint
//!   violation when both run the insert at once) and `granted` comes back
//!   empty. Found by stress-running this file 30x locally, ~2 failures per
//!   30 runs - rarer than the first case, which is exactly why a CI that
//!   only runs the suite once would keep reporting it as a flake.
//!
//! Serializing every test in the file is therefore the honest fix, and it
//! is cheap: this file's eight tests share one already-migrated database
//! and no per-test setup beyond `fixture()`, so running them one at a time
//! costs a handful of seconds, not the minutes a naive read of "all the
//! integration tests take ~3s each" would suggest. Softening an assertion
//! instead would be worse - the "denied before, allowed after" pair is the
//! entire claim of the last test, and the production equivalent of the race
//! is real too (the server's reconciler runs every 5 seconds), so a window
//! in which no reconciliation has happened is a *precondition* of these
//! tests, not a detail of them.
//!
//! A file-scoped lock is sufficient: `cargo test` runs each test *binary*
//! to completion before the next, so the only concurrency to defend
//! against is between tests inside this file.

mod support;

use skilj_core::access_control::{AccessLevel, RoleAccessMapping, RoleStatus};
use skilj_core::db;
use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use skilj_helpdesk::tenant_access::{
    reconcile_all_tenants, reconcile_company_access, recorded_company_tenants,
};
use std::sync::LazyLock;
use support::{
    accepted, graphql_request, mint_command_token, runtime, seed_role, seed_scoped_mapping,
    seed_superadmin, setup_graphql, sign_jwt, test_db, trigger, unique_name,
};

/// Held across the two tests that cannot overlap - see the module docs.
static EXCLUSIVE: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));

/// Keeps the global sweep and the before/after test off each other.
///
/// Callers keep their own usual `test_db().await.is_none()` skip guard
/// *before* calling this, exactly like every other test in this crate -
/// a run with no database still skips cleanly rather than failing (see
/// `tests/support/mod.rs`'s own provisioning doc comment), which is also
/// what makes the CI job's own "fail on a silent DB skip" step the only
/// place that treats a missing database as an error.
async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
    EXCLUSIVE.lock().await
}

/// The company's tenant, plus the superadmin the reconciler acts as.
struct Fixture {
    company_id: String,
    tenant_name: String,
    ops_role: skilj_core::access_control::Role,
}

/// Signs a company up, records a tenant for it, and creates that tenant
/// for real - the same three steps production makes, so the reconciler
/// has something realistic to reconcile.
async fn fixture() -> (axum::Router, axum::Router, db::Pool, Fixture) {
    let (skilj, pool, _mapping, _jwt) = setup_graphql().await;
    let graphql_router = skilj.graphql_router().await.unwrap();
    let rest_router = skilj.rest_router();

    let sign_up = mint_command_token(&pool, &_mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
    let record_tenant =
        mint_command_token(&pool, &_mapping, BOUNDED_CONTEXT, "RecordCompanyTenant").await;
    // Deliberately a `company_id` that the old
    // `format!("company-{company_id}")` scheme could not have named:
    // `unique_name`'s underscore+digest form contains characters a
    // bounded context name forbids, which is exactly why
    // `routing::tenant_name_for` now derives this instead. Deriving the
    // same way the provisioner does is also what keeps this test's tenant
    // name and production's the same string.
    let company_id = format!("acme-corp {}", unique_name("x"));
    let tenant_name = skilj_helpdesk::routing::tenant_name_for(&company_id);
    trigger(
        &rest_router,
        &sign_up,
        serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
    )
    .await;
    let response = trigger(
        &rest_router,
        &record_tenant,
        serde_json::json!({ "company_id": company_id, "tenant_name": tenant_name }),
    )
    .await;
    assert!(
        accepted(&response),
        "recording the tenant should succeed: {response:?}"
    );

    // The ops identity: superadmin, so it may both create the tenant and
    // act as the reconciler's caller. This is deliberately the *same*
    // role, mirroring `server.rs`'s own bootstrap where one role holds
    // both.
    let ops_role = seed_superadmin(&pool).await;
    let ops_jwt = sign_jwt(&ops_role.external_subject);
    let mutation = format!(
        r#"mutation {{
            createBoundedContextFromTemplate(
                template: {template:?} name: {tenant_name:?} roleId: {role_id:?}
                level: ADMIN canReadSensitive: true
            ) {{ name }}
        }}"#,
        template = BOUNDED_CONTEXT,
        tenant_name = tenant_name,
        role_id = ops_role.id,
    );
    let response = graphql_request(&graphql_router, &ops_jwt, &mutation).await;
    assert!(
        response.get("errors").is_none(),
        "createBoundedContextFromTemplate should succeed: {response:?}"
    );

    (
        graphql_router,
        rest_router,
        pool,
        Fixture {
            company_id,
            tenant_name,
            ops_role,
        },
    )
}

async fn mappings_for(
    pool: &db::Pool,
    context_name: &str,
    role_id: &str,
) -> Vec<RoleAccessMapping> {
    let bc = db::get_bounded_context(pool, context_name)
        .await
        .unwrap()
        .expect("bounded context should exist");
    db::list_active_role_access_mappings_for_bounded_context(pool, &bc)
        .await
        .unwrap()
        .into_iter()
        .filter(|m| m.role.id == role_id)
        .collect()
}

#[test]
fn a_customers_shared_grant_is_projected_into_their_own_tenant() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let _guard = exclusive().await;
        let (_graphql, _rest, pool, fixture) = fixture().await;
        let customer = seed_role(&pool, "acme-customer").await;
        seed_scoped_mapping(
            &pool,
            &customer,
            AccessLevel::Write,
            Some(fixture.company_id.clone()),
        )
        .await;

        // Before: nothing but the ops grant exists on the tenant.
        let before = mappings_for(&pool, &fixture.tenant_name, &customer.id).await;
        assert!(
            before.is_empty(),
            "a customer Role must not start out with tenant access: {before:?}"
        );

        let outcome = reconcile_company_access(
            &pool,
            &fixture.ops_role,
            &fixture.company_id,
            &fixture.tenant_name,
        )
        .await;
        assert_eq!(
            outcome.granted,
            vec![customer.id.clone()],
            "outcome: {outcome:?}"
        );
        assert!(outcome.revoked.is_empty());
        assert!(outcome.skipped.is_empty(), "skipped: {:?}", outcome.skipped);

        let after = mappings_for(&pool, &fixture.tenant_name, &customer.id).await;
        assert_eq!(
            after.len(),
            1,
            "the customer should have exactly one tenant grant"
        );
        assert_eq!(
            after[0].scope.as_deref(),
            Some(fixture.company_id.as_str()),
            "the tenant copy must carry the same company scope as the shared one"
        );
        assert_eq!(
            after[0].level,
            AccessLevel::Write,
            "never weaker than the shared grant"
        );
        assert_eq!(after[0].status, RoleStatus::Active);
    });
}

#[test]
fn reconciling_twice_changes_nothing_the_second_time() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let _guard = exclusive().await;
        let (_graphql, _rest, pool, fixture) = fixture().await;
        let customer = seed_role(&pool, "acme-customer").await;
        seed_scoped_mapping(
            &pool,
            &customer,
            AccessLevel::Write,
            Some(fixture.company_id.clone()),
        )
        .await;

        let first = reconcile_company_access(
            &pool,
            &fixture.ops_role,
            &fixture.company_id,
            &fixture.tenant_name,
        )
        .await;
        assert_eq!(
            first.granted.len(),
            1,
            "first pass grants: {:?}",
            first.granted
        );

        // The whole point of running this on an interval rather than once:
        // the steady state has to cost nothing and change nothing.
        let second = reconcile_company_access(
            &pool,
            &fixture.ops_role,
            &fixture.company_id,
            &fixture.tenant_name,
        )
        .await;
        assert!(
            second.granted.is_empty() && second.revoked.is_empty() && second.skipped.is_empty(),
            "a second pass should be a no-op: {second:?}"
        );
    });
}

#[test]
fn revoking_a_customer_on_the_shared_context_revokes_them_in_the_tenant_too() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let _guard = exclusive().await;
        let (_graphql, _rest, pool, fixture) = fixture().await;
        let customer = seed_role(&pool, "acme-customer").await;
        let shared = seed_scoped_mapping(
            &pool,
            &customer,
            AccessLevel::Write,
            Some(fixture.company_id.clone()),
        )
        .await;
        reconcile_company_access(&pool, &fixture.ops_role, &fixture.company_id, &fixture.tenant_name)
            .await;
        assert_eq!(
            mappings_for(&pool, &fixture.tenant_name, &customer.id).await.len(),
            1,
            "precondition: the tenant grant exists before the revocation"
        );

        // The authority drops the grant.
        db::revoke_active_role_access_mapping(&pool, &customer.id, BOUNDED_CONTEXT, chrono::Utc::now())
            .await
            .unwrap();

        let outcome =
            reconcile_company_access(&pool, &fixture.ops_role, &fixture.company_id, &fixture.tenant_name)
                .await;
        assert_eq!(
            outcome.revoked,
            vec![customer.id.clone()],
            "the tenant copy must be revoked too, or the revocation is a no-op in practice: {outcome:?}"
        );
        assert!(
            mappings_for(&pool, &fixture.tenant_name, &customer.id).await.is_empty(),
            "the customer should have lost tenant access"
        );

        // And the ops identity that created the tenant is untouched, or
        // the provisioner would be locked out of its own tenant.
        assert!(
            !mappings_for(&pool, &fixture.tenant_name, &fixture.ops_role.id).await.is_empty(),
            "the reconciler must never revoke the provisioning identity"
        );
        drop(shared);
    });
}

#[test]
fn downgrading_a_customer_on_the_shared_context_downgrades_them_in_the_tenant_too() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let _guard = exclusive().await;
        let (_graphql, _rest, pool, fixture) = fixture().await;
        let customer = seed_role(&pool, "acme-customer").await;
        seed_scoped_mapping(
            &pool,
            &customer,
            AccessLevel::Write,
            Some(fixture.company_id.clone()),
        )
        .await;
        reconcile_company_access(
            &pool,
            &fixture.ops_role,
            &fixture.company_id,
            &fixture.tenant_name,
        )
        .await;

        // The authority narrows the grant from Write to Read.
        db::revoke_active_role_access_mapping(
            &pool,
            &customer.id,
            BOUNDED_CONTEXT,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        seed_scoped_mapping(
            &pool,
            &customer,
            AccessLevel::Read,
            Some(fixture.company_id.clone()),
        )
        .await;

        let outcome = reconcile_company_access(
            &pool,
            &fixture.ops_role,
            &fixture.company_id,
            &fixture.tenant_name,
        )
        .await;
        assert_eq!(
            outcome.regranted,
            vec![customer.id.clone()],
            "a narrowed shared grant must narrow the tenant copy: {outcome:?}"
        );
        assert!(outcome.skipped.is_empty(), "skipped: {:?}", outcome.skipped);
        let after = mappings_for(&pool, &fixture.tenant_name, &customer.id).await;
        assert_eq!(after.len(), 1, "exactly one active tenant grant: {after:?}");
        assert_eq!(
            after[0].level,
            AccessLevel::Read,
            "the tenant copy must never stay stronger than the authority"
        );
    });
}

#[test]
fn one_companys_grants_are_never_projected_into_another_companys_tenant() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let _guard = exclusive().await;
        let (_graphql, _rest, pool, fixture) = fixture().await;
        let globex_customer = seed_role(&pool, "globex-customer").await;
        seed_scoped_mapping(
            &pool,
            &globex_customer,
            AccessLevel::Write,
            Some(unique_name("other-company")),
        )
        .await;

        reconcile_company_access(
            &pool,
            &fixture.ops_role,
            &fixture.company_id,
            &fixture.tenant_name,
        )
        .await;

        assert!(
            mappings_for(&pool, &fixture.tenant_name, &globex_customer.id)
                .await
                .is_empty(),
            "another company's customer must not gain access to this tenant"
        );
    });
}

#[test]
fn a_company_with_no_roles_yet_is_reconciled_cleanly_rather_than_failing() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let _guard = exclusive().await;
        let (_graphql, _rest, pool, fixture) = fixture().await;
        let outcome = reconcile_company_access(
            &pool,
            &fixture.ops_role,
            &fixture.company_id,
            &fixture.tenant_name,
        )
        .await;
        assert!(
            outcome.granted.is_empty() && outcome.revoked.is_empty() && outcome.skipped.is_empty(),
            "an empty reconciliation is a success, not an error: {outcome:?}"
        );
    });
}

#[test]
fn every_recorded_company_tenant_pair_is_reconciled() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        // This is the half of the conflict that causes it: the sweep
        // below reaches every other fixture in this shared database, so
        // this test must not run beside
        // `after_reconciliation_a_customer_can_submit_a_ticket_command_to_their_tenant`.
        let _guard = exclusive().await;
        let (_graphql, _rest, pool, fixture) = fixture().await;
        let customer = seed_role(&pool, "acme-customer").await;
        seed_scoped_mapping(&pool, &customer, AccessLevel::Write, Some(fixture.company_id.clone()))
            .await;

        let pairs = recorded_company_tenants(&pool).await.unwrap();
        assert!(
            pairs.iter().any(|(company_id, tenant_name)| {
                company_id == &fixture.company_id && tenant_name == &fixture.tenant_name
            }),
            "the recorded tenant should be discoverable from the shared context's own history: {pairs:?}"
        );

        let outcomes = reconcile_all_tenants(&pool, &fixture.ops_role).await;
        assert!(
            outcomes.iter().any(|o| o.company_id == fixture.company_id
                && o.granted.contains(&customer.id)),
            "the company-wide sweep should have granted this company's customer: {outcomes:?}"
        );
    });
}

/// Submit a `CreateTicket` as `jwt` naming `bounded_context` explicitly.
/// A free function rather than a closure so the caller can invoke it
/// twice against identical inputs - which is what makes the before/after
/// pair below attributable to the reconciler and nothing else.
async fn submit_create_ticket(
    router: &axum::Router,
    jwt: &str,
    bounded_context: &str,
    company_id: &str,
) -> serde_json::Value {
    let payload = serde_json::json!({
        "ticket_id": unique_name("ticket"), "company_id": company_id,
        "requester_id": unique_name("customer"), "logged_by_staff_id": null,
        "title": "routed at a tenant", "description": "d", "priority": "low",
    });
    let payload_literal = serde_json::to_string(&serde_json::to_string(&payload).unwrap()).unwrap();
    let mutation = format!(
        r#"mutation {{
            submitCommand(
                boundedContext: {bounded_context:?}
                commandTypeName: "CreateTicket"
                payload: {payload_literal}
            ) {{ accepted rejectionKind }}
        }}"#
    );
    graphql_request(router, jwt, &mutation).await
}

/// The end-to-end claim the whole module exists to enable: after
/// reconciliation, a customer whose grant lives only on the shared context
/// can actually submit a Ticket command *to their tenant* over GraphQL -
/// which is the thing that silently 403s if the reconciler didn't run, and
/// is invisible to every other test in this file.
///
/// Takes `exclusive()` because the "before" half of this test asserts an
/// unreconciled state, and `every_recorded_company_tenant_pair_is_reconciled`
/// grants exactly that out from under it - see the module docs.
#[test]
fn after_reconciliation_a_customer_can_submit_a_ticket_command_to_their_tenant() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let _guard = exclusive().await;
        let (graphql_router, _rest, pool, fixture) = fixture().await;

        // Mirror the lifecycle first, the way the replicator would, so the
        // tenant's `company_status` guard can pass - this test is about
        // *authorization*, not about the lifecycle mirror.
        let ops_mapping = db::get_active_role_access_mapping(&pool, &fixture.ops_role.id, &fixture.tenant_name)
            .await
            .unwrap()
            .expect("createBoundedContextFromTemplate granted the ops role on the tenant");
        let record_lifecycle =
            mint_command_token(&pool, &ops_mapping, &fixture.tenant_name, "RecordTenantLifecycle")
                .await;
        trigger(
            &_rest,
            &record_lifecycle,
            serde_json::json!({
                "company_id": fixture.company_id, "status": "trialing",
                "source_event_type": "CompanySignedUp",
            }),
        )
        .await;

        let customer = seed_role(&pool, "acme-customer").await;
        seed_scoped_mapping(&pool, &customer, AccessLevel::Write, Some(fixture.company_id.clone()))
            .await;
        let customer_jwt = sign_jwt(&customer.external_subject);

// Before reconciliation: the customer has a mapping on the shared
        // context only, so naming their own tenant is a 403 - no mapping
        // on that context at all.
        let response = submit_create_ticket(
            &graphql_router,
            &customer_jwt,
            &fixture.tenant_name,
            &fixture.company_id,
        )
        .await;
        // A caller with no mapping on the named context is refused at
        // *authorization*, before `decide()` ever runs - so GraphQL
        // answers `data: null` plus a `grant_not_active` error rather
        // than a `submitCommand { accepted: false }`. Asserting on
        // `accepted` alone would silently pass here for the wrong reason
        // (a missing field defaults to "not accepted"), which is exactly
        // the shape that would make this test unable to tell "denied" from
        // "rejected by a rule".
        assert!(
            response["data"].is_null(),
            "a customer with no tenant grant must not be able to submit there: {response:?}"
        );
        assert_eq!(
            response["errors"][0]["extensions"]["code"],
            "grant_not_active",
            "the refusal should be the authorization check specifically: {response:?}"
        );
        assert!(
            mappings_for(&pool, &fixture.tenant_name, &customer.id)
                .await
                .is_empty(),
            "precondition: still no tenant grant before the reconciler runs"
        );

        reconcile_company_access(&pool, &fixture.ops_role, &fixture.company_id, &fixture.tenant_name)
            .await;

        // After: the same request, same customer, same tenant.
        let response = submit_create_ticket(
            &graphql_router,
            &customer_jwt,
            &fixture.tenant_name,
            &fixture.company_id,
        )
        .await;
        let accepted = response["data"]["submitCommand"]["accepted"].as_bool();
        assert_eq!(
            accepted,
            Some(true),
            "after reconciliation the customer should be able to submit to their tenant: {response:?}"
        );
    });
}
