//! Ticket events, plus the `TicketFacts` fold and `TicketSnapshot`
//! every ticket command decides from.

use super::*;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketPriority {
    Low,
    Medium,
    High,
    Urgent,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketCreatedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub requester_id: String,
    pub logged_by_staff_id: Option<String>,
    pub title: String,
    pub description: String,
    pub priority: TicketPriority,
    /// Encrypted under the requester's key, like `title`/`description` -
    /// see `customer_fields`.
    pub requester_name: Option<String>,
    /// Encrypted under the requester's key - see `customer_fields`.
    pub requester_email: Option<String>,
    /// The SLA this ticket runs under: its company's policy for its
    /// priority, fixed at creation - see `crate::sla::TicketSla`. What
    /// `ScheduleFirstResponseSla`/`ScheduleResolutionSla` compute their
    /// deadlines from. `None` on events stored before the field existed.
    pub sla: Option<crate::sla::TicketSla>,
}

pub struct TicketCreated;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketCreated {
    type Payload = TicketCreatedPayload;
    const NAME: &'static str = "TicketCreated";
    /// Tagged on both, same technique as `skilj-demo`'s own
    /// `StudentEnrolled` (`courses.rs`): `AssignTicket`/`ResolveTicket`/
    /// `ReopenTicket` only ever need the "ticket" tag, but `CreateTicket`
    /// itself needs to see this company's own signup history too, so
    /// the creating event carries both tags up front.
    fn tag_mappings() -> Vec<TagMapping> {
        vec![
            TagMapping::new("ticket", "ticket_id"),
            TagMapping::new("company", "company_id"),
        ]
    }
    /// `src/bin/alerter.rs` reads this via an `EventReadToken` to track
    /// unhandled tickets - `EventType`'s own default (`false`) would
    /// 403 that read (docs/architecture.md §7.5): a type must opt in
    /// explicitly. Found the hard way, over a real REST request, once a
    /// real embedded Postgres was available in this sandbox to run
    /// against - see `tests/alerting_feed.rs`.
    fn event_read_allowed() -> bool {
        true
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        ticket_created_customer_fields()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketAssignedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub staff_id: String,
}

pub struct TicketAssigned;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketAssigned {
    type Payload = TicketAssignedPayload;
    const NAME: &'static str = "TicketAssigned";
    /// Also tagged with the agent, so `StaffWorkload` (and with it
    /// `AutoAssignTicket`'s query) sees the assignment - see that
    /// snapshot's doc comment. Only events appended since the tag was
    /// added carry it: skilj derives an event's tags once, when it is
    /// appended, and never re-tags stored events.
    fn tag_mappings() -> Vec<TagMapping> {
        [ticket_tag(), staff_tag("staff_id")].concat()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketResolvedPayload {
    pub ticket_id: String,
    pub company_id: String,
    /// Which resolution of this ticket this is: 1 for the first, 2 after
    /// one reopen, and so on (see `TicketFacts::resolutions`). It tags the
    /// auto-close deadline this schedules, so `CancelTicketAutoCloseOnReopen`
    /// can cancel exactly this resolution's deadline and no later one.
    /// `None` on events stored before the field existed.
    pub resolution: Option<u32>,
    /// The ticket's requester, copied from its `TicketCreated` so the
    /// auto-close deadline this schedules can name them (see
    /// `CloseTicketPayload::requester_id`). `None` on events stored
    /// before the field existed.
    pub requester_id: Option<String>,
    /// Who the ticket was assigned to (`TicketFacts::assignee_id`) - the
    /// agent this resolution takes a ticket off, tagged so their
    /// `StaffWorkload` sees it. `None` on events stored before the field
    /// existed.
    pub assignee_id: Option<String>,
}

pub struct TicketResolved;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketResolved {
    type Payload = TicketResolvedPayload;
    const NAME: &'static str = "TicketResolved";
    /// Also tagged with the assignee - see `assignee_id`.
    fn tag_mappings() -> Vec<TagMapping> {
        [ticket_tag(), staff_tag("assignee_id")].concat()
    }
    /// `src/bin/alerter.rs` reads this too - see
    /// `TicketCreated::event_read_allowed`'s own doc comment.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketReopenedPayload {
    pub ticket_id: String,
    pub company_id: String,
    /// The `TicketResolved::resolution` this reopen undoes - what
    /// `CancelTicketAutoCloseOnReopen` cancels by. `None` on events
    /// stored before the field existed.
    pub resolution: Option<u32>,
    /// The agent the reopened ticket goes back to - see
    /// `TicketResolvedPayload::assignee_id`. `None` on events stored
    /// before the field existed.
    pub assignee_id: Option<String>,
}

pub struct TicketReopened;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketReopened {
    type Payload = TicketReopenedPayload;
    const NAME: &'static str = "TicketReopened";
    /// Also tagged with the assignee - see `assignee_id`.
    fn tag_mappings() -> Vec<TagMapping> {
        [ticket_tag(), staff_tag("assignee_id")].concat()
    }
    /// `src/bin/alerter.rs` reads this too - see
    /// `TicketCreated::event_read_allowed`'s own doc comment.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketInfoRequestedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub staff_id: String,
    /// Encrypted under the requester's key: a question to the customer
    /// is part of their ticket's conversation.
    pub message: String,
    /// The ticket's requester, whose key `message` is encrypted under and
    /// whose `CustomerTickets` row it lands in. `None` on events stored
    /// before the field existed, whose message stays plaintext and is in
    /// no `CustomerTickets` row.
    pub requester_id: Option<String>,
}

pub struct TicketInfoRequested;

/// `specs/skilj-helpdesk.allium`'s `rule StaffRequestsInfo`'s own
/// outcome: `in_progress -> waiting_on_customer`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketInfoRequested {
    type Payload = TicketInfoRequestedPayload;
    const NAME: &'static str = "TicketInfoRequested";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["message"])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketCustomerRespondedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub requester_id: String,
    pub message: String,
}

pub struct TicketCustomerResponded;

/// `specs/skilj-helpdesk.allium`'s `rule CustomerReplies`'s own outcome:
/// `waiting_on_customer -> in_progress`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketCustomerResponded {
    type Payload = TicketCustomerRespondedPayload;
    const NAME: &'static str = "TicketCustomerResponded";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["message"])
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketClosedPayload {
    pub ticket_id: String,
    pub company_id: String,
}

pub struct TicketClosed;

/// `specs/skilj-helpdesk.allium`'s `rule TicketAutoCloses`: `resolved ->
/// closed`. "Auto" in the spec's own name refers to *who* decides
/// (nobody - a sweep, not a person), not to *how* the resulting state
/// change reaches skilj: `CloseTicket` below is an ordinary command, the
/// same as every other mutation in this file, submitted by skilj's own
/// native per-entity deadline mechanism (`ScheduleTicketAutoClose`
/// below, docs/architecture.md §46) rather than a customer or staff
/// member. See that reactor's own doc comment for why.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketClosed {
    type Payload = TicketClosedPayload;
    const NAME: &'static str = "TicketClosed";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    /// `src/bin/alerter.rs` reads this too - see
    /// `TicketCreated::event_read_allowed`'s own doc comment.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketEscalatedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub previous_priority: TicketPriority,
    pub new_priority: TicketPriority,
}

pub struct TicketEscalated;

/// A deliberate, documented extension of `specs/skilj-helpdesk.allium`'s
/// `rule TicketBecomesOverdue` - see that rule's own updated `@guidance`
/// note for why this pass turns "page a lead" into a real persisted
/// priority bump, not just a console alert. Submitted by
/// `src/bin/alerter.rs`'s own overdue sweep, the same "a background
/// binary submits an ordinary command" treatment `TicketClosed` gets
/// from `ScheduleTicketAutoClose` (below) and `CompanyActivated` gets
/// from `ScheduleCompanyTrialConversion`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketEscalated {
    type Payload = TicketEscalatedPayload;
    const NAME: &'static str = "TicketEscalated";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    /// `src/bin/alerter.rs` reads this itself (own output, consumed back)
    /// to stop re-submitting `EscalateTicket` for a ticket it (or another
    /// alerter instance) already escalated - see
    /// `TicketCreated::event_read_allowed`'s own doc comment for the
    /// general pattern.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketsMergedPayload {
    pub primary_ticket_id: String,
    pub duplicate_ticket_id: String,
    pub company_id: String,
    /// Who the duplicate was assigned to, if anyone: merging it away
    /// takes it off that agent's `StaffWorkload`. `None` for an
    /// unassigned duplicate, and on events stored before the field
    /// existed.
    pub duplicate_assignee_id: Option<String>,
}

pub struct TicketsMerged;

/// A showcase of skilj's own DCB model, not in the original spec: two
/// tickets, one event, no aggregate boundary needed - see
/// `MergeTickets::tag_mappings` below for the command side of the same
/// trick. Tagged on *both* ticket ids (two `TagMapping` entries under
/// the same `"ticket"` key), so any later command against either ticket
/// sees this in its own `matching_events`.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketsMerged {
    type Payload = TicketsMergedPayload;
    const NAME: &'static str = "TicketsMerged";
    fn tag_mappings() -> Vec<TagMapping> {
        vec![
            TagMapping::new("ticket", "primary_ticket_id"),
            TagMapping::new("ticket", "duplicate_ticket_id"),
            // The duplicate's agent - see `duplicate_assignee_id`.
            TagMapping::new(STAFF_TAG_KEY, "duplicate_assignee_id"),
        ]
    }
    /// `src/bin/alerter.rs` reads this to stop tracking the duplicate
    /// ticket as unhandled once merged away - see
    /// `TicketCreated::event_read_allowed`'s own doc comment.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketRatedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub rating: u8,
    /// Encrypted under the requester's key.
    pub comment: Option<String>,
    /// The customer who rated, whose key `comment` is encrypted under -
    /// see `TicketInfoRequestedPayload::requester_id`.
    pub requester_id: Option<String>,
}

pub struct TicketRated;

/// Not in the original spec - a CSAT survey response, standard practice
/// once a ticket is resolved (Zendesk, Freshdesk).
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketRated {
    type Payload = TicketRatedPayload;
    const NAME: &'static str = "TicketRated";
    fn tag_mappings() -> Vec<TagMapping> {
        ticket_tag()
    }
    fn sensitive_fields() -> Vec<SensitiveField> {
        customer_fields(&["comment"])
    }
    /// `src/csat_metrics.rs` reads this via an `EventReadToken` to
    /// record the rating *value* as a real metric - see
    /// `TicketCreated::event_read_allowed`'s own doc comment for why
    /// this default needs an explicit override. Everything else about
    /// a rating (who gave it, the comment) still only ever goes through
    /// GraphQL/`get_projection_state`, same as before this existed.
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketInternalNoteAddedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub staff_id: String,
    pub note: String,
}

pub struct TicketInternalNoteAdded;

/// Not in the original spec - a staff-only note. Deliberately never
/// folded into `CompanyActiveTickets`/`CustomerTickets`/`TicketSummary`
/// (see this file's own module doc comment on why keeping it structurally
/// separate, rather than tagging entries "internal" for the frontend to
/// filter, is what keeps it from customers).
///
/// `private_fields()` below closes the same admin-tooling surface
/// `AddInternalNote`'s own doc comment describes for the command side -
/// skilj's generic `queryEvents`/`countEvents`/`inspectEvent`
/// (`AdminAccess`-gated, `skilj-core::event_store::query_events`/
/// `inspect_event`) check only `access_mapping.level == Admin`, never
/// `event_read_allowed` (that flag only gates the *REST* feed's
/// `EventReadToken` path - `fetch_events`/`consume_events` - a
/// different function entirely). So even though this event type has no
/// `event_read_allowed() = true` override and is therefore unreachable
/// over REST, it was still fully readable in cleartext through the
/// GraphQL admin surface without this declaration - found in review,
/// not assumed from the REST-side block alone.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for TicketInternalNoteAdded {
    type Payload = TicketInternalNoteAddedPayload;
    const NAME: &'static str = "TicketInternalNoteAdded";
    /// Tagged on both, same reasoning as `TicketCreated`'s own doc
    /// comment: `TicketInternalNotes`'s own `OWNER_TAG_KEY` (see that
    /// projection's own doc comment) needs a "company" tag on *some*
    /// consuming event to derive an owner from, and this is the only
    /// one it consumes at all - `TicketCreated`'s own "company" tag
    /// alone isn't enough here, since `TicketSummary`/`CompanyActiveTickets`
    /// consume `TicketCreated` but `TicketInternalNotes` deliberately
    /// doesn't (see this file's own module doc comment on why).
    fn tag_mappings() -> Vec<TagMapping> {
        vec![
            TagMapping::new("ticket", "ticket_id"),
            TagMapping::new("company", "company_id"),
        ]
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
}

/// `specs/skilj-helpdesk.allium`'s `Ticket.status`, plus `Merged` - not
/// in the original spec, `MergeTickets`'s own outcome for the duplicate
/// side of a merge (see that command's doc comment). Every *other*
/// command's own status match already ends on a catch-all `Some(other)
/// => Rejected{..}` arm, so adding this variant needed no changes
/// anywhere else - verified by reading each one, not assumed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum TicketStatus {
    Open,
    InProgress,
    WaitingOnCustomer,
    Resolved,
    Closed,
    Merged,
}

/// Everything a ticket command's `decide()` reads from one ticket's
/// history, folded in one pass - same technique as `banking.rs`'s
/// `balance_of`/`courses.rs`'s roster folds, gathered into one struct so
/// it can also be `TicketSnapshot`'s stored state (issue #14). Every
/// single-ticket command, and `MergeTickets`, decides from this alone,
/// handed over by skilj as a snapshot item's state (skilj
/// docs/architecture.md §202).
///
/// Changing what this folds, or its shape, means bumping
/// `TicketSnapshot::VERSION`.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct TicketFacts {
    /// Which ticket this is - set up front by `of`, or by
    /// `TicketSnapshot::fold_for` from the tag value it folds for.
    /// Needed for `TicketsMerged`, the one event tagged with two tickets.
    pub ticket_id: String,
    /// `None` means the ticket doesn't exist (no `TicketCreated` found).
    pub status: Option<TicketStatus>,
    /// Off its `TicketCreated` - see `company_id`.
    pub company_id: Option<String>,
    /// Off its `TicketCreated`. Safe to copy out of history, unlike
    /// `requester_name`/`requester_email`: those are stored, and so seen
    /// here, as ciphertext, and copying them into a new sensitive field
    /// would encrypt them a second time.
    pub requester_id: Option<String>,
    /// The priority it was created with.
    pub created_priority: Option<TicketPriority>,
    /// Its priority now, once something has moved it off
    /// `created_priority` (`TicketEscalated`, `TicketPriorityChanged`) -
    /// read through `current_priority`. Defaulted so a state stored
    /// before the field existed still reads; `TicketSnapshot::VERSION`
    /// was bumped with it, so such a state is refolded anyway.
    #[serde(default)]
    pub priority: Option<TicketPriority>,
    /// How many times it has been resolved so far. Counts the events
    /// rather than reading `TicketResolved::resolution`, so a history that
    /// predates that field still numbers its next resolution correctly.
    pub resolutions: u32,
    pub escalated: bool,
    pub rated: bool,
    /// Whether staff have answered the customer yet - a `TicketInfoRequested`,
    /// a `TicketStaffReplied` (a macro's reply) or a `TicketResolved`. What meets the first-response SLA (see
    /// `ScheduleFirstResponseSla`).
    pub first_responded: bool,
    /// Whether a `TicketSlaBreached` already recorded each SLA's breach.
    pub first_response_breached: bool,
    pub resolution_breached: bool,
    /// Off its `TicketAssigned` - who works the ticket. Stamped onto the
    /// events that change that agent's load (`TicketResolved`,
    /// `TicketReopened`, `TicketsMerged`), so `StaffWorkload` sees them.
    pub assignee_id: Option<String>,
}

impl TicketFacts {
    /// Folded from `ticket_id`'s slice of `matching_events`.
    pub fn of(matching_events: &[HelpdeskEvent], ticket_id: &str) -> Self {
        let mut facts = Self {
            ticket_id: ticket_id.to_string(),
            ..Self::default()
        };
        for event in matching_events {
            facts.apply(event);
        }
        facts
    }

    fn apply(&mut self, event: &HelpdeskEvent) {
        match event {
            HelpdeskEvent::TicketCreated(p) if p.ticket_id == self.ticket_id => {
                self.ticket_id = p.ticket_id.clone();
                self.status = Some(TicketStatus::Open);
                self.company_id.get_or_insert_with(|| p.company_id.clone());
                self.requester_id
                    .get_or_insert_with(|| p.requester_id.clone());
                self.created_priority.get_or_insert(p.priority);
            }
            HelpdeskEvent::TicketAssigned(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::InProgress);
                self.assignee_id = Some(p.staff_id.clone());
            }
            HelpdeskEvent::TicketResolved(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::Resolved);
                self.resolutions += 1;
                self.first_responded = true;
            }
            HelpdeskEvent::TicketReopened(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::InProgress);
            }
            HelpdeskEvent::TicketInfoRequested(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::WaitingOnCustomer);
                self.first_responded = true;
            }
            HelpdeskEvent::TicketCustomerResponded(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::InProgress);
            }
            HelpdeskEvent::TicketClosed(p) if p.ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::Closed);
            }
            // Only the *duplicate* side becomes Merged - the primary's
            // own status is untouched by a merge (see `MergeTickets`'s
            // own doc comment), so this only ever matches
            // `duplicate_ticket_id`, never `primary_ticket_id`.
            HelpdeskEvent::TicketsMerged(p) if p.duplicate_ticket_id == self.ticket_id => {
                self.status = Some(TicketStatus::Merged);
            }
            HelpdeskEvent::TicketEscalated(p) if p.ticket_id == self.ticket_id => {
                self.escalated = true;
                self.priority = Some(p.new_priority);
            }
            HelpdeskEvent::TicketPriorityChanged(p) if p.ticket_id == self.ticket_id => {
                self.priority = Some(p.new_priority);
            }
            // A macro's plain reply answers the customer as much as a
            // question does.
            HelpdeskEvent::TicketStaffReplied(p) if p.ticket_id == self.ticket_id => {
                self.first_responded = true;
            }
            HelpdeskEvent::TicketRated(p) if p.ticket_id == self.ticket_id => {
                self.rated = true;
            }
            HelpdeskEvent::TicketSlaBreached(p) if p.ticket_id == self.ticket_id => match p.sla {
                crate::sla::SlaKind::FirstResponse => self.first_response_breached = true,
                crate::sla::SlaKind::Resolution => self.resolution_breached = true,
            },
            _ => {}
        }
    }

    /// What `EscalateTicket` bumps and `ApplyMacro` compares against:
    /// the last priority set, else the one it was created with.
    pub fn current_priority(&self) -> Option<TicketPriority> {
        self.priority.or(self.created_priority)
    }

    /// The company a ticket belongs to. Every ticket-lifecycle command
    /// past creation uses this to stamp `company_id` onto the event it
    /// emits, which is what lets `CompanyActiveTickets` below fold every
    /// ticket event for one ticket into the correct per-company projection
    /// instance - `AssignTicket`/`ResolveTicket`/etc.'s own payloads never
    /// carried `company_id` as caller input (there's no reason to trust a
    /// caller-supplied one when the real answer is already in the ticket's
    /// own history). Only called once `status` is known to be `Some`.
    pub(super) fn company_id(&self) -> &str {
        self.company_id
            .as_deref()
            .expect("a ticket with any status has a TicketCreated in its own history")
    }

    /// Rejects a command whose `requester_id` isn't this ticket's
    /// requester.
    ///
    /// The command's free text is encrypted under the key its own
    /// `requester_id` names, so a wrong one would file the text under
    /// another customer: readable to them, and out of reach of the real
    /// customer's erasure. Only meaningful once the ticket is known to
    /// exist.
    pub(super) fn reject_unless_requester(
        &self,
        requester_id: Option<&str>,
    ) -> Option<CommandDecision> {
        if requester_id.is_some() && requester_id == self.requester_id.as_deref() {
            return None;
        }
        Some(CommandDecision::Rejected {
            reason: format!(
                "requester_id {requester_id:?} is not ticket {}'s requester",
                self.ticket_id
            ),
            kind: "requester_mismatch".into(),
        })
    }
}

/// `TicketFacts`, kept per ticket - lets every single-ticket command
/// decide from a stored state plus the events since, instead of reading
/// the ticket's whole history each time (issue #14). Read through a
/// snapshot item (`ticket_snapshot_query`), also twice by `MergeTickets`,
/// once per ticket. See `docs/ticket-snapshot-report-2026-10-05.md`.
pub struct TicketSnapshot;

#[auto_register(BOUNDED_CONTEXT)]
impl Snapshot for TicketSnapshot {
    type State = TicketFacts;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "TicketSnapshot";
    const TAG_KEY: &'static str = "ticket";
    /// Same owner every ticket projection derives - a company-scoped
    /// Role can only `inspectSnapshot` its own company's tickets.
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    /// 2: `first_responded`/`*_breached` for the SLA deadlines (issue #42).
    /// 3: `TicketFacts::assignee_id` (issue #44).
    /// 4: `TicketFacts::priority` (issue #43's `TicketPriorityChanged`).
    const VERSION: u64 = 4;
    /// Keyed by ticket, so unlike `CompanyActiveTickets` the work spreads
    /// over every partition - see that projection's `PARTITION_COUNT`
    /// and `docs/partitioned-projection-report-2026-10-05.md`.
    const PARTITION_COUNT: u32 = 4;
    /// A `TicketsMerged` carries both tickets, and is folded once for
    /// each: `tag_value` says which side this state is.
    fn fold_for(state: &mut Self::State, tag_value: &str, event: &Self::Event) {
        if state.ticket_id.is_empty() {
            state.ticket_id = tag_value.to_string();
        }
        state.apply(event);
    }
}

/// `ticket_id`'s `TicketFacts` from `decision`'s snapshot items, then
/// `decide`. skilj hands the default state for a ticket with no events,
/// so a missing or unreadable state is a bug, reported as a rejection
/// rather than as a wrong decision.
pub(super) fn decide_on_ticket(
    decision: &Decision<'_, HelpdeskEvent>,
    ticket_id: &str,
    decide: impl FnOnce(&TicketFacts) -> CommandDecision,
) -> CommandDecision {
    match decision.snapshot::<TicketSnapshot>(ticket_id) {
        Some(mut facts) => {
            // The default state, for a ticket no event has been folded for.
            if facts.ticket_id.is_empty() {
                facts.ticket_id = ticket_id.to_string();
            }
            decide(&facts)
        }
        None => CommandDecision::Rejected {
            reason: format!("no readable TicketSnapshot state for ticket {ticket_id}"),
            kind: "snapshot_unreadable".into(),
        },
    }
}

/// The one-tier priority bump `EscalateTicket` applies - covered by that
/// command's own integration test (this file has no unit-test module of
/// its own; every other pure fold here is proven the same way, through
/// the REST surface). Clamped at `Urgent` rather than wrapping or
/// erroring: escalating an already-urgent ticket a second time is
/// rejected before this is ever called (see `EscalateTicket`'s own
/// `already_escalated` guard), but clamping here too means this
/// function is total and never needs to fail on its own account.
pub(super) fn escalate_priority(priority: TicketPriority) -> TicketPriority {
    match priority {
        TicketPriority::Low => TicketPriority::Medium,
        TicketPriority::Medium => TicketPriority::High,
        TicketPriority::High | TicketPriority::Urgent => TicketPriority::Urgent,
    }
}
