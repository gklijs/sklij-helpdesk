//! The two company-level read models outside `helpdesk`:
//! `marketing::MarketingSignals` and `activity::CompanyEngagement`
//! (issue #38). Each test drives the real commands - and, for marketing,
//! the real `CrossContextRoute`s - and then reads the folded state, first
//! straight from the projection table and then over GraphQL, where both
//! projections are staff-only (`TEAM_ONLY`) and company-scoped
//! (`OWNER_TAG_KEY`), the same two gates
//! `tests/cross_company_projection_scoping.rs` proves for
//! `TicketInternalNotes`.

mod support;

use skilj_core::access_control::AccessLevel;
use skilj_helpdesk::activity::CompanyEngagementState;
use skilj_helpdesk::helpdesk::STAFF_TEAM;
use skilj_helpdesk::marketing::MarketingSignalsState;
use std::time::Duration;
use support::{
    accepted, graphql_request, mint_command_token, projection_state, runtime, seed_mapping_for,
    seed_role, setup_all_contexts, sign_jwt, test_db, trigger, unique_name, wait_until,
    AllContextMappings,
};

const HELPDESK: &str = skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
const ACTIVITY: &str = skilj_helpdesk::activity::BOUNDED_CONTEXT;
const MARKETING: &str = skilj_helpdesk::marketing::BOUNDED_CONTEXT;

async fn submit(
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    router: &axum::Router,
    context: &str,
    command: &str,
    payload: serde_json::Value,
) {
    let token = mint_command_token(pool, mapping, context, command).await;
    let response = trigger(router, &token, payload).await;
    assert!(
        accepted(&response),
        "{command} should be accepted: {response:?}"
    );
}

async fn sign_up_company(
    pool: &skilj_core::db::Pool,
    mappings: &AllContextMappings,
    router: &axum::Router,
) -> String {
    let company_id = unique_name("company");
    submit(
        pool,
        &mappings.helpdesk,
        router,
        HELPDESK,
        "SignUpCompany",
        serde_json::json!({ "company_id": company_id, "name": "Acme Corp", "contact_email": "support@acme.example" }),
    )
    .await;
    company_id
}

fn projection_query(context: &str, name: &str, key: &str, fields: &str) -> String {
    format!(
        r#"query {{ projection(boundedContext: {context:?}, name: {name:?}, key: {key:?}) {{ ... on {context}_{name} {{ {fields} }} }} }}"#
    )
}

fn succeeded(response: &serde_json::Value) -> bool {
    response.get("errors").is_none() && !response["data"]["projection"].is_null()
}

/// Which gate refused a read - so a refusal for some other reason (a
/// typo'd type name, a missing grant) can't pass for the one under test.
fn error_code(response: &serde_json::Value) -> &str {
    response["errors"][0]["extensions"]["code"]
        .as_str()
        .unwrap_or_default()
}

/// A staff Role with Read on both contexts, a customer Role scoped to
/// `company_id`, and a staff Role scoped to some other company - the
/// three cases each projection's gates have to tell apart.
async fn readers(pool: &skilj_core::db::Pool, company_id: &str) -> (String, String, String) {
    let staff = seed_role(pool, STAFF_TEAM).await;
    let customer = seed_role(pool, "customer").await;
    let other_staff = seed_role(pool, STAFF_TEAM).await;
    for context in [ACTIVITY, MARKETING] {
        seed_mapping_for(pool, &staff, context, AccessLevel::Read, None).await;
        seed_mapping_for(
            pool,
            &customer,
            context,
            AccessLevel::Write,
            Some(company_id.to_string()),
        )
        .await;
        seed_mapping_for(
            pool,
            &other_staff,
            context,
            AccessLevel::Read,
            Some(unique_name("other-company")),
        )
        .await;
    }
    (
        sign_jwt(&staff.external_subject),
        sign_jwt(&customer.external_subject),
        sign_jwt(&other_staff.external_subject),
    )
}

/// Expire, reactivate, then flag declining engagement: one lapse, one
/// conversion and the flag, each routed into marketing and folded into
/// that company's `MarketingSignals` row - ending `converted`, since the
/// reactivation came last.
#[test]
fn marketing_signals_fold_every_routed_fact_for_staff_only() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mappings) = setup_all_contexts().await;
        let router = skilj.rest_router();
        let company_id = sign_up_company(&pool, &mappings, &router).await;
        let company = serde_json::json!({ "company_id": company_id });

        submit(&pool, &mappings.helpdesk, &router, HELPDESK, "ExpireCompanyTrial", company.clone()).await;
        // The lapse and the reactivation route independently, so wait for
        // the lapse first: otherwise the two could fold in either order.
        wait_until(Duration::from_secs(10), "TrialLapsed folded", || async {
            projection_state::<MarketingSignalsState>(&pool, MARKETING, "MarketingSignals", &company_id)
                .await
                .lapse_count
                == 1
        })
        .await;
        submit(&pool, &mappings.helpdesk, &router, HELPDESK, "ReactivateCompany", company.clone()).await;
        submit(
            &pool,
            &mappings.activity,
            &router,
            ACTIVITY,
            "RecordEngagementDecline",
            serde_json::json!({ "company_id": company_id, "flagged_at": "2026-10-01T00:00:00+00:00" }),
        )
        .await;

        wait_until(Duration::from_secs(10), "every marketing signal folded", || async {
            let state: MarketingSignalsState =
                projection_state(&pool, MARKETING, "MarketingSignals", &company_id).await;
            state.conversion_count == 1 && state.engagement_declined_at.is_some()
        })
        .await;
        let state: MarketingSignalsState =
            projection_state(&pool, MARKETING, "MarketingSignals", &company_id).await;
        assert_eq!(state.trial_status.as_deref(), Some("converted"));
        assert_eq!(state.lapse_count, 1);
        assert!(state.last_lapsed_at.is_some() && state.last_converted_at.is_some());
        assert_eq!(
            state.engagement_declined_at.as_deref(),
            Some("2026-10-01T00:00:00+00:00")
        );

        let graphql = skilj.graphql_router().await.unwrap();
        let (staff, customer, other_staff) = readers(&pool, &company_id).await;
        let query = projection_query(
            MARKETING,
            "MarketingSignals",
            &company_id,
            "trialStatus lapseCount conversionCount engagementDeclinedAt",
        );
        let read = graphql_request(&graphql, &staff, &query).await;
        assert!(succeeded(&read), "staff should read the signals: {read:?}");
        assert_eq!(read["data"]["projection"]["trialStatus"], "converted");
        assert_eq!(read["data"]["projection"]["conversionCount"], 1);
        let read = graphql_request(&graphql, &customer, &query).await;
        assert_eq!(error_code(&read), "not_on_required_team", "a customer must not read their own company\'s signals: {read:?}");
        let read = graphql_request(&graphql, &other_staff, &query).await;
        assert_eq!(error_code(&read), "grant_scope_mismatch", "staff scoped to another company must not read these: {read:?}");
    });
}

/// Two customer days and one staff day, then the decline flag: counts
/// per kind, the latest day per kind regardless of submission order, and
/// the flag.
#[test]
fn company_engagement_counts_person_days_for_staff_only() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mappings) = setup_all_contexts().await;
        let router = skilj.rest_router();
        let company_id = sign_up_company(&pool, &mappings, &router).await;
        let customer_sub = unique_name("customer");
        let staff_sub = unique_name("staff");

        for (kind, subject, day) in [
            ("customer", &customer_sub, "2026-10-11T00:00:00.000Z"),
            ("customer", &customer_sub, "2026-10-09T00:00:00.000Z"),
            ("staff", &staff_sub, "2026-10-10T00:00:00.000Z"),
        ] {
            submit(
                &pool,
                &mappings.activity,
                &router,
                ACTIVITY,
                "RecordDailyActivity",
                serde_json::json!({ "company_id": company_id, "person_kind": kind, "person_subject": subject, "day": day }),
            )
            .await;
        }
        submit(
            &pool,
            &mappings.activity,
            &router,
            ACTIVITY,
            "RecordEngagementDecline",
            serde_json::json!({ "company_id": company_id, "flagged_at": "2026-10-12T00:00:00+00:00" }),
        )
        .await;

        wait_until(Duration::from_secs(10), "CompanyEngagement catch-up", || async {
            projection_state::<CompanyEngagementState>(&pool, ACTIVITY, "CompanyEngagement", &company_id)
                .await
                .engagement_declined_at
                .is_some()
        })
        .await;
        let state: CompanyEngagementState =
            projection_state(&pool, ACTIVITY, "CompanyEngagement", &company_id).await;
        assert_eq!(state.customer_active_days, 2);
        assert_eq!(state.staff_active_days, 1);
        assert_eq!(state.last_customer_day.as_deref(), Some("2026-10-11T00:00:00.000Z"));
        assert_eq!(state.last_staff_day.as_deref(), Some("2026-10-10T00:00:00.000Z"));

        let graphql = skilj.graphql_router().await.unwrap();
        let (staff, customer, other_staff) = readers(&pool, &company_id).await;
        let query = projection_query(
            ACTIVITY,
            "CompanyEngagement",
            &company_id,
            "customerActiveDays staffActiveDays lastCustomerDay engagementDeclinedAt",
        );
        let read = graphql_request(&graphql, &staff, &query).await;
        assert!(succeeded(&read), "staff should read engagement: {read:?}");
        assert_eq!(read["data"]["projection"]["customerActiveDays"], 2);
        let read = graphql_request(&graphql, &customer, &query).await;
        assert_eq!(error_code(&read), "not_on_required_team", "a customer must not read their own company\'s engagement: {read:?}");
        let read = graphql_request(&graphql, &other_staff, &query).await;
        assert_eq!(error_code(&read), "grant_scope_mismatch", "staff scoped to another company must not read this: {read:?}");
    });
}
