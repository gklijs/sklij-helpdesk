//! State-machine and property tests propagated from `specs/*.allium`
//! (issue #36): the spec's transition graphs, rule guards and invariants,
//! checked against the real `decide()`/`decide_with()` of every command.
//!
//! Fixture style, like `tests/fixture/`: no database, no HTTP. The world
//! is a plain event history; a command is decided against it exactly as
//! skilj would hand it over - company, creation and macro-definition
//! commands get the history, ticket commands get each named ticket's
//! `TicketSnapshot` state (and `AutoAssignTicket` each candidate's
//! `StaffWorkload`) as JSON through `Decision`, the same path skilj takes -
//! and an accepted command's events are appended. What this leaves to the
//! Postgres suites is the same as for `tests/fixture/`: tag scoping,
//! concurrent writes, access control, and deadlines actually firing.
//!
//! Three layers:
//! - **The graphs, exhaustively.** Every ticket and company status
//!   reached through the real commands, every command tried in it: each
//!   accepted change is a declared edge, every declared edge has its
//!   witnessing rule, terminal states have no way out.
//! - **A reference model.** `Model` is the spec's rules transcribed - the
//!   `requires` guards and `ensures` - with nothing from the code: the
//!   ticket lifecycle, priority (escalation, SLA breaches, macros),
//!   load-based auto-assignment, SLA policies and macros. Random command
//!   sequences (proptest) run against both, which must agree on every
//!   accept/reject and on the resulting state, and the spec's invariants
//!   are checked after every step.
//! - **activity/marketing/knowledge_base.** Their guards as properties,
//!   and the `CrossContextRoute` wiring the specs model as rules.

use proptest::prelude::*;
use skilj::{CommandType, CrossContextRoute, Snapshot};
use skilj_core::plugin::{Decision, SnapshotState};
use skilj_core::shared::{CommandDecision, EventSpec};
use skilj_helpdesk::helpdesk::*;
use skilj_helpdesk::sla::{SlaKind, SlaPolicy, SlaTarget};
use skilj_helpdesk::{activity, alerting, knowledge_base as kb, marketing, scheduling};

// --- the spec's transition graphs (skilj-helpdesk.allium) ---

use TicketStatus::{Closed, InProgress, Merged, Open, Resolved, WaitingOnCustomer};

/// `entity Ticket`'s `transitions status`.
const TICKET_EDGES: &[(TicketStatus, TicketStatus)] = &[
    (Open, InProgress),
    (InProgress, WaitingOnCustomer),
    (WaitingOnCustomer, InProgress),
    (InProgress, Resolved),
    (Resolved, InProgress),
    (Resolved, Closed),
    (Open, Merged),
    (InProgress, Merged),
    (WaitingOnCustomer, Merged),
    (Resolved, Merged),
];
const TICKET_TERMINAL: &[TicketStatus] = &[Closed, Merged];
const TICKET_STATUSES: &[TicketStatus] = &[
    Open,
    InProgress,
    WaitingOnCustomer,
    Resolved,
    Closed,
    Merged,
];

/// `entity Company`'s `transitions status`.
const COMPANY_EDGES: &[(CompanyStatus, CompanyStatus)] = &[
    (CompanyStatus::Trialing, CompanyStatus::Active),
    (CompanyStatus::Trialing, CompanyStatus::Expired),
    (CompanyStatus::Expired, CompanyStatus::Active),
];
const COMPANY_TERMINAL: &[CompanyStatus] = &[CompanyStatus::Active];

// --- the world: an event history, decided against like skilj does ---

const REQUESTER: &str = "customer-1";
/// Who `Op::Assign` and every other staff command acts as.
const STAFF: &str = "staff-1";
const ALL_STAFF: &[&str] = &["staff-1", "staff-2", "staff-3"];

#[derive(Default)]
struct World {
    history: Vec<HelpdeskEvent>,
}

impl World {
    /// The ticket's `TicketSnapshot` state, folded as skilj folds it.
    fn ticket(&self, ticket_id: &str) -> TicketFacts {
        let mut state = TicketFacts::default();
        for event in &self.history {
            TicketSnapshot::fold_for(&mut state, ticket_id, event);
        }
        state
    }

    /// The agent's `StaffWorkload` state, folded as skilj folds it.
    fn workload(&self, staff_id: &str) -> StaffLoad {
        let mut state = StaffLoad::default();
        for event in &self.history {
            StaffWorkload::fold_for(&mut state, staff_id, event);
        }
        state
    }

    fn company(&self, company_id: &str) -> Option<CompanyStatus> {
        CompanyFacts::of(&self.history, company_id).status
    }

    fn apply(&mut self, decision: &CommandDecision) {
        if let CommandDecision::Accepted { events } = decision {
            self.history.extend(events.iter().map(rebuild));
        }
    }

    /// A command whose `decide()` reads the history.
    fn decide<C: CommandType<Event = HelpdeskEvent>>(
        &mut self,
        payload: C::Payload,
    ) -> CommandDecision {
        let decision = C::decide(&payload, &self.history);
        self.apply(&decision);
        decision
    }

    /// A command whose query has `TicketSnapshot` items, one per ticket in
    /// `tickets`, and `StaffWorkload` items, one per agent in `staff`. The
    /// history rides along as the query's events, for `ApplyMacro`'s macro
    /// item; every other command decides from the snapshots alone.
    fn decide_on<C: CommandType<Event = HelpdeskEvent>>(
        &mut self,
        payload: C::Payload,
        tickets: &[&str],
        staff: &[&str],
    ) -> CommandDecision {
        let state = |snapshot: &str, tag: &str, json: String| SnapshotState {
            snapshot: snapshot.into(),
            tag_value: tag.into(),
            state_json: json,
            folded_through: -1,
        };
        let mut states: Vec<SnapshotState> = tickets
            .iter()
            .map(|t| {
                let json = serde_json::to_string(&self.ticket(t)).unwrap();
                state(TicketSnapshot::NAME, t, json)
            })
            .collect();
        states.extend(staff.iter().map(|s| {
            let json = serde_json::to_string(&self.workload(s)).unwrap();
            state(StaffWorkload::NAME, s, json)
        }));
        let decision = C::decide_with(&payload, &Decision::new(&self.history, &states));
        self.apply(&decision);
        decision
    }

    fn run(&mut self, op: &Op) -> CommandDecision {
        match op.clone() {
            Op::SignUp(c) => self.decide::<SignUpCompany>(SignUpCompanyPayload {
                company_id: c.into(),
                name: format!("{c} Inc"),
                contact_email: format!("ops@{c}.example"),
            }),
            Op::Convert(c) => self.decide::<ConvertCompanyTrial>(ConvertCompanyTrialPayload {
                company_id: c.into(),
            }),
            Op::Expire(c) => self.decide::<ExpireCompanyTrial>(ExpireCompanyTrialPayload {
                company_id: c.into(),
            }),
            Op::Reactivate(c) => self.decide::<ReactivateCompany>(ReactivateCompanyPayload {
                company_id: c.into(),
            }),
            Op::SetSla(c, policy) => {
                self.decide::<SetCompanySlaPolicy>(SetCompanySlaPolicyPayload {
                    company_id: c.into(),
                    policy: policy.policy(),
                })
            }
            Op::Create {
                ticket,
                company,
                requester,
                priority,
            } => self.decide::<CreateTicket>(create_payload(ticket, company, requester, priority)),
            Op::Assign(t) => self.decide_on::<AssignTicket>(
                AssignTicketPayload {
                    ticket_id: t.into(),
                    staff_id: STAFF.into(),
                },
                &[t],
                &[],
            ),
            Op::AutoAssign(t, candidates) => self.decide_on::<AutoAssignTicket>(
                AutoAssignTicketPayload {
                    ticket_id: t.into(),
                    candidate_staff_ids: candidates.iter().map(|c| c.to_string()).collect(),
                },
                &[t],
                candidates,
            ),
            Op::RequestInfo(t, r) => self.decide_on::<RequestInfoFromCustomer>(
                RequestInfoFromCustomerPayload {
                    ticket_id: t.into(),
                    staff_id: STAFF.into(),
                    message: "which version?".into(),
                    requester_id: Some(r.into()),
                },
                &[t],
                &[],
            ),
            Op::Respond(t, r) => self.decide_on::<CustomerRespondsToTicket>(
                CustomerRespondsToTicketPayload {
                    ticket_id: t.into(),
                    requester_id: r.into(),
                    message: "the latest".into(),
                },
                &[t],
                &[],
            ),
            Op::Resolve(t) => self.decide_on::<ResolveTicket>(
                ResolveTicketPayload {
                    ticket_id: t.into(),
                },
                &[t],
                &[],
            ),
            Op::Reopen(t) => self.decide_on::<ReopenTicket>(
                ReopenTicketPayload {
                    ticket_id: t.into(),
                },
                &[t],
                &[],
            ),
            Op::Close(t) => self.decide_on::<CloseTicket>(
                CloseTicketPayload {
                    ticket_id: t.into(),
                    requester_id: Some(REQUESTER.into()),
                    requester_email: None,
                },
                &[t],
                &[],
            ),
            Op::Escalate(t) => self.decide_on::<EscalateTicket>(
                EscalateTicketPayload {
                    ticket_id: t.into(),
                },
                &[t],
                &[],
            ),
            Op::Breach(t, sla) => self.decide_on::<BreachTicketSla>(
                BreachTicketSlaPayload {
                    ticket_id: t.into(),
                    sla,
                },
                &[t],
                &[],
            ),
            Op::Merge(p, d) => self.decide_on::<MergeTickets>(
                MergeTicketsPayload {
                    primary_ticket_id: p.into(),
                    duplicate_ticket_id: d.into(),
                },
                &[p, d],
                &[],
            ),
            Op::Rate(t, r, rating) => self.decide_on::<RateTicket>(
                RateTicketPayload {
                    ticket_id: t.into(),
                    rating,
                    comment: None,
                    requester_id: Some(r.into()),
                },
                &[t],
                &[],
            ),
            Op::Note(t) => self.decide_on::<AddInternalNote>(
                AddInternalNotePayload {
                    ticket_id: t.into(),
                    staff_id: STAFF.into(),
                    note: "looked at it".into(),
                },
                &[t],
                &[],
            ),
            Op::CreateMacro(c, m, status, priority) => {
                self.decide::<CreateMacro>(macro_definition(c, m, status, priority))
            }
            Op::UpdateMacro(c, m, status, priority) => {
                self.decide::<UpdateMacro>(macro_definition(c, m, status, priority))
            }
            Op::DeleteMacro(c, m) => self.decide::<DeleteMacro>(MacroDeletedPayload {
                company_id: c.into(),
                macro_id: m.into(),
                staff_id: STAFF.into(),
            }),
            Op::ApplyMacro(t, c, m) => self.decide_on::<ApplyMacro>(
                ApplyMacroPayload {
                    ticket_id: t.into(),
                    company_id: c.into(),
                    macro_id: m.into(),
                    staff_id: STAFF.into(),
                },
                &[t],
                &[],
            ),
        }
    }
}

fn create_payload(
    ticket: &str,
    company: &str,
    requester: &str,
    priority: TicketPriority,
) -> CreateTicketPayload {
    CreateTicketPayload {
        ticket_id: ticket.into(),
        company_id: company.into(),
        requester_id: requester.into(),
        logged_by_staff_id: None,
        title: "Login broken".into(),
        description: "Since this morning".into(),
        priority,
        requester_name: None,
        requester_email: None,
    }
}

fn macro_definition(
    company: &str,
    macro_id: &str,
    status: Option<MacroStatus>,
    priority: Option<TicketPriority>,
) -> MacroDefinitionPayload {
    MacroDefinitionPayload {
        company_id: company.into(),
        macro_id: macro_id.into(),
        staff_id: STAFF.into(),
        name: format!("{macro_id} name"),
        body: "Hi, about {{ticket_id}} - {{staff_id}}".into(),
        status,
        priority,
    }
}

/// An accepted `EventSpec`, as the event skilj would store.
fn rebuild(spec: &EventSpec) -> HelpdeskEvent {
    fn p<T: serde::de::DeserializeOwned>(spec: &EventSpec) -> T {
        serde_json::from_value(spec.payload.clone())
            .unwrap_or_else(|e| panic!("{} payload: {e}", spec.event_type))
    }
    match spec.event_type.as_str() {
        "CompanySignedUp" => HelpdeskEvent::CompanySignedUp(p(spec)),
        "CompanyActivated" => HelpdeskEvent::CompanyActivated(p(spec)),
        "CompanyExpired" => HelpdeskEvent::CompanyExpired(p(spec)),
        "CompanyTenantProvisioned" => HelpdeskEvent::CompanyTenantProvisioned(p(spec)),
        "CompanyLifecycleMirrored" => HelpdeskEvent::CompanyLifecycleMirrored(p(spec)),
        "CompanySlaPolicySet" => HelpdeskEvent::CompanySlaPolicySet(p(spec)),
        "TicketCreated" => HelpdeskEvent::TicketCreated(p(spec)),
        "TicketAssigned" => HelpdeskEvent::TicketAssigned(p(spec)),
        "TicketResolved" => HelpdeskEvent::TicketResolved(p(spec)),
        "TicketReopened" => HelpdeskEvent::TicketReopened(p(spec)),
        "TicketInfoRequested" => HelpdeskEvent::TicketInfoRequested(p(spec)),
        "TicketCustomerResponded" => HelpdeskEvent::TicketCustomerResponded(p(spec)),
        "TicketClosed" => HelpdeskEvent::TicketClosed(p(spec)),
        "TicketEscalated" => HelpdeskEvent::TicketEscalated(p(spec)),
        "TicketSlaBreached" => HelpdeskEvent::TicketSlaBreached(p(spec)),
        "TicketsMerged" => HelpdeskEvent::TicketsMerged(p(spec)),
        "TicketRated" => HelpdeskEvent::TicketRated(p(spec)),
        "TicketInternalNoteAdded" => HelpdeskEvent::TicketInternalNoteAdded(p(spec)),
        "TicketStaffReplied" => HelpdeskEvent::TicketStaffReplied(p(spec)),
        "TicketPriorityChanged" => HelpdeskEvent::TicketPriorityChanged(p(spec)),
        "MacroCreated" => HelpdeskEvent::MacroCreated(p(spec)),
        "MacroUpdated" => HelpdeskEvent::MacroUpdated(p(spec)),
        "MacroDeleted" => HelpdeskEvent::MacroDeleted(p(spec)),
        other => panic!("unexpected event type {other}"),
    }
}

fn accepted(decision: &CommandDecision) -> bool {
    matches!(decision, CommandDecision::Accepted { .. })
}

fn events(decision: &CommandDecision) -> &[EventSpec] {
    match decision {
        CommandDecision::Accepted { events } => events,
        _ => &[],
    }
}

fn event<'a>(decision: &'a CommandDecision, event_type: &str) -> Option<&'a serde_json::Value> {
    events(decision)
        .iter()
        .find(|e| e.event_type == event_type)
        .map(|e| &e.payload)
}

// --- the commands, one per rule trigger ---

/// The SLA policies a company can set: the default, a faster one, and
/// one `valid_sla_policy` refuses.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Policy {
    Default,
    Fast,
    Invalid,
}

impl Policy {
    fn policy(self) -> SlaPolicy {
        let quick = SlaTarget {
            first_response_minutes: 30,
            resolution_minutes: 120,
        };
        match self {
            Policy::Default => SlaPolicy::default(),
            Policy::Fast => SlaPolicy {
                low: quick,
                medium: quick,
                high: quick,
                urgent: quick,
                ..SlaPolicy::default()
            },
            // A first response due after the resolution.
            Policy::Invalid => SlaPolicy {
                low: SlaTarget {
                    first_response_minutes: 600,
                    resolution_minutes: 60,
                },
                ..SlaPolicy::default()
            },
        }
    }
}

#[derive(Debug, Clone)]
enum Op {
    SignUp(&'static str),
    /// `TrialPeriodEnds`, charge succeeded.
    Convert(&'static str),
    /// `TrialPeriodEnds`, charge failed.
    Expire(&'static str),
    Reactivate(&'static str),
    SetSla(&'static str, Policy),
    Create {
        ticket: &'static str,
        company: &'static str,
        requester: &'static str,
        priority: TicketPriority,
    },
    Assign(&'static str),
    AutoAssign(&'static str, &'static [&'static str]),
    RequestInfo(&'static str, &'static str),
    Respond(&'static str, &'static str),
    Resolve(&'static str),
    Reopen(&'static str),
    Close(&'static str),
    Escalate(&'static str),
    Breach(&'static str, SlaKind),
    Merge(&'static str, &'static str),
    Rate(&'static str, &'static str, u8),
    Note(&'static str),
    CreateMacro(
        &'static str,
        &'static str,
        Option<MacroStatus>,
        Option<TicketPriority>,
    ),
    UpdateMacro(
        &'static str,
        &'static str,
        Option<MacroStatus>,
        Option<TicketPriority>,
    ),
    DeleteMacro(&'static str, &'static str),
    /// Ticket, company, macro.
    ApplyMacro(&'static str, &'static str, &'static str),
}

/// The three macros `ticket_in` gives `acme`.
const WAIT_MACRO: &str = "m-wait";
const RESOLVE_MACRO: &str = "m-resolve";
const REPLY_MACRO: &str = "m-reply";

/// Every single-ticket command, each with the arguments that make its
/// non-status guards pass - what the exhaustive graph walks try in every
/// state.
fn ticket_ops(t: &'static str) -> Vec<Op> {
    vec![
        Op::Assign(t),
        Op::AutoAssign(t, &["staff-1", "staff-2"]),
        Op::RequestInfo(t, REQUESTER),
        Op::Respond(t, REQUESTER),
        Op::Resolve(t),
        Op::Reopen(t),
        Op::Close(t),
        Op::Escalate(t),
        Op::Breach(t, SlaKind::FirstResponse),
        Op::Breach(t, SlaKind::Resolution),
        Op::Rate(t, REQUESTER, 5),
        Op::Note(t),
        Op::Merge("other", t),
        Op::Merge(t, "other"),
        Op::ApplyMacro(t, "acme", WAIT_MACRO),
        Op::ApplyMacro(t, "acme", RESOLVE_MACRO),
        Op::ApplyMacro(t, "acme", REPLY_MACRO),
    ]
}

// --- the exhaustive graph walks ---

/// A world holding ticket `t1` in `status`, plus an open ticket `other`
/// of the same company to merge with and three macros - built only
/// through the real commands, along declared edges.
fn ticket_in(status: TicketStatus) -> World {
    let mut world = World::default();
    let mut steps = vec![
        Op::SignUp("acme"),
        Op::CreateMacro(
            "acme",
            WAIT_MACRO,
            Some(MacroStatus::WaitingOnCustomer),
            None,
        ),
        Op::CreateMacro("acme", RESOLVE_MACRO, Some(MacroStatus::Resolved), None),
        Op::CreateMacro("acme", REPLY_MACRO, None, Some(TicketPriority::Urgent)),
        Op::Create {
            ticket: "other",
            company: "acme",
            requester: REQUESTER,
            priority: TicketPriority::Low,
        },
        Op::Create {
            ticket: "t1",
            company: "acme",
            requester: REQUESTER,
            priority: TicketPriority::Medium,
        },
    ];
    steps.extend(match status {
        Open => vec![],
        InProgress => vec![Op::Assign("t1")],
        WaitingOnCustomer => vec![Op::Assign("t1"), Op::RequestInfo("t1", REQUESTER)],
        Resolved => vec![Op::Assign("t1"), Op::Resolve("t1")],
        Closed => vec![Op::Assign("t1"), Op::Resolve("t1"), Op::Close("t1")],
        Merged => vec![Op::Merge("other", "t1")],
    });
    for step in &steps {
        assert!(
            accepted(&world.run(step)),
            "setting up {status:?}: {step:?}"
        );
    }
    assert_eq!(world.ticket("t1").status, Some(status));
    world
}

#[test]
fn every_accepted_ticket_command_moves_along_a_declared_edge() {
    for &from in TICKET_STATUSES {
        for op in ticket_ops("t1") {
            let mut world = ticket_in(from);
            let decision = world.run(&op);
            let to = world.ticket("t1").status.unwrap();
            if accepted(&decision) && to != from {
                assert!(
                    TICKET_EDGES.contains(&(from, to)),
                    "{op:?} moved t1 {from:?} -> {to:?}, not in the spec's graph"
                );
            }
        }
    }
}

#[test]
fn every_declared_ticket_edge_is_reached_by_its_witnessing_rules() {
    let witnesses: &[(TicketStatus, TicketStatus, Op)] = &[
        (Open, InProgress, Op::Assign("t1")), // StaffPicksUpTicket
        (Open, InProgress, Op::AutoAssign("t1", &["staff-2"])), // StaffAutoAssignsTicket
        (
            InProgress,
            WaitingOnCustomer,
            Op::RequestInfo("t1", REQUESTER),
        ), // StaffRequestsInfo
        (
            InProgress,
            WaitingOnCustomer,
            Op::ApplyMacro("t1", "acme", WAIT_MACRO),
        ), // StaffAppliesMacro
        (WaitingOnCustomer, InProgress, Op::Respond("t1", REQUESTER)), // CustomerReplies
        (InProgress, Resolved, Op::Resolve("t1")), // StaffResolvesTicket
        (
            InProgress,
            Resolved,
            Op::ApplyMacro("t1", "acme", RESOLVE_MACRO),
        ), // StaffAppliesMacro
        (Resolved, InProgress, Op::Reopen("t1")), // TicketReopened
        (Resolved, Closed, Op::Close("t1")),  // TicketAutoCloses / StaffClosesTicket
        (Open, Merged, Op::Merge("other", "t1")), // StaffMergesTickets
        (InProgress, Merged, Op::Merge("other", "t1")),
        (WaitingOnCustomer, Merged, Op::Merge("other", "t1")),
        (Resolved, Merged, Op::Merge("other", "t1")),
    ];
    for edge in TICKET_EDGES {
        assert!(
            witnesses.iter().any(|(f, t, _)| (*f, *t) == *edge),
            "{edge:?} has no witness"
        );
    }
    for (from, to, op) in witnesses {
        assert!(TICKET_EDGES.contains(&(*from, *to)));
        let mut world = ticket_in(*from);
        let decision = world.run(op);
        assert!(
            accepted(&decision),
            "{from:?} -> {to:?} via {op:?}: {decision:?}"
        );
        assert_eq!(world.ticket("t1").status, Some(*to), "{op:?} from {from:?}");
    }
}

#[test]
fn terminal_ticket_states_have_no_way_out_and_the_others_do() {
    for &from in TICKET_STATUSES {
        let exits = ticket_ops("t1")
            .into_iter()
            .filter(|op| {
                let mut world = ticket_in(from);
                accepted(&world.run(op)) && world.ticket("t1").status != Some(from)
            })
            .count();
        if TICKET_TERMINAL.contains(&from) {
            assert_eq!(exits, 0, "terminal {from:?} was left");
        } else {
            assert!(exits > 0, "non-terminal {from:?} has no exit");
        }
    }
}

#[test]
fn a_ticket_reaches_both_terminal_states_from_creation() {
    // Reachability: open -> in_progress -> waiting -> in_progress ->
    // resolved -> in_progress -> resolved -> closed ...
    let mut world = ticket_in(Open);
    for op in [
        Op::Assign("t1"),
        Op::RequestInfo("t1", REQUESTER),
        Op::Respond("t1", REQUESTER),
        Op::Resolve("t1"),
        Op::Reopen("t1"),
        Op::ApplyMacro("t1", "acme", RESOLVE_MACRO),
        Op::Rate("t1", REQUESTER, 4),
        Op::Close("t1"),
    ] {
        assert!(accepted(&world.run(&op)), "{op:?}");
    }
    assert_eq!(world.ticket("t1").status, Some(Closed));
    // ... and open -> merged.
    let mut world = ticket_in(Open);
    assert!(accepted(&world.run(&Op::Merge("other", "t1"))));
    assert_eq!(world.ticket("t1").status, Some(Merged));
    // The primary of a merge keeps its status.
    assert_eq!(world.ticket("other").status, Some(Open));
}

#[test]
fn a_reopened_ticket_has_met_its_resolution_sla() {
    // TicketBreachesResolutionSla requires `first_resolved_at = null`,
    // which a reopen doesn't clear (weeded 2026-10-12).
    let mut world = ticket_in(Resolved);
    assert!(accepted(&world.run(&Op::Reopen("t1"))));
    assert!(!accepted(
        &world.run(&Op::Breach("t1", SlaKind::Resolution))
    ));
    // A ticket never resolved can still breach it.
    assert!(accepted(
        &world.run(&Op::Breach("other", SlaKind::Resolution))
    ));
}

/// A world with `acme` in `status`, built through the real commands.
fn company_in(status: CompanyStatus) -> World {
    let mut world = World::default();
    let steps: Vec<Op> = match status {
        CompanyStatus::Trialing => vec![Op::SignUp("acme")],
        CompanyStatus::Active => vec![Op::SignUp("acme"), Op::Convert("acme")],
        CompanyStatus::Expired => vec![Op::SignUp("acme"), Op::Expire("acme")],
    };
    for step in &steps {
        assert!(accepted(&world.run(step)), "{step:?}");
    }
    world
}

#[test]
fn the_company_lifecycle_follows_its_declared_graph() {
    let all = [
        CompanyStatus::Trialing,
        CompanyStatus::Active,
        CompanyStatus::Expired,
    ];
    let ops = [
        Op::SignUp("acme"),
        Op::Convert("acme"),
        Op::Expire("acme"),
        Op::Reactivate("acme"),
        Op::SetSla("acme", Policy::Fast),
    ];
    let mut reached = Vec::new();
    for from in all {
        let mut exits = 0;
        for op in &ops {
            let mut world = company_in(from);
            let decision = world.run(op);
            let to = world.company("acme").unwrap();
            if to != from {
                assert!(accepted(&decision));
                assert!(
                    COMPANY_EDGES.contains(&(from, to)),
                    "{op:?}: {from:?} -> {to:?}"
                );
                reached.push((from, to));
                exits += 1;
            }
        }
        assert_eq!(exits == 0, COMPANY_TERMINAL.contains(&from), "{from:?}");
    }
    reached.sort_by_key(|e| format!("{e:?}"));
    let mut declared = COMPANY_EDGES.to_vec();
    declared.sort_by_key(|e| format!("{e:?}"));
    assert_eq!(reached, declared, "every declared edge, and nothing else");
    // CompanySignsUp: once per company_id.
    let mut world = company_in(CompanyStatus::Trialing);
    assert!(!accepted(&world.run(&Op::SignUp("acme"))));
}

#[test]
fn only_an_expired_company_is_refused_new_tickets() {
    for status in [
        CompanyStatus::Trialing,
        CompanyStatus::Active,
        CompanyStatus::Expired,
    ] {
        let mut world = company_in(status);
        let decision = world.run(&Op::Create {
            ticket: "t1",
            company: "acme",
            requester: REQUESTER,
            priority: TicketPriority::Low,
        });
        assert_eq!(
            accepted(&decision),
            status != CompanyStatus::Expired,
            "{status:?}"
        );
    }
    // And for a company that never signed up.
    let mut world = World::default();
    assert!(!accepted(&world.decide::<CreateTicket>(create_payload(
        "t1",
        "nobody",
        REQUESTER,
        TicketPriority::Low
    ))));
}

#[test]
fn an_expired_company_can_still_work_its_existing_tickets() {
    // "Creating new tickets is the only thing disallowed while expired."
    let mut world = ticket_in(InProgress);
    assert!(accepted(&world.run(&Op::Expire("acme"))));
    for op in [
        Op::RequestInfo("t1", REQUESTER),
        Op::Respond("t1", REQUESTER),
        Op::ApplyMacro("t1", "acme", REPLY_MACRO),
        Op::Resolve("t1"),
        Op::Close("t1"),
    ] {
        assert!(accepted(&world.run(&op)), "{op:?}");
    }
}

// --- the tenant rules (CompanyTenantIsRecorded, TenantLifecycleIsMirrored) ---

#[test]
fn a_tenant_is_recorded_once_and_only_for_a_known_company() {
    let record = |world: &World, tenant: &str| {
        RecordCompanyTenant::decide(
            &RecordCompanyTenantPayload {
                company_id: "acme".into(),
                tenant_name: tenant.into(),
            },
            &world.history,
        )
    };
    let mut world = World::default();
    assert!(!accepted(&record(&world, "tenant-a")), "company_not_found");
    world = company_in(CompanyStatus::Trialing);
    let decision = record(&world, "tenant-a");
    assert!(accepted(&decision));
    world.apply(&decision);
    assert!(
        !accepted(&record(&world, "tenant-b")),
        "requires: company.tenant = null"
    );
}

#[test]
fn the_lifecycle_mirror_only_moves_forward() {
    let statuses = [
        CompanyStatus::Trialing,
        CompanyStatus::Expired,
        CompanyStatus::Active,
    ];
    for mirrored in [
        None,
        Some(CompanyStatus::Trialing),
        Some(CompanyStatus::Expired),
        Some(CompanyStatus::Active),
    ] {
        for status in statuses {
            let history: Vec<HelpdeskEvent> = mirrored
                .map(|m| {
                    HelpdeskEvent::CompanyLifecycleMirrored(CompanyLifecycleMirroredPayload {
                        company_id: "acme".into(),
                        status: m,
                        source_event_type: "test".into(),
                    })
                })
                .into_iter()
                .collect();
            let decision = RecordTenantLifecycle::decide(
                &RecordTenantLifecyclePayload {
                    company_id: "acme".into(),
                    status,
                    source_event_type: "test".into(),
                },
                &history,
            );
            // rule TenantLifecycleIsMirrored's three requires, verbatim.
            let spec = Some(status) != mirrored
                && mirrored != Some(CompanyStatus::Active)
                && (status != CompanyStatus::Trialing || mirrored.is_none());
            assert_eq!(
                accepted(&decision),
                spec,
                "mirrored {mirrored:?}, status {status:?}"
            );
        }
    }
}

// --- the reference model: the spec's rules, transcribed ---

#[derive(Debug, Clone, PartialEq)]
struct SpecTicket {
    company: &'static str,
    requester: &'static str,
    status: TicketStatus,
    priority: TicketPriority,
    escalated: bool,
    rating: Option<u8>,
    assigned_to: Option<&'static str>,
    first_responded: bool,
    first_resolved: bool,
    first_response_breached: bool,
    resolution_breached: bool,
    /// `sla: target_for(sla_policy_for(company), priority)`, at creation.
    sla: skilj_helpdesk::sla::TicketSla,
}

impl SpecTicket {
    fn unhandled(&self) -> bool {
        !matches!(self.status, Resolved | Closed | Merged)
    }
}

/// `entity Macro`, plus whether its id was ever used.
#[derive(Debug, Clone)]
struct SpecMacro {
    company: &'static str,
    macro_id: &'static str,
    current: Option<(Option<MacroStatus>, Option<TicketPriority>)>,
}

#[derive(Debug, Default)]
struct Model {
    companies: Vec<(&'static str, CompanyStatus)>,
    sla_policies: Vec<(&'static str, SlaPolicy)>,
    tickets: Vec<(&'static str, SpecTicket)>,
    macros: Vec<SpecMacro>,
}

/// `escalate`'s black box: one tier up, clamped at urgent.
fn spec_escalate(priority: TicketPriority) -> TicketPriority {
    match priority {
        TicketPriority::Low => TicketPriority::Medium,
        TicketPriority::Medium => TicketPriority::High,
        TicketPriority::High | TicketPriority::Urgent => TicketPriority::Urgent,
    }
}

/// `target_for(policy, priority)`, with the business hours that go with
/// it.
fn spec_target(policy: &SlaPolicy, priority: TicketPriority) -> skilj_helpdesk::sla::TicketSla {
    let target = match priority {
        TicketPriority::Low => &policy.low,
        TicketPriority::Medium => &policy.medium,
        TicketPriority::High => &policy.high,
        TicketPriority::Urgent => &policy.urgent,
    };
    skilj_helpdesk::sla::TicketSla {
        business_hours: policy.business_hours.clone(),
        first_response_minutes: target.first_response_minutes,
        resolution_minutes: target.resolution_minutes,
    }
}

impl Model {
    fn company(&self, c: &str) -> Option<CompanyStatus> {
        self.companies
            .iter()
            .find(|(id, _)| *id == c)
            .map(|(_, s)| *s)
    }
    fn set_company(&mut self, c: &'static str, status: CompanyStatus) {
        match self.companies.iter_mut().find(|(id, _)| *id == c) {
            Some(entry) => entry.1 = status,
            None => self.companies.push((c, status)),
        }
    }
    /// `sla_policy_for(company)`: its own, or the default.
    fn sla_policy_for(&self, c: &str) -> SlaPolicy {
        self.sla_policies
            .iter()
            .rev()
            .find(|(id, _)| *id == c)
            .map(|(_, p)| p.clone())
            .unwrap_or_default()
    }
    fn ticket(&self, t: &str) -> Option<&SpecTicket> {
        self.tickets.iter().find(|(id, _)| *id == t).map(|(_, s)| s)
    }
    fn ticket_mut(&mut self, t: &str) -> &mut SpecTicket {
        &mut self.tickets.iter_mut().find(|(id, _)| *id == t).unwrap().1
    }
    fn macro_entry(&self, c: &str, m: &str) -> Option<&SpecMacro> {
        self.macros
            .iter()
            .find(|e| e.company == c && e.macro_id == m)
    }
    /// An agent's open tickets: assigned to them, in progress or waiting.
    fn load(&self, staff: &str) -> usize {
        self.tickets
            .iter()
            .filter(|(_, t)| {
                t.assigned_to == Some(staff) && matches!(t.status, InProgress | WaitingOnCustomer)
            })
            .count()
    }
    /// `least_loaded`'s black box: fewest open tickets, the first listed
    /// on a tie (a repeat counts in its first place).
    fn least_loaded(&self, candidates: &[&'static str]) -> Option<&'static str> {
        let mut distinct: Vec<&'static str> = Vec::new();
        for c in candidates {
            if !distinct.contains(c) {
                distinct.push(c);
            }
        }
        let mut best: Option<(&'static str, usize)> = None;
        for c in distinct {
            let load = self.load(c);
            if best.is_none_or(|(_, b)| load < b) {
                best = Some((c, load));
            }
        }
        best.map(|(c, _)| c)
    }

    /// Applies `op` if every `requires` holds; returns whether it did.
    fn step(&mut self, op: &Op) -> bool {
        let status_is = |model: &Self, t: &str, ok: &[TicketStatus]| {
            model.ticket(t).is_some_and(|s| ok.contains(&s.status))
        };
        let requester_is =
            |model: &Self, t: &str, r: &str| model.ticket(t).is_some_and(|s| s.requester == r);
        match *op {
            // CompanySignsUp
            Op::SignUp(c) => {
                let ok = self.company(c).is_none();
                if ok {
                    self.set_company(c, CompanyStatus::Trialing);
                }
                ok
            }
            // TrialPeriodEnds, both branches
            Op::Convert(c) | Op::Expire(c) => {
                let ok = self.company(c) == Some(CompanyStatus::Trialing);
                if ok {
                    let to = if matches!(op, Op::Convert(_)) {
                        CompanyStatus::Active
                    } else {
                        CompanyStatus::Expired
                    };
                    self.set_company(c, to);
                }
                ok
            }
            // CompanySubscribes
            Op::Reactivate(c) => {
                let ok = self.company(c) == Some(CompanyStatus::Expired);
                if ok {
                    self.set_company(c, CompanyStatus::Active);
                }
                ok
            }
            // CompanySetsSlaPolicy (on a company that exists)
            Op::SetSla(c, policy) => {
                let ok = self.company(c).is_some() && policy != Policy::Invalid;
                if ok {
                    self.sla_policies.push((c, policy.policy()));
                }
                ok
            }
            // CustomerCreatesTicket/StaffLogsTicketOnBehalf
            Op::Create {
                ticket,
                company,
                requester,
                priority,
            } => {
                let ok = self
                    .company(company)
                    .is_some_and(|s| s != CompanyStatus::Expired)
                    && self.ticket(ticket).is_none();
                if ok {
                    let sla = spec_target(&self.sla_policy_for(company), priority);
                    self.tickets.push((
                        ticket,
                        SpecTicket {
                            company,
                            requester,
                            status: Open,
                            priority,
                            escalated: false,
                            rating: None,
                            assigned_to: None,
                            first_responded: false,
                            first_resolved: false,
                            first_response_breached: false,
                            resolution_breached: false,
                            sla,
                        },
                    ));
                }
                ok
            }
            // StaffPicksUpTicket
            Op::Assign(t) => {
                let ok = self.transition(t, status_is(self, t, &[Open]), InProgress);
                if ok {
                    self.ticket_mut(t).assigned_to = Some(STAFF);
                }
                ok
            }
            // StaffAutoAssignsTicket
            Op::AutoAssign(t, candidates) => {
                let chosen = self.least_loaded(candidates);
                let ok = status_is(self, t, &[Open]) && chosen.is_some();
                if self.transition(t, ok, InProgress) {
                    self.ticket_mut(t).assigned_to = chosen;
                }
                ok
            }
            // StaffRequestsInfo
            Op::RequestInfo(t, r) => {
                let ok = requester_is(self, t, r) && status_is(self, t, &[InProgress]);
                if self.transition(t, ok, WaitingOnCustomer) {
                    self.ticket_mut(t).first_responded = true;
                }
                ok
            }
            // CustomerReplies
            Op::Respond(t, r) => {
                let ok = requester_is(self, t, r) && status_is(self, t, &[WaitingOnCustomer]);
                self.transition(t, ok, InProgress)
            }
            // StaffResolvesTicket
            Op::Resolve(t) => {
                let ok = self.transition(t, status_is(self, t, &[InProgress]), Resolved);
                if ok {
                    let ticket = self.ticket_mut(t);
                    ticket.first_responded = true;
                    ticket.first_resolved = true;
                }
                ok
            }
            // TicketReopened
            Op::Reopen(t) => self.transition(t, status_is(self, t, &[Resolved]), InProgress),
            // TicketAutoCloses/StaffClosesTicket
            Op::Close(t) => self.transition(t, status_is(self, t, &[Resolved]), Closed),
            // TicketBecomesOverdue
            Op::Escalate(t) => {
                let ok = self
                    .ticket(t)
                    .is_some_and(|s| s.unhandled() && !s.escalated);
                if ok {
                    let ticket = self.ticket_mut(t);
                    ticket.priority = spec_escalate(ticket.priority);
                    ticket.escalated = true;
                }
                ok
            }
            // TicketBreachesFirstResponseSla/TicketBreachesResolutionSla
            Op::Breach(t, sla) => {
                let ok = self.ticket(t).is_some_and(|s| {
                    s.unhandled()
                        && match sla {
                            SlaKind::FirstResponse => {
                                !s.first_responded && !s.first_response_breached
                            }
                            SlaKind::Resolution => !s.first_resolved && !s.resolution_breached,
                        }
                });
                if ok {
                    let ticket = self.ticket_mut(t);
                    match sla {
                        SlaKind::FirstResponse => ticket.first_response_breached = true,
                        SlaKind::Resolution => ticket.resolution_breached = true,
                    }
                    if !ticket.escalated {
                        ticket.priority = spec_escalate(ticket.priority);
                        ticket.escalated = true;
                    }
                }
                ok
            }
            // StaffMergesTickets
            Op::Merge(p, d) => {
                let mergeable = |model: &Self, t: &str| {
                    status_is(model, t, &[Open, InProgress, WaitingOnCustomer, Resolved])
                };
                let ok = p != d
                    && mergeable(self, p)
                    && mergeable(self, d)
                    && self.ticket(p).map(|s| s.company) == self.ticket(d).map(|s| s.company);
                self.transition(d, ok, Merged)
            }
            // CustomerRatesTicket
            Op::Rate(t, r, rating) => {
                let ok = requester_is(self, t, r)
                    && status_is(self, t, &[Resolved, Closed])
                    && self.ticket(t).is_some_and(|s| s.rating.is_none())
                    && (1..=5).contains(&rating);
                if ok {
                    self.ticket_mut(t).rating = Some(rating);
                }
                ok
            }
            // StaffAddsInternalNote - no requires beyond the ticket existing.
            Op::Note(t) => self.ticket(t).is_some(),
            // StaffCreatesMacro: a macro id is never reused, deleted or not.
            Op::CreateMacro(c, m, status, priority) => {
                let ok = self.company(c).is_some() && self.macro_entry(c, m).is_none();
                if ok {
                    self.macros.push(SpecMacro {
                        company: c,
                        macro_id: m,
                        current: Some((status, priority)),
                    });
                }
                ok
            }
            // StaffEditsMacro
            Op::UpdateMacro(c, m, status, priority) => {
                let ok = self.macro_entry(c, m).is_some_and(|e| e.current.is_some());
                if ok {
                    let entry = self
                        .macros
                        .iter_mut()
                        .find(|e| e.company == c && e.macro_id == m)
                        .unwrap();
                    entry.current = Some((status, priority));
                }
                ok
            }
            // StaffDeletesMacro
            Op::DeleteMacro(c, m) => {
                let ok = self.macro_entry(c, m).is_some_and(|e| e.current.is_some());
                if ok {
                    let entry = self
                        .macros
                        .iter_mut()
                        .find(|e| e.company == c && e.macro_id == m)
                        .unwrap();
                    entry.current = None;
                }
                ok
            }
            // StaffAppliesMacro
            Op::ApplyMacro(t, c, m) => {
                let Some(ticket) = self.ticket(t) else {
                    return false;
                };
                let Some((sets_status, sets_priority)) =
                    self.macro_entry(c, m).and_then(|e| e.current)
                else {
                    return false;
                };
                let ok = ticket.company == c
                    && !matches!(ticket.status, Closed | Merged)
                    && (sets_status.is_none() || ticket.status == InProgress);
                if ok {
                    let ticket = self.ticket_mut(t);
                    if let Some(priority) = sets_priority {
                        ticket.priority = priority;
                    }
                    ticket.first_responded = true;
                    match sets_status {
                        Some(MacroStatus::WaitingOnCustomer) => ticket.status = WaitingOnCustomer,
                        Some(MacroStatus::Resolved) => {
                            ticket.status = Resolved;
                            ticket.first_resolved = true;
                        }
                        None => {}
                    }
                }
                ok
            }
        }
    }

    fn transition(&mut self, t: &str, ok: bool, to: TicketStatus) -> bool {
        if ok {
            self.ticket_mut(t).status = to;
        }
        ok
    }
}

// --- the property: model and code agree, and the invariants hold ---

const COMPANIES: &[&str] = &["acme", "globex"];
const TICKETS: &[&str] = &["t1", "t2"];
const REQUESTERS: &[&str] = &["customer-1", "customer-2"];
const MACROS: &[&str] = &["m1", "m2"];
const CANDIDATES: &[&[&str]] = &[
    &[],
    &["staff-1"],
    &["staff-2", "staff-1"],
    &["staff-3", "staff-2", "staff-3", "staff-1"],
];

fn priority() -> impl Strategy<Value = TicketPriority> {
    prop_oneof![
        Just(TicketPriority::Low),
        Just(TicketPriority::Medium),
        Just(TicketPriority::High),
        Just(TicketPriority::Urgent),
    ]
}

fn macro_status() -> impl Strategy<Value = Option<MacroStatus>> {
    prop_oneof![
        Just(None),
        Just(Some(MacroStatus::WaitingOnCustomer)),
        Just(Some(MacroStatus::Resolved)),
    ]
}

#[allow(clippy::type_complexity)]
fn macro_definition_strategy() -> impl Strategy<
    Value = (
        &'static str,
        &'static str,
        Option<MacroStatus>,
        Option<TicketPriority>,
    ),
> {
    (
        proptest::sample::select(COMPANIES),
        proptest::sample::select(MACROS),
        macro_status(),
        proptest::option::of(priority()),
    )
}

fn op() -> impl Strategy<Value = Op> {
    let company = proptest::sample::select(COMPANIES);
    let ticket = proptest::sample::select(TICKETS);
    let requester = proptest::sample::select(REQUESTERS);
    let macro_id = proptest::sample::select(MACROS);
    let sla = prop_oneof![Just(SlaKind::FirstResponse), Just(SlaKind::Resolution)];
    let policy = prop_oneof![
        Just(Policy::Default),
        Just(Policy::Fast),
        Just(Policy::Invalid)
    ];
    prop_oneof![
        1 => company.clone().prop_map(Op::SignUp),
        1 => company.clone().prop_map(Op::Convert),
        1 => company.clone().prop_map(Op::Expire),
        1 => company.clone().prop_map(Op::Reactivate),
        1 => (company.clone(), policy).prop_map(|(c, p)| Op::SetSla(c, p)),
        3 => (ticket.clone(), company.clone(), requester.clone(), priority())
            .prop_map(|(ticket, company, requester, priority)| Op::Create { ticket, company, requester, priority }),
        2 => ticket.clone().prop_map(Op::Assign),
        2 => (ticket.clone(), proptest::sample::select(CANDIDATES)).prop_map(|(t, c)| Op::AutoAssign(t, c)),
        1 => (ticket.clone(), requester.clone()).prop_map(|(t, r)| Op::RequestInfo(t, r)),
        1 => (ticket.clone(), requester.clone()).prop_map(|(t, r)| Op::Respond(t, r)),
        2 => ticket.clone().prop_map(Op::Resolve),
        1 => ticket.clone().prop_map(Op::Reopen),
        1 => ticket.clone().prop_map(Op::Close),
        1 => ticket.clone().prop_map(Op::Escalate),
        2 => (ticket.clone(), sla).prop_map(|(t, s)| Op::Breach(t, s)),
        1 => (ticket.clone(), ticket.clone()).prop_map(|(p, d)| Op::Merge(p, d)),
        1 => (ticket.clone(), requester, 0u8..=6).prop_map(|(t, r, n)| Op::Rate(t, r, n)),
        1 => ticket.clone().prop_map(Op::Note),
        1 => macro_definition_strategy().prop_map(|(c, m, s, p)| Op::CreateMacro(c, m, s, p)),
        1 => macro_definition_strategy().prop_map(|(c, m, s, p)| Op::UpdateMacro(c, m, s, p)),
        1 => (company.clone(), macro_id.clone()).prop_map(|(c, m)| Op::DeleteMacro(c, m)),
        3 => (ticket, company, macro_id).prop_map(|(t, c, m)| Op::ApplyMacro(t, c, m)),
    ]
}

/// The spec's invariants over the code's own state, after a step.
fn check_invariants(
    world: &World,
    before: &[(String, Option<TicketStatus>)],
    before_companies: &[(String, Option<CompanyStatus>)],
) {
    for (ticket, was) in before {
        let now = world.ticket(ticket).status;
        match (*was, now) {
            (Some(from), Some(to)) if from != to => {
                assert!(
                    TICKET_EDGES.contains(&(from, to)),
                    "{ticket}: {from:?} -> {to:?} is not a declared edge"
                );
                assert!(
                    !TICKET_TERMINAL.contains(&from),
                    "{ticket} left terminal {from:?}"
                );
            }
            (Some(from), None) => panic!("{ticket} stopped existing from {from:?}"),
            (None, Some(to)) => assert_eq!(to, Open, "{ticket} was created {to:?}, not open"),
            _ => {}
        }
    }
    for (company, was) in before_companies {
        let now = world.company(company);
        match (*was, now) {
            (Some(from), Some(to)) if from != to => {
                assert!(
                    COMPANY_EDGES.contains(&(from, to)),
                    "{company}: {from:?} -> {to:?}"
                );
            }
            (None, Some(to)) => {
                assert_eq!(to, CompanyStatus::Trialing, "{company} signed up {to:?}")
            }
            (Some(from), None) => panic!("{company} stopped existing from {from:?}"),
            _ => {}
        }
    }
    // Per-ticket history facts the spec's fields imply.
    for ticket in TICKETS {
        let count = |pred: &dyn Fn(&HelpdeskEvent) -> bool| {
            world.history.iter().filter(|e| pred(e)).count()
        };
        let created =
            count(&|e| matches!(e, HelpdeskEvent::TicketCreated(p) if p.ticket_id == *ticket));
        let escalations =
            count(&|e| matches!(e, HelpdeskEvent::TicketEscalated(p) if p.ticket_id == *ticket));
        let ratings =
            count(&|e| matches!(e, HelpdeskEvent::TicketRated(p) if p.ticket_id == *ticket));
        let breaches = |kind: SlaKind| {
            count(
                &|e| matches!(e, HelpdeskEvent::TicketSlaBreached(p) if p.ticket_id == *ticket && p.sla == kind),
            )
        };
        // A ticket is created once - by any company (weeded 2026-10-12).
        assert!(created <= 1, "{ticket} created {created} times");
        // `escalated` is a once-only fact; one rating per ticket; each SLA
        // breached at most once.
        assert!(escalations <= 1, "{ticket} escalated {escalations} times");
        assert!(ratings <= 1, "{ticket} rated {ratings} times");
        for kind in [SlaKind::FirstResponse, SlaKind::Resolution] {
            assert!(breaches(kind) <= 1, "{ticket} breached {kind:?} twice");
        }
        let facts = world.ticket(ticket);
        assert_eq!(facts.escalated, escalations == 1);
        assert_eq!(facts.rated, ratings == 1);
    }
    // StaffMergesTickets: requires primary.company = duplicate.company.
    for event in &world.history {
        if let HelpdeskEvent::TicketsMerged(p) = event {
            assert_eq!(
                world.ticket(&p.primary_ticket_id).company_id,
                world.ticket(&p.duplicate_ticket_id).company_id
            );
            assert_ne!(p.primary_ticket_id, p.duplicate_ticket_id);
        }
    }
    // An agent's `StaffWorkload` is exactly their open tickets: assigned
    // to them, in progress or waiting - what `least_loaded` counts.
    for staff in ALL_STAFF {
        let open: std::collections::BTreeSet<String> = TICKETS
            .iter()
            .filter(|t| {
                let facts = world.ticket(t);
                facts.assignee_id.as_deref() == Some(*staff)
                    && matches!(facts.status, Some(InProgress | WaitingOnCustomer))
            })
            .map(|t| t.to_string())
            .collect();
        assert_eq!(
            world.workload(staff).open_ticket_ids,
            open,
            "{staff}'s load"
        );
    }
}

fn assert_agrees(model: &Model, world: &World) {
    for ticket in TICKETS {
        let facts = world.ticket(ticket);
        match model.ticket(ticket) {
            None => assert_eq!(facts.status, None, "{ticket}"),
            Some(spec) => {
                assert_eq!(facts.status, Some(spec.status), "{ticket} status");
                assert_eq!(
                    facts.company_id.as_deref(),
                    Some(spec.company),
                    "{ticket} company"
                );
                assert_eq!(
                    facts.requester_id.as_deref(),
                    Some(spec.requester),
                    "{ticket} requester"
                );
                assert_eq!(
                    facts.current_priority(),
                    Some(spec.priority),
                    "{ticket} priority"
                );
                assert_eq!(facts.escalated, spec.escalated, "{ticket} escalated");
                assert_eq!(facts.rated, spec.rating.is_some(), "{ticket} rated");
                assert_eq!(
                    facts.assignee_id.as_deref(),
                    spec.assigned_to,
                    "{ticket} assigned_to"
                );
                assert_eq!(
                    facts.first_responded, spec.first_responded,
                    "{ticket} first_responded_at"
                );
                assert_eq!(
                    facts.first_response_breached, spec.first_response_breached,
                    "{ticket} first_response_breached"
                );
                assert_eq!(
                    facts.resolution_breached, spec.resolution_breached,
                    "{ticket} resolution_breached"
                );
            }
        }
    }
    for company in COMPANIES {
        assert_eq!(world.company(company), model.company(company), "{company}");
    }
}

proptest! {
    #![proptest_config(ProptestConfig { cases: 1024, ..ProptestConfig::default() })]

    #[test]
    fn random_command_sequences_follow_the_spec(ops in proptest::collection::vec(op(), 1..80)) {
        let mut world = World::default();
        let mut model = Model::default();
        for op in &ops {
            let before: Vec<_> = TICKETS.iter().map(|t| (t.to_string(), world.ticket(t).status)).collect();
            let before_companies: Vec<_> = COMPANIES.iter().map(|c| (c.to_string(), world.company(c))).collect();
            let ticket_before = match op {
                Op::Escalate(t) | Op::Breach(t, _) | Op::ApplyMacro(t, _, _) | Op::AutoAssign(t, _) => model.ticket(t).cloned(),
                _ => None,
            };
            let chosen = match op {
                Op::AutoAssign(_, candidates) => model.least_loaded(candidates),
                _ => None,
            };

            let decision = world.run(op);
            let spec_accepts = model.step(op);
            prop_assert_eq!(accepted(&decision), spec_accepts, "{:?}: code {:?}", op, decision);

            if let (true, Some(before)) = (spec_accepts, &ticket_before) {
                // `ensures: ticket.priority = escalate(ticket.priority)` -
                // from its current priority, a macro's change included.
                if let Some(escalation) = event(&decision, "TicketEscalated") {
                    let escalation: TicketEscalatedPayload = serde_json::from_value(escalation.clone()).unwrap();
                    prop_assert!(!before.escalated);
                    prop_assert_eq!(escalation.previous_priority, before.priority);
                    prop_assert_eq!(escalation.new_priority, spec_escalate(before.priority));
                }
                // A breach escalates only a ticket not escalated yet.
                if let Op::Breach(..) = op {
                    prop_assert_eq!(event(&decision, "TicketEscalated").is_some(), !before.escalated);
                }
                // StaffAppliesMacro: a priority change only when it differs.
                if let Some(change) = event(&decision, "TicketPriorityChanged") {
                    let change: TicketPriorityChangedPayload = serde_json::from_value(change.clone()).unwrap();
                    prop_assert_eq!(change.previous_priority, before.priority);
                    prop_assert_ne!(change.new_priority, before.priority);
                }
                // StaffAutoAssignsTicket: the least-loaded candidate.
                if let Op::AutoAssign(..) = op {
                    let assigned = event(&decision, "TicketAssigned").expect("an assignment");
                    prop_assert_eq!(assigned["staff_id"].as_str(), chosen);
                }
            }
            if let (Op::Create { ticket, priority, .. }, CommandDecision::Accepted { events }) = (op, &decision) {
                let created: TicketCreatedPayload = serde_json::from_value(events[0].payload.clone()).unwrap();
                // UrgentTicketNeedsImmediateAttention: a lead is paged at
                // creation iff the ticket is urgent.
                prop_assert_eq!(
                    alerting::evaluate_ticket_created(&created).is_some(),
                    *priority == TicketPriority::Urgent
                );
                // `sla: target_for(sla_policy_for(company), priority)`.
                prop_assert_eq!(created.sla.as_ref(), Some(&model.ticket(ticket).unwrap().sla));
            }

            check_invariants(&world, &before, &before_companies);
            assert_agrees(&model, &world);
        }
    }
}

// --- config defaults ---

#[test]
fn config_defaults_match_the_spec() {
    // `config.trial_duration = 1.month`, `config.auto_close_after = 7.days`,
    // unless overridden for a demo.
    if std::env::var_os("TRIAL_DURATION_DAYS").is_none() {
        assert_eq!(scheduling::trial_duration(), chrono::Duration::days(30));
    }
    if std::env::var_os("AUTO_CLOSE_AFTER_DAYS").is_none() {
        assert_eq!(scheduling::auto_close_after(), chrono::Duration::days(7));
    }
    // `config.unhandled_alert_after = 4.hours` is inclusive at the boundary.
    let now = chrono::Utc::now();
    let four_hours = chrono::Duration::hours(4);
    assert!(alerting::is_overdue(now - four_hours, now, four_hours));
    assert!(!alerting::is_overdue(
        now - four_hours + chrono::Duration::seconds(1),
        now,
        four_hours
    ));
}

// --- activity.allium ---

#[derive(Debug, Clone)]
enum ActivityOp {
    Record {
        company: &'static str,
        kind: activity::PersonKind,
        person: &'static str,
        day: &'static str,
    },
    Decline {
        company: &'static str,
    },
}

fn activity_op() -> impl Strategy<Value = ActivityOp> {
    let company = proptest::sample::select(&["acme", "globex"][..]);
    prop_oneof![
        3 => (
            company.clone(),
            prop_oneof![Just(activity::PersonKind::Customer), Just(activity::PersonKind::Staff)],
            proptest::sample::select(&["alice", "bob"][..]),
            proptest::sample::select(&["2026-10-01T00:00:00Z", "2026-10-02T00:00:00Z"][..]),
        )
            .prop_map(|(company, kind, person, day)| ActivityOp::Record { company, kind, person, day }),
        1 => company.prop_map(|company| ActivityOp::Decline { company }),
    ]
}

fn rebuild_activity(spec: &EventSpec) -> activity::ActivityEvent {
    match spec.event_type.as_str() {
        "DailyActivityRecorded" => activity::ActivityEvent::DailyActivityRecorded(
            serde_json::from_value(spec.payload.clone()).unwrap(),
        ),
        "CompanyEngagementDeclined" => activity::ActivityEvent::CompanyEngagementDeclined(
            serde_json::from_value(spec.payload.clone()).unwrap(),
        ),
        other => panic!("unexpected event type {other}"),
    }
}

proptest! {
    /// `DailyActivityIsRecorded`: once per company+person+day;
    /// `EngagementDeclineIsRecorded`: once per company, ever.
    #[test]
    fn activity_is_recorded_once_per_day_and_decline_once_per_company(ops in proptest::collection::vec(activity_op(), 1..40)) {
        let mut history: Vec<activity::ActivityEvent> = Vec::new();
        let mut days: Vec<(&str, &str, &str)> = Vec::new();
        let mut flagged: Vec<&str> = Vec::new();
        for op in &ops {
            let (decision, spec_accepts) = match *op {
                ActivityOp::Record { company, kind, person, day } => {
                    let spec = !days.contains(&(company, person, day));
                    if spec {
                        days.push((company, person, day));
                    }
                    let decision = <activity::RecordDailyActivity as CommandType>::decide(
                        &activity::RecordDailyActivityPayload {
                            company_id: company.into(),
                            person_kind: kind,
                            person_subject: person.into(),
                            day: day.into(),
                        },
                        &history,
                    );
                    (decision, spec)
                }
                ActivityOp::Decline { company } => {
                    let spec = !flagged.contains(&company);
                    if spec {
                        flagged.push(company);
                    }
                    let decision = <activity::RecordEngagementDecline as CommandType>::decide(
                        &activity::RecordEngagementDeclinePayload {
                            company_id: company.into(),
                            flagged_at: "2026-10-12T00:00:00Z".into(),
                        },
                        &history,
                    );
                    (decision, spec)
                }
            };
            prop_assert_eq!(accepted(&decision), spec_accepts, "{:?}", op);
            if let CommandDecision::Accepted { events } = &decision {
                history.extend(events.iter().map(rebuild_activity));
            }
        }
    }
}

// --- marketing.allium ---

#[test]
fn helpdesk_and_activity_facts_are_routed_into_marketing() {
    // TrialLapseIsRoutedFromHelpdesk / TrialConversionIsRoutedFromHelpdesk:
    // the company carried over, the time stamped at route time.
    let lapse = marketing::HelpdeskExpiryToTrialLapse::route(&CompanyExpiredPayload {
        company_id: "acme".into(),
    })
    .expect("every expiry is routed");
    assert_eq!(lapse.company_id, "acme");
    assert!(chrono::DateTime::parse_from_rfc3339(&lapse.lapsed_at).is_ok());
    let conversion =
        marketing::HelpdeskActivationToTrialConversion::route(&CompanyActivatedPayload {
            company_id: "acme".into(),
        })
        .expect("every activation is routed");
    assert_eq!(conversion.company_id, "acme");
    assert!(chrono::DateTime::parse_from_rfc3339(&conversion.converted_at).is_ok());
    // EngagementDeclineIsRoutedFromActivity: flagged_at carried through.
    let flag = marketing::ActivityEngagementDeclineToMarketingFlag::route(
        &activity::CompanyEngagementDeclinedPayload {
            company_id: "acme".into(),
            flagged_at: "2026-10-01T00:00:00Z".into(),
        },
    )
    .expect("every decline is routed");
    assert_eq!(
        (flag.company_id.as_str(), flag.flagged_at.as_str()),
        ("acme", "2026-10-01T00:00:00Z")
    );
}

#[test]
fn marketing_records_every_routed_fact_including_repeats() {
    // TrialLapseIsRecorded/TrialConversionIsRecorded/EngagementDeclineIsFlagged:
    // no requires - a company can lapse, reactivate and lapse again.
    let mut history = Vec::new();
    for round in 0..3 {
        let at = format!("2026-10-0{}T00:00:00Z", round + 1);
        let decisions = [
            <marketing::RecordTrialLapse as CommandType>::decide(
                &marketing::RecordTrialLapsePayload {
                    company_id: "acme".into(),
                    lapsed_at: at.clone(),
                },
                &history,
            ),
            <marketing::RecordTrialConversion as CommandType>::decide(
                &marketing::RecordTrialConversionPayload {
                    company_id: "acme".into(),
                    converted_at: at.clone(),
                },
                &history,
            ),
            <marketing::RecordEngagementDecline as CommandType>::decide(
                &marketing::RecordEngagementDeclinePayload {
                    company_id: "acme".into(),
                    flagged_at: at.clone(),
                },
                &history,
            ),
        ];
        for (decision, (event_type, field)) in decisions.iter().zip([
            ("TrialLapsed", "lapsed_at"),
            ("TrialConverted", "converted_at"),
            ("EngagementDeclineFlagged", "flagged_at"),
        ]) {
            let CommandDecision::Accepted { events } = decision else {
                panic!("{event_type} rejected: {decision:?}")
            };
            assert_eq!(events.len(), 1);
            assert_eq!(events[0].event_type, event_type);
            assert_eq!(events[0].payload["company_id"], "acme");
            assert_eq!(events[0].payload[field], at.as_str());
            let payload = events[0].payload.clone();
            history.push(match event_type {
                "TrialLapsed" => {
                    marketing::MarketingEvent::TrialLapsed(serde_json::from_value(payload).unwrap())
                }
                "TrialConverted" => marketing::MarketingEvent::TrialConverted(
                    serde_json::from_value(payload).unwrap(),
                ),
                _ => marketing::MarketingEvent::EngagementDeclineFlagged(
                    serde_json::from_value(payload).unwrap(),
                ),
            });
        }
    }
}

// --- knowledge_base.allium ---

#[test]
fn every_resolution_is_routed_as_an_article_suggestion() {
    // ArticleSuggestionIsRoutedFromHelpdesk.
    let suggestion = kb::HelpdeskResolutionToArticleSuggestion::route(&TicketResolvedPayload {
        ticket_id: "t1".into(),
        company_id: "acme".into(),
        resolution: Some(2),
        requester_id: Some(REQUESTER.into()),
        assignee_id: Some(STAFF.into()),
    })
    .expect("every resolution is routed");
    assert_eq!(
        (
            suggestion.ticket_id.as_str(),
            suggestion.company_id.as_str()
        ),
        ("t1", "acme")
    );
    assert!(chrono::DateTime::parse_from_rfc3339(&suggestion.suggested_at).is_ok());
}

#[derive(Debug, Clone)]
enum KbOp {
    Suggest(&'static str),
    Dismiss(&'static str),
    /// Article, the suggestion it's drafted from, whether the title is
    /// blank.
    Draft(&'static str, Option<&'static str>, bool),
    Edit(&'static str, bool),
    Publish(&'static str),
}

fn kb_op() -> impl Strategy<Value = KbOp> {
    let ticket = proptest::sample::select(&["t1", "t2"][..]);
    let article = proptest::sample::select(&["a1", "a2"][..]);
    let blank = proptest::bool::weighted(0.15);
    prop_oneof![
        2 => ticket.clone().prop_map(KbOp::Suggest),
        1 => ticket.clone().prop_map(KbOp::Dismiss),
        2 => (article.clone(), proptest::option::of(ticket), blank)
            .prop_map(|(a, t, b)| KbOp::Draft(a, t, b)),
        2 => (article.clone(), blank).prop_map(|(a, b)| KbOp::Edit(a, b)),
        2 => article.prop_map(KbOp::Publish),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SpecSuggestion {
    Pending,
    Drafted,
    Dismissed,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SpecArticle {
    Draft,
    Published,
    Edited,
}

fn rebuild_kb(spec: &EventSpec) -> kb::KnowledgeBaseEvent {
    let payload = spec.payload.clone();
    match spec.event_type.as_str() {
        "ArticleSuggested" => {
            kb::KnowledgeBaseEvent::ArticleSuggested(serde_json::from_value(payload).unwrap())
        }
        "SuggestionDismissed" => {
            kb::KnowledgeBaseEvent::SuggestionDismissed(serde_json::from_value(payload).unwrap())
        }
        "ArticleDrafted" => {
            kb::KnowledgeBaseEvent::ArticleDrafted(serde_json::from_value(payload).unwrap())
        }
        "ArticleEdited" => {
            kb::KnowledgeBaseEvent::ArticleEdited(serde_json::from_value(payload).unwrap())
        }
        "ArticlePublished" => {
            kb::KnowledgeBaseEvent::ArticlePublished(serde_json::from_value(payload).unwrap())
        }
        other => panic!("unexpected event type {other}"),
    }
}

proptest! {
    /// knowledge_base.allium's rules as a model: one suggestion per
    /// ticket, drafted or dismissed only while pending; an article id
    /// used once; publishing only a draft or an edit, each publication
    /// one revision on.
    #[test]
    fn the_knowledge_base_follows_its_rules(ops in proptest::collection::vec(kb_op(), 1..40)) {
        let mut history: Vec<kb::KnowledgeBaseEvent> = Vec::new();
        let mut suggestions: Vec<(&str, SpecSuggestion)> = Vec::new();
        let mut articles: Vec<(&str, SpecArticle, u32)> = Vec::new();
        for op in &ops {
            let title = |blank: bool| if blank { "  ".to_string() } else { "How to log in".to_string() };
            let suggestion = |s: &Vec<(&str, SpecSuggestion)>, t: &str| s.iter().find(|(id, _)| *id == t).map(|(_, st)| *st);
            let article = |a: &Vec<(&str, SpecArticle, u32)>, id: &str| a.iter().find(|(x, _, _)| *x == id).map(|(_, s, r)| (*s, *r));
            let (decision, spec) = match *op {
                // ArticleIsSuggested
                KbOp::Suggest(t) => {
                    let ok = suggestion(&suggestions, t).is_none();
                    if ok { suggestions.push((t, SpecSuggestion::Pending)); }
                    (<kb::SuggestArticle as CommandType>::decide(&kb::SuggestArticlePayload {
                        ticket_id: t.into(), company_id: "acme".into(), suggested_at: "2026-10-12T00:00:00Z".into(),
                    }, &history), ok)
                }
                // StaffDismissesSuggestion
                KbOp::Dismiss(t) => {
                    let ok = suggestion(&suggestions, t) == Some(SpecSuggestion::Pending);
                    if ok { suggestions.iter_mut().find(|(id, _)| *id == t).unwrap().1 = SpecSuggestion::Dismissed; }
                    (<kb::DismissSuggestion as CommandType>::decide(&kb::DismissSuggestionPayload {
                        ticket_id: t.into(), reason: Some("not general".into()),
                    }, &history), ok)
                }
                // StaffDraftsArticle
                KbOp::Draft(a, from, blank) => {
                    let ok = !blank
                        && article(&articles, a).is_none()
                        && from.is_none_or(|t| suggestion(&suggestions, t) == Some(SpecSuggestion::Pending));
                    if ok {
                        articles.push((a, SpecArticle::Draft, 0));
                        if let Some(t) = from {
                            suggestions.iter_mut().find(|(id, _)| *id == t).unwrap().1 = SpecSuggestion::Drafted;
                        }
                    }
                    (<kb::DraftArticle as CommandType>::decide(&kb::DraftArticlePayload {
                        article_id: a.into(), from_ticket_id: from.map(String::from), title: title(blank), body: "Use SSO.".into(),
                    }, &history), ok)
                }
                // StaffEditsArticle
                KbOp::Edit(a, blank) => {
                    let ok = !blank && article(&articles, a).is_some();
                    if ok {
                        let entry = articles.iter_mut().find(|(x, _, _)| *x == a).unwrap();
                        if entry.1 == SpecArticle::Published { entry.1 = SpecArticle::Edited; }
                    }
                    (<kb::EditArticle as CommandType>::decide(&kb::EditArticlePayload {
                        article_id: a.into(), title: title(blank), body: "Use SSO, then retry.".into(),
                    }, &history), ok)
                }
                // StaffPublishesArticle
                KbOp::Publish(a) => {
                    let ok = matches!(article(&articles, a), Some((SpecArticle::Draft | SpecArticle::Edited, _)));
                    let decision = <kb::PublishArticle as CommandType>::decide(&kb::PublishArticlePayload {
                        article_id: a.into(),
                    }, &history);
                    if ok {
                        let entry = articles.iter_mut().find(|(x, _, _)| *x == a).unwrap();
                        entry.1 = SpecArticle::Published;
                        entry.2 += 1;
                        // `revision = revision + 1`.
                        prop_assert_eq!(&event(&decision, "ArticlePublished").unwrap()["revision"], &serde_json::json!(entry.2));
                    }
                    (decision, ok)
                }
            };
            prop_assert_eq!(accepted(&decision), spec, "{:?}: code {:?}", op, decision);
            history.extend(events(&decision).iter().map(rebuild_kb));
        }
    }
}
