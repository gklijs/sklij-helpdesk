//! Integration tests for the company signup/trial/subscription lifecycle
//! (`rule CompanySignsUp`, `TrialPeriodEnds`, `CompanySubscribes`) - real
//! HTTP requests through `Skilj::rest_router()`, against a real (or
//! embedded, or skipped - see `support::test_db`) Postgres.
//!
//! Split from a single `tests/helpdesk.rs` into several files by
//! concern (matching `skilj-demo`'s own `tests/banking.rs`/
//! `tests/courses.rs` split) - not just organisation: each `tests/*.rs`
//! file is its own compiled binary with its own `TEST_DB` and its own
//! embedded Postgres instance, and each `#[test]`'s own `setup()` builds
//! a brand new `Skilj` (its own connection pool, never explicitly torn
//! down between tests - `postgresql_embedded::PostgreSQL`'s cleanup
//! relies on process exit, and `static TEST_DB`'s contents are never
//! dropped even then). Enough accumulated tests sharing one embedded
//! instance exhausts it (`PoolTimedOut`, reproducible, not a race) -
//! confirmed by comparing against `skilj-demo`'s own suite, which never
//! puts more than 6 tests in one binary and never hits this. Keeping
//! each file's own test count comfortably under that ceiling is the fix
//! here, not a change to the shared harness or to skilj itself.

mod support;

use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use support::{accepted, mint_command_token, rejection_kind, runtime, setup, test_db, trigger, unique_name};

async fn token(
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    command_type_name: &str,
) -> String {
    mint_command_token(pool, mapping, BOUNDED_CONTEXT, command_type_name).await
}

#[test]
fn company_signup_succeeds() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;

        let response = trigger(
            &router,
            &sign_up,
            serde_json::json!({
                "company_id": unique_name("company"),
                "name": "Acme Corp",
                "contact_email": "support@acme.example",
            }),
        )
        .await;
        assert!(accepted(&response), "signup should be accepted: {response:?}");
    });
}

/// `SignUpCompany`'s own duplicate guard - not itself in the spec's
/// happy path, but a natural `requires`-shaped edge case worth locking
/// down: the same company_id can't sign up twice.
#[test]
fn signing_up_the_same_company_twice_is_rejected() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let company_id = unique_name("company");

        let payload = serde_json::json!({
            "company_id": company_id,
            "name": "Acme Corp",
            "contact_email": "support@acme.example",
        });
        let response = trigger(&router, &sign_up, payload.clone()).await;
        assert!(accepted(&response));

        let response = trigger(&router, &sign_up, payload).await;
        assert!(!accepted(&response), "second signup should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "already_signed_up");
    });
}

/// The bug `helpdesk.rs`'s own `CreateTicket` doc comment describes:
/// the spec originally required `company.status = active` for ticket
/// creation, which would have blocked it for the entire free trial.
/// Fixed in the spec (`requires: company.status != expired`) and here -
/// a ticket must be creatable while still `trialing`, never having
/// touched `ConvertCompanyTrial` at all.
#[test]
fn tickets_can_be_created_during_the_trial_without_converting_first() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let company_id = unique_name("company");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        let response = trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": unique_name("ticket"), "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
            }),
        )
        .await;
        assert!(accepted(&response), "ticket creation must work during the trial, not just after converting: {response:?}");
    });
}

/// `rule TrialPeriodEnds`'s success branch, submitted the way
/// `src/bin/scheduler.rs` really submits it.
#[test]
fn converting_a_trialing_company_activates_it() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let convert = token(&pool, &mapping, "ConvertCompanyTrial").await;
        let company_id = unique_name("company");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        let response = trigger(&router, &convert, serde_json::json!({ "company_id": company_id })).await;
        assert!(accepted(&response), "converting a trialing company should succeed: {response:?}");

        // converting twice is rejected - it's already active, not trialing
        let response = trigger(&router, &convert, serde_json::json!({ "company_id": company_id })).await;
        assert!(!accepted(&response));
        assert_eq!(rejection_kind(&response), "company_not_trialing");
    });
}

/// `rule TrialPeriodEnds`'s failure branch, and the consequence the
/// spec's own resolved design note describes: ticket creation is
/// blocked, nothing else about the company's existing data is affected.
#[test]
fn expiring_a_company_blocks_new_tickets_but_not_reactivation() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let expire = token(&pool, &mapping, "ExpireCompanyTrial").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let reactivate = token(&pool, &mapping, "ReactivateCompany").await;
        let company_id = unique_name("company");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        let response = trigger(&router, &expire, serde_json::json!({ "company_id": company_id })).await;
        assert!(accepted(&response), "expiring a trialing company should succeed: {response:?}");

        let response = trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": unique_name("ticket"), "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
            }),
        )
        .await;
        assert!(!accepted(&response), "an expired company can't create tickets: {response:?}");
        assert_eq!(rejection_kind(&response), "company_expired");

        // "they can always come back" - reactivation is unconditional
        let response = trigger(&router, &reactivate, serde_json::json!({ "company_id": company_id })).await;
        assert!(accepted(&response), "reactivating an expired company should succeed: {response:?}");

        let response = trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": unique_name("ticket"), "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "t2", "description": "d2", "priority": "low",
            }),
        )
        .await;
        assert!(accepted(&response), "ticket creation should work again after reactivating: {response:?}");
    });
}
