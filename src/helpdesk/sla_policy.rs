//! SLA policies (issue #42) - `specs/skilj-helpdesk.allium`'s `rule
//! CompanySetsSlaPolicy`/`TicketBreachesSla`: a company's policy, the two
//! deadlines every new ticket gets from it, what cancels them, and what
//! happens when one fires. The policy types and the business-hours
//! arithmetic are `crate::sla`'s.
//!
//! The policy lives on the company (`CompanySlaPolicySet`), and is copied
//! onto each ticket as it's created (`TicketCreatedPayload::sla`): a
//! `ScheduleDeadline` only sees its source event's payload, so the
//! ticket's own creation has to carry everything its deadlines are
//! computed from. A company that never set a policy gets
//! `SlaPolicy::default()`, stamped the same way.

use super::*;
use crate::sla::{add_business_minutes, SlaKind, SlaPolicy};

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CompanySlaPolicySetPayload {
    pub company_id: String,
    pub policy: SlaPolicy,
}

pub struct CompanySlaPolicySet;

/// `SetCompanySlaPolicy`'s outcome. Only the latest one counts - see
/// `CompanyFacts::sla_policy`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for CompanySlaPolicySet {
    type Payload = CompanySlaPolicySetPayload;
    const NAME: &'static str = "CompanySlaPolicySet";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SetCompanySlaPolicyPayload {
    pub company_id: String,
    pub policy: SlaPolicy,
}

pub struct SetCompanySlaPolicy;

/// `specs/skilj-helpdesk.allium`'s `rule CompanySetsSlaPolicy`: replaces
/// the company's whole policy. Applies to tickets created from then on -
/// a ticket's deadlines were fixed when it was created.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for SetCompanySlaPolicy {
    type Payload = SetCompanySlaPolicyPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "SetCompanySlaPolicy";
    fn tag_mappings() -> Vec<TagMapping> {
        company_tag()
    }
    /// Only whether the company exists: a new policy replaces the old one
    /// whatever it was.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![company_status_query()]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        if company_status(matching_events, &payload.company_id).is_none() {
            return CommandDecision::Rejected {
                reason: format!("company {} does not exist", payload.company_id),
                kind: "company_not_found".into(),
            };
        }
        if let Err(reason) = payload.policy.validate() {
            return CommandDecision::Rejected {
                reason,
                kind: "invalid_sla_policy".into(),
            };
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "CompanySlaPolicySet".into(),
                payload: serde_json::json!({
                    "company_id": payload.company_id,
                    "policy": payload.policy,
                }),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketSlaBreachedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub sla: SlaKind,
    /// When the SLA was due, RFC 3339: the firing deadline's own
    /// `scheduled_at` (skilj 0.2.0's `Decision::deadline`), not when the
    /// deadline loop got to it - after an outage the two can be far
    /// apart. `None` for a breach submitted by hand, and for one recorded
    /// before this field existed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub due_at: Option<String>,
}

pub struct TicketSlaBreached;

/// `specs/skilj-helpdesk.allium`'s `rule TicketBreachesSla`: one of a
/// ticket's SLA targets passed with the ticket still waiting on it. Always
/// committed together with a `TicketEscalated` unless the ticket was
/// already escalated - see `BreachTicketSla`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketSlaBreached {
    type Payload = TicketSlaBreachedPayload;
    const NAME: &'static str = "TicketSlaBreached";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct BreachTicketSlaPayload {
    pub ticket_id: String,
    pub sla: SlaKind,
}

pub struct BreachTicketSla;

/// `specs/skilj-helpdesk.allium`'s `rule TicketBreachesSla`. Submitted by
/// `ScheduleFirstResponseSla`/`ScheduleResolutionSla` when their deadline
/// comes due, the same way `ScheduleTicketAutoClose` submits `CloseTicket`.
///
/// The cancels below normally keep a met SLA's deadline from firing at
/// all; `decide()` still checks, so a deadline that fires anyway (a
/// cancel lost to a rolling deploy - see `CancelDeadline`'s own doc
/// comment - or a hand-submitted breach) can't escalate a ticket that met
/// its target.
///
/// The escalation is `EscalateTicket`'s own, one priority tier, and only
/// if the ticket isn't escalated already (by the alerter's overdue sweep,
/// a person, or its other SLA): the breach itself is always recorded.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for BreachTicketSla {
    type Payload = BreachTicketSlaPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "BreachTicketSla";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    /// See `ticket_snapshot_query`.
    fn consistency_query() -> Vec<QueryItemMapping> {
        ticket_snapshot_query()
    }
    fn decide_with(
        payload: &Self::Payload,
        decision: &Decision<'_, Self::Event>,
    ) -> CommandDecision {
        let due_at = decision.deadline().map(|d| d.scheduled_at);
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts, due_at)
        })
    }
}

impl BreachTicketSla {
    /// `due_at` is the firing deadline's `scheduled_at`; `None` when
    /// submitted by hand.
    fn decide_on(
        payload: &BreachTicketSlaPayload,
        facts: &TicketFacts,
        due_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> CommandDecision {
        match facts.status {
            None => {
                return CommandDecision::Rejected {
                    reason: format!("ticket {} does not exist", payload.ticket_id),
                    kind: "ticket_not_found".into(),
                }
            }
            Some(TicketStatus::Resolved | TicketStatus::Closed | TicketStatus::Merged) => {
                return CommandDecision::Rejected {
                    reason: format!(
                        "ticket {} is already handled - no SLA left to breach",
                        payload.ticket_id
                    ),
                    kind: "ticket_not_unhandled".into(),
                }
            }
            Some(
                TicketStatus::Open | TicketStatus::InProgress | TicketStatus::WaitingOnCustomer,
            ) => {}
        }
        if payload.sla == SlaKind::FirstResponse && facts.first_responded {
            return CommandDecision::Rejected {
                reason: format!(
                    "ticket {} has already had a first response",
                    payload.ticket_id
                ),
                kind: "sla_already_met".into(),
            };
        }
        // The resolution SLA runs to the first resolution only, so a
        // reopened ticket has met it even though it is unhandled again.
        if payload.sla == SlaKind::Resolution && facts.resolutions > 0 {
            return CommandDecision::Rejected {
                reason: format!("ticket {} has already been resolved", payload.ticket_id),
                kind: "sla_already_met".into(),
            };
        }
        let already_breached = match payload.sla {
            SlaKind::FirstResponse => facts.first_response_breached,
            SlaKind::Resolution => facts.resolution_breached,
        };
        if already_breached {
            return CommandDecision::Rejected {
                reason: format!(
                    "ticket {}'s {:?} SLA has already been breached",
                    payload.ticket_id, payload.sla
                ),
                kind: "sla_already_breached".into(),
            };
        }
        let mut breach = serde_json::json!({
            "ticket_id": payload.ticket_id,
            "company_id": facts.company_id(),
            "sla": payload.sla,
        });
        if let Some(due_at) = due_at {
            breach["due_at"] = due_at.to_rfc3339().into();
        }
        let mut events = vec![EventSpec {
            event_type: "TicketSlaBreached".into(),
            payload: breach,
        }];
        if !facts.escalated {
            // Its current priority - a macro may have moved it.
            let previous_priority = facts
                .current_priority()
                .expect("a ticket with any status has a TicketCreated in its own history");
            events.push(EventSpec {
                event_type: "TicketEscalated".into(),
                payload: serde_json::json!({
                    "ticket_id": payload.ticket_id,
                    "company_id": facts.company_id(),
                    "previous_priority": previous_priority,
                    "new_priority": escalate_priority(previous_priority),
                }),
            });
        }
        CommandDecision::Accepted { events }
    }
}

/// The deadline both SLA schedules share: `kind`'s target from the
/// ticket's own `TicketSla`, counted in its business hours from now.
///
/// "Now" is when skilj's schedule loop processes the `TicketCreated`, not
/// when it committed - normally well under a second later, and the same
/// clock `ScheduleTicketAutoClose` uses. Tagged with the ticket alone: a
/// ticket is created once, so each schedule holds at most one row per
/// ticket, and a cancel names only its own schedule's rows (see
/// `CancelDeadline::Deadline`), so the ticket tag can't reach the other
/// SLA's deadline the way it would reach a later resolution's auto-close.
///
/// `None` for a `TicketCreated` stored before tickets carried an SLA.
fn sla_deadline(
    source: &TicketCreatedPayload,
    kind: SlaKind,
) -> Option<DeadlineSpec<BreachTicketSlaPayload>> {
    let sla = source.sla.as_ref()?;
    let minutes = match kind {
        SlaKind::FirstResponse => sla.first_response_minutes,
        SlaKind::Resolution => sla.resolution_minutes,
    };
    Some(DeadlineSpec {
        fire_at: add_business_minutes(chrono::Utc::now(), minutes, &sla.business_hours),
        expires_at: None,
        recurrence: None,
        tags: vec![ticket_tag_value(&source.ticket_id)],
        payload: BreachTicketSlaPayload {
            ticket_id: source.ticket_id.clone(),
            sla: kind,
        },
    })
}

/// The first-response half of `rule TicketBreachesSla`: due
/// `first_response_minutes` of business time after the ticket is created.
/// Cancelled by the first staff message to the customer
/// (`CancelFirstResponseSlaOnStaffReply`, or
/// `CancelFirstResponseSlaOnMacroReply` for a macro's reply) or by resolving
/// (`CancelFirstResponseSlaOnResolve`). Picking a ticket up is not a
/// response: the customer hears nothing from it.
pub struct ScheduleFirstResponseSla;

impl ScheduleDeadline for ScheduleFirstResponseSla {
    type Source = TicketCreated;
    type Target = BreachTicketSla;
    const NAME: &'static str = "ScheduleFirstResponseSla";
    /// Tickets created before this deploy carry no SLA anyway.
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn schedule(
        source_payload: &TicketCreatedPayload,
    ) -> Option<DeadlineSpec<BreachTicketSlaPayload>> {
        sla_deadline(source_payload, SlaKind::FirstResponse)
    }
}

/// The resolution half of `rule TicketBreachesSla`, cancelled by resolving
/// (`CancelResolutionSlaOnResolve`). Runs once, to the first resolution: a
/// reopen doesn't restart it - a reopened ticket was already resolved in
/// time once, and the alerter's overdue sweep still covers it.
pub struct ScheduleResolutionSla;

impl ScheduleDeadline for ScheduleResolutionSla {
    type Source = TicketCreated;
    type Target = BreachTicketSla;
    const NAME: &'static str = "ScheduleResolutionSla";
    /// See `ScheduleFirstResponseSla::START_FROM`.
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn schedule(
        source_payload: &TicketCreatedPayload,
    ) -> Option<DeadlineSpec<BreachTicketSlaPayload>> {
        sla_deadline(source_payload, SlaKind::Resolution)
    }
}

/// A staff question to the customer is this helpdesk's staff reply - it
/// meets the first-response SLA.
pub struct CancelFirstResponseSlaOnStaffReply;

impl CancelDeadline for CancelFirstResponseSlaOnStaffReply {
    type Source = TicketInfoRequested;
    type Deadline = ScheduleFirstResponseSla;
    const NAME: &'static str = "CancelFirstResponseSlaOnStaffReply";
    /// Matches `ScheduleFirstResponseSla::START_FROM`.
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn cancel_tags(source_payload: &TicketInfoRequestedPayload) -> Option<Vec<Tag>> {
        Some(vec![ticket_tag_value(&source_payload.ticket_id)])
    }
}

/// A macro's plain reply (`ApplyMacro` without a status, or resolving -
/// see `TicketStaffReplied`) meets the first-response SLA the same way.
/// A macro that asks the customer something emits a `TicketInfoRequested`
/// instead, which `CancelFirstResponseSlaOnStaffReply` already covers.
pub struct CancelFirstResponseSlaOnMacroReply;

impl CancelDeadline for CancelFirstResponseSlaOnMacroReply {
    type Source = TicketStaffReplied;
    type Deadline = ScheduleFirstResponseSla;
    const NAME: &'static str = "CancelFirstResponseSlaOnMacroReply";
    /// Matches `ScheduleFirstResponseSla::START_FROM`.
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn cancel_tags(source_payload: &TicketStaffRepliedPayload) -> Option<Vec<Tag>> {
        Some(vec![ticket_tag_value(&source_payload.ticket_id)])
    }
}

/// Resolving a ticket nobody had replied to yet meets its first-response
/// SLA too: the resolution is the response.
pub struct CancelFirstResponseSlaOnResolve;

impl CancelDeadline for CancelFirstResponseSlaOnResolve {
    type Source = TicketResolved;
    type Deadline = ScheduleFirstResponseSla;
    const NAME: &'static str = "CancelFirstResponseSlaOnResolve";
    /// Matches `ScheduleFirstResponseSla::START_FROM`.
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn cancel_tags(source_payload: &TicketResolvedPayload) -> Option<Vec<Tag>> {
        Some(vec![ticket_tag_value(&source_payload.ticket_id)])
    }
}

/// Resolving meets the resolution SLA.
pub struct CancelResolutionSlaOnResolve;

impl CancelDeadline for CancelResolutionSlaOnResolve {
    type Source = TicketResolved;
    type Deadline = ScheduleResolutionSla;
    const NAME: &'static str = "CancelResolutionSlaOnResolve";
    /// Matches `ScheduleResolutionSla::START_FROM`.
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn cancel_tags(source_payload: &TicketResolvedPayload) -> Option<Vec<Tag>> {
        Some(vec![ticket_tag_value(&source_payload.ticket_id)])
    }
}
