//! End-to-end proof of issue #42's SLA deadlines against a real skilj:
//! creating a ticket schedules a first-response and a resolution deadline
//! in its company's business hours, a staff reply or a resolution cancels
//! them, and a deadline that comes due records the breach and escalates
//! the ticket. The decisions themselves are pinned in
//! `tests/fixture/sla.rs`, the business-hours arithmetic in `src/sla.rs`.
//!
//! Reads the `deadlines` table directly, as `tests/ticket_auto_close_cancel.rs`
//! does - no skilj API lists pending deadlines. Every company here sets
//! its own policy (or relies on the default), so nothing depends on
//! process-wide env vars and the tests can share one binary.

mod support;

use chrono::{DateTime, Datelike, Duration as ChronoDuration, NaiveTime, Utc};
use skilj_helpdesk::helpdesk::{TicketSummaryState, BOUNDED_CONTEXT};
use skilj_helpdesk::sla::{add_business_minutes, BusinessHours, SlaPolicy, SlaTarget};
use std::collections::HashMap;
use std::time::Duration;
use support::{
    accepted, mint_command_token, projection_state, rejection_kind, runtime, setup, test_db,
    trigger, unique_name, wait_until,
};

const COMMANDS: [&str; 9] = [
    "SignUpCompany",
    "SetCompanySlaPolicy",
    "CreateTicket",
    "AssignTicket",
    "RequestInfoFromCustomer",
    "ResolveTicket",
    "BreachTicketSla",
    "CreateMacro",
    "ApplyMacro",
];

/// One SLA deadline row, as `(status, fire_at)`.
type Row = (String, DateTime<Utc>);

/// This ticket's rows of both SLA schedules, by schedule name.
async fn sla_deadlines(pool: &skilj_core::db::Pool, ticket_id: &str) -> HashMap<String, Row> {
    let ticket_tag = serde_json::json!([{ "key": "ticket", "value": ticket_id }]).to_string();
    let rows: Vec<(String, String, i64)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT schedule_name, status, (EXTRACT(EPOCH FROM fire_at) * 1000000)::bigint \
         FROM \"bc_{BOUNDED_CONTEXT}\".deadlines \
         WHERE schedule_name IN ('ScheduleFirstResponseSla', 'ScheduleResolutionSla') \
           AND tags @> $1::jsonb"
    )))
    .bind(ticket_tag)
    .fetch_all(pool)
    .await
    .unwrap();
    rows.into_iter()
        .map(|(schedule, status, micros)| {
            let fire_at = DateTime::from_timestamp_micros(micros).unwrap();
            (schedule, (status, fire_at))
        })
        .collect()
}

fn statuses(rows: &HashMap<String, Row>) -> (Option<&str>, Option<&str>) {
    (
        rows.get("ScheduleFirstResponseSla").map(|r| r.0.as_str()),
        rows.get("ScheduleResolutionSla").map(|r| r.0.as_str()),
    )
}

struct Helpdesk {
    router: axum::Router,
    pool: skilj_core::db::Pool,
    tokens: HashMap<&'static str, String>,
}

impl Helpdesk {
    async fn start() -> Self {
        let (skilj, pool, mapping) = setup().await;
        let mut tokens = HashMap::new();
        for command in COMMANDS {
            tokens.insert(
                command,
                mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, command).await,
            );
        }
        Self {
            router: skilj.rest_router(),
            pool,
            tokens,
        }
    }

    async fn submit(&self, command: &str, payload: serde_json::Value) -> serde_json::Value {
        trigger(&self.router, &self.tokens[command], payload).await
    }

    async fn ok(&self, command: &str, payload: serde_json::Value) {
        let response = self.submit(command, payload).await;
        assert!(
            accepted(&response),
            "{command} should be accepted: {response:?}"
        );
    }

    /// A signed-up company, with `policy` set if given.
    async fn company(&self, policy: Option<&SlaPolicy>) -> String {
        let company_id = unique_name("company");
        self.ok(
            "SignUpCompany",
            serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
        )
        .await;
        if let Some(policy) = policy {
            self.ok(
                "SetCompanySlaPolicy",
                serde_json::json!({ "company_id": company_id, "policy": policy }),
            )
            .await;
        }
        company_id
    }

    async fn ticket(&self, company_id: &str, priority: &str) -> String {
        let ticket_id = unique_name("ticket");
        self.ok(
            "CreateTicket",
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": "customer-1",
                "logged_by_staff_id": null, "title": "t", "description": "d", "priority": priority,
            }),
        )
        .await;
        ticket_id
    }

    async fn wait_for(&self, ticket_id: &str, what: &str, expected: (Option<&str>, Option<&str>)) {
        wait_until(Duration::from_secs(10), what, || {
            let pool = self.pool.clone();
            let ticket_id = ticket_id.to_string();
            async move { statuses(&sla_deadlines(&pool, &ticket_id).await) == expected }
        })
        .await;
    }
}

/// The deadlines are counted in the company's business hours. Its window
/// here is one hour, on one weekday, two days from today: a target longer
/// than an hour has to skip every night, weekend and other weekday until
/// the same weekday next week.
#[test]
fn creating_a_ticket_schedules_both_sla_deadlines_in_business_hours() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let helpdesk = Helpdesk::start().await;
        let day = Utc::now().date_naive() + ChronoDuration::days(2);
        let policy = SlaPolicy {
            business_hours: BusinessHours {
                open_hour: 9,
                close_hour: 10,
                working_days: vec![day.weekday().number_from_monday() as u8],
            },
            high: SlaTarget {
                first_response_minutes: 90,
                resolution_minutes: 120,
            },
            ..SlaPolicy::default()
        };
        let company_id = helpdesk.company(Some(&policy)).await;

        let before = Utc::now();
        let ticket_id = helpdesk.ticket(&company_id, "high").await;
        helpdesk
            .wait_for(
                &ticket_id,
                "both SLA deadlines are scheduled",
                (Some("pending"), Some("pending")),
            )
            .await;
        let after = Utc::now();

        let rows = sla_deadlines(&helpdesk.pool, &ticket_id).await;
        let week_later = (day + ChronoDuration::days(7))
            .and_time(NaiveTime::MIN)
            .and_utc();
        for (schedule, minutes, expected) in [
            // 60 minutes on `day`, the other 30 a week later.
            (
                "ScheduleFirstResponseSla",
                90,
                week_later + ChronoDuration::minutes(9 * 60 + 30),
            ),
            (
                "ScheduleResolutionSla",
                120,
                week_later + ChronoDuration::hours(10),
            ),
        ] {
            let fire_at = rows[schedule].1;
            let earliest = add_business_minutes(before, minutes, &policy.business_hours);
            let latest = add_business_minutes(after, minutes, &policy.business_hours);
            assert!(
                earliest <= fire_at && fire_at <= latest,
                "{schedule}: {fire_at}"
            );
            // Unless the test ran across midnight, which moves `day`.
            if before.date_naive() == after.date_naive() {
                assert_eq!(fire_at, expected, "{schedule}");
            }
        }
    });
}

#[test]
fn a_first_staff_reply_cancels_only_the_first_response_sla() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let helpdesk = Helpdesk::start().await;
        // No policy: the default one.
        let company_id = helpdesk.company(None).await;
        let ticket_id = helpdesk.ticket(&company_id, "medium").await;
        helpdesk.ok("AssignTicket", serde_json::json!({ "ticket_id": ticket_id, "staff_id": "staff-1" })).await;
        helpdesk
            .wait_for(&ticket_id, "both SLA deadlines are scheduled", (Some("pending"), Some("pending")))
            .await;
        helpdesk
            .ok(
                "RequestInfoFromCustomer",
                serde_json::json!({
                    "ticket_id": ticket_id, "staff_id": "staff-1",
                    "message": "Which browser?", "requester_id": "customer-1",
                }),
            )
            .await;
        helpdesk
            .wait_for(
                &ticket_id,
                "the staff reply cancels the first-response SLA and leaves the resolution SLA running",
                (Some("cancelled"), Some("pending")),
            )
            .await;
    });
}

/// A macro's plain reply (`TicketStaffReplied`) is a first response too.
#[test]
fn a_macro_reply_cancels_only_the_first_response_sla() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let helpdesk = Helpdesk::start().await;
        let company_id = helpdesk.company(None).await;
        let ticket_id = helpdesk.ticket(&company_id, "medium").await;
        let macro_id = unique_name("macro");
        helpdesk
            .ok(
                "CreateMacro",
                serde_json::json!({
                    "company_id": company_id, "macro_id": macro_id, "staff_id": "staff-1",
                    "name": "Ack", "body": "Looking into {{ticket_id}}", "status": null, "priority": null,
                }),
            )
            .await;
        helpdesk
            .wait_for(&ticket_id, "both SLA deadlines are scheduled", (Some("pending"), Some("pending")))
            .await;
        // Still open - a plain reply needs no pick-up.
        helpdesk
            .ok(
                "ApplyMacro",
                serde_json::json!({
                    "ticket_id": ticket_id, "company_id": company_id,
                    "macro_id": macro_id, "staff_id": "staff-1",
                }),
            )
            .await;
        helpdesk
            .wait_for(
                &ticket_id,
                "the macro reply cancels the first-response SLA and leaves the resolution SLA running",
                (Some("cancelled"), Some("pending")),
            )
            .await;
        let response = helpdesk
            .submit(
                "BreachTicketSla",
                serde_json::json!({ "ticket_id": ticket_id, "sla": "first_response" }),
            )
            .await;
        assert_eq!(rejection_kind(&response), "sla_already_met", "{response:?}");
    });
}

#[test]
fn resolving_cancels_both_sla_deadlines() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let helpdesk = Helpdesk::start().await;
        let company_id = helpdesk.company(None).await;
        let ticket_id = helpdesk.ticket(&company_id, "urgent").await;
        helpdesk
            .ok(
                "AssignTicket",
                serde_json::json!({ "ticket_id": ticket_id, "staff_id": "staff-1" }),
            )
            .await;
        helpdesk
            .ok(
                "ResolveTicket",
                serde_json::json!({ "ticket_id": ticket_id }),
            )
            .await;
        helpdesk
            .wait_for(
                &ticket_id,
                "resolving cancels both SLA deadlines",
                (Some("cancelled"), Some("cancelled")),
            )
            .await;
    });
}

/// A zero-minute first-response target on round-the-clock hours is due
/// the moment the ticket is created, so skilj fires it straight away -
/// nothing in this test submits `BreachTicketSla` until after it has.
#[test]
fn a_breached_sla_records_the_breach_and_escalates_the_ticket() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let helpdesk = Helpdesk::start().await;
        let policy = SlaPolicy {
            business_hours: BusinessHours {
                open_hour: 0,
                close_hour: 24,
                working_days: (1..=7).collect(),
            },
            low: SlaTarget {
                first_response_minutes: 0,
                resolution_minutes: 60 * 24 * 30,
            },
            ..SlaPolicy::default()
        };
        let company_id = helpdesk.company(Some(&policy)).await;
        let ticket_id = helpdesk.ticket(&company_id, "low").await;

        helpdesk
            .wait_for(
                &ticket_id,
                "the first-response SLA fires and the resolution SLA keeps waiting",
                (Some("fired"), Some("pending")),
            )
            .await;
        wait_until(
            Duration::from_secs(10),
            "the breach escalates the ticket",
            || {
                let pool = helpdesk.pool.clone();
                let ticket_id = ticket_id.clone();
                async move {
                    let state: TicketSummaryState =
                        projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
                    state.escalated && state.priority.as_deref() == Some("medium")
                }
            },
        )
        .await;

        // The breach itself was recorded, so it can't be recorded twice.
        let again = helpdesk
            .submit(
                "BreachTicketSla",
                serde_json::json!({ "ticket_id": ticket_id, "sla": "first_response" }),
            )
            .await;
        assert!(!accepted(&again));
        assert_eq!(rejection_kind(&again), "sla_already_breached");
    });
}
