//! `TicketSnapshot` against a real database (issue #14): skilj's catch-up
//! keeps one row per ticket, and bumping `Snapshot::VERSION` - simulated
//! here by setting a stored row back to an older version, since the
//! const can't change inside one test binary - must neither be trusted
//! by a decision nor leave a row that's missing the ticket's earlier
//! history. skilj 0.0.9 refolds such a row from the tag's whole history
//! the next time an event for it is caught up (docs/architecture.md
//! §152 in the skilj repo). Decision equivalence itself is covered,
//! exhaustively, by `tests/fixture/ticket_snapshot.rs`.

mod support;

use skilj::Snapshot;
use skilj_helpdesk::helpdesk::{
    CompanyFacts, CompanySnapshot, CompanyStatus, TicketFacts, TicketPriority, TicketSnapshot,
    TicketStatus, BOUNDED_CONTEXT,
};
use std::time::Duration;
use support::{
    accepted, mint_command_token, rejection_kind, runtime, setup, test_db, trigger, unique_name,
    wait_until,
};

/// The stored row for `ticket_id`: `(snapshot_version, as_of_sequence, state)`.
async fn stored(pool: &skilj_core::db::Pool, ticket_id: &str) -> Option<(i64, i64, TicketFacts)> {
    let row: Option<(i64, i64, String)> = sqlx::query_as(
        "SELECT snapshot_version, as_of_sequence, state::text FROM bc_helpdesk.snapshots \
         WHERE snapshot_name = $1 AND tag_key = 'ticket' AND tag_value = $2",
    )
    .bind(TicketSnapshot::NAME)
    .bind(ticket_id)
    .fetch_optional(pool)
    .await
    .unwrap();
    row.map(|(version, as_of, state)| {
        // An older-version row may hold any shape; read it leniently.
        (
            version,
            as_of,
            serde_json::from_str(&state).unwrap_or_default(),
        )
    })
}

/// The sequence of the last event tagged with this ticket.
async fn last_ticket_event(pool: &skilj_core::db::Pool, ticket_id: &str) -> i64 {
    sqlx::query_scalar("SELECT max(sequence) FROM bc_helpdesk.events WHERE tags @> $1::jsonb")
        .bind(serde_json::json!([{ "key": "ticket", "value": ticket_id }]).to_string())
        .fetch_one(pool)
        .await
        .unwrap()
}

/// The `resolution` number of the ticket's latest `TicketResolved`.
async fn latest_resolution(pool: &skilj_core::db::Pool, ticket_id: &str) -> i64 {
    let payload: String = sqlx::query_scalar(
        "SELECT payload::text FROM bc_helpdesk.events \
         WHERE event_type_name = 'TicketResolved' AND tags @> $1::jsonb \
         ORDER BY sequence DESC LIMIT 1",
    )
    .bind(serde_json::json!([{ "key": "ticket", "value": ticket_id }]).to_string())
    .fetch_one(pool)
    .await
    .unwrap();
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
    payload["resolution"].as_i64().unwrap()
}

async fn wait_until_current(pool: &skilj_core::db::Pool, ticket_id: &str) {
    let last = last_ticket_event(pool, ticket_id).await;
    wait_until(Duration::from_secs(10), "TicketSnapshot catch-up", || async {
        matches!(stored(pool, ticket_id).await, Some((v, as_of, _)) if v == TicketSnapshot::VERSION as i64 && as_of >= last)
    })
    .await;
}

#[test]
fn an_older_version_row_is_ignored_and_then_refolded_from_the_whole_history() {
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
        let requester_id = unique_name("customer");
        let run = |command: &'static str, payload: serde_json::Value| {
            let router = router.clone();
            let token = tokens[command].clone();
            async move {
                let response = trigger(&router, &token, payload).await;
                assert!(accepted(&response), "{command} should be accepted: {response:?}");
            }
        };

        run("SignUpCompany", serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        run(
            "CreateTicket",
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": requester_id,
                "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
            }),
        )
        .await;
        run("AssignTicket", serde_json::json!({ "ticket_id": ticket_id, "staff_id": "staff-1" })).await;
        run("ResolveTicket", serde_json::json!({ "ticket_id": ticket_id })).await;
        run("ReopenTicket", serde_json::json!({ "ticket_id": ticket_id })).await;

        // The catch-up keeps a current row, folded from nothing.
        wait_until_current(&pool, &ticket_id).await;
        let (_, _, state) = stored(&pool, &ticket_id).await.unwrap();
        assert_eq!(state.ticket_id, ticket_id);
        assert_eq!(state.status, Some(TicketStatus::InProgress));
        assert_eq!(state.company_id.as_deref(), Some(company_id.as_str()));
        assert_eq!(state.requester_id.as_deref(), Some(requester_id.as_str()));
        assert_eq!(state.created_priority, Some(TicketPriority::Low));
        assert_eq!(state.resolutions, 1);

        // "Deploy" a new VERSION: the stored row is now an older one, and
        // its old shape says nothing about this ticket any more.
        sqlx::query(
            "UPDATE bc_helpdesk.snapshots SET snapshot_version = $1, state = '{\"retired\": true}'::jsonb \
             WHERE snapshot_name = $2 AND tag_value = $3",
        )
        .bind(TicketSnapshot::VERSION as i64 - 1)
        .bind(TicketSnapshot::NAME)
        .bind(&ticket_id)
        .execute(&pool)
        .await
        .unwrap();

        // Not trusted: the decision still sees the whole history, so this
        // is the ticket's *second* resolution.
        run("ResolveTicket", serde_json::json!({ "ticket_id": ticket_id })).await;
        assert_eq!(latest_resolution(&pool, &ticket_id).await, 2);

        // And the next catch-up refolds the row from the whole history -
        // not just from the event that touched it.
        wait_until_current(&pool, &ticket_id).await;
        let (_, _, state) = stored(&pool, &ticket_id).await.unwrap();
        assert_eq!(state.ticket_id, ticket_id);
        assert_eq!(state.status, Some(TicketStatus::Resolved));
        assert_eq!(state.company_id.as_deref(), Some(company_id.as_str()));
        assert_eq!(state.created_priority, Some(TicketPriority::Low));
        assert_eq!(state.resolutions, 2);

        // Decisions from the refolded row are right too.
        run("ReopenTicket", serde_json::json!({ "ticket_id": ticket_id })).await;
        run("ResolveTicket", serde_json::json!({ "ticket_id": ticket_id })).await;
        assert_eq!(latest_resolution(&pool, &ticket_id).await, 3);
    });
}

/// skilj falls back to plain `decide()`, silently, whenever it won't use
/// a snapshot (a command deriving more than the one `ticket` tag, say) -
/// every other test would still pass with the snapshot dead weight. So
/// plant a current-version row that disagrees with the history and check
/// the decision follows the row: proof the stored state is what's read.
#[test]
fn single_ticket_commands_really_decide_from_the_stored_row() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let mut tokens = std::collections::HashMap::new();
        for command in ["SignUpCompany", "CreateTicket", "AssignTicket", "ResolveTicket"] {
            tokens.insert(command, mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, command).await);
        }
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");
        for (command, payload) in [
            ("SignUpCompany", serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })),
            (
                "CreateTicket",
                serde_json::json!({
                    "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                    "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
                }),
            ),
            ("AssignTicket", serde_json::json!({ "ticket_id": ticket_id, "staff_id": "staff-1" })),
        ] {
            let response = trigger(&router, &tokens[command], payload).await;
            assert!(accepted(&response), "{command}: {response:?}");
        }
        wait_until_current(&pool, &ticket_id).await;

        let (_, _, mut planted) = stored(&pool, &ticket_id).await.unwrap();
        planted.resolutions = 41;
        sqlx::query(
            "UPDATE bc_helpdesk.snapshots SET state = $1::jsonb \
             WHERE snapshot_name = $2 AND tag_value = $3",
        )
        .bind(serde_json::to_string(&planted).unwrap())
        .bind(TicketSnapshot::NAME)
        .bind(&ticket_id)
        .execute(&pool)
        .await
        .unwrap();

        let response = trigger(&router, &tokens["ResolveTicket"], serde_json::json!({ "ticket_id": ticket_id })).await;
        assert!(accepted(&response), "{response:?}");
        assert_eq!(latest_resolution(&pool, &ticket_id).await, 42);
    });
}

/// `CompanySnapshot`'s counterpart to the test above: plant an `Expired`
/// status on a current-version company row whose history says trialing,
/// and check `CreateTicket` follows the row.
#[test]
fn create_ticket_really_decides_from_the_stored_company_row() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
        let create = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "CreateTicket").await;
        let company_id = unique_name("company");
        let ticket = |ticket_id: String| {
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": "customer-1",
                "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
            })
        };
        let response = trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        assert!(accepted(&response), "{response:?}");
        let response = trigger(&router, &create, ticket(unique_name("ticket"))).await;
        assert!(accepted(&response), "{response:?}");

        let stored_company = || async {
            sqlx::query_as::<_, (i64, String)>(
                "SELECT snapshot_version, state::text FROM bc_helpdesk.snapshots \
                 WHERE snapshot_name = $1 AND tag_key = 'company' AND tag_value = $2",
            )
            .bind(CompanySnapshot::NAME)
            .bind(&company_id)
            .fetch_optional(&pool)
            .await
            .unwrap()
        };
        wait_until(Duration::from_secs(10), "CompanySnapshot catch-up", || async {
            matches!(stored_company().await, Some((v, state))
                if v == CompanySnapshot::VERSION as i64
                    && serde_json::from_str::<CompanyFacts>(&state).unwrap().ticket_ids.len() == 1)
        })
        .await;

        let (_, state) = stored_company().await.unwrap();
        let mut planted: CompanyFacts = serde_json::from_str(&state).unwrap();
        planted.status = Some(CompanyStatus::Expired);
        sqlx::query(
            "UPDATE bc_helpdesk.snapshots SET state = $1::jsonb \
             WHERE snapshot_name = $2 AND tag_value = $3",
        )
        .bind(serde_json::to_string(&planted).unwrap())
        .bind(CompanySnapshot::NAME)
        .bind(&company_id)
        .execute(&pool)
        .await
        .unwrap();

        let response = trigger(&router, &create, ticket(unique_name("ticket"))).await;
        assert_eq!(rejection_kind(&response), "company_expired", "{response:?}");
    });
}
