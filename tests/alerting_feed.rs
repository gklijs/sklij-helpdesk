//! Integration tests for the alerting path - `src/bin/alerter.rs`'s
//! exact real route (`GET /v1/events/consume?mode=auto`), exercised
//! in-process. See `tests/company.rs`'s own doc comment for why this is
//! a separate binary/file rather than one large `tests/helpdesk.rs`.

mod support;

use skilj_helpdesk::alerting::evaluate_ticket_created;
use skilj_helpdesk::helpdesk::{TicketCreatedPayload, BOUNDED_CONTEXT};
use support::{consume_auto, mint_command_token, mint_event_read_token, runtime, setup, test_db, trigger, unique_name};

async fn token(
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    command_type_name: &str,
) -> String {
    mint_command_token(pool, mapping, BOUNDED_CONTEXT, command_type_name).await
}

/// Proves the whole "listen to events, decide to alert" pipeline the
/// spec's own design note resolved on - the same `GET
/// /v1/events/consume?mode=auto` route, the same `EventReadToken`
/// scoping, and the same `evaluate_ticket_created` decision
/// `src/bin/alerter.rs` runs against a live server, just driven
/// in-process here instead of over a real socket.
#[test]
fn urgent_ticket_creation_is_visible_on_the_event_feed_and_triggers_an_alert() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let read_ticket_created =
            mint_event_read_token(&pool, &mapping, BOUNDED_CONTEXT, "TicketCreated").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "Site is down", "description": "500s everywhere", "priority": "urgent",
            }),
        )
        .await;

        // The alerter's own real first step: consume the feed. A fresh
        // EventReadToken starts from the beginning of this event type's
        // whole history, shared across every test in this file (one
        // embedded Postgres, one bounded context) - so this may include
        // another test's own ticket too. Find ours rather than assuming
        // we're the only one on the feed, same as any real consumer
        // sharing a bounded context with other callers would have to.
        let consumed = consume_auto(&router, &read_ticket_created).await;
        let events = consumed["events"].as_array().expect("events array");
        let our_event = events
            .iter()
            .find(|e| e["payload"]["ticket_id"] == ticket_id)
            .unwrap_or_else(|| panic!("our own TicketCreated should be on the feed: {consumed:?}"));
        assert_eq!(our_event["eventType"], "TicketCreated");

        // The alerter's own real second step: decode and decide.
        let payload: TicketCreatedPayload = serde_json::from_value(our_event["payload"].clone()).unwrap();
        let alert = evaluate_ticket_created(&payload);
        assert!(alert.is_some(), "an urgent ticket must alert");
        assert_eq!(alert.unwrap().ticket_id, ticket_id);

        // Auto-advance mode: the same fetch again returns nothing new -
        // the point of server-tracked, at-most-once delivery
        // (docs/architecture.md §7.4), which is exactly what makes a
        // simple poll-loop-with-no-local-state (src/bin/alerter.rs) safe.
        let consumed_again = consume_auto(&router, &read_ticket_created).await;
        assert_eq!(consumed_again["events"].as_array().unwrap().len(), 0);
    });
}

/// A non-urgent ticket is still on the feed (the alerter sees
/// everything) but doesn't decide to alert - `evaluate_ticket_created`
/// is the actual gate, not the feed itself.
#[test]
fn non_urgent_ticket_creation_is_on_the_feed_but_does_not_alert() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let read_ticket_created =
            mint_event_read_token(&pool, &mapping, BOUNDED_CONTEXT, "TicketCreated").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "How do I export CSV?", "description": "Just curious", "priority": "low",
            }),
        )
        .await;

        // See the other test's own comment on why this filters by
        // ticket_id rather than assuming it's the only event on the
        // feed - tests in this file share one embedded Postgres.
        let consumed = consume_auto(&router, &read_ticket_created).await;
        let events = consumed["events"].as_array().unwrap();
        let our_event = events
            .iter()
            .find(|e| e["payload"]["ticket_id"] == ticket_id)
            .unwrap_or_else(|| panic!("our own TicketCreated should be on the feed: {consumed:?}"));
        let payload: TicketCreatedPayload = serde_json::from_value(our_event["payload"].clone()).unwrap();
        assert!(evaluate_ticket_created(&payload).is_none(), "a low-priority ticket must not alert");
    });
}
