//! The ticket read models: `TicketSummary`, the frontend's two eager
//! reads (`CompanyActiveTickets`/`CustomerTickets`) and the staff-only
//! `TicketInternalNotes`.

use super::*;

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct TicketSummaryState {
    pub status: Option<String>,
    /// A plain string, not `Option<TicketPriority>` - found the hard
    /// way, over a real GraphQL request (`tests/graphql.rs`): a
    /// `schemars`-derived enum's JSON Schema shape doesn't match what
    /// `skilj-graphql`'s mapper recognises as a scalar, so it falls
    /// back to its own documented behaviour (docs/architecture.md
    /// §5.1) - an opaque, *double*-JSON-encoded string
    /// (`"\"urgent\""`, not `"urgent"`). Not a bug to work around at
    /// the GraphQL layer; a plain string field here is simply the
    /// right shape for a read-model a GraphQL client will actually
    /// query.
    pub priority: Option<String>,
    pub assigned_staff_id: Option<String>,
    /// Set once by `TicketEscalated` (see that event's own doc comment),
    /// never cleared - same "once escalated, stays escalated" treatment
    /// `EscalateTicket`'s own `already_escalated` guard already gives it.
    pub escalated: bool,
    /// Set once by `TicketRated` - unlike `TicketInternalNoteAdded`
    /// (deliberately kept out of every projection, see that event's own
    /// doc comment), a CSAT rating has no customer-visibility concern:
    /// the customer who left it, and any staff member, both already see
    /// this fine either way, so folding it in here is a real UX need
    /// (the frontend needs to know a ticket's already been rated so it
    /// doesn't keep showing the rating form), not scope creep.
    pub rating: Option<u8>,
    /// The staff member whose `TicketInfoRequested` was the first reply
    /// the customer got - set once, by the first one, and never moved by
    /// a later round of the back-and-forth. Who answered first is not
    /// when: `project` sees an event's payload, not when it was stored,
    /// so a first-response *time* has nothing here to be derived from.
    ///
    /// Added after tickets already existed, which is what a rebuild is
    /// for (README's "Changing a projection: zero-downtime rebuilds"):
    /// until one is run, a ticket answered before the deploy stays at
    /// `None`, or names whoever answered *after* it instead of first.
    /// `tests/projection_rebuild.rs` walks both through to the switch-over.
    pub first_responder_staff_id: Option<String>,
}

fn priority_str(priority: TicketPriority) -> &'static str {
    match priority {
        TicketPriority::Low => "low",
        TicketPriority::Medium => "medium",
        TicketPriority::High => "high",
        TicketPriority::Urgent => "urgent",
    }
}

/// Keyed by `ticket_id`. `specs/skilj-helpdesk.allium`'s `unhandled`
/// derived field is `status not in {resolved, closed}` - not stored
/// here directly; a caller derives it from `status`.
pub struct TicketSummary;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for TicketSummary {
    type State = TicketSummaryState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "TicketSummary";
    /// See `TicketInternalNotes`'s own doc comment for the vulnerability
    /// this closes and what it doesn't - `TicketCreated`'s own "company"
    /// tag (already there for `CreateTicket`'s own consistency check) is
    /// what an instance's owner derives from here; every other event
    /// this projection consumes only tags "ticket", so an owner once
    /// established at creation is never touched again by anything else,
    /// exactly the "an event lacking the tag leaves it untouched"
    /// behaviour `docs/architecture.md` §23 (in the sibling `skilj`
    /// repo) describes.
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    fn consumed_event_types() -> Vec<&'static str> {
        vec![
            "TicketCreated",
            "TicketAssigned",
            "TicketResolved",
            "TicketReopened",
            "TicketInfoRequested",
            "TicketCustomerResponded",
            "TicketClosed",
            "TicketEscalated",
            "TicketsMerged",
            "TicketRated",
        ]
    }
    fn sync() -> bool {
        true
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        match event {
            HelpdeskEvent::CompanySignedUp(_)
            | HelpdeskEvent::CompanyActivated(_)
            | HelpdeskEvent::CompanyExpired(_)
            | HelpdeskEvent::CompanyTenantProvisioned(_)
            // Deliberately absent from `consumed_event_types()` above
            // (see that event's own doc comment) - listed here only
            // because `keys`/`project` take `&HelpdeskEvent`
            // unconditionally, so the match has to stay exhaustive over
            // every variant even ones this projection never actually
            // gets invoked for.
            | HelpdeskEvent::CompanyLifecycleMirrored(_)
            | HelpdeskEvent::TicketInternalNoteAdded(_) => vec![],
            HelpdeskEvent::TicketCreated(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketAssigned(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketResolved(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketReopened(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketInfoRequested(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketCustomerResponded(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketClosed(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketEscalated(p) => vec![p.ticket_id.clone()],
            HelpdeskEvent::TicketRated(p) => vec![p.ticket_id.clone()],
            // Fans out to *both* tickets - unlike every other event here,
            // one `TicketsMerged` updates two projection instances. See
            // `project`'s own handling of the two keys below.
            HelpdeskEvent::TicketsMerged(p) => {
                vec![p.primary_ticket_id.clone(), p.duplicate_ticket_id.clone()]
            }
        }
    }
    fn project(state: &mut Self::State, event: &Self::Event, key: &str) {
        match event {
            HelpdeskEvent::CompanySignedUp(_)
            | HelpdeskEvent::CompanyActivated(_)
            | HelpdeskEvent::CompanyExpired(_)
            | HelpdeskEvent::CompanyTenantProvisioned(_)
            // A lifecycle mirror, not a ticket fact: this projection is
            // keyed by `ticket_id` and a mirror carries no ticket at all,
            // so there is nothing here for it to contribute. Listed
            // alongside the other company-lifecycle events above for the
            // same reason they are - exhaustiveness, not intent.
            | HelpdeskEvent::CompanyLifecycleMirrored(_)
            | HelpdeskEvent::TicketInternalNoteAdded(_) => {}
            HelpdeskEvent::TicketCreated(p) => {
                state.status = Some("open".into());
                state.priority = Some(priority_str(p.priority).to_string());
            }
            HelpdeskEvent::TicketAssigned(p) => {
                state.status = Some("in_progress".into());
                state.assigned_staff_id = Some(p.staff_id.clone());
            }
            HelpdeskEvent::TicketResolved(_) => {
                state.status = Some("resolved".into());
            }
            HelpdeskEvent::TicketReopened(_) => {
                state.status = Some("in_progress".into());
            }
            HelpdeskEvent::TicketInfoRequested(p) => {
                state.status = Some("waiting_on_customer".into());
                state
                    .first_responder_staff_id
                    .get_or_insert_with(|| p.staff_id.clone());
            }
            HelpdeskEvent::TicketCustomerResponded(_) => {
                state.status = Some("in_progress".into());
            }
            HelpdeskEvent::TicketClosed(_) => {
                state.status = Some("closed".into());
            }
            HelpdeskEvent::TicketEscalated(p) => {
                state.priority = Some(priority_str(p.new_priority).to_string());
                state.escalated = true;
            }
            HelpdeskEvent::TicketRated(p) => {
                state.rating = Some(p.rating);
            }
            // Only the duplicate's own instance (this projection is
            // keyed per-ticket, so `key` tells the two fanned-out calls
            // apart - see `keys` above) becomes "merged"; the primary's
            // own instance is untouched, matching `TicketFacts`'s own
            // treatment in this file's command-decision helpers.
            HelpdeskEvent::TicketsMerged(p) => {
                if key == p.duplicate_ticket_id {
                    state.status = Some("merged".into());
                }
            }
        }
    }
}

// --- the frontend's two ticket reads ---

/// One turn of the `StaffRequestsInfo`/`CustomerReplies` back-and-forth
/// (`rule StaffRequestsInfo`/`CustomerReplies` in the spec) - the actual
/// conversation those two rules previously carried no content for.
/// Nothing stops the cycle repeating (assign → ask → reply → ask again),
/// so this accumulates across as many rounds as actually happen, not
/// just one.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketMessage {
    pub author_id: String,
    pub from_staff: bool,
    pub text: String,
}

/// A ticket's lifecycle, with nothing its customer wrote - see
/// `CompanyActiveTickets`.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct TicketQueueEntry {
    pub ticket_id: String,
    pub status: String,
    pub priority: String,
    pub requester_id: String,
    pub assigned_staff_id: Option<String>,
    /// See `TicketSummaryState::escalated`/`::rating`'s own doc comments -
    /// identical reasoning, mirrored here since `frontend/` reads this
    /// projection, not `TicketSummary`.
    pub escalated: bool,
    pub rating: Option<u8>,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct CompanyActiveTicketsState {
    pub tickets: std::collections::HashMap<String, TicketQueueEntry>,
}

/// Keyed by `company_id`: every ticket of one company that can still
/// change, for the staff queue and for the customer view's status column.
///
/// A ticket leaves once nothing can happen to it any more: a merged
/// duplicate right away, a closed ticket once it carries a rating
/// (`CustomerRatesTicket` still accepts a closed, unrated ticket, and
/// this is the read the customer rates from). Without that, one row held
/// every ticket a company ever had, and skilj rewrites the whole row on
/// every fold - see `docs/partitioned-projection-report-2026-10-05.md`.
/// A closed ticket nobody rates still stays.
///
/// Holds no customer-written text, on purpose. That text is encrypted
/// under each customer's own key (`customer_fields`), and skilj decrypts
/// a projection row only against the row's own key - a company here - so
/// any of it folded in would read as ciphertext to everyone. It lives in
/// `CustomerTickets` instead; the frontend joins the two by `ticket_id`.
///
/// Replaces `CompanyTicketList`, which held that text in plaintext. A
/// registered projection can't lose a field (skilj's schema compatibility
/// rule), so this is a new projection rather than a slimmed-down old one;
/// the old one's stored state is no longer registered or updated. It was
/// `CompanyTicketQueue` before tickets started leaving it, renamed rather
/// than kept so a fresh fold from history drops the finished tickets
/// already stored - the schema didn't change, so skilj would not have
/// rebuilt it on its own.
///
/// What a customer still sees through this: the ids, statuses and
/// requester ids of their company's other tickets, never their content.
///
/// Async, unlike every other projection here (issue #15): it's the one
/// read whose key (a company) every ticket event of that company
/// contends on - the obvious candidate for `PARTITION_COUNT`. The cost
/// is read-your-writes: a ticket just created can be missing from this
/// queue for up to one `async_projection_poll_interval`, which is why
/// the frontend's dashboard reads it with `waitForSequence` set to its
/// own last write's sequence (`AfterWrite` in `dashboard.rs`). Issue #12
/// measured the trade: ~7% more accepted commands/s at saturation, for
/// a median lag of ~20 events - see
/// `docs/async-projection-report-2026-10-06.md`.
pub struct CompanyActiveTickets;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for CompanyActiveTickets {
    type State = CompanyActiveTicketsState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CompanyActiveTickets";
    /// See `TicketSummary`'s own doc comment - identical fix, identical
    /// reasoning. Keyed by `company_id` itself here (unlike
    /// `TicketSummary`'s `ticket_id`), so the derived owner ends up
    /// equal to the key for every instance - a degenerate but correct
    /// case of the same general mechanism, not special-cased.
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    fn consumed_event_types() -> Vec<&'static str> {
        vec![
            "TicketCreated",
            "TicketAssigned",
            "TicketResolved",
            "TicketReopened",
            "TicketInfoRequested",
            "TicketCustomerResponded",
            "TicketClosed",
            "TicketEscalated",
            "TicketsMerged",
            "TicketRated",
        ]
    }
    /// Lets up to this many `server` instances sharing one database
    /// fold disjoint slices of the queue concurrently, instead of each
    /// redoing all of it. Only helps with more than one instance, and
    /// only up to the number of companies in a context: keys are
    /// companies, so a per-company tenant context (one key) always
    /// lands in a single partition. Rebuilds stay single-instance
    /// regardless. See `docs/partitioned-projection-report-2026-10-05.md`.
    const PARTITION_COUNT: u32 = 4;
    fn keys(event: &Self::Event) -> Vec<String> {
        match event {
            HelpdeskEvent::CompanySignedUp(_)
            | HelpdeskEvent::CompanyActivated(_)
            | HelpdeskEvent::CompanyExpired(_)
            | HelpdeskEvent::CompanyTenantProvisioned(_)
            // Deliberately absent from `consumed_event_types()` above -
            // see that event's own doc comment, and `TicketSummary::keys`'s
            // own identical comment for why the match still needs this
            // arm regardless.
            //
            // Unlike `TicketSummary::keys`, this projection *could*
            // return a meaningful key here (a mirror carries the
            // `company_id` this projection is keyed by). It still must
            // not: `keys` only decides which instances to touch, and
            // `consumed_event_types()` is what decides whether `project`
            // runs at all - returning a key for an unconsumed event
            // would fold a lifecycle fact into a ticket queue under the
            // "an event lacking the tag leaves it untouched" contract
            // this projection otherwise keeps.
            | HelpdeskEvent::CompanyLifecycleMirrored(_)
            | HelpdeskEvent::TicketInternalNoteAdded(_) => vec![],
            HelpdeskEvent::TicketCreated(p) => vec![p.company_id.clone()],
            // Every ticket-lifecycle event past creation now carries its
            // own `company_id` too (stamped by each command's own
            // `decide()` via `TicketFacts::company_id` - see that
            // function's doc comment) precisely so this projection's
            // instance key is always the real company, not a stand-in.
            HelpdeskEvent::TicketAssigned(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketResolved(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketReopened(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketInfoRequested(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketCustomerResponded(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketClosed(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketEscalated(p) => vec![p.company_id.clone()],
            HelpdeskEvent::TicketRated(p) => vec![p.company_id.clone()],
            // Unlike `TicketSummary` (keyed per-ticket, so it fans this
            // out to two instances), this projection is keyed per
            // *company* - both tickets already belong to the same one
            // (`MergeTickets`'s own `tickets_belong_to_different_companies`
            // guard), so one key here is correct; `project` below reaches
            // into `state.tickets` for the specific duplicate ticket_id.
            HelpdeskEvent::TicketsMerged(p) => vec![p.company_id.clone()],
        }
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        let mut set_status = |ticket_id: &str, status: &str| {
            if let Some(entry) = state.tickets.get_mut(ticket_id) {
                entry.status = status.into();
            }
        };
        match event {
            HelpdeskEvent::CompanySignedUp(_)
            | HelpdeskEvent::CompanyActivated(_)
            | HelpdeskEvent::CompanyExpired(_)
            | HelpdeskEvent::CompanyTenantProvisioned(_)
            // A lifecycle mirror carries no ticket - see
            // `CompanyActiveTickets::keys`'s own comment on the same event.
            | HelpdeskEvent::CompanyLifecycleMirrored(_)
            | HelpdeskEvent::TicketInternalNoteAdded(_) => {}
            HelpdeskEvent::TicketCreated(p) => {
                state.tickets.insert(
                    p.ticket_id.clone(),
                    TicketQueueEntry {
                        ticket_id: p.ticket_id.clone(),
                        status: "open".into(),
                        priority: priority_str(p.priority).to_string(),
                        requester_id: p.requester_id.clone(),
                        assigned_staff_id: None,
                        escalated: false,
                        rating: None,
                    },
                );
            }
            HelpdeskEvent::TicketAssigned(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.status = "in_progress".into();
                    entry.assigned_staff_id = Some(p.staff_id.clone());
                }
            }
            HelpdeskEvent::TicketResolved(p) => set_status(&p.ticket_id, "resolved"),
            HelpdeskEvent::TicketReopened(p) => set_status(&p.ticket_id, "in_progress"),
            HelpdeskEvent::TicketInfoRequested(p) => {
                set_status(&p.ticket_id, "waiting_on_customer")
            }
            HelpdeskEvent::TicketCustomerResponded(p) => set_status(&p.ticket_id, "in_progress"),
            HelpdeskEvent::TicketClosed(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    if entry.rating.is_some() {
                        state.tickets.remove(&p.ticket_id);
                    } else {
                        entry.status = "closed".into();
                    }
                }
            }
            HelpdeskEvent::TicketsMerged(p) => {
                state.tickets.remove(&p.duplicate_ticket_id);
            }
            HelpdeskEvent::TicketEscalated(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.priority = priority_str(p.new_priority).to_string();
                    entry.escalated = true;
                }
            }
            HelpdeskEvent::TicketRated(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    if entry.status == "closed" {
                        state.tickets.remove(&p.ticket_id);
                    } else {
                        entry.rating = Some(p.rating);
                    }
                }
            }
        }
    }
}

/// What one customer wrote, or was asked, on one ticket - see
/// `CustomerTickets`. Every string here is held encrypted under that
/// customer's key.
#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct CustomerTicketContent {
    pub ticket_id: String,
    pub title: String,
    pub description: String,
    pub messages: Vec<TicketMessage>,
    pub rating_comment: Option<String>,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct CustomerTicketsState {
    pub tickets: std::collections::HashMap<String, CustomerTicketContent>,
}

/// Keyed by `requester_id`: the content of every ticket one customer
/// filed - title, description, the conversation and their rating comment.
///
/// Keyed by the customer because that's what makes it readable. Each
/// field is folded in as the ciphertext it's stored as (`customer_fields`),
/// and skilj decrypts a projection row only when the caller may read the
/// row's own key's data: with `can_read_sensitive` (staff, see
/// `server.rs`), or as the subject itself, which the customer is - their
/// IdP subject is their `requester_id`. Another customer of the same
/// company can read the row (`OWNER_TAG_KEY` scopes it to the company, no
/// further) but only ever sees ciphertext. And once the customer is
/// forgotten (`CUSTOMER_SUBJECT`), nobody can decrypt it any more.
///
/// Only the four events that carry customer text are consumed; status and
/// the rest come from `CompanyActiveTickets`. Events stored before
/// `TicketInfoRequested`/`TicketRated` carried a `requester_id` are
/// skipped - their plaintext isn't erasable either way.
pub struct CustomerTickets;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for CustomerTickets {
    type State = CustomerTicketsState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "CustomerTickets";
    /// `TicketCreated`'s "company" tag sets the owner, as for
    /// `TicketSummary`. A customer belongs to one company
    /// (`specs/skilj-helpdesk.allium`'s `entity Customer`), so their row
    /// has one owner.
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    fn consumed_event_types() -> Vec<&'static str> {
        vec![
            "TicketCreated",
            "TicketInfoRequested",
            "TicketCustomerResponded",
            "TicketRated",
        ]
    }
    fn sync() -> bool {
        true
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        match event {
            HelpdeskEvent::TicketCreated(p) => vec![p.requester_id.clone()],
            HelpdeskEvent::TicketCustomerResponded(p) => vec![p.requester_id.clone()],
            HelpdeskEvent::TicketInfoRequested(p) => p.requester_id.iter().cloned().collect(),
            HelpdeskEvent::TicketRated(p) => p.requester_id.iter().cloned().collect(),
            // Not consumed - see `TicketSummary::keys` on why the match
            // still has to name them.
            HelpdeskEvent::CompanySignedUp(_)
            | HelpdeskEvent::CompanyActivated(_)
            | HelpdeskEvent::CompanyExpired(_)
            | HelpdeskEvent::CompanyTenantProvisioned(_)
            | HelpdeskEvent::CompanyLifecycleMirrored(_)
            | HelpdeskEvent::TicketAssigned(_)
            | HelpdeskEvent::TicketResolved(_)
            | HelpdeskEvent::TicketReopened(_)
            | HelpdeskEvent::TicketClosed(_)
            | HelpdeskEvent::TicketEscalated(_)
            | HelpdeskEvent::TicketsMerged(_)
            | HelpdeskEvent::TicketInternalNoteAdded(_) => vec![],
        }
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        match event {
            HelpdeskEvent::TicketCreated(p) => {
                state.tickets.insert(
                    p.ticket_id.clone(),
                    CustomerTicketContent {
                        ticket_id: p.ticket_id.clone(),
                        title: p.title.clone(),
                        description: p.description.clone(),
                        messages: Vec::new(),
                        rating_comment: None,
                    },
                );
            }
            HelpdeskEvent::TicketInfoRequested(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.messages.push(TicketMessage {
                        author_id: p.staff_id.clone(),
                        from_staff: true,
                        text: p.message.clone(),
                    });
                }
            }
            HelpdeskEvent::TicketCustomerResponded(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.messages.push(TicketMessage {
                        author_id: p.requester_id.clone(),
                        from_staff: false,
                        text: p.message.clone(),
                    });
                }
            }
            HelpdeskEvent::TicketRated(p) => {
                if let Some(entry) = state.tickets.get_mut(&p.ticket_id) {
                    entry.rating_comment = p.comment.clone();
                }
            }
            // Not consumed - see `keys`.
            _ => {}
        }
    }
}

// --- internal notes, staff-only by access control (TEAM_ONLY below) ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct TicketInternalNote {
    pub staff_id: String,
    pub note: String,
}

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct TicketInternalNotesState {
    pub notes: Vec<TicketInternalNote>,
}

/// Keyed by `ticket_id`. A *separate* projection from `TicketSummary`/
/// `CompanyActiveTickets`/`CustomerTickets`, on purpose: those are what
/// `frontend/` fetches eagerly to render a ticket at all, for customers
/// too, so keeping `TicketInternalNoteAdded` out of them is what keeps
/// it from customers (see that event's own doc comment). This projection exists
/// purely so staff have something to fetch on demand (`frontend/`'s own
/// "Notes" toggle, a second, separate query - not folded into the
/// eager one).
///
/// **Both the cross-company and the same-company staff-vs-customer gaps
/// are now closed.** A security review found this projection (and
/// `TicketSummary`/`CompanyTicketList`, since replaced by
/// `CompanyActiveTickets`/`CustomerTickets`) readable by any Role with *any*
/// mapping on the bounded context, regardless of which company the
/// queried key actually belonged to - `skilj-graphql`'s
/// `require_read_mapping` checked only that. skilj's own fix
/// (`docs/architecture.md` §23 in the sibling `skilj` repo) added
/// `OWNER_TAG_KEY`/`RoleAccessMapping.scope`, adopted here (this
/// projection's `OWNER_TAG_KEY` below, `TicketInternalNoteAdded`'s own
/// "company" tag, and `server.rs`'s demo customer Role scoped to its
/// own company) - closing the cross-company half for all three
/// projections, `tests/cross_company_projection_scoping.rs` proves it
/// live.
///
/// `OWNER_TAG_KEY` alone left a second, different-axis gap open: a
/// customer scoped to their *own* company could still read this
/// projection for their own tickets, seeing staff-only notes the
/// feature was built to keep from them regardless of company - a role
/// dimension (staff or not), not a tenancy one, which `scope` was never
/// built to express. skilj 0.0.4 closes exactly that with
/// `Projection::TEAM_ONLY` (`docs/architecture.md` §31-32 in the
/// sibling `skilj` repo, Codeberg issue #17): a whole-projection gate,
/// independent of and composed with `OWNER_TAG_KEY` rather than a
/// refinement of it - a query must satisfy both, each failing on its
/// own terms (`GrantScopeMismatch` vs. `NotOnRequiredTeam`). Adopted
/// here as `TEAM_ONLY = Some(STAFF_TEAM)` below, matched against
/// `server.rs`'s staff Role(s), whose `Role.name` is literally
/// `STAFF_TEAM` for exactly this reason (no separate "team" field
/// exists on `Role` - `name` doubles as the team identifier `TEAM_ONLY`
/// compares against). `STAFF_TEAM` (this file's own top-level const) is
/// the one place that string lives - `AddInternalNote`/
/// `TicketInternalNoteAdded`'s own `private_fields()` and `server.rs`'s
/// seeding both reference it rather than repeating the literal, found
/// worth doing in review after the two independent gates almost drifted
/// apart under separate literals.
/// No superadmin bypass, deliberately - see `Projection::TEAM_ONLY`'s
/// own doc comment. `tests/cross_company_projection_scoping.rs` now
/// proves this half live too: a company-A customer reading company A's
/// *own* internal notes is rejected, not just company B's.
pub struct TicketInternalNotes;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for TicketInternalNotes {
    type State = TicketInternalNotesState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "TicketInternalNotes";
    const OWNER_TAG_KEY: Option<&'static str> = Some("company");
    const TEAM_ONLY: Option<&'static str> = Some(STAFF_TEAM);
    fn consumed_event_types() -> Vec<&'static str> {
        vec!["TicketInternalNoteAdded"]
    }
    fn sync() -> bool {
        true
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        match event {
            HelpdeskEvent::TicketInternalNoteAdded(p) => vec![p.ticket_id.clone()],
            _ => vec![],
        }
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        if let HelpdeskEvent::TicketInternalNoteAdded(p) = event {
            state.notes.push(TicketInternalNote {
                staff_id: p.staff_id.clone(),
                note: p.note.clone(),
            });
        }
    }
}
