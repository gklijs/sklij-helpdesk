//! Load-based auto-assignment (issue #44): `AutoAssignTicket` hands an
//! open ticket to whichever of the named agents has the fewest open
//! tickets, read from `StaffWorkload`.
//!
//! The DCB showcase `MergeTickets` started: there is no "agent" aggregate
//! owning that agent's tickets, and no "team" aggregate owning the agents.
//! The rule "pick the least-loaded candidate" reads one tag, `staff`,
//! across every ticket each candidate works - one `StaffWorkload` snapshot
//! item per candidate - next to the ticket's own `TicketSnapshot`. Any
//! event carrying one of those `staff` tags that commits between the
//! command's read and its append is a conflict, and skilj decides again
//! with it folded in (`db::submit_command`'s redispatch). So two
//! auto-assignments racing over the same agents never both pick the
//! agent that was least loaded before either of them: the second one sees
//! the first one's `TicketAssigned` and picks again.
//!
//! What carries the `staff` tag: `TicketAssigned` (on `staff_id`), and the
//! three events that change an assigned ticket's load - `TicketResolved`
//! and `TicketReopened` (on `assignee_id`) and `TicketsMerged` (on
//! `duplicate_assignee_id`), stamped from `TicketFacts::assignee_id`.
//! `TicketClosed` doesn't need it: only a resolved ticket closes, and a
//! resolved one is already off its agent's load. `TicketInfoRequested`/
//! `TicketCustomerResponded` neither: waiting on the customer is still
//! the agent's ticket. There is no reassignment: `AssignTicket` (and this
//! command) only take an `open` ticket.
//!
//! Events stored before these tags existed don't carry them - skilj
//! derives an event's tags once, when it is appended, and a later
//! `tag_mappings()` never re-tags history. So an assignment made before
//! this was deployed is not in its agent's `StaffWorkload`, and the agent
//! looks that much less loaded until those tickets are resolved (their
//! resolution then removes a ticket the set never held, a no-op - which
//! is why the state is a set of ticket ids rather than a counter that
//! could go negative). Re-tagging stored events is not something skilj
//! offers; a re-append migration would be the only fix, and for a
//! queue that turns over in days it isn't worth one.
//!
//! The trigger is a staff command, not a reaction to `TicketCreated`:
//! which agents are eligible (who is on shift, on which queue) is
//! something this helpdesk has no model of, so the caller - a lead's
//! "auto-assign" button, or a dispatcher bot that knows the rota - names
//! the candidates. A watcher submitting it on every `TicketCreated` would
//! need that model first, and could then submit this command unchanged.

use std::collections::BTreeSet;

use super::*;

/// The tag key `StaffWorkload` is keyed by - a staff member's id.
pub const STAFF_TAG_KEY: &str = "staff";

/// One agent's open tickets: assigned to them, and not since resolved or
/// merged away. `WaitingOnCustomer` counts - still theirs to finish.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct StaffLoad {
    pub open_ticket_ids: BTreeSet<String>,
}

impl StaffLoad {
    pub fn open_tickets(&self) -> usize {
        self.open_ticket_ids.len()
    }
}

/// `StaffLoad`, kept per agent: every event tagged `staff: <id>` folded
/// in. See this module's doc comment for which events carry the tag.
pub struct StaffWorkload;

#[auto_register(BOUNDED_CONTEXT)]
impl Snapshot for StaffWorkload {
    type State = StaffLoad;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "StaffWorkload";
    const TAG_KEY: &'static str = STAFF_TAG_KEY;
    /// None: none of the staff-tagged events carries a `company` tag to
    /// derive an owner from, so a company-scoped Role can't
    /// `inspectSnapshot` these rows (fails closed); deciding from them is
    /// unaffected.
    const OWNER_TAG_KEY: Option<&'static str> = None;
    const VERSION: u64 = 1;
    /// Compares the payload's agent with `tag_value` rather than trusting
    /// the tag alone, so the fold is right whatever else an event is
    /// tagged with.
    fn fold_for(state: &mut Self::State, tag_value: &str, event: &Self::Event) {
        let is = |agent: &Option<String>| agent.as_deref() == Some(tag_value);
        match event {
            HelpdeskEvent::TicketAssigned(p) if p.staff_id == tag_value => {
                state.open_ticket_ids.insert(p.ticket_id.clone());
            }
            HelpdeskEvent::TicketReopened(p) if is(&p.assignee_id) => {
                state.open_ticket_ids.insert(p.ticket_id.clone());
            }
            HelpdeskEvent::TicketResolved(p) if is(&p.assignee_id) => {
                state.open_ticket_ids.remove(&p.ticket_id);
            }
            HelpdeskEvent::TicketsMerged(p) if is(&p.duplicate_assignee_id) => {
                state.open_ticket_ids.remove(&p.duplicate_ticket_id);
            }
            _ => {}
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct AutoAssignTicketPayload {
    pub ticket_id: String,
    /// The agents eligible for this ticket. Ties go to the one listed
    /// first, so the order is the caller's rotation order; a repeat is
    /// ignored.
    pub candidate_staff_ids: Vec<String>,
}

pub struct AutoAssignTicket;

/// Not in the original spec - see this module's doc comment, and
/// `specs/skilj-helpdesk.allium`'s `rule StaffAutoAssignsTicket`. Emits
/// the same `TicketAssigned` `AssignTicket` does, so everything
/// downstream (projections, alerter, `TicketFacts`) is unchanged.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for AutoAssignTicket {
    type Payload = AutoAssignTicketPayload;
    type Event = HelpdeskEvent;
    const NAME: &'static str = "AutoAssignTicket";
    fn tag_mappings() -> Vec<TagMapping> {
        [ticket_tag(), staff_tag("candidate_staff_ids")].concat()
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    /// The ticket's `TicketSnapshot`, and one `StaffWorkload` item per
    /// candidate (a list-valued field derives one item per value).
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![
            QueryItemMapping::snapshot::<TicketSnapshot>("ticket_id"),
            QueryItemMapping::snapshot::<StaffWorkload>("candidate_staff_ids"),
        ]
    }
    fn decide_with(
        payload: &Self::Payload,
        decision: &Decision<'_, Self::Event>,
    ) -> CommandDecision {
        let candidates = distinct(&payload.candidate_staff_ids);
        if candidates.is_empty() {
            return CommandDecision::Rejected {
                reason: "no candidate staff named - nobody to assign the ticket to".into(),
                kind: "no_candidates".into(),
            };
        }
        let mut loads = Vec::with_capacity(candidates.len());
        for candidate in &candidates {
            match decision.snapshot::<StaffWorkload>(candidate) {
                Some(load) => loads.push((*candidate, load.open_tickets())),
                None => {
                    return CommandDecision::Rejected {
                        reason: format!("no readable StaffWorkload state for staff {candidate}"),
                        kind: "snapshot_unreadable".into(),
                    }
                }
            }
        }
        decide_on_ticket(decision, &payload.ticket_id, |facts| {
            Self::decide_on(payload, facts, &loads)
        })
    }
}

impl AutoAssignTicket {
    fn decide_on(
        payload: &AutoAssignTicketPayload,
        facts: &TicketFacts,
        loads: &[(&str, usize)],
    ) -> CommandDecision {
        match facts.status {
            None => CommandDecision::Rejected {
                reason: format!("ticket {} does not exist", payload.ticket_id),
                kind: "ticket_not_found".into(),
            },
            Some(TicketStatus::Open) => {
                let staff_id = least_loaded(loads).expect("candidates checked non-empty");
                CommandDecision::Accepted {
                    events: vec![EventSpec {
                        event_type: "TicketAssigned".into(),
                        payload: serde_json::json!({
                            "ticket_id": payload.ticket_id,
                            "company_id": facts.company_id(),
                            "staff_id": staff_id,
                        }),
                    }],
                }
            }
            Some(other) => CommandDecision::Rejected {
                reason: format!(
                    "ticket {} is {other:?}, not open - only an open ticket can be assigned",
                    payload.ticket_id
                ),
                kind: "ticket_not_open".into(),
            },
        }
    }
}

/// `ids` in order, each once.
fn distinct(ids: &[String]) -> Vec<&str> {
    let mut seen = BTreeSet::new();
    ids.iter()
        .map(String::as_str)
        .filter(|id| seen.insert(*id))
        .collect()
}

/// The agent with the fewest open tickets; the earliest listed on a tie.
pub fn least_loaded<'a>(loads: &[(&'a str, usize)]) -> Option<&'a str> {
    loads
        .iter()
        .enumerate()
        .min_by_key(|(position, (_, open))| (*open, *position))
        .map(|(_, (staff_id, _))| *staff_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_least_loaded_wins_and_a_tie_goes_to_the_first_listed() {
        assert_eq!(least_loaded(&[("a", 2), ("b", 1), ("c", 1)]), Some("b"));
        assert_eq!(least_loaded(&[("a", 0), ("b", 0)]), Some("a"));
        assert_eq!(least_loaded(&[]), None);
    }

    #[test]
    fn a_repeated_candidate_counts_once_in_its_first_place() {
        let ids = ["b", "a", "b"].map(String::from);
        assert_eq!(distinct(&ids), ["b", "a"]);
    }
}
