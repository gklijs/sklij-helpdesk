//! The "marketing" bounded context: a staff-visible record of company
//! signals worth a marketing follow-up - no outreach automation, no
//! paging. Implements `specs/marketing.allium` in full - see that file
//! for the domain spec, and `specs/activity-marketing-event-model.md`
//! (its own precursor doc) for the wider picture.
//!
//! This is the half of the two-context pair that showcases skilj 0.0.4's
//! `CrossContextRoute` end to end: `RecordTrialLapse`/
//! `RecordTrialConversion`/`RecordEngagementDecline` below are never
//! `rest_trigger_allowed` - nothing submits them except the three routes
//! at the bottom of this file, reacting to a *source* event committing
//! in `helpdesk` or `activity`. `lib.rs`'s own `register()` is what
//! actually wires each route in - unlike `EventType`/`CommandType`,
//! `CrossContextRoute` has no `#[auto_register]` support
//! (`skilj_core::plugin::SkiljBuilder::cross_context_route` is a
//! separate, explicit builder call), so each one is registered by name
//! there rather than by attribute here.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skilj::{auto_register, CommandType, CrossContextRoute, EventType};
use skilj_core::event_store::Event;
use skilj_core::plugin::BoundedContextEvent;
use skilj_core::shared::{CommandDecision, EventSpec, TagMapping};

use crate::activity;
use crate::helpdesk;

pub const BOUNDED_CONTEXT: &str = "marketing";

fn company_tag() -> Vec<TagMapping> {
    vec![TagMapping {
        key: "company".into(),
        field: "company_id".into(),
    }]
}

// --- events ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TrialLapsedPayload {
    pub company_id: String,
    pub lapsed_at: String,
}

pub struct TrialLapsed;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TrialLapsed {
    type Payload = TrialLapsedPayload;
    const NAME: &'static str = "TrialLapsed";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TrialConvertedPayload {
    pub company_id: String,
    pub converted_at: String,
}

pub struct TrialConverted;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TrialConverted {
    type Payload = TrialConvertedPayload;
    const NAME: &'static str = "TrialConverted";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EngagementDeclineFlaggedPayload {
    pub company_id: String,
    pub flagged_at: String,
}

pub struct EngagementDeclineFlagged;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for EngagementDeclineFlagged {
    type Payload = EngagementDeclineFlaggedPayload;
    const NAME: &'static str = "EngagementDeclineFlagged";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn event_read_allowed() -> bool {
        true
    }
}

/// This bounded context's own hand-written event enum - same technique
/// `src/helpdesk.rs`'s own `HelpdeskEvent` already uses.
pub enum MarketingEvent {
    TrialLapsed(TrialLapsedPayload),
    TrialConverted(TrialConvertedPayload),
    EngagementDeclineFlagged(EngagementDeclineFlaggedPayload),
}

impl BoundedContextEvent for MarketingEvent {
    fn try_from_event(event: &Event) -> Option<Result<Self, serde_json::Error>> {
        match event.event_type.name.as_str() {
            "TrialLapsed" => {
                Some(serde_json::from_str(&event.payload).map(MarketingEvent::TrialLapsed))
            }
            "TrialConverted" => {
                Some(serde_json::from_str(&event.payload).map(MarketingEvent::TrialConverted))
            }
            "EngagementDeclineFlagged" => Some(
                serde_json::from_str(&event.payload).map(MarketingEvent::EngagementDeclineFlagged),
            ),
            _ => None,
        }
    }
}

// --- commands ---
//
// Each accepted unconditionally - `specs/marketing.allium`'s own
// "Rejected when: none identified yet" for all three (see that file's
// open question on whether TrialLapsed/TrialConverted should ever
// reject a repeat). No `rest_trigger_allowed` override on any of
// these: see this file's own doc comment for why - only the routes at
// the bottom ever submit them.

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordTrialLapsePayload {
    pub company_id: String,
    pub lapsed_at: String,
}

pub struct RecordTrialLapse;

#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RecordTrialLapse {
    type Payload = RecordTrialLapsePayload;
    type Event = MarketingEvent;
    const NAME: &'static str = "RecordTrialLapse";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn decide(payload: &Self::Payload, _matching_events: &[Self::Event]) -> CommandDecision {
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "TrialLapsed".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "lapsed_at": payload.lapsed_at,
                }),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RecordTrialConversionPayload {
    pub company_id: String,
    pub converted_at: String,
}

pub struct RecordTrialConversion;

#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RecordTrialConversion {
    type Payload = RecordTrialConversionPayload;
    type Event = MarketingEvent;
    const NAME: &'static str = "RecordTrialConversion";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn decide(payload: &Self::Payload, _matching_events: &[Self::Event]) -> CommandDecision {
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "TrialConverted".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "converted_at": payload.converted_at,
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

/// marketing's own command - distinct type from `activity::
/// RecordEngagementDecline`; they live in different bounded contexts so
/// this isn't a real collision, just a naming note (see
/// `specs/marketing.allium`'s own open question).
pub struct RecordEngagementDecline;

#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RecordEngagementDecline {
    type Payload = RecordEngagementDeclinePayload;
    type Event = MarketingEvent;
    const NAME: &'static str = "RecordEngagementDecline";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    fn decide(payload: &Self::Payload, _matching_events: &[Self::Event]) -> CommandDecision {
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "EngagementDeclineFlagged".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "flagged_at": payload.flagged_at,
                }),
            }],
        }
    }
}

// --- cross-context routes ---
//
// `specs/marketing.allium`'s own "Cross-context wiring" note, made real:
// each `route()` below is a pure function of the source payload alone
// (no I/O, per `CrossContextRoute::route`'s own contract) - `lapsed_at`/
// `converted_at` are freshly stamped here since neither
// `CompanyExpiredPayload` nor `CompanyActivatedPayload` carries its own
// timestamp (see `src/helpdesk.rs`), matching the spec's own
// `ensures: ... lapsed_at: now` / `converted_at: now`. The engagement-
// decline route instead carries the source event's own `flagged_at`
// through unchanged, matching `specs/marketing.allium`'s own
// `rule EngagementDeclineIsRoutedFromActivity`.

/// `helpdesk.CompanyExpired -> marketing.RecordTrialLapse`.
pub struct HelpdeskExpiryToTrialLapse;

impl CrossContextRoute for HelpdeskExpiryToTrialLapse {
    type Source = helpdesk::CompanyExpired;
    type Target = RecordTrialLapse;
    const NAME: &'static str = "HelpdeskExpiryToTrialLapse";
    fn route(
        source_payload: &helpdesk::CompanyExpiredPayload,
    ) -> Option<RecordTrialLapsePayload> {
        Some(RecordTrialLapsePayload {
            company_id: source_payload.company_id.clone(),
            lapsed_at: chrono::Utc::now().to_rfc3339(),
        })
    }
}

/// `helpdesk.CompanyActivated -> marketing.RecordTrialConversion` - fires
/// for either edge into `active` (`ConvertCompanyTrial`'s `trialing ->
/// active` or `ReactivateCompany`'s `expired -> active`), since both
/// commit the same `CompanyActivated` event (see `src/helpdesk.rs`'s own
/// doc comment on that event).
pub struct HelpdeskActivationToTrialConversion;

impl CrossContextRoute for HelpdeskActivationToTrialConversion {
    type Source = helpdesk::CompanyActivated;
    type Target = RecordTrialConversion;
    const NAME: &'static str = "HelpdeskActivationToTrialConversion";
    fn route(
        source_payload: &helpdesk::CompanyActivatedPayload,
    ) -> Option<RecordTrialConversionPayload> {
        Some(RecordTrialConversionPayload {
            company_id: source_payload.company_id.clone(),
            converted_at: chrono::Utc::now().to_rfc3339(),
        })
    }
}

/// `activity.CompanyEngagementDeclined -> marketing.RecordEngagementDecline`.
pub struct ActivityEngagementDeclineToMarketingFlag;

impl CrossContextRoute for ActivityEngagementDeclineToMarketingFlag {
    type Source = activity::CompanyEngagementDeclined;
    type Target = RecordEngagementDecline;
    const NAME: &'static str = "ActivityEngagementDeclineToMarketingFlag";
    fn route(
        source_payload: &activity::CompanyEngagementDeclinedPayload,
    ) -> Option<RecordEngagementDeclinePayload> {
        Some(RecordEngagementDeclinePayload {
            company_id: source_payload.company_id.clone(),
            flagged_at: source_payload.flagged_at.clone(),
        })
    }
}
