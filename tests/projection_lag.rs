//! `src/projection_lag.rs` against real catch-up: lag shows up while an
//! async projection is behind, goes back to zero once catch-up has run,
//! and sync projections are never reported.

mod support;

use skilj::Skilj;
use skilj_core::bootstrap::ContextCreator;
use skilj_core::event_store::{BoundedContext, BoundedContextStatus};
use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use skilj_helpdesk::projection_lag::{self, SeriesKey};
use std::time::Duration;
use support::{
    accepted, fresh_database, mint_command_token, runtime, seed_admin, setup, test_db,
    test_master_key, test_now, trigger, unique_name, wait_until,
};

fn company_active_tickets() -> SeriesKey {
    SeriesKey {
        bounded_context: BOUNDED_CONTEXT.to_string(),
        projection: "CompanyActiveTickets".to_string(),
    }
}

/// Signs up a company and opens one ticket for it: two events, the second
/// one consumed by `CompanyActiveTickets`.
async fn sign_up_and_open_ticket(
    router: &axum::Router,
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
) {
    let sign_up = mint_command_token(pool, mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
    let create_ticket = mint_command_token(pool, mapping, BOUNDED_CONTEXT, "CreateTicket").await;
    let company_id = unique_name("company");
    let response = trigger(
        router,
        &sign_up,
        serde_json::json!({
            "company_id": company_id,
            "name": "Acme Corp",
            "contact_email": "support@acme.example",
        }),
    )
    .await;
    assert!(accepted(&response), "signup: {response:?}");
    let response = trigger(
        router,
        &create_ticket,
        serde_json::json!({
            "ticket_id": unique_name("ticket"),
            "company_id": company_id,
            "requester_id": unique_name("customer"),
            "logged_by_staff_id": null,
            "title": "Can't log in",
            "description": "Getting a 500 on the login page",
            "priority": "high",
        }),
    )
    .await;
    assert!(accepted(&response), "ticket: {response:?}");
}

/// Catch-up ticks once at startup and then sleeps for the poll interval,
/// so with an hour's interval the projection stays where that first tick
/// left it. A database of its own, so no other `Skilj` in this binary
/// (or, under `DATABASE_URL`, any earlier binary) catches it up.
#[test]
fn an_async_projection_that_has_not_caught_up_reports_its_lag() {
    runtime().block_on(async {
        let Some((database_url, pool)) = fresh_database("projection_lag").await else {
            return;
        };
        // `seed_admin` creates the context only once per binary, and the
        // other test here may already have done that, in `test_db()`.
        skilj_core::db::insert_bounded_context(
            &pool,
            &BoundedContext {
                name: BOUNDED_CONTEXT.to_string(),
                status: BoundedContextStatus::Active,
                created_at: test_now(),
                created_by: ContextCreator::SystemCreator,
                template: None,
            },
        )
        .await
        .unwrap();
        let mapping = seed_admin(&pool).await;
        let (skilj, _) = skilj_helpdesk::register(Skilj::builder(database_url))
            .encryption_master_key(test_master_key())
            .reconciliation_role(mapping.role.external_subject.clone())
            .async_projection_poll_interval(Duration::from_secs(3600))
            .build()
            .await
            .unwrap();
        // Let the startup tick finish first.
        tokio::time::sleep(Duration::from_secs(1)).await;

        sign_up_and_open_ticket(&skilj.rest_router(), &pool, &mapping).await;

        let lags = projection_lag::sample(&pool).await.unwrap();
        assert_eq!(
            lags.get(&company_active_tickets()),
            Some(&2),
            "the two events since the startup tick are lag: {lags:?}"
        );
        assert_eq!(
            lags.keys().collect::<Vec<_>>(),
            vec![&company_active_tickets()],
            "only the async projection is reported, never a sync one"
        );
    });
}

/// The last event (a signup) is one `CompanyActiveTickets` doesn't
/// consume, so this also shows catch-up moves `caught_up_to` all the way
/// to the head rather than to the last event it folded.
#[test]
fn a_caught_up_async_projection_reports_zero() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        sign_up_and_open_ticket(&router, &pool, &mapping).await;
        let sign_up = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
        let response = trigger(
            &router,
            &sign_up,
            serde_json::json!({
                "company_id": unique_name("company"),
                "name": "Globex",
                "contact_email": "support@globex.example",
            }),
        )
        .await;
        assert!(accepted(&response), "signup: {response:?}");

        wait_until(
            Duration::from_secs(10),
            "CompanyActiveTickets lag 0",
            || {
                let pool = pool.clone();
                async move {
                    let lags = projection_lag::sample(&pool).await.unwrap();
                    lags.get(&company_active_tickets()) == Some(&0)
                }
            },
        )
        .await;
    });
}
