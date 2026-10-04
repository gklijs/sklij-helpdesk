//! The `activity` and `marketing` bounded contexts, and the three
//! `CrossContextRoute`s feeding marketing from the other two.

use serde_json::json;
use skilj::CrossContextRoute;
use skilj_helpdesk::activity::*;
use skilj_helpdesk::{helpdesk, marketing};
use skilj_test_fixture::command::GivenEvents;

use crate::events::spec;

fn activity_on(person: &str, day: &str) -> ActivityEvent {
    ActivityEvent::DailyActivityRecorded(DailyActivityRecordedPayload {
        company_id: "acme".into(),
        person_kind: PersonKind::Customer,
        person_subject: person.into(),
        day: day.into(),
    })
}

fn record(person: &str, day: &str) -> RecordDailyActivityPayload {
    RecordDailyActivityPayload {
        company_id: "acme".into(),
        person_kind: PersonKind::Customer,
        person_subject: person.into(),
        day: day.into(),
    }
}

// --- activity ---

#[test]
fn a_persons_first_activity_of_the_day_is_recorded() {
    GivenEvents::<RecordDailyActivity>::new()
        .when(record("customer-1", "2026-10-04"))
        .then_accepted(vec![spec(
            "DailyActivityRecorded",
            json!({
                "company_id": "acme",
                "person_kind": "customer",
                "person_subject": "customer-1",
                "day": "2026-10-04",
            }),
        )]);
}

#[test]
fn a_person_is_recorded_at_most_once_per_day() {
    GivenEvents::<RecordDailyActivity>::new()
        .event(activity_on("customer-1", "2026-10-04"))
        .when(record("customer-1", "2026-10-04"))
        .then_rejected("already_recorded_today");
}

#[test]
fn another_day_or_another_person_is_a_new_record() {
    GivenEvents::<RecordDailyActivity>::new()
        .event(activity_on("customer-1", "2026-10-03"))
        .event(activity_on("customer-2", "2026-10-04"))
        .when(record("customer-1", "2026-10-04"))
        .then(crate::events::assert_accepted);
}

#[test]
fn engagement_decline_is_flagged_once_per_company() {
    GivenEvents::<RecordEngagementDecline>::new()
        .when(RecordEngagementDeclinePayload {
            company_id: "acme".into(),
            flagged_at: "2026-10-04T00:00:00Z".into(),
        })
        .then_accepted(vec![spec(
            "CompanyEngagementDeclined",
            json!({ "company_id": "acme", "flagged_at": "2026-10-04T00:00:00Z" }),
        )]);
    GivenEvents::<RecordEngagementDecline>::new()
        .event(ActivityEvent::CompanyEngagementDeclined(
            CompanyEngagementDeclinedPayload {
                company_id: "acme".into(),
                flagged_at: "2026-10-01T00:00:00Z".into(),
            },
        ))
        .when(RecordEngagementDeclinePayload {
            company_id: "acme".into(),
            flagged_at: "2026-10-04T00:00:00Z".into(),
        })
        .then_rejected("already_flagged");
}

// --- marketing ---

#[test]
fn marketing_records_every_routed_signal_as_is() {
    GivenEvents::<marketing::RecordTrialLapse>::new()
        .when(marketing::RecordTrialLapsePayload {
            company_id: "acme".into(),
            lapsed_at: "2026-10-04T00:00:00Z".into(),
        })
        .then_accepted(vec![spec(
            "TrialLapsed",
            json!({ "company_id": "acme", "lapsed_at": "2026-10-04T00:00:00Z" }),
        )]);
    GivenEvents::<marketing::RecordTrialConversion>::new()
        .when(marketing::RecordTrialConversionPayload {
            company_id: "acme".into(),
            converted_at: "2026-10-04T00:00:00Z".into(),
        })
        .then_accepted(vec![spec(
            "TrialConverted",
            json!({ "company_id": "acme", "converted_at": "2026-10-04T00:00:00Z" }),
        )]);
    GivenEvents::<marketing::RecordEngagementDecline>::new()
        .when(marketing::RecordEngagementDeclinePayload {
            company_id: "acme".into(),
            flagged_at: "2026-10-04T00:00:00Z".into(),
        })
        .then_accepted(vec![spec(
            "EngagementDeclineFlagged",
            json!({ "company_id": "acme", "flagged_at": "2026-10-04T00:00:00Z" }),
        )]);
}

#[test]
fn marketing_commands_do_not_deduplicate() {
    // No guard on purpose (specs/marketing.allium, Open Questions): a
    // company can lapse, reactivate and lapse again, and each cycle is
    // its own signal.
    GivenEvents::<marketing::RecordTrialLapse>::new()
        .event(marketing::MarketingEvent::TrialLapsed(
            marketing::TrialLapsedPayload {
                company_id: "acme".into(),
                lapsed_at: "2026-09-01T00:00:00Z".into(),
            },
        ))
        .when(marketing::RecordTrialLapsePayload {
            company_id: "acme".into(),
            lapsed_at: "2026-10-04T00:00:00Z".into(),
        })
        .then(crate::events::assert_accepted);
}

// --- routes ---

#[test]
fn helpdesk_lifecycle_routes_carry_the_company_over() {
    let lapse = marketing::HelpdeskExpiryToTrialLapse::route(&helpdesk::CompanyExpiredPayload {
        company_id: "acme".into(),
    })
    .expect("every expiry is routed");
    assert_eq!(lapse.company_id, "acme");
    assert!(chrono::DateTime::parse_from_rfc3339(&lapse.lapsed_at).is_ok());

    let conversion =
        marketing::HelpdeskActivationToTrialConversion::route(&helpdesk::CompanyActivatedPayload {
            company_id: "acme".into(),
        })
        .expect("every activation is routed");
    assert_eq!(conversion.company_id, "acme");
    assert!(chrono::DateTime::parse_from_rfc3339(&conversion.converted_at).is_ok());
}

#[test]
fn the_engagement_route_keeps_the_original_flag_time() {
    let flag = marketing::ActivityEngagementDeclineToMarketingFlag::route(
        &CompanyEngagementDeclinedPayload {
            company_id: "acme".into(),
            flagged_at: "2026-10-01T00:00:00Z".into(),
        },
    )
    .expect("every decline is routed");
    assert_eq!(flag.company_id, "acme");
    assert_eq!(flag.flagged_at, "2026-10-01T00:00:00Z");
}
