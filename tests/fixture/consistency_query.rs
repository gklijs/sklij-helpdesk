//! Every command's `consistency_query()` against its `decide()` - all
//! but the ticket ones, whose query is `TicketSnapshot` items, checked
//! by `ticket_snapshot.rs`: from the
//! events its query selects, `decide()` must reach the decision it
//! reaches from every event carrying one of its tags - what skilj handed
//! it before the query (skilj docs/architecture.md §198).
//!
//! skilj-test-fixture hands `decide()` its given events as they are, so
//! the selection is made here, by skilj's rules: an item matches an
//! event of one of its types (any, when it names none) that carries all
//! of its tags; `latest`/`last(n)` keep the item's n newest matches; the
//! items are OR'd and the result is in sequence order, each event once.
//! Every prefix of each history is checked, so each is a state the
//! command could meet.

use serde_json::Value;
use skilj::{CommandType, EventType};
use skilj_core::shared::{CommandDecision, TagMapping};
use skilj_helpdesk::activity::{self, *};
use skilj_helpdesk::helpdesk::*;

use crate::events::*;

/// An event as its type name and payload, so a history can be selected
/// from and handed back to `decide()`. The event enums aren't `Clone`.
pub(crate) trait Described: Sized {
    fn describe(&self) -> (&'static str, Value);
    fn rebuild(event_type: &str, payload: &Value) -> Self;
    fn tag_mappings(event_type: &str) -> Vec<TagMapping>;
}

macro_rules! described {
    ($enum:ident { $($variant:ident),* $(,)? }) => {
        impl Described for $enum {
            fn describe(&self) -> (&'static str, Value) {
                match self {
                    $($enum::$variant(p) => (
                        <$variant as EventType>::NAME,
                        serde_json::to_value(p).unwrap(),
                    ),)*
                }
            }
            fn rebuild(event_type: &str, payload: &Value) -> Self {
                match event_type {
                    $(<$variant as EventType>::NAME => {
                        $enum::$variant(serde_json::from_value(payload.clone()).unwrap())
                    })*
                    other => unreachable!("{other}"),
                }
            }
            fn tag_mappings(event_type: &str) -> Vec<TagMapping> {
                match event_type {
                    $(<$variant as EventType>::NAME => <$variant as EventType>::tag_mappings(),)*
                    other => unreachable!("{other}"),
                }
            }
        }
    };
}

described!(HelpdeskEvent {
    CompanySignedUp,
    CompanyActivated,
    CompanyExpired,
    CompanyTenantProvisioned,
    CompanyLifecycleMirrored,
    TicketCreated,
    TicketAssigned,
    TicketResolved,
    TicketReopened,
    TicketInfoRequested,
    TicketCustomerResponded,
    TicketClosed,
    TicketEscalated,
    TicketsMerged,
    TicketRated,
    TicketInternalNoteAdded,
});

described!(ActivityEvent {
    DailyActivityRecorded,
    CompanyEngagementDeclined,
});

struct Stored {
    event_type: &'static str,
    payload: Value,
    tags: Vec<(String, String)>,
}

/// The tags `mappings` derive from `payload`. Every mapped field here is
/// a plain string.
pub(crate) fn derive(mappings: &[TagMapping], payload: &Value) -> Vec<(String, String)> {
    mappings
        .iter()
        .filter_map(|m| {
            let value = payload.get(m.field.to_string())?.as_str()?;
            Some((m.key.to_string(), value.to_string()))
        })
        .collect()
}

fn store<E: Described>(history: &[E]) -> Vec<Stored> {
    history
        .iter()
        .map(|event| {
            let (event_type, payload) = event.describe();
            let tags = derive(&E::tag_mappings(event_type), &payload);
            Stored {
                event_type,
                payload,
                tags,
            }
        })
        .collect()
}

fn rebuild<E: Described>(history: &[Stored], keep: &[bool]) -> Vec<E> {
    history
        .iter()
        .zip(keep)
        .filter(|(_, keep)| **keep)
        .map(|(s, _)| E::rebuild(s.event_type, &s.payload))
        .collect()
}

/// What skilj handed `decide()` without a query: every event carrying
/// any of the command's tags - as they were before the queries.
/// `CreateTicket` was tagged by company alone; its `ticket` tag is only
/// there for its query to look the ticket up by.
fn tagged<C: CommandType>(history: &[Stored], payload: &Value) -> Vec<C::Event>
where
    C::Event: Described,
{
    let mut mappings = C::tag_mappings();
    if C::NAME == CreateTicket::NAME {
        mappings.retain(|m| m.key == "company");
    }
    let tags = derive(&mappings, payload);
    let keep: Vec<bool> = history
        .iter()
        .map(|s| s.tags.iter().any(|t| tags.contains(t)))
        .collect();
    rebuild(history, &keep)
}

/// What the command's query selects.
fn queried<C: CommandType>(history: &[Stored], payload: &Value) -> (Vec<C::Event>, usize)
where
    C::Event: Described,
{
    let mut keep = vec![false; history.len()];
    for item in C::consistency_query() {
        let tags = derive(&item.tag_mappings, payload);
        assert_eq!(tags.len(), item.tag_mappings.len(), "{}", C::NAME);
        let matches: Vec<usize> = (0..history.len())
            .filter(|&i| {
                let s = &history[i];
                (item.event_types.is_empty() || item.event_types.iter().any(|t| t == s.event_type))
                    && tags.iter().all(|t| s.tags.contains(t))
            })
            .collect();
        let start = item
            .latest
            .map_or(0, |n| matches.len().saturating_sub(n as usize));
        for &i in &matches[start..] {
            keep[i] = true;
        }
    }
    let count = keep.iter().filter(|k| **k).count();
    (rebuild(history, &keep), count)
}

/// `CommandDecision` has no `PartialEq`; its `Debug` output covers every
/// field, event payloads included.
fn same(a: &CommandDecision, b: &CommandDecision) -> bool {
    format!("{a:?}") == format!("{b:?}")
}

/// Checks `payload` at every prefix of `history`, and returns how many
/// events the query selected at most against how many the tags did.
fn assert_query_agrees<C: CommandType>(payload: &C::Payload, history: &[C::Event]) -> (usize, usize)
where
    C::Event: Described,
{
    assert!(
        !C::consistency_query().is_empty(),
        "{} has no query",
        C::NAME
    );
    let json = serde_json::to_value(payload).unwrap();
    let stored = store(history);
    let (mut most_queried, mut most_tagged) = (0, 0);
    for end in 0..=stored.len() {
        let prefix = &stored[..end];
        let tagged = tagged::<C>(prefix, &json);
        let (queried, count) = queried::<C>(prefix, &json);
        most_queried = most_queried.max(count);
        most_tagged = most_tagged.max(tagged.len());
        let (by_tags, by_query) = (C::decide(payload, &tagged), C::decide(payload, &queried));
        assert!(
            same(&by_tags, &by_query),
            "{} after {end} of {} events, {json}:\n  tags:  {by_tags:?}\n  query: {by_query:?}",
            C::NAME,
            stored.len(),
        );
    }
    (most_queried, most_tagged)
}

/// Two companies; tickets in every status, a long back-and-forth with
/// notes, a reopen, both sides of a merge, and a ticket id that both
/// companies use.
fn helpdesk_history() -> Vec<HelpdeskEvent> {
    let mut h = vec![
        signed_up("acme"),
        signed_up("globex"),
        created("t1", "acme", TicketPriority::Medium),
        created("t2", "acme", TicketPriority::Low),
        created("t3", "acme", TicketPriority::High),
        tenant_provisioned("acme", "tenant-acme"),
        assigned("t1", "acme", "staff-1"),
        created("t1", "globex", TicketPriority::Urgent),
    ];
    for round in 0..4 {
        h.push(info_requested("t1", "acme", &format!("question {round}")));
        h.push(note_added("t1", "acme", &format!("note {round}")));
        h.push(customer_responded("t1", "acme", &format!("answer {round}")));
    }
    h.extend([
        activated("acme"),
        escalated("t1", "acme", TicketPriority::Medium, TicketPriority::High),
        assigned("t2", "acme", "staff-2"),
        merged("t2", "t3", "acme"),
        note_added("t3", "acme", "merged away"),
        resolved("t1", "acme", 1),
        reopened("t1", "acme", 1),
        mirrored("acme", CompanyStatus::Active),
        resolved("t1", "acme", 2),
        rated("t1", "acme", 4),
        expired("globex"),
        resolved("t2", "acme", 1),
        closed("t1", "acme"),
        created("t4", "acme", TicketPriority::Medium),
        note_added("t4", "acme", "first look"),
        merged("t4", "t2", "acme"),
    ]);
    h
}

#[test]
fn every_helpdesk_command_decides_the_same_from_its_query() {
    let history = helpdesk_history();
    let companies = ["acme", "globex", "nobody"];
    let tickets = ["t1", "t2", "t3", "t4", "t5"];

    for company in companies {
        let company_id = company.to_string();
        assert_query_agrees::<SignUpCompany>(
            &SignUpCompanyPayload {
                company_id: company_id.clone(),
                name: "Acme".into(),
                contact_email: "ops@acme.example".into(),
            },
            &history,
        );
        assert_query_agrees::<RecordCompanyTenant>(
            &RecordCompanyTenantPayload {
                company_id: company_id.clone(),
                tenant_name: "tenant-x".into(),
            },
            &history,
        );
        for status in [
            CompanyStatus::Trialing,
            CompanyStatus::Active,
            CompanyStatus::Expired,
        ] {
            assert_query_agrees::<RecordTenantLifecycle>(
                &RecordTenantLifecyclePayload {
                    company_id: company_id.clone(),
                    status,
                    source_event_type: "test".into(),
                },
                &history,
            );
        }
        assert_query_agrees::<ConvertCompanyTrial>(
            &ConvertCompanyTrialPayload {
                company_id: company_id.clone(),
            },
            &history,
        );
        assert_query_agrees::<ExpireCompanyTrial>(
            &ExpireCompanyTrialPayload {
                company_id: company_id.clone(),
            },
            &history,
        );
        assert_query_agrees::<ReactivateCompany>(
            &ReactivateCompanyPayload {
                company_id: company_id.clone(),
            },
            &history,
        );
        for ticket in tickets {
            assert_query_agrees::<CreateTicket>(
                &CreateTicketPayload {
                    ticket_id: ticket.into(),
                    company_id: company_id.clone(),
                    requester_id: "customer-1".into(),
                    logged_by_staff_id: None,
                    title: "t".into(),
                    description: "d".into(),
                    priority: TicketPriority::Low,
                    requester_name: None,
                    requester_email: None,
                },
                &history,
            );
        }
    }
}

#[test]
fn the_queries_leave_out_what_a_long_history_is_made_of() {
    let history = helpdesk_history();
    // A new ticket reads the company's latest lifecycle event, not its
    // tickets.
    let (queried, tagged) = assert_query_agrees::<CreateTicket>(
        &CreateTicketPayload {
            ticket_id: "t9".into(),
            company_id: "acme".into(),
            requester_id: "customer-1".into(),
            logged_by_staff_id: None,
            title: "t".into(),
            description: "d".into(),
            priority: TicketPriority::Low,
            requester_name: None,
            requester_email: None,
        },
        &history,
    );
    assert_eq!(queried, 1);
    assert!(tagged > 5, "{tagged}");
}

fn activity(company: &str, person: &str, day: &str) -> ActivityEvent {
    ActivityEvent::DailyActivityRecorded(DailyActivityRecordedPayload {
        company_id: company.into(),
        person_kind: PersonKind::Customer,
        person_subject: person.into(),
        day: day.into(),
    })
}

fn declined(company: &str) -> ActivityEvent {
    ActivityEvent::CompanyEngagementDeclined(CompanyEngagementDeclinedPayload {
        company_id: company.into(),
        flagged_at: "2026-10-01T00:00:00Z".into(),
    })
}

#[test]
fn every_activity_command_decides_the_same_from_its_query() {
    let mut history = Vec::new();
    for day in ["2026-10-01", "2026-10-02", "2026-10-03"] {
        for (company, person) in [("c1", "p1"), ("c1", "p2"), ("c2", "p1"), ("c2", "p3")] {
            history.push(activity(company, person, day));
        }
    }
    history.insert(5, declined("c1"));
    history.push(declined("c1"));

    for company in ["c1", "c2", "c3"] {
        for person in ["p1", "p2", "p3"] {
            for day in ["2026-10-02", "2026-10-03", "2026-10-04"] {
                assert_query_agrees::<RecordDailyActivity>(
                    &RecordDailyActivityPayload {
                        company_id: company.into(),
                        person_kind: PersonKind::Staff,
                        person_subject: person.into(),
                        day: day.into(),
                    },
                    &history,
                );
            }
        }
        assert_query_agrees::<activity::RecordEngagementDecline>(
            &activity::RecordEngagementDeclinePayload {
                company_id: company.into(),
                flagged_at: "2026-10-05T00:00:00Z".into(),
            },
            &history,
        );
    }
    // p1 at c1: three days, not the company's 6 plus p1's 3 at c2.
    let (queried, tagged) = assert_query_agrees::<RecordDailyActivity>(
        &RecordDailyActivityPayload {
            company_id: "c1".into(),
            person_kind: PersonKind::Staff,
            person_subject: "p1".into(),
            day: "2026-10-04".into(),
        },
        &history,
    );
    assert_eq!((queried, tagged), (3, 11));
}

/// They read nothing, and say so.
#[test]
fn marketing_commands_read_nothing() {
    use skilj_core::shared::QueryItemMapping;
    use skilj_helpdesk::marketing::*;
    for items in [
        RecordTrialLapse::consistency_query(),
        RecordTrialConversion::consistency_query(),
        RecordEngagementDecline::consistency_query(),
    ] {
        assert_eq!(items, vec![QueryItemMapping::nothing()]);
    }
}
