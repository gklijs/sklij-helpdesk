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
use skilj_core::shared::{CommandDecision, QueryItemMapping, TagMapping};
use skilj_helpdesk::activity::{self, *};
use skilj_helpdesk::helpdesk::*;
use skilj_helpdesk::knowledge_base::*;

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
    MacroCreated,
    MacroUpdated,
    MacroDeleted,
    TicketStaffReplied,
    TicketPriorityChanged,
    CompanySlaPolicySet,
    TicketSlaBreached,
});

described!(ActivityEvent {
    DailyActivityRecorded,
    CompanyEngagementDeclined,
});

described!(KnowledgeBaseEvent {
    ArticleSuggested,
    SuggestionDismissed,
    ArticleDrafted,
    ArticleEdited,
    ArticlePublished,
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
/// any of the command's tags. For `CreateTicket` that includes the
/// `ticket` tag: a ticket id is used once across all companies (see
/// `CompanyFacts::apply`), so another company's `TicketCreated` under the
/// same id is part of what it decides from.
fn tagged<C: CommandType>(history: &[Stored], payload: &Value) -> Vec<C::Event>
where
    C::Event: Described,
{
    let tags = derive(&C::tag_mappings(), payload);
    let keep: Vec<bool> = history
        .iter()
        .map(|s| s.tags.iter().any(|t| tags.contains(t)))
        .collect();
    rebuild(history, &keep)
}

/// Which of `history` the non-snapshot items of `items` select - a
/// snapshot item's events reach `decide_with()` folded into its state,
/// not as events (see `ticket_snapshot.rs`).
fn keep_for(
    items: &[QueryItemMapping],
    history: &[Stored],
    payload: &Value,
    name: &str,
) -> Vec<bool> {
    let mut keep = vec![false; history.len()];
    for item in items.iter().filter(|item| item.snapshot.is_none()) {
        let tags = derive(&item.tag_mappings, payload);
        assert_eq!(tags.len(), item.tag_mappings.len(), "{name}");
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
    keep
}

/// The events `C`'s query hands `decide_with()` beside its snapshot
/// states - what `GivenTicket` gives a command whose query mixes both.
pub(crate) fn selected_events<C: CommandType>(
    history: &[C::Event],
    payload: &Value,
) -> Vec<C::Event>
where
    C::Event: Described,
{
    let stored = store(history);
    let keep = keep_for(&C::consistency_query(), &stored, payload, C::NAME);
    rebuild(&stored, &keep)
}

/// What the command's query selects.
fn queried<C: CommandType>(history: &[Stored], payload: &Value) -> (Vec<C::Event>, usize)
where
    C::Event: Described,
{
    let keep = keep_for(&C::consistency_query(), history, payload, C::NAME);
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
        sla_policy_set("acme", skilj_helpdesk::sla::SlaPolicy::default()),
        assigned("t1", "acme", "staff-1"),
        // Stored before ticket ids were global: `CreateTicket` now
        // rejects this, but a history may still hold one.
        created("t1", "globex", TicketPriority::Urgent),
    ];
    for round in 0..4 {
        h.push(info_requested("t1", "acme", &format!("question {round}")));
        h.push(note_added("t1", "acme", &format!("note {round}")));
        h.push(customer_responded("t1", "acme", &format!("answer {round}")));
    }
    h.extend([
        activated("acme"),
        macro_created("m1", "acme", None, None),
        macro_created("m1", "globex", Some(MacroStatus::Resolved), None),
        sla_policy_set("globex", skilj_helpdesk::sla::SlaPolicy::default()),
        escalated("t1", "acme", TicketPriority::Medium, TicketPriority::High),
        macro_created(
            "m2",
            "acme",
            Some(MacroStatus::WaitingOnCustomer),
            Some(TicketPriority::Urgent),
        ),
        staff_replied("t1", "acme", "on it"),
        priority_changed("t1", "acme", TicketPriority::High, TicketPriority::Low),
        macro_updated(
            "m1",
            "acme",
            Some(MacroStatus::Resolved),
            Some(TicketPriority::High),
        ),
        assigned("t2", "acme", "staff-2"),
        macro_created("m3", "acme", None, Some(TicketPriority::Low)),
        macro_deleted("m2", "acme"),
        merged("t2", "t3", "acme"),
        note_added("t3", "acme", "merged away"),
        resolved("t1", "acme", 1),
        reopened("t1", "acme", 1),
        mirrored("acme", CompanyStatus::Active),
        // A second policy for acme: only this, the latest, may count.
        sla_policy_set("acme", {
            let mut policy = skilj_helpdesk::sla::SlaPolicy::default();
            policy.low.first_response_minutes = 1;
            policy
        }),
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
        assert_query_agrees::<SetCompanySlaPolicy>(
            &SetCompanySlaPolicyPayload {
                company_id: company_id.clone(),
                policy: skilj_helpdesk::sla::SlaPolicy::default(),
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
fn every_macro_command_decides_the_same_from_its_query() {
    let history = helpdesk_history();
    for company in ["acme", "globex", "nobody"] {
        for macro_id in ["m1", "m2", "m3", "m4"] {
            let definition =
                macro_definition(macro_id, company, None, Some(TicketPriority::Urgent));
            assert_query_agrees::<CreateMacro>(&definition, &history);
            assert_query_agrees::<UpdateMacro>(&definition, &history);
            assert_query_agrees::<DeleteMacro>(
                &MacroDeletedPayload {
                    company_id: company.into(),
                    macro_id: macro_id.into(),
                    staff_id: "staff-1".into(),
                },
                &history,
            );
        }
    }
    // One macro's latest event, not the company's tickets and other macros.
    let (queried, tagged) =
        assert_query_agrees::<UpdateMacro>(&macro_definition("m1", "acme", None, None), &history);
    assert_eq!(queried, 1);
    assert!(tagged > 10, "{tagged}");
}

/// `ApplyMacro`'s query is a `TicketSnapshot` item plus the macro's
/// latest event. The snapshot item's state is the same either way, so
/// what's checked is the other half: from the macro's latest event,
/// `decide_with()` must reach the decision it reaches from every event
/// carrying the company's or the macro's tag.
#[test]
fn apply_macro_decides_the_same_from_its_query() {
    use skilj::Snapshot;
    use skilj_core::plugin::{Decision, SnapshotState};

    let history = helpdesk_history();
    let stored = store(&history);
    for end in 0..=history.len() {
        let prefix = &history[..end];
        for company in ["acme", "globex"] {
            for ticket in ["t1", "t2", "t3", "t4", "t5"] {
                for macro_id in ["m1", "m2", "m3", "m4"] {
                    let payload = ApplyMacroPayload {
                        ticket_id: ticket.into(),
                        company_id: company.into(),
                        macro_id: macro_id.into(),
                        staff_id: "staff-9".into(),
                    };
                    let json = serde_json::to_value(&payload).unwrap();
                    let snapshots = [SnapshotState {
                        snapshot: TicketSnapshot::NAME.into(),
                        tag_value: ticket.into(),
                        state_json: serde_json::to_string(&crate::ticket_snapshot::ticket_state(
                            prefix, ticket,
                        ))
                        .unwrap(),
                        folded_through: -1,
                    }];
                    let tags = derive(&[company_tag_mapping(), macro_tag_mapping()], &json);
                    let keep: Vec<bool> = stored[..end]
                        .iter()
                        .map(|s| s.tags.iter().any(|t| tags.contains(t)))
                        .collect();
                    let tagged: Vec<HelpdeskEvent> = rebuild(&stored[..end], &keep);
                    let queried = selected_events::<ApplyMacro>(prefix, &json);
                    assert!(queried.len() <= 1, "{json}");
                    let by_tags =
                        ApplyMacro::decide_with(&payload, &Decision::new(&tagged, &snapshots));
                    let by_query =
                        ApplyMacro::decide_with(&payload, &Decision::new(&queried, &snapshots));
                    assert!(
                        same(&by_tags, &by_query),
                        "ApplyMacro after {end} events, {json}:\n  tags:  {by_tags:?}\n  query: {by_query:?}"
                    );
                }
            }
        }
    }
}

fn company_tag_mapping() -> TagMapping {
    TagMapping {
        key: "company".into(),
        field: "company_id".into(),
    }
}

fn macro_tag_mapping() -> TagMapping {
    TagMapping {
        key: "macro".into(),
        field: "macro_id".into(),
    }
}

#[test]
fn the_queries_leave_out_what_a_long_history_is_made_of() {
    let history = helpdesk_history();
    // A new ticket reads the company's latest lifecycle event and its
    // latest SLA policy, not its tickets.
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
    assert_eq!(queried, 2);
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
    use skilj_helpdesk::marketing::*;
    for items in [
        RecordTrialLapse::consistency_query(),
        RecordTrialConversion::consistency_query(),
        RecordEngagementDecline::consistency_query(),
    ] {
        assert_eq!(items, vec![QueryItemMapping::nothing()]);
    }
}

fn kb_drafted(article: &str, ticket: &str, title: &str) -> KnowledgeBaseEvent {
    KnowledgeBaseEvent::ArticleDrafted(ArticleDraftedPayload {
        article_id: article.into(),
        from_ticket_id: Some(ticket.into()),
        title: title.into(),
        body: format!("{title}!"),
    })
}

/// Three suggestions - one dismissed, two drafted - and two articles, one
/// published then edited. Every command, against every id in play plus
/// one that isn't. `DraftArticle` only from a suggestion: a scratch
/// draft's missing `from_ticket_id` derives a valueless tag in skilj,
/// which `derive` above can't represent.
#[test]
fn every_knowledge_base_command_decides_the_same_from_its_query() {
    let suggested = |ticket: &str| {
        KnowledgeBaseEvent::ArticleSuggested(ArticleSuggestedPayload {
            ticket_id: ticket.into(),
            company_id: "c1".into(),
            suggested_at: "2026-10-12T00:00:00Z".into(),
        })
    };
    let history = vec![
        suggested("t1"),
        suggested("t2"),
        suggested("t3"),
        KnowledgeBaseEvent::SuggestionDismissed(SuggestionDismissedPayload {
            ticket_id: "t2".into(),
            reason: None,
        }),
        kb_drafted("a1", "t1", "One"),
        KnowledgeBaseEvent::ArticlePublished(ArticlePublishedPayload {
            article_id: "a1".into(),
            title: "One".into(),
            body: "One!".into(),
            revision: 1,
        }),
        KnowledgeBaseEvent::ArticleEdited(ArticleEditedPayload {
            article_id: "a1".into(),
            title: "One, revised".into(),
            body: "One, revised!".into(),
        }),
        kb_drafted("a2", "t3", "Two"),
    ];
    for ticket in ["t1", "t2", "t3", "t4"] {
        assert_query_agrees::<SuggestArticle>(
            &SuggestArticlePayload {
                ticket_id: ticket.into(),
                company_id: "c1".into(),
                suggested_at: "2026-10-13T00:00:00Z".into(),
            },
            &history,
        );
        assert_query_agrees::<DismissSuggestion>(
            &DismissSuggestionPayload {
                ticket_id: ticket.into(),
                reason: None,
            },
            &history,
        );
        for article in ["a1", "a3"] {
            assert_query_agrees::<DraftArticle>(
                &DraftArticlePayload {
                    article_id: article.into(),
                    from_ticket_id: Some(ticket.into()),
                    title: "New".into(),
                    body: "New!".into(),
                },
                &history,
            );
        }
    }
    for article in ["a1", "a2", "a3"] {
        assert_query_agrees::<EditArticle>(
            &EditArticlePayload {
                article_id: article.into(),
                title: "Edit".into(),
                body: "Edit!".into(),
            },
            &history,
        );
        assert_query_agrees::<PublishArticle>(
            &PublishArticlePayload {
                article_id: article.into(),
            },
            &history,
        );
    }
}
