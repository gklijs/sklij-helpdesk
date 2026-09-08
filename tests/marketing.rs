//! Integration tests for specs/marketing.allium - the three
//! `CrossContextRoute`s (`TrialLapseIsRoutedFromHelpdesk`/
//! `TrialLapseIsRecorded`, `TrialConversionIsRoutedFromHelpdesk`/
//! `TrialConversionIsRecorded`, `EngagementDeclineIsRoutedFromActivity`/
//! `EngagementDeclineIsFlagged`), against the real `src/marketing.rs`
//! implementation and the three `CrossContextRoute`s `lib.rs`'s own
//! `register()` wires in.
//!
//! Unlike tests/activity.rs's own commands, `RecordTrialLapse`/
//! `RecordTrialConversion`/`RecordEngagementDecline` (marketing's own)
//! are never submitted directly here - per the spec's own header, they're
//! CrossContextRoute targets, reachable only by the real routing
//! infrastructure reacting to a *source* event committing (in helpdesk
//! or activity). So each test below drives the source side (a real
//! helpdesk company-lifecycle command, or a direct activity submission)
//! and reads marketing's own event feed for the routed result - the
//! same "trace the trigger emission graph, then read events" cross-
//! module chain shape the propagate skill's own guidance describes.
//!
//! The route itself runs on `Skilj::build()`'s own background poll task
//! (`cross_context_route_poll_interval`, 500ms by default - see
//! `src/marketing.rs`'s own doc comment), not synchronously inside the
//! source command's own HTTP response - so every wait below goes through
//! `wait_until` rather than a single `consume_auto` call right after
//! `trigger()`, the same "real elapsed time, not `trigger()`'s own
//! synchronous request/response" shape `tests/alerter_slack_webhook.rs`
//! already uses. `consume_auto`'s own `mode=auto` is destructive
//! (at-most-once, auto-advancing - see tests/alerting_feed.rs's own doc
//! comment), so the event found inside the polling predicate is captured
//! out through a shared `Mutex`, not re-fetched afterwards.
//!
//! `surface MarketingOutreachSignals` (an `exposes`-only surface, no
//! `provides`) has no dedicated test here: the spec's own Excludes
//! section defers the dashboard's concrete read shape ("this file only
//! names what data it needs, not how it's laid out"), so there's no
//! endpoint name yet to test against without guessing one. Its
//! `surface_exposure` obligation - that each of TrialLapsed/
//! TrialConverted/EngagementDeclineFlagged's fields are readable - is
//! covered instead by the event-feed assertions each test below already
//! makes on those three entities directly.

mod support;

use chrono::Utc;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{
    accepted, consume_auto, mint_command_token, mint_event_read_token, runtime, seed_mapping_for,
    seed_superadmin, setup_all_contexts, test_db, trigger, unique_name, wait_until,
};

const HELPDESK: &str = skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
const MARKETING: &str = "marketing";
const ACTIVITY: &str = "activity";

async fn sign_up_company(
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    router: &axum::Router,
) -> String {
    let sign_up = mint_command_token(pool, mapping, HELPDESK, "SignUpCompany").await;
    let company_id = unique_name("company");
    let response = trigger(
        router,
        &sign_up,
        serde_json::json!({ "company_id": company_id, "name": "Acme Corp", "contact_email": "support@acme.example" }),
    )
    .await;
    assert!(accepted(&response), "company signup should be accepted: {response:?}");
    company_id
}

/// Polls `read_token`'s own feed (via `wait_until`) until an event whose
/// `payload.company_id` matches turns up, or `what` times out - the
/// found event is captured out through a `Mutex` from inside the
/// predicate itself, since `consume_auto`'s `mode=auto` would otherwise
/// have already consumed it off the feed by the time a caller could
/// re-fetch it.
async fn wait_for_routed_event(
    router: &axum::Router,
    read_token: &str,
    company_id: &str,
    what: &str,
) -> serde_json::Value {
    let found: Arc<Mutex<Option<serde_json::Value>>> = Arc::new(Mutex::new(None));
    wait_until(Duration::from_secs(10), what, || {
        let found = found.clone();
        async move {
            let consumed = consume_auto(router, read_token).await;
            let Some(events) = consumed["events"].as_array() else {
                return false;
            };
            let Some(event) = events.iter().find(|e| e["payload"]["company_id"] == company_id) else {
                return false;
            };
            *found.lock().unwrap() = Some(event.clone());
            true
        }
    })
    .await;
    let result = found.lock().unwrap().clone();
    result.unwrap_or_else(|| panic!("{what}: timed out with nothing matching on the feed"))
}

/// `rule TrialLapseIsRoutedFromHelpdesk` + `rule TrialLapseIsRecorded`:
/// helpdesk's own `ExpireCompanyTrial` (`trialing -> expired`, the real
/// command behind `rule TrialPeriodEnds`'s failure branch) should
/// produce a `marketing.TrialLapsed` fact, routed via
/// `helpdesk.CompanyExpired -> marketing.RecordTrialLapse`.
#[test]
fn expiring_a_trial_routes_a_trial_lapsed_record_to_marketing() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mappings) = setup_all_contexts().await;
        let router = skilj.rest_router();
        let company_id = sign_up_company(&pool, &mappings.helpdesk, &router).await;

        let expire = mint_command_token(&pool, &mappings.helpdesk, HELPDESK, "ExpireCompanyTrial").await;
        let response = trigger(&router, &expire, serde_json::json!({ "company_id": company_id })).await;
        assert!(accepted(&response), "expiring a trialing company should succeed: {response:?}");

        let read_lapsed = mint_event_read_token(&pool, &mappings.marketing, MARKETING, "TrialLapsed").await;
        let our_event = wait_for_routed_event(
            &router,
            &read_lapsed,
            &company_id,
            "TrialLapsed routed to marketing after ExpireCompanyTrial",
        )
        .await;
        assert_eq!(our_event["eventType"], "TrialLapsed");
        assert!(our_event["payload"]["lapsed_at"].is_string(), "lapsed_at should be present: {our_event:?}");
    });
}

/// `rule TrialConversionIsRoutedFromHelpdesk` + `rule
/// TrialConversionIsRecorded`, both edges into `active` - `transitions_to
/// active` doesn't distinguish the source state, matching helpdesk's own
/// single `CompanyActivated` event for both `ConvertCompanyTrial`
/// (`trialing -> active`) and `ReactivateCompany` (`expired -> active`).
#[test]
fn either_route_into_active_produces_a_trial_converted_record() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mappings) = setup_all_contexts().await;
        let router = skilj.rest_router();
        let read_converted = mint_event_read_token(&pool, &mappings.marketing, MARKETING, "TrialConverted").await;

        // trialing -> active, via ConvertCompanyTrial.
        let converted_company = sign_up_company(&pool, &mappings.helpdesk, &router).await;
        let convert = mint_command_token(&pool, &mappings.helpdesk, HELPDESK, "ConvertCompanyTrial").await;
        let response = trigger(&router, &convert, serde_json::json!({ "company_id": converted_company })).await;
        assert!(accepted(&response), "converting a trialing company should succeed: {response:?}");

        let our_event = wait_for_routed_event(
            &router,
            &read_converted,
            &converted_company,
            "TrialConverted routed to marketing after ConvertCompanyTrial (trialing -> active)",
        )
        .await;
        assert_eq!(our_event["eventType"], "TrialConverted");
        assert!(our_event["payload"]["converted_at"].is_string(), "converted_at should be present: {our_event:?}");

        // expired -> active, via ExpireCompanyTrial then ReactivateCompany.
        let reactivated_company = sign_up_company(&pool, &mappings.helpdesk, &router).await;
        let expire = mint_command_token(&pool, &mappings.helpdesk, HELPDESK, "ExpireCompanyTrial").await;
        trigger(&router, &expire, serde_json::json!({ "company_id": reactivated_company })).await;
        let reactivate = mint_command_token(&pool, &mappings.helpdesk, HELPDESK, "ReactivateCompany").await;
        let response = trigger(&router, &reactivate, serde_json::json!({ "company_id": reactivated_company })).await;
        assert!(accepted(&response), "reactivating an expired company should succeed: {response:?}");

        let our_event = wait_for_routed_event(
            &router,
            &read_converted,
            &reactivated_company,
            "TrialConverted routed to marketing after ReactivateCompany (expired -> active)",
        )
        .await;
        assert_eq!(our_event["eventType"], "TrialConverted");
        assert!(our_event["payload"]["converted_at"].is_string(), "converted_at should be present: {our_event:?}");
    });
}

/// `rule EngagementDeclineIsRoutedFromActivity` + `rule
/// EngagementDeclineIsFlagged`: a `RecordEngagementDecline` submitted
/// into *activity* (the same way tests/activity.rs's own
/// `engagement_decline_is_flagged_once_and_rejected_on_repeat` submits
/// it) should produce an `EngagementDeclineFlagged` fact in marketing,
/// routed via `activity.CompanyEngagementDeclined ->
/// marketing.RecordEngagementDecline`.
#[test]
fn activity_engagement_decline_routes_a_flagged_record_to_marketing() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mappings) = setup_all_contexts().await;
        let router = skilj.rest_router();
        let company_id = sign_up_company(&pool, &mappings.helpdesk, &router).await;

        let watcher = seed_superadmin(&pool).await;
        let watcher_mapping =
            seed_mapping_for(&pool, &watcher, ACTIVITY, skilj_core::access_control::AccessLevel::Admin, None).await;
        let record_decline =
            mint_command_token(&pool, &watcher_mapping, ACTIVITY, "RecordEngagementDecline").await;
        let flagged_at = Utc::now();
        let response = trigger(
            &router,
            &record_decline,
            serde_json::json!({ "company_id": company_id, "flagged_at": flagged_at.to_rfc3339() }),
        )
        .await;
        assert!(accepted(&response), "the engagement-decline flag itself should succeed in activity: {response:?}");

        let read_flagged =
            mint_event_read_token(&pool, &mappings.marketing, MARKETING, "EngagementDeclineFlagged").await;
        let our_event = wait_for_routed_event(
            &router,
            &read_flagged,
            &company_id,
            "EngagementDeclineFlagged routed to marketing after activity's RecordEngagementDecline",
        )
        .await;
        assert_eq!(our_event["eventType"], "EngagementDeclineFlagged");
        // `EngagementDeclineIsRoutedFromActivity` carries the source
        // event's own `flagged_at` through unchanged, not a fresh `now`.
        assert_eq!(our_event["payload"]["flagged_at"], flagged_at.to_rfc3339());
    });
}
