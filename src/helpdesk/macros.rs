//! Macros (canned responses, issue #43): a company's reusable staff
//! replies, each optionally also setting the ticket's status and/or
//! priority, and `ApplyMacro`, which turns one into those changes on a
//! ticket in a single decision.
//!
//! ## Why this lives in `helpdesk`, not in its own bounded context
//!
//! A separate `macros` context (the shape `activity.rs`/`marketing.rs`
//! have) was the first idea, and doesn't hold up. A skilj consistency
//! query is per bounded context (see `company_status`'s note in
//! `mod.rs`): `ApplyMacro`'s `decide()` could not read another context's
//! macro events, only the ticket. The workaround - the caller resolving
//! the macro itself and sending its content along - would make a "macro"
//! an unchecked client-side template: a deleted or just-edited macro
//! would still be applied, nothing would conflict, and the command would
//! carry free text for no reason. Kept here, one decision reads the
//! ticket's `TicketSnapshot` state *and* the macro's latest event, so an
//! edit or delete committed meanwhile conflicts the append and the
//! command is retried against the new macro.
//!
//! The same placement keeps macros wherever a company's tickets are: they
//! are per-company traffic that follows the company into its tenant
//! (`routing.rs` classifies every macro command as `Ticket`), which a
//! separate context could not do without its own tenancy story.
//!
//! Macros are tagged `company` *and* `macro`, so a macro command reads
//! one macro's latest event, never the company's tickets or other macros.

use super::*;

fn macro_tag() -> Vec<TagMapping> {
    vec![TagMapping {
        key: "macro".into(),
        field: "macro_id".into(),
    }]
}

/// The three events that make up a macro's history.
const MACRO_EVENTS: [&str; 3] = ["MacroCreated", "MacroUpdated", "MacroDeleted"];

/// What every macro command reads: this macro's latest event at this
/// company. Its last event is all of its state - `MacroCreated`/
/// `MacroUpdated` carry the whole definition, `MacroDeleted` ends it.
/// `macro` first: an item is looked up by its first tag, and a macro has
/// far fewer events than its company.
fn macro_query() -> QueryItemMapping {
    QueryItemMapping::types(&MACRO_EVENTS)
        .tagged(macro_tag())
        .tagged(company_tag())
        .latest()
}

/// The status a macro may move a ticket to. Only the two a staff reply
/// naturally ends in - asking the customer something, or answering it -
/// each reached through the same rule as its own command
/// (`RequestInfoFromCustomer`/`ResolveTicket`). Picking up (`open ->
/// in_progress`) is an assignment, not a reply, and closing is the
/// auto-close's alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum MacroStatus {
    WaitingOnCustomer,
    Resolved,
}

fn macro_status_str(status: MacroStatus) -> &'static str {
    match status {
        MacroStatus::WaitingOnCustomer => "waiting_on_customer",
        MacroStatus::Resolved => "resolved",
    }
}

// --- events ---

/// A macro's whole definition - the payload of `CreateMacro`/`UpdateMacro`
/// and of the events they emit.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MacroDefinitionPayload {
    pub company_id: String,
    pub macro_id: String,
    /// Who saved this version.
    pub staff_id: String,
    pub name: String,
    /// The reply text. `{{ticket_id}}` and `{{staff_id}}` are filled in
    /// when it's applied (see `render_macro_body`); nothing about the
    /// customer is, since their name is only held encrypted.
    pub body: String,
    pub status: Option<MacroStatus>,
    pub priority: Option<TicketPriority>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MacroDeletedPayload {
    pub company_id: String,
    pub macro_id: String,
    pub staff_id: String,
}

/// Every macro event is tagged by company and by macro - see this
/// module's doc comment.
fn macro_event_tags() -> Vec<TagMapping> {
    [macro_tag(), company_tag()].concat()
}

pub struct MacroCreated;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for MacroCreated {
    type Payload = MacroDefinitionPayload;
    const NAME: &'static str = "MacroCreated";
    fn tag_mappings() -> Vec<TagMapping> {
        macro_event_tags()
    }
}

pub struct MacroUpdated;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for MacroUpdated {
    type Payload = MacroDefinitionPayload;
    const NAME: &'static str = "MacroUpdated";
    fn tag_mappings() -> Vec<TagMapping> {
        macro_event_tags()
    }
}

pub struct MacroDeleted;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for MacroDeleted {
    type Payload = MacroDeletedPayload;
    const NAME: &'static str = "MacroDeleted";
    fn tag_mappings() -> Vec<TagMapping> {
        macro_event_tags()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketStaffRepliedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub staff_id: String,
    /// Encrypted under the requester's key, like `TicketInfoRequested`'s
    /// message: a reply is part of the customer's conversation.
    pub message: String,
    /// The ticket's requester, copied from its `TicketCreated` - whose
    /// key `message` is encrypted under and whose `CustomerTickets` row
    /// it lands in.
    pub requester_id: String,
    /// The macro this reply came from.
    pub macro_id: Option<String>,
}

pub struct TicketStaffReplied;

/// A staff reply that doesn't, by itself, move the ticket - unlike
/// `TicketInfoRequested`, which is the reply that asks the customer
/// something and waits on them. Only `ApplyMacro` emits it so far; a
/// macro that sets `waiting_on_customer` emits a `TicketInfoRequested`
/// carrying the reply instead, so every reply is exactly one event.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketStaffReplied {
    type Payload = TicketStaffRepliedPayload;
    const NAME: &'static str = "TicketStaffReplied";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["message"])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketPriorityChangedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub staff_id: String,
    pub previous_priority: TicketPriority,
    pub new_priority: TicketPriority,
    /// The macro that changed it.
    pub macro_id: Option<String>,
}

pub struct TicketPriorityChanged;

/// A staff member setting a ticket's priority - `TicketEscalated` is the
/// overdue sweep's own one-tier bump and stays separate (it is once-only,
/// this is not). Only `ApplyMacro` emits it so far.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketPriorityChanged {
    type Payload = TicketPriorityChangedPayload;
    const NAME: &'static str = "TicketPriorityChanged";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
}

// --- the macro fold ---

/// One macro's state, folded from a macro command's `matching_events` -
/// its latest event, by `macro_query`, though the fold takes any number.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct MacroFacts {
    /// Any event of this id at this company, deleted or not: an id is
    /// never reused, the same rule `CreateTicket` keeps for ticket ids.
    pub used: bool,
    /// `None` when it never existed or was deleted.
    pub current: Option<MacroDefinitionPayload>,
}

impl MacroFacts {
    pub fn of(matching_events: &[HelpdeskEvent], company_id: &str, macro_id: &str) -> Self {
        let mut facts = Self::default();
        let this = |c: &str, m: &str| c == company_id && m == macro_id;
        for event in matching_events {
            match event {
                HelpdeskEvent::MacroCreated(p) | HelpdeskEvent::MacroUpdated(p)
                    if this(&p.company_id, &p.macro_id) =>
                {
                    facts.used = true;
                    facts.current = Some(p.clone());
                }
                HelpdeskEvent::MacroDeleted(p) if this(&p.company_id, &p.macro_id) => {
                    facts.used = true;
                    facts.current = None;
                }
                _ => {}
            }
        }
        facts
    }
}

fn macro_not_found(company_id: &str, macro_id: &str) -> CommandDecision {
    CommandDecision::Rejected {
        reason: format!("company {company_id} has no macro {macro_id}"),
        kind: "macro_not_found".into(),
    }
}

/// A macro needs a name to be listed by and a body to send.
fn reject_invalid_definition(payload: &MacroDefinitionPayload) -> Option<CommandDecision> {
    if payload.name.trim().is_empty() || payload.body.trim().is_empty() {
        return Some(CommandDecision::Rejected {
            reason: "a macro needs a non-empty name and body".into(),
            kind: "invalid_macro".into(),
        });
    }
    None
}

fn definition_event(event_type: &str, payload: &MacroDefinitionPayload) -> CommandDecision {
    CommandDecision::Accepted {
        events: vec![EventSpec {
            event_type: event_type.into(),
            payload: serde_json::to_value(payload).expect("a macro definition serialises"),
        }],
    }
}

// --- commands ---

pub struct CreateMacro;

/// `specs/skilj-helpdesk.allium`'s `rule StaffCreatesMacro`: the company
/// must have signed up (expired is fine - expiry only stops new tickets),
/// and the id must be unused.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for CreateMacro {
    type Payload = MacroDefinitionPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CreateMacro";
    fn tag_mappings() -> Vec<TagMapping> {
        macro_event_tags()
    }
    /// The company's latest lifecycle event, and this macro id's latest
    /// event.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![company_status_query(), macro_query()]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        if company_status(matching_events, &payload.company_id).is_none() {
            return CommandDecision::Rejected {
                reason: format!("company {} has not signed up", payload.company_id),
                kind: "company_not_found".into(),
            };
        }
        if let Some(rejected) = reject_invalid_definition(payload) {
            return rejected;
        }
        if MacroFacts::of(matching_events, &payload.company_id, &payload.macro_id).used {
            return CommandDecision::Rejected {
                reason: format!("macro {} already exists", payload.macro_id),
                kind: "macro_already_exists".into(),
            };
        }
        definition_event("MacroCreated", payload)
    }
}

pub struct UpdateMacro;

/// `specs/skilj-helpdesk.allium`'s `rule StaffEditsMacro`: replaces the
/// whole definition of a macro that exists.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for UpdateMacro {
    type Payload = MacroDefinitionPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "UpdateMacro";
    fn tag_mappings() -> Vec<TagMapping> {
        macro_event_tags()
    }
    /// See `macro_query`.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![macro_query()]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        let facts = MacroFacts::of(matching_events, &payload.company_id, &payload.macro_id);
        if facts.current.is_none() {
            return macro_not_found(&payload.company_id, &payload.macro_id);
        }
        if let Some(rejected) = reject_invalid_definition(payload) {
            return rejected;
        }
        definition_event("MacroUpdated", payload)
    }
}

pub struct DeleteMacro;

/// `specs/skilj-helpdesk.allium`'s `rule StaffDeletesMacro`.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for DeleteMacro {
    type Payload = MacroDeletedPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "DeleteMacro";
    fn tag_mappings() -> Vec<TagMapping> {
        macro_event_tags()
    }
    /// See `macro_query`.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![macro_query()]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        let facts = MacroFacts::of(matching_events, &payload.company_id, &payload.macro_id);
        if facts.current.is_none() {
            return macro_not_found(&payload.company_id, &payload.macro_id);
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "MacroDeleted".into(),
                payload: serde_json::to_value(payload).expect("a macro deletion serialises"),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ApplyMacroPayload {
    pub ticket_id: String,
    /// The company whose macro this is - must be the ticket's own.
    pub company_id: String,
    pub macro_id: String,
    pub staff_id: String,
}

pub struct ApplyMacro;

/// `specs/skilj-helpdesk.allium`'s `rule StaffAppliesMacro`: one command,
/// up to three events, all or nothing - the reply, a priority change if
/// the macro sets a different one, and the status change if it sets one.
///
/// Each part keeps its own command's rule, by calling that command's own
/// decision rather than restating it: `waiting_on_customer` is
/// `RequestInfoFromCustomer` (and its event carries the reply),
/// `resolved` is `ResolveTicket` - both require `in_progress`. A reply or
/// a priority change on its own needs a ticket that isn't closed or
/// merged. If any part is rejected the whole command is.
///
/// The payload holds no free text: the reply comes from the macro, and
/// the requester it's encrypted for from the ticket's own history, so
/// this command, unlike `RequestInfoFromCustomer`, needs no
/// `sensitive_fields` of its own.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for ApplyMacro {
    type Payload = ApplyMacroPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "ApplyMacro";
    fn tag_mappings() -> Vec<TagMapping> {
        [ticket_tag(), macro_tag(), company_tag()].concat()
    }
    /// The ticket's `TicketSnapshot` state and the macro's latest event,
    /// in one query - see this module's doc comment for why that's the
    /// point of macros living in this context.
    fn consistency_query() -> Vec<QueryItemMapping> {
        [ticket_snapshot_query(), vec![macro_query()]].concat()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide_with(
        payload: &Self::Payload,
        decision: &Decision<'_, Self::Event>,
    ) -> CommandDecision {
        let found = MacroFacts::of(decision.events(), &payload.company_id, &payload.macro_id);
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts, &found)
        })
    }
}

impl ApplyMacro {
    fn decide_on(
        payload: &ApplyMacroPayload,
        facts: &TicketFacts,
        found: &MacroFacts,
    ) -> CommandDecision {
        let Some(status) = facts.status else {
            return CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            };
        };
        let company_id = facts.company_id();
        if company_id != payload.company_id {
            return CommandDecision::Rejected {
                reason: format!(
                    "ticket {} belongs to another company than {}",
                    payload.ticket_id, payload.company_id
                ),
                kind: "ticket_belongs_to_another_company".into(),
            };
        }
        let Some(definition) = &found.current else {
            return macro_not_found(&payload.company_id, &payload.macro_id);
        };
        if matches!(status, TicketStatus::Closed | TicketStatus::Merged) {
            return CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {status:?} - nothing more can be said on it",
                    payload.ticket_id
                ),
                kind: "ticket_not_replyable".into(),
            };
        }
        let requester_id = facts
            .requester_id
            .clone()
            .expect("a ticket with any status has a TicketCreated in its own history");
        let message = render_macro_body(&definition.body, payload);

        let mut events = Vec::new();
        if let Some(new_priority) = definition.priority {
            let previous_priority = facts
                .current_priority()
                .expect("a ticket with any status has a TicketCreated in its own history");
            if new_priority != previous_priority {
                events.push(EventSpec {
                    event_type: "TicketPriorityChanged".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "company_id": company_id,
                        "staff_id": payload.staff_id,
                        "previous_priority": previous_priority,
                        "new_priority": new_priority,
                        "macro_id": payload.macro_id,
                    }),
                });
            }
        }
        let staff_reply = || EventSpec {
            event_type: "TicketStaffReplied".into(),
            payload: serde_json::json!({
                "ticket_id": payload.ticket_id,
                "company_id": company_id,
                "staff_id": payload.staff_id,
                "message": message,
                "requester_id": requester_id,
                "macro_id": payload.macro_id,
            }),
        };
        let status_change = match definition.status {
            None => {
                events.push(staff_reply());
                return CommandDecision::Accepted { events };
            }
            // The reply *is* the question - one event, as
            // `RequestInfoFromCustomer` would have emitted it.
            Some(MacroStatus::WaitingOnCustomer) => RequestInfoFromCustomer::decide_on(
                &RequestInfoFromCustomerPayload {
                    ticket_id: payload.ticket_id.clone(),
                    staff_id: payload.staff_id.clone(),
                    message: message.clone(),
                    requester_id: Some(requester_id.clone()),
                },
                facts,
            ),
            Some(MacroStatus::Resolved) => {
                events.push(staff_reply());
                ResolveTicket::decide_on(
                    &ResolveTicketPayload {
                        ticket_id: payload.ticket_id.clone(),
                    },
                    facts,
                )
            }
        };
        match status_change {
            CommandDecision::Accepted { events: more } => {
                events.extend(more);
                CommandDecision::Accepted { events }
            }
            rejected => rejected,
        }
    }
}

/// A macro's body with its placeholders filled in for one application.
/// Only what the command can vouch for: the ticket and the staff member
/// applying it. An unknown `{{...}}` is left as written.
pub fn render_macro_body(body: &str, payload: &ApplyMacroPayload) -> String {
    body.replace("{{ticket_id}}", &payload.ticket_id)
        .replace("{{staff_id}}", &payload.staff_id)
}

// --- the staff macro library ---

/// One macro as the staff UI lists it. `status`/`priority` are plain
/// strings for the same GraphQL reason `TicketSummaryState::priority` is.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct MacroEntry {
    pub macro_id: String,
    pub name: String,
    pub body: String,
    pub status: Option<String>,
    pub priority: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct CompanyMacrosState {
    pub macros: std::collections::BTreeMap<String, MacroEntry>,
}

/// Keyed by `company_id`: every macro a company currently has, for staff
/// to pick from. Staff-only (`TEAM_ONLY`), like `TicketInternalNotes`:
/// a macro is internal tooling, and nothing a customer needs.
pub struct CompanyMacros;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for CompanyMacros {
    type State = CompanyMacrosState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CompanyMacros";
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    const TEAM_ONLY: Option<&'static str> = Some(STAFF_TEAM);
    fn consumed_event_types() -> Vec<&'static str> {
        MACRO_EVENTS.to_vec()
    }
    fn sync() -> bool {
        true
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        match event {
            HelpdeskEvent::MacroCreated(p) | HelpdeskEvent::MacroUpdated(p) => {
                vec![p.company_id.clone()]
            }
            HelpdeskEvent::MacroDeleted(p) => vec![p.company_id.clone()],
            _ => vec![],
        }
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        match event {
            HelpdeskEvent::MacroCreated(p) | HelpdeskEvent::MacroUpdated(p) => {
                state.macros.insert(
                    p.macro_id.clone(),
                    MacroEntry {
                        macro_id: p.macro_id.clone(),
                        name: p.name.clone(),
                        body: p.body.clone(),
                        status: p.status.map(|s| macro_status_str(s).to_string()),
                        priority: p.priority.map(|p| priority_str(p).to_string()),
                    },
                );
            }
            HelpdeskEvent::MacroDeleted(p) => {
                state.macros.remove(&p.macro_id);
            }
            _ => {}
        }
    }
}
