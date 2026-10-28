//! The ticket commands, and the auto-close deadline they schedule and
//! cancel.

use super::*;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CreateTicketPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub requester_id: String,
    pub logged_by_staff_id: Option<String>,
    pub title: String,
    pub description: String,
    pub priority: TicketPriority,
    /// See `TicketCreatedPayload::requester_name`.
    pub requester_name: Option<String>,
    /// See `TicketCreatedPayload::requester_email`.
    pub requester_email: Option<String>,
}

pub struct CreateTicket;

/// `specs/skilj-helpdesk.allium`'s `rule CustomerCreatesTicket`/
/// `StaffLogsTicketOnBehalf`, merged into one command
/// (`logged_by_staff_id` tells the two cases apart) - the spec keeps
/// them as two triggers because they're two different surfaces
/// (`CustomerPortal` vs. `StaffTicketQueue`); at the `decide()` level
/// they're the same decision, so one `CommandType` covers both, the
/// same way the spec's own `logged_by: StaffMember?` already unifies
/// them on the `Ticket` entity.
///
/// `requires: company.status != expired` - implemented in full now that
/// `company_status` tracks the real lifecycle (this was originally
/// written against `specs/skilj-helpdesk.allium`'s own
/// `requires: company.status = active`, which turned out to be a bug in
/// the spec itself, caught while wiring this up for real: it would have
/// blocked ticket creation during the free trial entirely, which
/// contradicts the whole point of a trial - fixed in the spec alongside
/// this code, not worked around here).
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for CreateTicket {
    type Payload = CreateTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CreateTicket";
    /// The company for its status, and the ticket so that a used id is
    /// found by its own tag rather than among all the company's tickets.
    fn tag_mappings() -> Vec<TagMapping> {
        [company_tag(), ticket_tag()].concat()
    }
    /// The company's latest lifecycle event, whether any company already
    /// has a ticket with this id, and the company's latest SLA policy.
    /// Before this, `decide()` read every event tagged with the company -
    /// every ticket it ever filed - and another of its tickets committed
    /// meanwhile was a conflict. Only the `ticket` tag on the second item:
    /// a ticket id is global (see `CompanyFacts::apply`), so another
    /// company's use of it must be seen too.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![
            company_status_query(),
            QueryItemMapping::types(&["TicketCreated"]).tagged(ticket_tag()),
            QueryItemMapping::types(&["CompanySlaPolicySet"])
                .tagged(company_tag())
                .latest(),
        ]
    }
    /// The stored command holds the same customer data as the event it
    /// produces, so it needs the same protection.
    fn sensitive_fields() -> Vec<SensitiveField> {
        ticket_created_customer_fields()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        create_ticket(
            payload,
            &CompanyFacts::of(matching_events, &payload.company_id),
        )
    }
}

/// `CreateTicket`'s decision.
fn create_ticket(payload: &CreateTicketPayload, company: &CompanyFacts) -> CommandDecision {
    match company.status {
        None => {
            return CommandDecision::Rejected {
                reason: format!("company {} has not signed up", payload.company_id),
                kind: "company_not_found".into(),
            };
        }
        Some(CompanyStatus::Expired) => {
            return CommandDecision::Rejected {
                reason: format!(
                    "company {} is expired - subscribe to keep creating tickets",
                    payload.company_id
                ),
                kind: "company_expired".into(),
            };
        }
        Some(CompanyStatus::Trialing | CompanyStatus::Active) => {}
    }
    if company.ticket_ids.contains(&payload.ticket_id) {
        return CommandDecision::Rejected {
            reason: format!("ticket {} already exists", payload.ticket_id),
            kind: "ticket_already_exists".into(),
        };
    }
    CommandDecision::Accepted {
        events: vec![EventSpec {
            event_type: "TicketCreated".into(),
            payload: serde_json::json!({
                "ticket_id": payload.ticket_id,
                "company_id": payload.company_id,
                "requester_id": payload.requester_id,
                "logged_by_staff_id": payload.logged_by_staff_id,
                "title": payload.title,
                "description": payload.description,
                "priority": payload.priority,
                "requester_name": payload.requester_name,
                "requester_email": payload.requester_email,
                "sla": company
                    .sla_policy
                    .clone()
                    .unwrap_or_default()
                    .for_priority(payload.priority),
            }),
        }],
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AssignTicketPayload {
    pub ticket_id: String,
    pub staff_id: String,
}

pub struct AssignTicket;

/// `specs/skilj-helpdesk.allium`'s `rule StaffPicksUpTicket`:
/// `requires: ticket.status = open`.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for AssignTicket {
    type Payload = AssignTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "AssignTicket";
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
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts)
        })
    }
}

impl AssignTicket {
    fn decide_on(payload: &AssignTicketPayload, facts: &TicketFacts) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Open) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketAssigned".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "staff_id": payload.staff_id,
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not open - only an open ticket can be picked up",
                    payload.ticket_id
                ),
                kind: "ticket_not_open".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ResolveTicketPayload {
    pub ticket_id: String,
}

pub struct ResolveTicket;

/// `specs/skilj-helpdesk.allium`'s `rule StaffResolvesTicket`:
/// `requires: ticket.status = in_progress`.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for ResolveTicket {
    type Payload = ResolveTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "ResolveTicket";
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
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts)
        })
    }
}

impl ResolveTicket {
    pub(super) fn decide_on(
        payload: &ResolveTicketPayload,
        facts: &TicketFacts,
    ) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::InProgress) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketResolved".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "resolution": facts.resolutions + 1,
                        "requester_id": facts.requester_id,
                        "assignee_id": facts.assignee_id,
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not in progress - only a picked-up ticket can be resolved",
                    payload.ticket_id
                ),
                kind: "ticket_not_in_progress".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ReopenTicketPayload {
    pub ticket_id: String,
}

pub struct ReopenTicket;

/// `specs/skilj-helpdesk.allium`'s `rule TicketReopened`: `requires:
/// ticket.status = resolved`.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for ReopenTicket {
    type Payload = ReopenTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "ReopenTicket";
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
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts)
        })
    }
}

impl ReopenTicket {
    fn decide_on(payload: &ReopenTicketPayload, facts: &TicketFacts) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Resolved) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketReopened".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "resolution": facts.resolutions,
                        "assignee_id": facts.assignee_id,
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not resolved - only a resolved ticket can be reopened",
                    payload.ticket_id
                ),
                kind: "ticket_not_resolved".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RequestInfoFromCustomerPayload {
    pub ticket_id: String,
    pub staff_id: String,
    /// Encrypted under the requester's key, here and in the event.
    pub message: String,
    /// The ticket's requester. Required, though optional in the schema
    /// (a field added later must be): without it the stored command would
    /// hold `message` in plaintext. See `TicketFacts::reject_unless_requester`.
    pub requester_id: Option<String>,
}

pub struct RequestInfoFromCustomer;

/// `specs/skilj-helpdesk.allium`'s `rule StaffRequestsInfo`: `requires:
/// ticket.status = in_progress`.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for RequestInfoFromCustomer {
    type Payload = RequestInfoFromCustomerPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "RequestInfoFromCustomer";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["message"])
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
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts)
        })
    }
}

impl RequestInfoFromCustomer {
    pub(super) fn decide_on(
        payload: &RequestInfoFromCustomerPayload,
        facts: &TicketFacts,
    ) -> CommandDecision {
        let status = facts.status;
        if status.is_some() {
            if let Some(rejected) = facts.reject_unless_requester(payload.requester_id.as_deref()) {
                return rejected;
            }
        }
        match status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::InProgress) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketInfoRequested".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "staff_id": payload.staff_id,
                        "message": payload.message,
                        "requester_id": payload.requester_id,
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not in progress - can only ask a picked-up ticket's customer for more information",
                    payload.ticket_id
                ),
                kind: "ticket_not_in_progress".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CustomerRespondsToTicketPayload {
    pub ticket_id: String,
    /// Must be the ticket's requester - see `TicketFacts::reject_unless_requester`.
    pub requester_id: String,
    /// Encrypted under the requester's key, here and in the event.
    pub message: String,
}

pub struct CustomerRespondsToTicket;

/// `specs/skilj-helpdesk.allium`'s `rule CustomerReplies`: `requires:
/// ticket.status = waiting_on_customer`.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for CustomerRespondsToTicket {
    type Payload = CustomerRespondsToTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CustomerRespondsToTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["message"])
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
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts)
        })
    }
}

impl CustomerRespondsToTicket {
    fn decide_on(
        payload: &CustomerRespondsToTicketPayload,
        facts: &TicketFacts,
    ) -> CommandDecision {
        let status = facts.status;
        if status.is_some() {
            if let Some(rejected) = facts.reject_unless_requester(Some(&payload.requester_id)) {
                return rejected;
            }
        }
        match status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::WaitingOnCustomer) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketCustomerResponded".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "requester_id": payload.requester_id,
                        "message": payload.message,
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not waiting on the customer - nothing to respond to",
                    payload.ticket_id
                ),
                kind: "ticket_not_waiting_on_customer".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CloseTicketPayload {
    pub ticket_id: String,
    /// Whose ticket this is - set by `ScheduleTicketAutoClose` from
    /// `TicketResolvedPayload::requester_id`, so a pending auto-close
    /// deadline names its customer. skilj's `forgetSubject` finds the
    /// deadlines to resolve as `forgotten` through the target command's
    /// `sensitive_fields` subjects, which is why `CloseTicket` declares
    /// `requester_email` below.
    pub requester_id: Option<String>,
    /// Where a closure notice would go, encrypted under the requester's
    /// key. The auto-close deadline leaves it `None`: the only email it
    /// could copy is `TicketCreated`'s, which it sees as ciphertext (see
    /// `TicketFacts::requester_id`). Declaring it is what ties a
    /// `CloseTicket` payload to its customer for `forgetSubject` -
    /// skilj has no way to declare a subject without a sensitive field.
    pub requester_email: Option<String>,
}

pub struct CloseTicket;

/// `specs/skilj-helpdesk.allium`'s `rule TicketAutoCloses`: `requires:
/// ticket.status = resolved`. Submitted by `ScheduleTicketAutoClose`
/// below - see `TicketClosed`'s own doc comment.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for CloseTicket {
    type Payload = CloseTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CloseTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        vec![SensitiveField {
            field: "requester_email".into(),
            subject_key: CUSTOMER_SUBJECT.into(),
            subject_field: "requester_id".into(),
        }]
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
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts)
        })
    }
}

impl CloseTicket {
    fn decide_on(payload: &CloseTicketPayload, facts: &TicketFacts) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Resolved) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketClosed".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                    }),
                }],
            },
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not resolved - only a resolved ticket auto-closes",
                    payload.ticket_id
                ),
                kind: "ticket_not_resolved".into(),
            },
        }
    }
}

/// `specs/skilj-helpdesk.allium`'s `rule TicketAutoCloses`, ported the
/// same way `ScheduleCompanyTrialConversion` above is - see that one's
/// doc comment for the `fire_at`/`START_FROM` reasoning, identical here.
///
/// A reopen cancels the deadline - see `CancelTicketAutoCloseOnReopen`.
/// A ticket that is closed by hand or merged away while its deadline is
/// pending needs no cancel: the deadline fires once and
/// `CloseTicket::decide()` rejects it ("not resolved"), which
/// docs/architecture.md §46 treats as a normal outcome ("a `Target`
/// command that ... gets rejected by its own `decide()` is marked
/// `fired` too").
///
/// Tagged twice: `ticket` like every other ticket deadline, and
/// `ticket_resolution` for this one resolution, which is the only tag
/// the cancel names. Events stored before `TicketResolved::resolution`
/// existed get the `ticket` tag only, so a reopen never cancels them and
/// `decide()` is their only guard, as before.
pub struct ScheduleTicketAutoClose;

impl ScheduleDeadline for ScheduleTicketAutoClose {
    type Source = TicketResolved;
    type Target = CloseTicket;
    const NAME: &'static str = "ScheduleTicketAutoClose";
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn schedule(
        source_payload: &TicketResolvedPayload,
    ) -> Option<DeadlineSpec<CloseTicketPayload>> {
        let mut tags = vec![ticket_tag_value(&source_payload.ticket_id)];
        if let Some(resolution) = source_payload.resolution {
            tags.push(ticket_resolution_tag_value(
                &source_payload.ticket_id,
                resolution,
            ));
        }
        Some(DeadlineSpec {
            fire_at: chrono::Utc::now() + scheduling::auto_close_after(),
            expires_at: None,
            recurrence: None,
            tags,
            payload: CloseTicketPayload {
                ticket_id: source_payload.ticket_id.clone(),
                requester_id: source_payload.requester_id.clone(),
                requester_email: None,
            },
        })
    }
}

/// Cancels a resolved ticket's pending auto-close when it is reopened -
/// `specs/skilj-helpdesk.allium`'s `rule TicketReopened` ends the
/// resolution `rule TicketAutoCloses` was waiting on.
///
/// `CloseTicket::decide()` alone is not enough here. Resolve, reopen and
/// resolve again within `auto_close_after`, and the first resolution's
/// deadline comes due while the ticket is `resolved` again: `decide()`
/// accepts, and the ticket closes days before its second resolution's
/// wait is up.
///
/// It cancels by `ticket_resolution` rather than by `ticket` because
/// skilj's schedule and cancel loops keep separate cursors, and nothing
/// stops the schedule loop running ahead (docs/architecture.md §130 only
/// holds a cancel back until its schedule catches up, not the reverse).
/// If the schedule loop has already turned the second `TicketResolved`
/// into a deadline when this cancel reaches the reopen before it, a
/// `ticket` tag would cancel that one too, and the ticket would never
/// auto-close. The resolution number makes the reopen name only the
/// deadline it actually ends. The opposite race - the deadline coming
/// due before this cancel has run - is skilj's own to hold back (§131).
pub struct CancelTicketAutoCloseOnReopen;

impl CancelDeadline for CancelTicketAutoCloseOnReopen {
    type Source = TicketReopened;
    type Deadline = ScheduleTicketAutoClose;
    const NAME: &'static str = "CancelTicketAutoCloseOnReopen";
    /// Matches `ScheduleTicketAutoClose::START_FROM`: a reopen from
    /// before this deploy has no deadline of this shape to cancel.
    const START_FROM: DeadlinePollStartFrom = DeadlinePollStartFrom::Latest;
    fn cancel_tags(source_payload: &TicketReopenedPayload) -> Option<Vec<Tag>> {
        let resolution = source_payload.resolution?;
        Some(vec![ticket_resolution_tag_value(
            &source_payload.ticket_id,
            resolution,
        )])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EscalateTicketPayload {
    pub ticket_id: String,
}

pub struct EscalateTicket;

/// Not in the original spec - see `TicketEscalated`'s own doc comment,
/// and `specs/skilj-helpdesk.allium`'s updated `rule
/// TicketBecomesOverdue`. Submitted by `src/bin/alerter.rs`'s own
/// overdue sweep, never by a person - same "a background reactor submits
/// an ordinary command" treatment `CloseTicket` gets from
/// `ScheduleTicketAutoClose` and `ConvertCompanyTrial` gets from
/// `ScheduleCompanyTrialConversion`.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for EscalateTicket {
    type Payload = EscalateTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "EscalateTicket";
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
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts)
        })
    }
}

impl EscalateTicket {
    fn decide_on(payload: &EscalateTicketPayload, facts: &TicketFacts) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Resolved | TicketStatus::Closed | TicketStatus::Merged) => {
                CommandDecision::Rejected {
                    reason: format!(
                        "ticket {} is already handled - nothing to escalate",
                        payload.ticket_id
                    ),
                    kind: "ticket_not_unhandled".into(),
                }
            }
            Some(
                TicketStatus::Open | TicketStatus::InProgress | TicketStatus::WaitingOnCustomer,
            ) => {
                let already_escalated = facts.escalated;
                if already_escalated {
                    return CommandDecision::Rejected {
                        reason: format!("ticket {} has already been escalated", payload.ticket_id),
                        kind: "already_escalated".into(),
                    };
                }
                // Its current priority, which `ApplyMacro` may have moved
                // off the one it was created with.
                let previous_priority = facts
                    .current_priority()
                    .expect("a ticket with any status has a TicketCreated in its own history");
                let new_priority = escalate_priority(previous_priority);
                CommandDecision::Accepted {
                    events: vec![EventSpec {
                        event_type: "TicketEscalated".into(),
                        payload: serde_json::json!({
                            "ticket_id": payload.ticket_id,
                            "company_id": facts.company_id(),
                            "previous_priority": previous_priority,
                            "new_priority": new_priority,
                        }),
                    }],
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct MergeTicketsPayload {
    pub primary_ticket_id: String,
    pub duplicate_ticket_id: String,
}

pub struct MergeTickets;

/// Not in the original spec - the showcase of skilj's own DCB model this
/// crate hadn't yet demonstrated: `tag_mappings` below declares *two*
/// `"ticket"` tags (one per payload field), and its query one
/// `TicketSnapshot` item per ticket, so it decides on both tickets'
/// states - no classic aggregate boundary, no two-phase commit, one
/// ordinary decision about two entities' consistency at once. See `TicketsMerged`'s own doc comment
/// for the event side of the same trick.
impl MergeTickets {
    fn primary() -> Vec<TagMapping> {
        vec![TagMapping::new("ticket", "primary_ticket_id")]
    }

    fn duplicate() -> Vec<TagMapping> {
        vec![TagMapping::new("ticket", "duplicate_ticket_id")]
    }
}

#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for MergeTickets {
    type Payload = MergeTicketsPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "MergeTickets";
    fn tag_mappings() -> Vec<TagMapping> {
        [Self::primary(), Self::duplicate()].concat()
    }
    /// Each ticket's `TicketSnapshot` state - one snapshot item per
    /// ticket, so both are read from their stored rows.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![
            QueryItemMapping::snapshot::<TicketSnapshot>("primary_ticket_id"),
            QueryItemMapping::snapshot::<TicketSnapshot>("duplicate_ticket_id"),
        ]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide_with(
        payload: &Self::Payload,
        decision: &Decision<'_, Self::Event>,
    ) -> CommandDecision {
        if payload.primary_ticket_id == payload.duplicate_ticket_id {
            return CommandDecision::Rejected {
                reason: "a ticket cannot be merged into itself".into(),
                kind: "cannot_merge_ticket_into_itself".into(),
            };
        }
        decide_on_ticket(decision, &payload.primary_ticket_id, |primary| {
            decide_on_ticket(decision, &payload.duplicate_ticket_id, |duplicate| {
                Self::decide_on(payload, primary, duplicate)
            })
        })
    }
}

impl MergeTickets {
    fn decide_on(
        payload: &MergeTicketsPayload,
        primary: &TicketFacts,
        duplicate: &TicketFacts,
    ) -> CommandDecision {
        match (primary.status, duplicate.status) {
            (None, _) => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.primary_ticket_id),
                kind: "primary_ticket_not_found".into(),
            },
            (_, None) => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.duplicate_ticket_id),
                kind: "duplicate_ticket_not_found".into(),
            },
            (Some(TicketStatus::Closed | TicketStatus::Merged), _) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is already closed or merged - not mergeable",
                    payload.primary_ticket_id
                ),
                kind: "primary_ticket_not_mergeable".into(),
            },
            (_, Some(TicketStatus::Closed | TicketStatus::Merged)) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is already closed or merged - not mergeable",
                    payload.duplicate_ticket_id
                ),
                kind: "duplicate_ticket_not_mergeable".into(),
            },
            (Some(_), Some(_)) => {
                let primary_company = primary.company_id();
                if primary_company != duplicate.company_id() {
                    return CommandDecision::Rejected {
                        reason: "the two tickets belong to different companies".into(),
                        kind: "tickets_belong_to_different_companies".into(),
                    };
                }
                CommandDecision::Accepted {
                    events: vec![EventSpec {
                        event_type: "TicketsMerged".into(),
                        payload: serde_json::json!({
                            "primary_ticket_id": payload.primary_ticket_id,
                            "duplicate_ticket_id": payload.duplicate_ticket_id,
                            "company_id": primary_company,
                            "duplicate_assignee_id": duplicate.assignee_id,
                        }),
                    }],
                }
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct RateTicketPayload {
    pub ticket_id: String,
    pub rating: u8,
    /// Encrypted under the requester's key, here and in the event.
    pub comment: Option<String>,
    /// The ticket's requester - required, see
    /// `RequestInfoFromCustomerPayload::requester_id`.
    pub requester_id: Option<String>,
}

pub struct RateTicket;

/// Not in the original spec - a CSAT survey response (see `TicketRated`'s
/// own doc comment). Requires `resolved` *or* `closed`, not just
/// `closed`: real tools survey right after resolution, not after
/// `config.auto_close_after`'s own multi-day wait.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for RateTicket {
    type Payload = RateTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "RateTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["comment"])
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
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts)
        })
    }
}

impl RateTicket {
    fn decide_on(payload: &RateTicketPayload, facts: &TicketFacts) -> CommandDecision {
        let status = facts.status;
        if status.is_some() {
            if let Some(rejected) = facts.reject_unless_requester(payload.requester_id.as_deref()) {
                return rejected;
            }
        }
        match status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Resolved | TicketStatus::Closed) => {
                if !(1..=5).contains(&payload.rating) {
                    return CommandDecision::Rejected {
                        reason: format!("rating {} is not between 1 and 5", payload.rating),
                        kind: "invalid_rating".into(),
                    };
                }
                let already_rated = facts.rated;
                if already_rated {
                    return CommandDecision::Rejected {
                        reason: format!("ticket {} has already been rated", payload.ticket_id),
                        kind: "already_rated".into(),
                    };
                }
                CommandDecision::Accepted {
                    events: vec![EventSpec {
                        event_type: "TicketRated".into(),
                        payload: serde_json::json!({
                            "ticket_id": payload.ticket_id,
                            "company_id": facts.company_id(),
                            "rating": payload.rating,
                            "comment": payload.comment,
                            "requester_id": payload.requester_id,
                        }),
                    }],
                }
            }
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not resolved or closed - nothing to rate yet",
                    payload.ticket_id
                ),
                kind: "ticket_not_ratable".into(),
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AddInternalNotePayload {
    pub ticket_id: String,
    pub staff_id: String,
    pub note: String,
}

pub struct AddInternalNote;

/// Not in the original spec - a staff-only note (see
/// `TicketInternalNoteAdded`'s own doc comment for why it's kept out of
/// the customer-facing projections below). Allowed at any ticket status,
/// including after close - a real audit trail doesn't stop just because
/// the ticket did.
///
/// `private_fields()` below is one of two places
/// `REQUIRES_CAPABILITY = Some(STAFF_CAPABILITY)` on `TicketInternalNotes`
/// (see that projection's own doc comment) doesn't reach - the other is
/// `TicketInternalNoteAdded`'s own identical `private_fields()`, the event
/// this command's own `decide()` emits. Both exist because skilj's generic
/// `CommandQuery`/`EventQuery` (`fetchCommands`/`queryEvents`/
/// `countEvents`/`inspectEvent` - GraphQL, `AdminAccess`-gated,
/// skilj-inspector/skilj-tui's own read path, not anything this crate
/// builds itself) can show any command or event's raw payload to an
/// admin-mapped Role regardless of a projection's gate, which only
/// gates `ProjectionQuery`. `staff_id`/`note` as
/// `PrivateFieldKind::Capability` naming `STAFF_CAPABILITY` on both
/// closes it, on the same terms as the projection: a grant reads them
/// only by holding the capability - no superadmin bypass, and since
/// skilj 0.2.0 no longer by its Role's name either (docs/architecture.md
/// §228 in the sibling `skilj` repo). A deliberate choice: even this
/// project's own platform operators can't read a ticket's internal notes
/// through generic admin tooling without also holding the capability -
/// accepted here since "staff-only" is this feature's entire point, not
/// a boundary meant to stop only customers.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for AddInternalNote {
    type Payload = AddInternalNotePayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "AddInternalNote";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn private_fields() -> Vec<PrivateField> {
        vec![
            PrivateField {
                field: "staff_id".into(),
                kind: PrivateFieldKind::Capability,
                capability: Some(STAFF_CAPABILITY.into()),
                addressee_field: None,
            },
            PrivateField {
                field: "note".into(),
                kind: PrivateFieldKind::Capability,
                capability: Some(STAFF_CAPABILITY.into()),
                addressee_field: None,
            },
        ]
    }
    /// See `ticket_snapshot_query`.
    fn consistency_query() -> Vec<QueryItemMapping> {
        ticket_snapshot_query()
    }
    fn decide_with(
        payload: &Self::Payload,
        decision: &Decision<'_, Self::Event>,
    ) -> CommandDecision {
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts)
        })
    }
}

impl AddInternalNote {
    fn decide_on(payload: &AddInternalNotePayload, facts: &TicketFacts) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(_) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "TicketInternalNoteAdded".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": facts.company_id(),
                        "staff_id": payload.staff_id,
                        "note": payload.note,
                    }),
                }],
            },
        }
    }
}
