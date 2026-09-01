//! The alerting service: a separately-deployable unit, on purpose - see
//! `specs/skilj-helpdesk.allium`'s own resolved design note on why this
//! is a plain consumer of skilj's existing REST event feed rather than
//! anything built into skilj itself, and `src/alerting.rs` for the pure
//! decision logic this binary just drives.
//!
//! Talks to a *running* skilj server over HTTP - `GET
//! /v1/events/consume?mode=auto`, server-tracked/auto-advancing
//! (docs/architecture.md §7.4), the "stateless worker" row of that
//! table. Not exercised by `cargo test` (that would mean spawning and
//! tearing down a real listening server, a different kind of test than
//! this crate's other integration tests, which drive `Skilj::rest_router()`
//! in-process via `tower::ServiceExt::oneshot`); `src/alerting.rs`'s own
//! tests, plus `tests/alerting_feed.rs`'s `urgent_ticket_creation_is_visible_on_the_event_feed_and_triggers_an_alert`
//! (which exercises the exact same consume-and-decode path this binary
//! uses, just in-process), are where the real coverage is. This file
//! stays a thin loop around already-tested pieces.
//!
//! Configuration (env vars, deliberately minimal - no config-loading
//! crate, matching skilj's own §2.4 choice):
//!   SKILJ_BASE_URL          - e.g. "http://localhost:3000" (default)
//!   TICKET_CREATED_TOKEN    - an EventReadToken credential ("id.secret")
//!                             scoped to the "helpdesk"/"TicketCreated"
//!                             event type, minted by an admin over
//!                             GraphQL (see the spec's own AccessToken
//!                             notes) - required, this binary exits if
//!                             it's missing.
//!
//! What actually happens on an alert is deliberately a `println!`: the
//! real channel (email, Slack, PagerDuty, ...) is exactly the piece
//! `specs/skilj-helpdesk.allium`'s Excludes section leaves open -
//! "the alerting service's concern, not this spec's". Swap
//! `send_alert` below for a real integration; nothing else in this
//! file needs to change.

use skilj_helpdesk::alerting::evaluate_ticket_created;
use skilj_helpdesk::helpdesk::TicketCreatedPayload;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    let base_url =
        std::env::var("SKILJ_BASE_URL").unwrap_or_else(|_| "http://localhost:3000".to_string());
    let token = std::env::var("TICKET_CREATED_TOKEN").unwrap_or_else(|_| {
        eprintln!("TICKET_CREATED_TOKEN must be set - an EventReadToken for helpdesk/TicketCreated");
        std::process::exit(1);
    });

    let client = reqwest::Client::new();
    println!("alerter: polling {base_url}/v1/events/consume every {POLL_INTERVAL:?}");

    loop {
        if let Err(e) = poll_once(&client, &base_url, &token).await {
            eprintln!("alerter: poll failed, will retry: {e}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn poll_once(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<(), reqwest::Error> {
    let response = client
        .get(format!("{base_url}/v1/events/consume"))
        .query(&[("mode", "auto")])
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;

    let body: ConsumeResponse = response.json().await?;
    for event in body.events {
        if event.event_type != "TicketCreated" {
            continue;
        }
        let payload: TicketCreatedPayload = match serde_json::from_value(event.payload) {
            Ok(p) => p,
            Err(e) => {
                eprintln!("alerter: couldn't decode TicketCreated payload: {e}");
                continue;
            }
        };
        if let Some(alert) = evaluate_ticket_created(&payload) {
            send_alert(&alert);
        }
    }
    Ok(())
}

/// The one place a real channel integration plugs in - see this file's
/// own module doc comment.
fn send_alert(alert: &skilj_helpdesk::alerting::Alert) {
    println!(
        "ALERT [{:?}]: ticket {} (company {}) needs a lead's attention",
        alert.reason, alert.ticket_id, alert.company_id
    );
}

/// Mirrors `skilj-rest`'s own (private) `ConsumeResponse`/`EventDto` -
/// this binary only reads the two fields it needs.
#[derive(serde::Deserialize)]
struct ConsumeResponse {
    events: Vec<EventDto>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct EventDto {
    event_type: String,
    payload: serde_json::Value,
}
