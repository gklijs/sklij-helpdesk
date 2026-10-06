//! A delivery that parks, shows up where an operator would look, and is
//! redriven with `retryParkedDelivery` (`src/parked_deliveries.rs`'s own
//! doc comment for why this needs watching).
//!
//! **What actually parks.** A target command that *rejects* does not: skilj
//! treats a rejection as the delivery's answer, records it, and moves on.
//! Only an *error* (the target context's database failing, say) that
//! outlasts the retry policy parks. So this test forces one: a trigger on
//! marketing's `events` table makes every append fail, the way an outage
//! of that schema would, until the test drops it again. The route is
//! `HelpdeskExpiryToTrialLapse`, the one `tests/marketing.rs` already
//! drives end to end without a failure.
//!
//! Runs in a database of its own (`fresh_database`): the trigger breaks
//! marketing for every writer, and under `DATABASE_URL` (CI) every test
//! binary shares one database.

mod support;

use skilj::Skilj;
use skilj_core::access_control::AccessLevel;
use skilj_core::bootstrap::ContextCreator;
use skilj_core::db::{self, Pool};
use skilj_core::event_store::{BoundedContext, BoundedContextStatus};
use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT as HELPDESK;
use skilj_helpdesk::parked_deliveries;
use std::time::Duration;
use support::{
    accepted, consume_auto, fresh_database, graphql_request, mint_command_token,
    mint_event_read_token, runtime, seed_mapping_for, seed_role, serve_jwks, sign_jwt,
    test_idp_config, test_master_key, test_now, trigger, unique_name, wait_until,
};

const MARKETING: &str = "marketing";
const ROUTE: &str = "HelpdeskExpiryToTrialLapse";

/// Two quick attempts, then park - the default policy takes about 15s of
/// backoff before it gives up.
const QUICK_PARK: skilj_retry::RetryPolicy =
    skilj_retry::RetryPolicy::bounded(Duration::from_millis(50), 1.0, Duration::from_millis(50), 2);

async fn insert_context(pool: &Pool, name: &str) {
    db::insert_bounded_context(
        pool,
        &BoundedContext {
            name: name.to_string(),
            status: BoundedContextStatus::Active,
            created_at: test_now(),
            created_by: ContextCreator::SystemCreator,
            template: None,
        },
    )
    .await
    .unwrap();
}

/// Every append to marketing fails until [`end_outage`].
async fn start_outage(pool: &Pool) {
    sqlx::raw_sql(
        r#"
        CREATE FUNCTION "bc_marketing".simulated_outage() RETURNS trigger
            LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'simulated marketing outage'; END $$;
        CREATE TRIGGER simulated_outage BEFORE INSERT ON "bc_marketing".events
            FOR EACH ROW EXECUTE FUNCTION "bc_marketing".simulated_outage();
        "#,
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn end_outage(pool: &Pool) {
    sqlx::raw_sql(
        r#"
        DROP TRIGGER simulated_outage ON "bc_marketing".events;
        DROP FUNCTION "bc_marketing".simulated_outage();
        "#,
    )
    .execute(pool)
    .await
    .unwrap();
}

async fn parked_in_marketing(router: &axum::Router, jwt: &str) -> Vec<serde_json::Value> {
    let response = graphql_request(
        router,
        jwt,
        &format!(
            "query {{ parkedDeliveries(boundedContext: {MARKETING:?}) {{ \
             id kind source identifier targetBoundedContext targetCommandType requestJson \
             error attemptCount }} }}"
        ),
    )
    .await;
    assert!(response.get("errors").is_none(), "{response}");
    response["data"]["parkedDeliveries"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn a_route_that_keeps_failing_parks_is_counted_and_redrives() {
    runtime().block_on(async {
        let Some((database_url, pool)) = fresh_database("parked_deliveries").await else {
            return;
        };
        for name in [HELPDESK, "activity", MARKETING] {
            insert_context(&pool, name).await;
        }
        let role = seed_role(&pool, "parked-deliveries-admin").await;
        let helpdesk = seed_mapping_for(&pool, &role, HELPDESK, AccessLevel::Admin, None).await;
        seed_mapping_for(&pool, &role, "activity", AccessLevel::Admin, None).await;
        let marketing = seed_mapping_for(&pool, &role, MARKETING, AccessLevel::Admin, None).await;
        let jwks_url = serve_jwks().await;
        let jwt = sign_jwt(&role.external_subject);

        let (skilj, _) = skilj_helpdesk::register(Skilj::builder(database_url))
            .encryption_master_key(test_master_key())
            .reconciliation_role(role.external_subject.clone())
            .identity_provider(test_idp_config(&jwks_url))
            .cross_context_route_retry_policy(QUICK_PARK)
            .build()
            .await
            .unwrap();
        let rest = skilj.rest_router();
        let graphql = skilj.graphql_router().await.unwrap();

        // Nothing parked yet, so nothing for the gauge to report.
        assert!(parked_deliveries::sample(&pool).await.unwrap().is_empty());

        // 1. A trial lapses while marketing is down.
        start_outage(&pool).await;
        let sign_up = mint_command_token(&pool, &helpdesk, HELPDESK, "SignUpCompany").await;
        let company_id = unique_name("company");
        let response = trigger(
            &rest,
            &sign_up,
            serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
        )
        .await;
        assert!(accepted(&response), "{response:?}");
        let expire = mint_command_token(&pool, &helpdesk, HELPDESK, "ExpireCompanyTrial").await;
        let response = trigger(&rest, &expire, serde_json::json!({ "company_id": company_id })).await;
        assert!(accepted(&response), "{response:?}");

        // 2. The route gives up and parks it, in the target context.
        wait_until(
            Duration::from_secs(15),
            "the failing TrialLapsed route to park",
            || async { !parked_in_marketing(&graphql, &jwt).await.is_empty() },
        )
        .await;
        let parked = parked_in_marketing(&graphql, &jwt).await;
        assert_eq!(parked.len(), 1, "{parked:?}");
        let delivery = &parked[0];
        assert_eq!(delivery["kind"], "CROSS_CONTEXT_ROUTE");
        assert!(
            delivery["source"].as_str().unwrap().ends_with(ROUTE),
            "{delivery}"
        );
        assert_eq!(delivery["targetBoundedContext"], MARKETING);
        assert_eq!(delivery["targetCommandType"], "RecordTrialLapse");
        assert_eq!(delivery["attemptCount"], 2);
        assert!(
            delivery["error"]
                .as_str()
                .unwrap()
                .contains("simulated marketing outage"),
            "{delivery}"
        );
        let request: serde_json::Value =
            serde_json::from_str(delivery["requestJson"].as_str().unwrap()).unwrap();
        assert_eq!(request["company_id"], company_id);

        // 3. What the server's gauge would record: one, under marketing.
        let counts = parked_deliveries::sample(&pool).await.unwrap();
        let series: Vec<_> = counts.iter().collect();
        assert_eq!(series.len(), 1, "{counts:?}");
        let (key, count) = series[0];
        assert_eq!(key.bounded_context, MARKETING);
        assert_eq!(key.kind, "cross_context_route");
        assert_eq!(key.source, delivery["source"].as_str().unwrap());
        assert_eq!(*count, 1);

        // 4. A retry while marketing is still down fails and stays parked,
        // with the attempt counted.
        let id = delivery["id"].as_str().unwrap();
        let retry = format!(
            "mutation {{ retryParkedDelivery(boundedContext: {MARKETING:?}, id: {id:?}) {{ id }} }}"
        );
        let response = graphql_request(&graphql, &jwt, &retry).await;
        assert!(response.get("errors").is_some(), "{response}");
        let parked = parked_in_marketing(&graphql, &jwt).await;
        assert_eq!(parked.len(), 1, "{parked:?}");
        assert_eq!(parked[0]["attemptCount"], 3);

        // 5. Marketing recovers; the redrive delivers it and clears it.
        end_outage(&pool).await;
        let response = graphql_request(&graphql, &jwt, &retry).await;
        assert!(response.get("errors").is_none(), "{response}");
        assert_eq!(response["data"]["retryParkedDelivery"]["id"], id);
        assert!(parked_in_marketing(&graphql, &jwt).await.is_empty());
        assert!(parked_deliveries::sample(&pool).await.unwrap().is_empty());

        let read_lapsed = mint_event_read_token(&pool, &marketing, MARKETING, "TrialLapsed").await;
        let consumed = consume_auto(&rest, &read_lapsed).await;
        let events = consumed["events"].as_array().unwrap();
        assert!(
            events
                .iter()
                .any(|e| e["payload"]["company_id"] == company_id.as_str()),
            "the redriven TrialLapsed should be on marketing's feed: {consumed}"
        );

        skilj.shutdown(Duration::from_secs(10)).await;
    });
}
