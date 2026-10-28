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
//! `src/helpdesk/mod.rs`'s own module doc comment already establishes -
//! `company_id` is helpdesk's own caller-supplied `Company.company_id`
//! (see that file), `person_subject` is whichever of `Customer`/
//! `StaffMember`'s own `external_subject` the caller is, and `day`/
//! `flagged_at` are RFC 3339 strings the caller supplies rather than a
//! typed timestamp this crate reformats - see `specs/activity.allium`'s
//! own comment on `DailyActivityRecorded.day` for why (no dedicated Date
//! primitive, calendar-day granularity, caller-truncated).

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skilj::{auto_register, CommandType, EventType, Projection};
use skilj_core::event_store::Event;
use skilj_core::plugin::BoundedContextEvent;
use skilj_core::shared::{CommandDecision, EventSpec, QueryItemMapping, TagMapping};

use crate::helpdesk::STAFF_CAPABILITY;

pub const BOUNDED_CONTEXT: &str = "activity";

fn company_tag() -> Vec<TagMapping> {
    vec![TagMapping::new("company", "company_id")]
}

fn person_tag() -> Vec<TagMapping> {
    vec![TagMapping::new("person", "person_subject")]
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
            TagMapping::new("company", "company_id"),
            TagMapping::new("person", "person_subject"),
        ]
    }
    /// `tests/activity.rs`'s own event-feed assertions read this the
    /// same way `src/helpdesk/`'s `CompanySignedUp`/etc. already do -
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
/// `src/helpdesk/`'s own `HelpdeskEvent` already uses.
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
        [company_tag(), person_tag()].concat()
    }
    /// This person's records at this company - carrying both tags, not
    /// either, which without a query was every record of the whole
    /// company plus every record of the person. `person` first: an item
    /// is looked up by its first tag, and a person has far fewer records
    /// than their company.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![QueryItemMapping::types(&["DailyActivityRecorded"])
            .tagged(person_tag())
            .tagged(company_tag())]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        // `matching_events` is every day this person was recorded at
        // this company, so the day is checked here, in application code.
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
    /// Whether the company was flagged at all: its latest flag.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![QueryItemMapping::types(&["CompanyEngagementDeclined"])
            .tagged(company_tag())
            .latest()]
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

// --- projection ---

/// How active one company's people are, from `DailyActivityRecorded`'s
/// one-record-per-person-per-day facts. Counts, not subjects: the staff
/// dashboard needs to see that a company has gone quiet, not who.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CompanyEngagementState {
    /// Person-days, not people: one customer active on three days is 3.
    pub customer_active_days: u32,
    pub staff_active_days: u32,
    /// The latest `day` recorded, by string order - `day` is a
    /// caller-truncated RFC 3339 midnight, so that is date order.
    pub last_customer_day: Option<String>,
    pub last_staff_day: Option<String>,
    /// Set once, as `RecordEngagementDecline` only accepts once.
    pub engagement_declined_at: Option<String>,
}

/// Keyed by `company_id`. Staff-only, gated the same way as
/// `marketing::MarketingSignals` (see its doc comment). Async, the
/// default: every dashboard load pings `RecordDailyActivity`, and that
/// write doesn't need to wait on this read model.
pub struct CompanyEngagement;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for CompanyEngagement {
    type State = CompanyEngagementState;
    type Event = ActivityEvent;
    const NAME: &'static str = "CompanyEngagement";
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    const REQUIRES_CAPABILITY: Option<&'static str> = Some(STAFF_CAPABILITY);
    fn consumed_event_types() -> Vec<&'static str> {
        vec!["DailyActivityRecorded", "CompanyEngagementDeclined"]
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        let company_id = match event {
            ActivityEvent::DailyActivityRecorded(p) => &p.company_id,
            ActivityEvent::CompanyEngagementDeclined(p) => &p.company_id,
        };
        vec![company_id.clone()]
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        match event {
            ActivityEvent::DailyActivityRecorded(p) => {
                let (days, last_day) = match p.person_kind {
                    PersonKind::Customer => (
                        &mut state.customer_active_days,
                        &mut state.last_customer_day,
                    ),
                    PersonKind::Staff => (&mut state.staff_active_days, &mut state.last_staff_day),
                };
                *days += 1;
                if last_day.as_deref() < Some(p.day.as_str()) {
                    *last_day = Some(p.day.clone());
                }
            }
            ActivityEvent::CompanyEngagementDeclined(p) => {
                state.engagement_declined_at = Some(p.flagged_at.clone());
            }
        }
    }
}
