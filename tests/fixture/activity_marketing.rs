//! The `activity` and `marketing` bounded contexts, and the three
//! `CrossContextRoute`s feeding marketing from the other two.

use serde_json::json;
use skilj::CrossContextRoute;
use skilj_helpdesk::activity::*;
use skilj_helpdesk::{helpdesk, marketing};
use skilj_test_fixture::command::GivenEvents;

use crate::events::spec;

/// The instant the fixture hands `decide_with` as `Decision::now()` - the
/// time the stamping commands record (skilj 0.2.0).
fn at(rfc3339: &str) -> chrono::DateTime<chrono::Utc> {
    rfc3339.parse().expect("an RFC 3339 instant")
}

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
    // `flagged_at` is the submission's own instant, not caller-supplied.
    GivenEvents::<RecordEngagementDecline>::new()
        .now(at("2026-10-04T00:00:00Z"))
        .when(RecordEngagementDeclinePayload {
            company_id: "acme".into(),
        })
        .then_accepted(vec![spec(
            "CompanyEngagementDeclined",
            json!({ "company_id": "acme", "flagged_at": "2026-10-04T00:00:00+00:00" }),
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
        })
        .then_rejected("already_flagged");
}

// --- marketing ---

#[test]
fn marketing_records_every_routed_signal_as_is() {
    // Lapse and conversion are stamped with the submission's instant; the
    // decline keeps the `flagged_at` activity recorded.
    GivenEvents::<marketing::RecordTrialLapse>::new()
        .now(at("2026-10-04T00:00:00Z"))
        .when(marketing::RecordTrialLapsePayload {
            company_id: "acme".into(),
        })
        .then_accepted(vec![spec(
            "TrialLapsed",
            json!({ "company_id": "acme", "lapsed_at": "2026-10-04T00:00:00+00:00" }),
        )]);
    GivenEvents::<marketing::RecordTrialConversion>::new()
        .now(at("2026-10-04T00:00:00Z"))
        .when(marketing::RecordTrialConversionPayload {
            company_id: "acme".into(),
        })
        .then_accepted(vec![spec(
            "TrialConverted",
            json!({ "company_id": "acme", "converted_at": "2026-10-04T00:00:00+00:00" }),
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

    let conversion =
        marketing::HelpdeskActivationToTrialConversion::route(&helpdesk::CompanyActivatedPayload {
            company_id: "acme".into(),
        })
        .expect("every activation is routed");
    assert_eq!(conversion.company_id, "acme");
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

// --- projections ---

#[test]
fn company_engagement_counts_person_days_per_kind_and_keeps_the_latest_day() {
    let staff_day = ActivityEvent::DailyActivityRecorded(DailyActivityRecordedPayload {
        company_id: "acme".into(),
        person_kind: PersonKind::Staff,
        person_subject: "staff-1".into(),
        day: "2026-10-03".into(),
    });
    skilj_test_fixture::projection::GivenEvents::<CompanyEngagement>::new()
        .event(activity_on("customer-1", "2026-10-04"))
        .event(activity_on("customer-2", "2026-10-02"))
        .event(staff_day)
        .event(ActivityEvent::CompanyEngagementDeclined(
            CompanyEngagementDeclinedPayload {
                company_id: "acme".into(),
                flagged_at: "2026-10-20T00:00:00Z".into(),
            },
        ))
        .then(|state| {
            assert_eq!(state.customer_active_days, 2);
            assert_eq!(state.staff_active_days, 1);
            assert_eq!(state.last_customer_day.as_deref(), Some("2026-10-04"));
            assert_eq!(state.last_staff_day.as_deref(), Some("2026-10-03"));
            assert_eq!(
                state.engagement_declined_at.as_deref(),
                Some("2026-10-20T00:00:00Z")
            );
        });
}

#[test]
fn marketing_signals_read_lapsed_until_a_conversion_lands() {
    use marketing::*;
    skilj_test_fixture::projection::GivenEvents::<MarketingSignals>::new()
        .event(MarketingEvent::TrialLapsed(TrialLapsedPayload {
            company_id: "acme".into(),
            lapsed_at: "2026-10-01".into(),
        }))
        .then(|state| {
            assert_eq!(state.trial_status.as_deref(), Some("lapsed"));
            assert_eq!(state.lapse_count, 1);
            assert_eq!(state.conversion_count, 0);
        });
}

/// The two lifecycle routes deliver independently, so a lapse can land
/// after the conversion (a parked lapse redriven later always does - issue
/// #29). Helpdesk can't expire an active company, so `converted` stands;
/// the late lapse is still counted.
#[test]
fn marketing_signals_never_go_back_from_converted_and_count_each() {
    use marketing::*;
    let lapsed = |at: &str| {
        MarketingEvent::TrialLapsed(TrialLapsedPayload {
            company_id: "acme".into(),
            lapsed_at: at.into(),
        })
    };
    let converted = |at: &str| {
        MarketingEvent::TrialConverted(TrialConvertedPayload {
            company_id: "acme".into(),
            converted_at: at.into(),
        })
    };
    skilj_test_fixture::projection::GivenEvents::<MarketingSignals>::new()
        .event(lapsed("2026-10-01"))
        .event(converted("2026-10-02"))
        .event(lapsed("2026-11-01"))
        .event(MarketingEvent::EngagementDeclineFlagged(
            EngagementDeclineFlaggedPayload {
                company_id: "acme".into(),
                flagged_at: "2026-10-20".into(),
            },
        ))
        .then(|state| {
            assert_eq!(state.trial_status.as_deref(), Some("converted"));
            assert_eq!(state.lapse_count, 2);
            assert_eq!(state.last_lapsed_at.as_deref(), Some("2026-11-01"));
            assert_eq!(state.conversion_count, 1);
            assert_eq!(state.last_converted_at.as_deref(), Some("2026-10-02"));
            assert_eq!(state.engagement_declined_at.as_deref(), Some("2026-10-20"));
        });
}
