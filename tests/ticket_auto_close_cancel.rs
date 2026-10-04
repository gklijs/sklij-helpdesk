//! End-to-end proof of `CancelTicketAutoCloseOnReopen`: resolve, reopen
//! and resolve again, and only the first resolution's auto-close deadline
//! is cancelled - the second stays pending.
//!
//! Its own test binary rather than part of `tests/native_deadlines.rs`,
//! which sets `AUTO_CLOSE_AFTER_DAYS=0` for its whole process so its
//! deadlines fire at once. Here they must *not* fire: this test runs with
//! the real 7-day default, so both rows stay put and their status says
//! only what the cancel did. It reads the `deadlines` table directly,
//! since no projection exposes pending deadlines.

mod support;

use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use std::time::Duration;
use support::{
    accepted, mint_command_token, runtime, setup, test_db, trigger, unique_name, wait_until,
};

/// Every `ScheduleTicketAutoClose` row for this ticket, as
/// `(status, ticket_resolution tag value)`, sorted by the latter.
async fn auto_close_deadlines(
    pool: &skilj_core::db::Pool,
    ticket_id: &str,
) -> Vec<(String, String)> {
    let ticket_tag = serde_json::json!([{ "key": "ticket", "value": ticket_id }]).to_string();
    let rows: Vec<(String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT status, tags::text FROM \"bc_{BOUNDED_CONTEXT}\".deadlines \
         WHERE schedule_name = 'ScheduleTicketAutoClose' AND tags @> $1::jsonb"
    )))
    .bind(ticket_tag)
    .fetch_all(pool)
    .await
    .unwrap();
    let mut deadlines: Vec<(String, String)> = rows
        .into_iter()
        .map(|(status, tags)| {
            let tags: serde_json::Value = serde_json::from_str(&tags).unwrap();
            let resolution = tags
                .as_array()
                .unwrap()
                .iter()
                .find(|tag| tag["key"] == "ticket_resolution")
                .and_then(|tag| tag["value"].as_str())
                .expect("every new auto-close deadline carries a ticket_resolution tag")
                .to_string();
            (status, resolution)
        })
        .collect();
    deadlines.sort_by(|a, b| a.1.cmp(&b.1));
    deadlines
}

#[test]
fn reopening_cancels_only_the_reopened_resolutions_auto_close() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let mut tokens = std::collections::HashMap::new();
        for command in ["SignUpCompany", "CreateTicket", "AssignTicket", "ResolveTicket", "ReopenTicket"] {
            tokens.insert(command, mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, command).await);
        }
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        for (command, payload) in [
            (
                "SignUpCompany",
                serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
            ),
            (
                "CreateTicket",
                serde_json::json!({
                    "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                    "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
                }),
            ),
            ("AssignTicket", serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") })),
            ("ResolveTicket", serde_json::json!({ "ticket_id": ticket_id })),
            ("ReopenTicket", serde_json::json!({ "ticket_id": ticket_id })),
            ("ResolveTicket", serde_json::json!({ "ticket_id": ticket_id })),
        ] {
            let response = trigger(&router, &tokens[command], payload).await;
            assert!(accepted(&response), "{command} should be accepted: {response:?}");
        }

        let first = format!("{ticket_id}#1");
        let second = format!("{ticket_id}#2");
        let expected = vec![("cancelled".to_string(), first), ("pending".to_string(), second)];
        wait_until(
            Duration::from_secs(10),
            "the first resolution's auto-close is cancelled and the second's is pending",
            || {
                let pool = pool.clone();
                let ticket_id = ticket_id.clone();
                let expected = expected.clone();
                async move { auto_close_deadlines(&pool, &ticket_id).await == expected }
            },
        )
        .await;
    });
}
