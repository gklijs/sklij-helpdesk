//! The scheduler: the other separately-deployable unit this pass adds,
//! alongside `src/bin/alerter.rs`. Drives the two rules from
//! `specs/skilj-helpdesk.allium` that mutate real domain state on a
//! deadline - `rule TrialPeriodEnds` and `rule TicketAutoCloses` -
//! neither of which skilj's own `system_triggered` `EventType`
//! scheduling fits: that mechanism fires one event on a shared cron
//! schedule (docs/architecture.md §1.3's `EventType::
//! system_triggered_schedule`/`scheduled_payload()`), not once per
//! entity against that entity's *own* deadline (each company's own
//! trial end, each ticket's own `resolved_at + 7.days`). That's a real
//! design fork, not a gap: rather than misuse a global-cron primitive
//! or extend skilj-core itself (out of scope for an application built
//! on it), this pass resolves it the same way alerting did - an
//! ordinary periodic REST-driven consumer, submitting ordinary
//! commands (`ConvertCompanyTrial`/`ExpireCompanyTrial`/`CloseTicket`
//! in `helpdesk.rs` - all just more `CommandType` impls, nothing skilj
//! itself needed to grow).
//!
//! Not exercised by `cargo test`, for the same reason
//! `alerter.rs` isn't (see that file's own doc comment) - the pure
//! deadline/mock-payment logic is `src/scheduling.rs`'s own, tested
//! there without any HTTP or Postgres involved.
//!
//! **Restart safety**: see `alerter.rs`'s own doc comment - the exact
//! same in-memory-state-plus-non-replayable-`mode=auto`-cursor gap
//! applies here, over `rule TrialPeriodEnds`/`rule TicketAutoCloses`
//! instead of `rule TicketBecomesOverdue`, fixed the identical way
//! (`load_state`/`save_state` below, byte-for-byte the same shape as
//! `alerter.rs`'s own - not shared between the two binaries for the
//! same no-common-library-boundary reason `consume`/`submit_command`
//! already aren't).
//!
//! Configuration (env vars, same minimal style as `alerter.rs`):
//!   SKILJ_BASE_URL                     - default "http://localhost:3000"
//!   TRIAL_DURATION_DAYS                - default 30 (matches the spec's
//!                                         `config.trial_duration = 1.month`)
//!   AUTO_CLOSE_AFTER_DAYS              - default 7  (matches
//!                                         `config.auto_close_after`)
//!   SCHEDULER_STATE_FILE               - default "scheduler-state.json";
//!                                         empty string disables
//!                                         checkpointing - see
//!                                         `alerter.rs`'s own
//!                                         ALERTER_STATE_FILE doc.
//!   Seven EventReadTokens ("id.secret"), one per event type this binary
//!   needs to track company/ticket state from:
//!     COMPANY_SIGNED_UP_TOKEN, COMPANY_ACTIVATED_TOKEN,
//!     COMPANY_EXPIRED_TOKEN, TICKET_RESOLVED_TOKEN,
//!     TICKET_REOPENED_TOKEN, TICKET_CLOSED_TOKEN, TICKETS_MERGED_TOKEN
//!     (the last one exists only to drop a resolved-then-merged
//!     duplicate ticket out of `resolved_tickets` below - without it,
//!     `rule TicketAutoCloses` would resubmit a `CloseTicket` for that
//!     same duplicate every tick, forever, each one correctly rejected
//!     by `helpdesk.rs`'s own "Merged, not resolved" check but never
//!     actually stopping - a real bug this project found and fixed the
//!     same way `ALERTER_EVENT_TYPES`'s own `TicketsMerged` handling
//!     already did for `rule TicketBecomesOverdue`, just missed here
//!     originally since `MergeTickets` postdates this file's own first
//!     pass)
//!   Three CommandTokens, one per mutation this binary submits:
//!     CONVERT_COMPANY_TRIAL_TOKEN, EXPIRE_COMPANY_TRIAL_TOKEN,
//!     CLOSE_TICKET_TOKEN
//!
//! Ten tokens is a lot of environment configuration for one small
//! binary - a direct, visible consequence of skilj's own per-type token
//! scoping (`AccessToken` in specs/skilj.allium: one capability, one
//! registered type, per token). A real deployment would likely load
//! these from a mounted secrets file rather than nine separate env
//! vars; this pass keeps the plain-env-var style `alerter.rs` already
//! established rather than introducing a second configuration
//! convention for one binary.
//!
//! Telemetry: `skilj_helpdesk::telemetry::init` as service
//! `"skilj-helpdesk-scheduler"` - see `alerter.rs`'s own doc comment for
//! why `_telemetry` below is just held, not explicitly shut down.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use skilj_helpdesk::scheduling::{mock_charge_succeeds, should_auto_close, trial_period_has_ended};
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration as StdDuration;

const POLL_INTERVAL: StdDuration = StdDuration::from_secs(30);

struct Config {
    base_url: String,
    trial_duration: chrono::Duration,
    auto_close_after: chrono::Duration,
    company_signed_up_token: String,
    company_activated_token: String,
    company_expired_token: String,
    ticket_resolved_token: String,
    ticket_reopened_token: String,
    ticket_closed_token: String,
    tickets_merged_token: String,
    convert_company_trial_token: String,
    expire_company_trial_token: String,
    close_ticket_token: String,
    /// `None` when `SCHEDULER_STATE_FILE` is set to an empty string -
    /// see `alerter.rs`'s identical field for why this exists.
    state_file: Option<PathBuf>,
}

impl Config {
    fn from_env() -> Self {
        let required = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| {
                eprintln!("{name} must be set");
                std::process::exit(1);
            })
        };
        let days = |name: &str, default: i64| -> i64 {
            std::env::var(name)
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(default)
        };
        let state_file = match std::env::var("SCHEDULER_STATE_FILE") {
            Ok(s) if s.is_empty() => None,
            Ok(s) => Some(PathBuf::from(s)),
            Err(_) => Some(PathBuf::from("scheduler-state.json")),
        };
        Config {
            base_url: std::env::var("SKILJ_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:3000".to_string()),
            trial_duration: chrono::Duration::days(days("TRIAL_DURATION_DAYS", 30)),
            auto_close_after: chrono::Duration::days(days("AUTO_CLOSE_AFTER_DAYS", 7)),
            company_signed_up_token: required("COMPANY_SIGNED_UP_TOKEN"),
            company_activated_token: required("COMPANY_ACTIVATED_TOKEN"),
            company_expired_token: required("COMPANY_EXPIRED_TOKEN"),
            ticket_resolved_token: required("TICKET_RESOLVED_TOKEN"),
            ticket_reopened_token: required("TICKET_REOPENED_TOKEN"),
            ticket_closed_token: required("TICKET_CLOSED_TOKEN"),
            tickets_merged_token: required("TICKETS_MERGED_TOKEN"),
            convert_company_trial_token: required("CONVERT_COMPANY_TRIAL_TOKEN"),
            expire_company_trial_token: required("EXPIRE_COMPANY_TRIAL_TOKEN"),
            close_ticket_token: required("CLOSE_TICKET_TOKEN"),
            state_file,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// company_id -> when `CompanySignedUp` fired (its trial clock start).
    trialing_companies: HashMap<String, DateTime<Utc>>,
    /// ticket_id -> when `TicketResolved` last fired. A duplicate
    /// merged away via `MergeTickets` (`TicketsMerged` below) is
    /// removed from here the same way `TicketReopened`/`TicketClosed`
    /// already do - it's just as terminal, and `rule TicketAutoCloses`
    /// would otherwise resubmit a `CloseTicket` for it forever, always
    /// rejected, never actually stopping.
    resolved_tickets: HashMap<String, DateTime<Utc>>,
}

#[tokio::main]
async fn main() {
    let _telemetry = skilj_helpdesk::telemetry::init("skilj-helpdesk-scheduler");

    let config = Config::from_env();
    let client = reqwest::Client::new();
    let mut state = match &config.state_file {
        Some(path) => load_state(path),
        None => State::default(),
    };
    println!("scheduler: polling {} every {POLL_INTERVAL:?}", config.base_url);

    loop {
        if let Err(e) = tick(&client, &config, &mut state).await {
            eprintln!("scheduler: tick failed, will retry: {e}");
            tracing::warn!(error = %e, "scheduler: tick failed, will retry");
        }
        // See `alerter.rs`'s identical checkpoint call for why this
        // happens every tick, success or not.
        if let Some(path) = &config.state_file {
            save_state(path, &state);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Byte-for-byte the same shape as `alerter.rs`'s own `load_state` -
/// see that one's doc comment.
fn load_state(path: &std::path::Path) -> State {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(state) => {
                println!("scheduler: resumed tracking state from {}", path.display());
                state
            }
            Err(e) => {
                eprintln!(
                    "scheduler: {} exists but couldn't be parsed ({e}) - starting fresh",
                    path.display()
                );
                State::default()
            }
        },
        Err(_) => State::default(),
    }
}

/// Byte-for-byte the same shape as `alerter.rs`'s own `save_state` -
/// see that one's doc comment.
fn save_state(path: &std::path::Path, state: &State) {
    let tmp = path.with_extension("json.tmp");
    let write = std::fs::write(&tmp, serde_json::to_vec(state).expect("State always serializes"))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = write {
        eprintln!("scheduler: couldn't checkpoint state to {}: {e}", path.display());
    }
}

async fn tick(
    client: &reqwest::Client,
    config: &Config,
    state: &mut State,
) -> Result<(), reqwest::Error> {
    // --- track state from the event feed ---
    for (event_type, payload, created_at) in
        consume(client, &config.base_url, &config.company_signed_up_token).await?
    {
        if event_type == "CompanySignedUp" {
            if let Some(company_id) = payload["company_id"].as_str() {
                state
                    .trialing_companies
                    .insert(company_id.to_string(), created_at);
            }
        }
    }
    for (_, payload, _) in consume(client, &config.base_url, &config.company_activated_token).await? {
        if let Some(company_id) = payload["company_id"].as_str() {
            state.trialing_companies.remove(company_id);
        }
    }
    for (_, payload, _) in consume(client, &config.base_url, &config.company_expired_token).await? {
        if let Some(company_id) = payload["company_id"].as_str() {
            state.trialing_companies.remove(company_id);
        }
    }
    for (_, payload, created_at) in
        consume(client, &config.base_url, &config.ticket_resolved_token).await?
    {
        if let Some(ticket_id) = payload["ticket_id"].as_str() {
            state
                .resolved_tickets
                .insert(ticket_id.to_string(), created_at);
        }
    }
    for (_, payload, _) in consume(client, &config.base_url, &config.ticket_reopened_token).await? {
        if let Some(ticket_id) = payload["ticket_id"].as_str() {
            state.resolved_tickets.remove(ticket_id);
        }
    }
    for (_, payload, _) in consume(client, &config.base_url, &config.ticket_closed_token).await? {
        if let Some(ticket_id) = payload["ticket_id"].as_str() {
            state.resolved_tickets.remove(ticket_id);
        }
    }
    for (_, payload, _) in consume(client, &config.base_url, &config.tickets_merged_token).await? {
        if let Some(duplicate_ticket_id) = payload["duplicate_ticket_id"].as_str() {
            state.resolved_tickets.remove(duplicate_ticket_id);
        }
    }

    // --- act on deadlines: rule TrialPeriodEnds ---
    let now = Utc::now();
    let due_companies: Vec<(String, DateTime<Utc>)> = state
        .trialing_companies
        .iter()
        .filter(|(_, signed_up_at)| trial_period_has_ended(**signed_up_at, now, config.trial_duration))
        .map(|(id, at)| (id.clone(), *at))
        .collect();
    for (company_id, _) in due_companies {
        let (token, event_type) = if mock_charge_succeeds() {
            (&config.convert_company_trial_token, "ConvertCompanyTrial")
        } else {
            (&config.expire_company_trial_token, "ExpireCompanyTrial")
        };
        match submit_command(client, &config.base_url, token, serde_json::json!({ "company_id": company_id })).await {
            Ok(()) => {
                println!("scheduler: {event_type} for company {company_id}");
                tracing::info!(company_id = %company_id, %event_type, "scheduler: submitted command");
                state.trialing_companies.remove(&company_id);
            }
            Err(e) => {
                eprintln!("scheduler: {event_type} for company {company_id} failed: {e}");
                tracing::warn!(error = %e, company_id = %company_id, %event_type, "scheduler: command rejected/failed");
            }
        }
    }

    // --- act on deadlines: rule TicketAutoCloses ---
    let due_tickets: Vec<String> = state
        .resolved_tickets
        .iter()
        .filter(|(_, resolved_at)| should_auto_close(**resolved_at, now, config.auto_close_after))
        .map(|(id, _)| id.clone())
        .collect();
    for ticket_id in due_tickets {
        match submit_command(
            client,
            &config.base_url,
            &config.close_ticket_token,
            serde_json::json!({ "ticket_id": ticket_id }),
        )
        .await
        {
            Ok(()) => {
                println!("scheduler: CloseTicket for ticket {ticket_id}");
                tracing::info!(ticket_id = %ticket_id, "scheduler: submitted CloseTicket");
                state.resolved_tickets.remove(&ticket_id);
            }
            Err(e) => {
                eprintln!("scheduler: CloseTicket for ticket {ticket_id} failed: {e}");
                tracing::warn!(error = %e, ticket_id = %ticket_id, "scheduler: CloseTicket rejected/failed");
            }
        }
    }

    Ok(())
}

/// One `GET /v1/events/consume?mode=auto` call, decoded down to just
/// what this binary needs: each event's type name, its payload, and
/// when skilj itself recorded it (`metadata.createdAt` - see this
/// file's own doc comment on why that's the timestamp used, not a
/// self-reported payload field).
async fn consume(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<(String, serde_json::Value, DateTime<Utc>)>, reqwest::Error> {
    #[derive(serde::Deserialize)]
    struct ConsumeResponse {
        events: Vec<EventDto>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct EventDto {
        event_type: String,
        payload: serde_json::Value,
        metadata: Metadata,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Metadata {
        created_at: DateTime<Utc>,
    }

    let response = client
        .get(format!("{base_url}/v1/events/consume"))
        .query(&[("mode", "auto")])
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;
    let body: ConsumeResponse = response.json().await?;
    Ok(body
        .events
        .into_iter()
        .map(|e| (e.event_type, e.payload, e.metadata.created_at))
        .collect())
}

/// One `POST /v1/commands/trigger` call. A rejection is logged by the
/// caller (via the `Err` case falling through to its own `eprintln!`)
/// rather than treated as a transport error - same "a business
/// rejection is 200, not an error status" contract `tests/support/mod.rs`'s
/// own `trigger` helper relies on.
async fn submit_command(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    payload: serde_json::Value,
) -> Result<(), String> {
    let response = client
        .post(format!("{base_url}/v1/commands/trigger"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "payload": payload }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if body["accepted"].as_bool() == Some(true) {
        Ok(())
    } else {
        Err(format!(
            "rejected: {}",
            body["rejectionReason"].as_str().unwrap_or("unknown")
        ))
    }
}
