//! Integration tests for specs/activity.allium - `rule
//! DailyActivityIsRecorded` and `rule EngagementDeclineIsRecorded`.
//!
//! No `src/activity.rs` exists yet (see the spec's own header): these
//! tests exercise the *intended* interface - the "activity" bounded
//! context, `RecordDailyActivity`/`RecordEngagementDecline` command
//! types - against the current, unimplemented crate. Nothing here
//! references a Rust symbol from a not-yet-existing module, so the file
//! compiles today; each `#[test]` instead fails at runtime with a clear
//! `db::get_command_type` panic ("activity/RecordDailyActivity must
//! already be registered") until `src/activity.rs` registers the real
//! `#[auto_register]` types. That panic is this suite's "red" - turning
//! it green is the implementation's job, not this file's.
//!
//! See tests/company.rs's own doc comment for why this is a separate
//! binary/file rather than folded into an existing one.

mod support;

use chrono::{SubsecRound, Utc};
use support::{
    accepted, consume_auto, mint_command_token, mint_event_read_token, rejection_kind, runtime,
    seed_mapping_for, seed_superadmin, setup_all_contexts, test_db, trigger, unique_name,
    AllContextMappings,
};

const HELPDESK: &str = skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
const ACTIVITY: &str = "activity";

/// A company via helpdesk's own `SignUpCompany` - every activity fact
/// below needs a real `company_id` to key off, and helpdesk is the only
/// bounded context that creates one (see specs/activity.allium's own
/// "helpdesk is only ever a source" framing).
async fn sign_up_company(
    pool: &skilj_core::db::Pool,
    mappings: &AllContextMappings,
    router: &axum::Router,
) -> String {
    let sign_up = mint_command_token(pool, &mappings.helpdesk, HELPDESK, "SignUpCompany").await;
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

/// Midnight UTC of "today" - `DailyActivityRecorded.day`'s own comment
/// in the spec: calendar-day granularity, caller-truncated, since Allium
/// has no dedicated Date primitive.
fn today() -> chrono::DateTime<Utc> {
    Utc::now().trunc_subsecs(0).date_naive().and_hms_opt(0, 0, 0).unwrap().and_utc()
}

/// `rule DailyActivityIsRecorded`'s success and rejection branches, via
/// `surface CustomerActivityPing` - the same "call it unconditionally on
/// every dashboard load, let `decide()` throttle" shape the spec's own
/// comment on the rule describes.
#[test]
fn customer_daily_activity_ping_is_recorded_and_dedupes_same_day() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mappings) = setup_all_contexts().await;
        let router = skilj.rest_router();
        let company_id = sign_up_company(&pool, &mappings, &router).await;

        let record_activity =
            mint_command_token(&pool, &mappings.activity, ACTIVITY, "RecordDailyActivity").await;
        let read_activity =
            mint_event_read_token(&pool, &mappings.activity, ACTIVITY, "DailyActivityRecorded").await;
        let person_subject = unique_name("customer-subject");
        let day = today();

        let payload = serde_json::json!({
            "company_id": company_id,
            "person_kind": "customer",
            "person_subject": person_subject,
            "day": day.to_rfc3339(),
        });
        let response = trigger(&router, &record_activity, payload.clone()).await;
        assert!(accepted(&response), "a customer's first ping today should be accepted: {response:?}");

        // entity-fields.DailyActivityRecorded / rule-entity-creation.DailyActivityIsRecorded:
        // the committed event carries exactly the fields `ensures` names.
        let consumed = consume_auto(&router, &read_activity).await;
        let events = consumed["events"].as_array().expect("events array");
        let our_event = events
            .iter()
            .find(|e| e["payload"]["company_id"] == company_id && e["payload"]["person_subject"] == person_subject)
            .unwrap_or_else(|| panic!("our own DailyActivityRecorded should be on the feed: {consumed:?}"));
        assert_eq!(our_event["eventType"], "DailyActivityRecorded");
        assert_eq!(our_event["payload"]["company_id"], company_id);
        assert_eq!(our_event["payload"]["person_kind"], "customer");
        assert_eq!(our_event["payload"]["person_subject"], person_subject);
        assert_eq!(our_event["payload"]["day"], day.to_rfc3339());

        // rule-failure.DailyActivityIsRecorded: the frontend calls this
        // unconditionally - a second ping the same company+person+day is
        // rejected, not silently accepted again.
        let response = trigger(&router, &record_activity, payload).await;
        assert!(!accepted(&response), "a duplicate same-day ping should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "already_recorded_today");
    });
}

/// The staff-side mirror of the test above, via `surface
/// StaffActivityPing` - confirms `person_kind` distinguishes the two
/// (enum-comparable.PersonKind) rather than colliding on the same
/// company+day.
#[test]
fn staff_daily_activity_ping_uses_staff_person_kind() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mappings) = setup_all_contexts().await;
        let router = skilj.rest_router();
        let company_id = sign_up_company(&pool, &mappings, &router).await;

        let record_activity =
            mint_command_token(&pool, &mappings.activity, ACTIVITY, "RecordDailyActivity").await;
        let read_activity =
            mint_event_read_token(&pool, &mappings.activity, ACTIVITY, "DailyActivityRecorded").await;
        let person_subject = unique_name("staff-subject");
        let day = today();

        let response = trigger(
            &router,
            &record_activity,
            serde_json::json!({
                "company_id": company_id,
                "person_kind": "staff",
                "person_subject": person_subject,
                "day": day.to_rfc3339(),
            }),
        )
        .await;
        assert!(accepted(&response), "a staff member's ping should be accepted: {response:?}");

        let consumed = consume_auto(&router, &read_activity).await;
        let events = consumed["events"].as_array().unwrap();
        let our_event = events
            .iter()
            .find(|e| e["payload"]["person_subject"] == person_subject)
            .unwrap_or_else(|| panic!("our own DailyActivityRecorded should be on the feed: {consumed:?}"));
        assert_eq!(our_event["payload"]["company_id"], company_id);
        assert_eq!(our_event["payload"]["person_kind"], "staff");
        assert_eq!(our_event["payload"]["day"], day.to_rfc3339());
    });
}

/// `rule EngagementDeclineIsRecorded`'s success and rejection branches,
/// via `surface EngagementDeclineDetection` - submitted the way the new
/// scheduled binary (`EngagementWatcher`, a superadmin `Role`, not a
/// per-company `StaffMember`/`Customer`) really would.
#[test]
fn engagement_decline_is_flagged_once_and_rejected_on_repeat() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mappings) = setup_all_contexts().await;
        let router = skilj.rest_router();
        let company_id = sign_up_company(&pool, &mappings, &router).await;

        // EngagementWatcher: identified_by `skilj/Role where superadmin =
        // true`, granted its own mapping onto the activity context - the
        // helpdesk-scoped admin mapping `setup_all_contexts` already
        // grants isn't what the spec's actor declaration describes.
        let watcher = seed_superadmin(&pool).await;
        let watcher_mapping =
            seed_mapping_for(&pool, &watcher, ACTIVITY, skilj_core::access_control::AccessLevel::Admin, None).await;
        let record_decline =
            mint_command_token(&pool, &watcher_mapping, ACTIVITY, "RecordEngagementDecline").await;
        let read_decline =
            mint_event_read_token(&pool, &mappings.activity, ACTIVITY, "CompanyEngagementDeclined").await;
        let flagged_at = Utc::now().trunc_subsecs(6);

        let payload = serde_json::json!({ "company_id": company_id, "flagged_at": flagged_at.to_rfc3339() });
        let response = trigger(&router, &record_decline, payload.clone()).await;
        assert!(accepted(&response), "the first engagement-decline flag for this company should be accepted: {response:?}");

        let consumed = consume_auto(&router, &read_decline).await;
        let events = consumed["events"].as_array().unwrap();
        let our_event = events
            .iter()
            .find(|e| e["payload"]["company_id"] == company_id)
            .unwrap_or_else(|| panic!("our own CompanyEngagementDeclined should be on the feed: {consumed:?}"));
        assert_eq!(our_event["eventType"], "CompanyEngagementDeclined");
        assert_eq!(our_event["payload"]["flagged_at"], flagged_at.to_rfc3339());

        // "fires once, not once per check while the condition holds" -
        // the spec's own confirmed note on the rule.
        let response = trigger(&router, &record_decline, payload).await;
        assert!(!accepted(&response), "flagging the same company twice should be rejected: {response:?}");
        assert_eq!(rejection_kind(&response), "already_flagged");
    });
}

// `surface_actor` obligations for CustomerActivityPing/StaffActivityPing/
// EngagementDeclineDetection - deliberately not tested separately here.
// This codebase's real access control is `RoleAccessMapping`/
// `CommandToken` scoping, where the token *is* the command selector: a
// `/v1/commands/trigger` call is dispatched to whatever command the
// bearer token was minted for, so there is no request shape by which a
// caller could even attempt to submit `RecordDailyActivity` through a
// token minted for a different command (tried during propagation: a
// `SignUpCompany` token given a `RecordDailyActivity`-shaped payload
// gets a 400 at the payload-schema layer, before any command-identity
// check would run - proving schema validation, not actor restriction).
// `facing`'s Customer/StaffMember/EngagementWatcher distinction is a
// spec-level behavioural contract the frontend and the scheduled binary
// are expected to honour, not a type skilj's own access-control layer
// can see - the same gap tests/cross_company_projection_scoping.rs's own
// doc comment documents for `RoleAccessMapping.scope`. What *is*
// mechanically enforced (a token only ever submits its own command) is a
// skilj-core guarantee, not particular to these two new contexts.
