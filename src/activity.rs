//! The "activity" bounded context: per-person daily activity, and
//! whether a company's own customers have gone quiet. Implements
//! `specs/activity.allium` in full - see that file for the domain spec,
//! and its own Excludes section for what this context deliberately
//! doesn't decide (what "gone quiet" means is `EngagementWatcher`'s own
//! decision logic, not modeled here or in this file).
//!
//! One of two new bounded contexts (with `marketing.rs`) built to
//! showcase skilj 0.0.4's `CrossContextRoute` - see `marketing.rs`'s own
//! doc comment for the route wiring itself; this file is only ever a
//! route *source*, never a target.
//!
//! Every id here is caller-supplied, the same convention
//! `src/helpdesk.rs`'s own module doc comment already establishes -
//! `company_id` is helpdesk's own caller-supplied `Company.company_id`
//! (see that file), `person_subject` is whichever of `Customer`/
//! `StaffMember`'s own `external_subject` the caller is, and `day`/
//! `flagged_at` are RFC 3339 strings the caller supplies rather than a
//! typed timestamp this crate reformats - see `specs/activity.allium`'s
//! own comment on `DailyActivityRecorded.day` for why (no dedicated Date
//! primitive, calendar-day granularity, caller-truncated).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skilj::{auto_register, CommandType, EventType};
use skilj_core::event_store::Event;
use skilj_core::plugin::BoundedContextEvent;
use skilj_core::shared::{CommandDecision, EventSpec, TagMapping};

pub const BOUNDED_CONTEXT: &str = "activity";

fn company_tag() -> Vec<TagMapping> {
    vec![TagMapping {
        key: "company".into(),
        field: "company_id".into(),
    }]
}

/// `specs/activity.allium`'s `enum PersonKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum PersonKind {
    Customer,
    Staff,
}

// --- events ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DailyActivityRecordedPayload {
    pub company_id: String,
    pub person_kind: PersonKind,
    pub person_subject: String,
    pub day: String,
}

pub struct DailyActivityRecorded;

/// `specs/activity.allium`'s `entity DailyActivityRecorded`/`event
/// DailyActivityRecorded`. Tagged on company AND person (not day - see
/// `RecordDailyActivity::decide` below for why the exact day match
/// happens in application code instead) so the per-day uniqueness guard
/// only has to read this one person's own history at this one company,
/// not this whole bounded context's.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for DailyActivityRecorded {
    type Payload = DailyActivityRecordedPayload;
    const NAME: &'static str = "DailyActivityRecorded";
    fn tag_mappings() -> Vec<TagMapping> {
        vec![
            TagMapping {
                key: "company".into(),
                field: "company_id".into(),
            },
            TagMapping {
                key: "person".into(),
                field: "person_subject".into(),
            },
        ]
    }
    /// `tests/activity.rs`'s own event-feed assertions read this the
    /// same way `src/helpdesk.rs`'s `CompanySignedUp`/etc. already do -
    /// see that file's own doc comment on why this must opt in.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanyEngagementDeclinedPayload {
    pub company_id: String,
    pub flagged_at: String,
}

pub struct CompanyEngagementDeclined;

/// `specs/activity.allium`'s `entity CompanyEngagementDeclined`/`event
/// CompanyEngagementDeclined` - the source side of `marketing.rs`'s own
/// `ActivityEngagementDeclineToMarketingFlag` `CrossContextRoute`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanyEngagementDeclined {
    type Payload = CompanyEngagementDeclinedPayload;
    const NAME: &'static str = "CompanyEngagementDeclined";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    /// Needed for two independent readers: `tests/activity.rs`'s own
    /// `EventReadToken`-based assertions, and (in a real deployment)
    /// whatever reads this bounded context's feed the way
    /// `alerter.rs` reads helpdesk's - the `CrossContextRoute`'s own
    /// background poll task in `marketing.rs` goes through skilj's
    /// internal event-store access instead, not this flag, so this is
    /// about external readers, not the route.
    fn event_read_allowed() -> bool {
        true
    }
}

/// This bounded context's own hand-written event enum - same technique
/// `src/helpdesk.rs`'s own `HelpdeskEvent` already uses.
pub enum ActivityEvent {
    DailyActivityRecorded(DailyActivityRecordedPayload),
    CompanyEngagementDeclined(CompanyEngagementDeclinedPayload),
}

impl BoundedContextEvent for ActivityEvent {
    fn try_from_event(event: &Event) -> Option<Result<Self, serde_json::Error>> {
        match event.event_type.name.as_str() {
            "DailyActivityRecorded" => {
                Some(serde_json::from_str(&event.payload).map(ActivityEvent::DailyActivityRecorded))
            }
            "CompanyEngagementDeclined" => Some(
                serde_json::from_str(&event.payload).map(ActivityEvent::CompanyEngagementDeclined),
            ),
            _ => None,
        }
    }
}

// --- commands ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordDailyActivityPayload {
    pub company_id: String,
    pub person_kind: PersonKind,
    pub person_subject: String,
    pub day: String,
}

pub struct RecordDailyActivity;

/// `specs/activity.allium`'s `rule DailyActivityIsRecorded`, behind
/// `surface CustomerActivityPing`/`StaffActivityPing` - the frontend
/// calls this unconditionally on every dashboard load and lets `decide()`
/// throttle, the same idempotent-by-rejection convention
/// `SignUpCompany` uses for a duplicate signup (see
/// `specs/skilj-helpdesk.allium`'s own `rule CompanySignsUp`).
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RecordDailyActivity {
    type Payload = RecordDailyActivityPayload;
    type Event = ActivityEvent;
    const NAME: &'static str = "RecordDailyActivity";
    fn tag_mappings() -> Vec<TagMapping> {
        vec![
            TagMapping {
                key: "company".into(),
                field: "company_id".into(),
            },
            TagMapping {
                key: "person".into(),
                field: "person_subject".into(),
            },
        ]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        // `matching_events` is every DailyActivityRecorded tagged with
        // this company OR this person (union - see
        // `DailyActivityRecorded::tag_mappings` above), so the exact
        // company+person+day triple is checked here, in application
        // code, the same "broader tag scope, precise application-level
        // match" shape `MergeTickets::decide` already relies on for its
        // own two-ticket read in `helpdesk.rs`.
        let already_recorded = matching_events.iter().any(|e| match e {
            ActivityEvent::DailyActivityRecorded(p) => {
                p.company_id == payload.company_id
                    && p.person_subject == payload.person_subject
                    && p.day == payload.day
            }
            _ => false,
        });
        if already_recorded {
            return CommandDecision::Rejected {
                reason: format!(
                    "{} at company {} already has an activity record for {}",
                    payload.person_subject, payload.company_id, payload.day
                ),
                kind: "already_recorded_today".into(),
            };
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "DailyActivityRecorded".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "person_kind": payload.person_kind,
                    "person_subject": payload.person_subject,
                    "day": payload.day,
                }),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordEngagementDeclinePayload {
    pub company_id: String,
    pub flagged_at: String,
}

pub struct RecordEngagementDecline;

/// `specs/activity.allium`'s `rule EngagementDeclineIsRecorded`, behind
/// `surface EngagementDeclineDetection` - submitted by `EngagementWatcher`
/// (a superadmin `Role`, per that actor's own `identified_by`), never a
/// person. Distinct `CommandType` from `marketing::RecordEngagementDecline`
/// - different bounded contexts, not a real collision (see
/// `specs/marketing.allium`'s own naming note and open question).
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RecordEngagementDecline {
    type Payload = RecordEngagementDeclinePayload;
    type Event = ActivityEvent;
    const NAME: &'static str = "RecordEngagementDecline";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        let already_flagged = matching_events.iter().any(|e| {
            matches!(e, ActivityEvent::CompanyEngagementDeclined(p) if p.company_id == payload.company_id)
        });
        if already_flagged {
            return CommandDecision::Rejected {
                reason: format!("company {} was already flagged", payload.company_id),
                kind: "already_flagged".into(),
            };
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "CompanyEngagementDeclined".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "flagged_at": payload.flagged_at,
                }),
            }],
        }
    }
}
