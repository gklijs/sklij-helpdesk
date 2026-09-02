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
//! uses, just in-process), are where the real coverage is.
//!
//! **Two jobs now, not one.** `rule UrgentTicketNeedsImmediateAttention`
//! (react to `TicketCreated`, page immediately) is unchanged from the
//! original pass. `rule TicketBecomesOverdue` is newly finished here -
//! `src/alerting.rs`'s own doc comment used to note its trigger
//! (`is_overdue`) was written and tested but "not wired up this pass."
//! Finishing it turned out to mean more than printing an alert: see
//! `skilj_helpdesk::helpdesk::TicketEscalated`'s own doc comment for why
//! this now submits a real `EscalateTicket` command (a documented,
//! deliberate extension of the spec, not just the original console-only
//! trigger) - `tick` below tracks each unhandled ticket's own age the
//! same tracked-state-plus-sweep shape `scheduler.rs` already uses for
//! its own two deadline rules.
//!
//! **Restart safety.** `mode=auto`'s cursor is server-tracked and
//! non-replayable (docs/architecture.md §7.4) - once an event's been
//! served, it's gone from this token's own stream for good. That's
//! fine for `send_alert` below, which needs no memory at all, but
//! `rule TicketBecomesOverdue` is a *sweep* over "every currently
//! unhandled ticket," not a per-event reaction - so if `state` only
//! ever lived in this process's own memory, a restart would silently
//! and permanently drop every ticket that was already open before it,
//! forever, unless some *other* event happened to touch that same
//! ticket again later. That's a materially bigger loss than the design
//! note's own accepted "occasional missed events on crash" - that
//! tradeoff is about a handful of events landing during the crash
//! window, not the entire pre-crash world going dark. `load_state`/
//! `save_state` below close that gap: `state` is checkpointed to a
//! local JSON file after every tick (cheap - it's a handful of ticket
//! ids), and reloaded on startup if present, so a restart resumes
//! within one `POLL_INTERVAL` of where it left off instead of forgetting
//! everything. `scheduler.rs` has the identical fix, for the identical
//! reason, over its own two deadline rules.
//!
//! Configuration (env vars, deliberately minimal - no config-loading
//! crate, matching skilj's own §2.4 choice):
//!   SKILJ_BASE_URL               - default "http://localhost:3000"
//!   UNHANDLED_ALERT_AFTER_HOURS  - default 4 (matches the spec's
//!                                  `config.unhandled_alert_after`) - set
//!                                  to `0` to escalate overdue tickets
//!                                  immediately, for a demo.
//!   ALERTER_STATE_FILE           - default "alerter-state.json"
//!                                  (relative to CWD - a real deployment
//!                                  should point this at a persistent
//!                                  volume, or this binary is back to
//!                                  losing state on every restart/
//!                                  redeploy) - set to an empty string
//!                                  to disable checkpointing entirely
//!                                  and go back to pure in-memory state.
//!   Six EventReadTokens ("id.secret"), each this binary's *own* -
//!   never shared with `scheduler.rs`'s tokens for the same event types,
//!   see `server.rs`'s own `ALERTER_EVENT_TYPES` doc comment for why:
//!     TICKET_CREATED_TOKEN, TICKET_RESOLVED_TOKEN, TICKET_REOPENED_TOKEN,
//!     TICKET_CLOSED_TOKEN, TICKET_ESCALATED_TOKEN, TICKETS_MERGED_TOKEN
//!   One CommandToken:
//!     ESCALATE_TICKET_TOKEN
//!   SLACK_WEBHOOK_URL             - optional; unset means console-only
//!                                  (the original behaviour, and still
//!                                  what every alert does regardless).
//!
//! What actually happens on an alert was deliberately just a `println!`,
//! the real channel (email, Slack, PagerDuty, ...) being exactly the
//! piece `specs/skilj-helpdesk.allium`'s Excludes section leaves open:
//! "the alerting service's concern, not this spec's". `send_alert`
//! below is now a *worked example* of swapping that placeholder for a
//! real one (Slack's own incoming-webhook API - a POST of `{"text":
//! ...}`, no SDK needed), proving the seam the doc comment above used
//! to only describe actually works - console output stays unconditional
//! either way, Slack is additive when `SLACK_WEBHOOK_URL` is set. A
//! second real channel (PagerDuty, say) would plug in the exact same
//! way, right alongside it.
//!
//! Telemetry: `skilj_helpdesk::telemetry::init` (see that module's own
//! doc comment) as service `"skilj-helpdesk-alerter"` - same OTLP
//! opt-in as `server.rs`. This binary already loops forever with no
//! graceful-shutdown path, so `_telemetry` below is just kept alive for
//! `main`'s own lifetime rather than explicitly torn down; the OTLP
//! batch exporters still flush periodically on their own.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use skilj_helpdesk::alerting::{evaluate_ticket_created, is_overdue};
use skilj_helpdesk::helpdesk::TicketCreatedPayload;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

struct Config {
    base_url: String,
    ticket_created_token: String,
    ticket_resolved_token: String,
    ticket_reopened_token: String,
    ticket_closed_token: String,
    ticket_escalated_token: String,
    tickets_merged_token: String,
    escalate_ticket_token: String,
    unhandled_alert_after: chrono::Duration,
    /// `None` when `ALERTER_STATE_FILE` is set to an empty string -
    /// checkpointing opted out of, back to pure in-memory `state`.
    state_file: Option<PathBuf>,
    /// `None` when `SLACK_WEBHOOK_URL` is unset - `send_alert` below
    /// then stays console-only, exactly the original behaviour.
    slack_webhook_url: Option<String>,
}

impl Config {
    fn from_env() -> Self {
        let required = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| {
                eprintln!("{name} must be set");
                std::process::exit(1);
            })
        };
        let hours: i64 = std::env::var("UNHANDLED_ALERT_AFTER_HOURS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        let state_file = match std::env::var("ALERTER_STATE_FILE") {
            Ok(s) if s.is_empty() => None,
            Ok(s) => Some(PathBuf::from(s)),
            Err(_) => Some(PathBuf::from("alerter-state.json")),
        };
        Config {
            base_url: std::env::var("SKILJ_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:3000".to_string()),
            ticket_created_token: required("TICKET_CREATED_TOKEN"),
            ticket_resolved_token: required("TICKET_RESOLVED_TOKEN"),
            ticket_reopened_token: required("TICKET_REOPENED_TOKEN"),
            ticket_closed_token: required("TICKET_CLOSED_TOKEN"),
            ticket_escalated_token: required("TICKET_ESCALATED_TOKEN"),
            tickets_merged_token: required("TICKETS_MERGED_TOKEN"),
            escalate_ticket_token: required("ESCALATE_TICKET_TOKEN"),
            unhandled_alert_after: chrono::Duration::hours(hours),
            state_file,
            slack_webhook_url: std::env::var("SLACK_WEBHOOK_URL").ok().filter(|s| !s.is_empty()),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// ticket_id -> its own original `TicketCreated` timestamp (skilj's
    /// own event metadata, not a payload field - see `scheduler.rs`'s
    /// identical reasoning for `trial_started_at`). Kept forever, even
    /// past resolution: a reopened ticket's age is still measured from
    /// its own original creation, never reset.
    created_at: HashMap<String, DateTime<Utc>>,
    /// ticket_id -> company_id, populated alongside `created_at` - so an
    /// overdue-escalation alert (unlike an urgent-on-creation one, which
    /// already has this straight off `TicketCreated`'s own payload) can
    /// still report which company it's for.
    company_id: HashMap<String, String>,
    /// `specs/skilj-helpdesk.allium`'s own `unhandled: status not in
    /// {resolved, closed}` derived field, tracked directly - `merged`
    /// counts as handled too, for the same "nothing left to do" reason
    /// `ticket_status`'s own catch-all treatment in `helpdesk.rs` gives it.
    unhandled: HashSet<String>,
    /// Ticket ids already escalated (this alerter's own submission, or
    /// read back off the same `TicketEscalated` stream another instance
    /// produced) - stops resubmitting `EscalateTicket` every poll once
    /// it's already been done.
    escalated: HashSet<String>,
}

#[tokio::main]
async fn main() {
    let _telemetry = skilj_helpdesk::telemetry::init("skilj-helpdesk-alerter");

    let config = Config::from_env();
    let client = reqwest::Client::new();
    let mut state = match &config.state_file {
        Some(path) => load_state(path),
        None => State::default(),
    };
    println!(
        "alerter: polling {} every {POLL_INTERVAL:?} (escalating tickets unhandled for {:?})",
        config.base_url, config.unhandled_alert_after
    );

    loop {
        if let Err(e) = tick(&client, &config, &mut state).await {
            eprintln!("alerter: poll failed, will retry: {e}");
            tracing::warn!(error = %e, "alerter: poll failed, will retry");
        }
        // Checkpoint after every tick, success or not - `tick` may have
        // already applied some of this round's `consume()` results
        // before a later one failed, and there's no reason to lose
        // that progress too. See this file's own "Restart safety" doc
        // comment for why this exists at all.
        if let Some(path) = &config.state_file {
            save_state(path, &state);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Best-effort load: a missing file (first run, or checkpointing just
/// turned on) or unparseable one (a format change, a hand-edited file)
/// both fall back to `State::default()` rather than refusing to start -
/// this is a recovery aid, not a durability guarantee this binary
/// should ever block its own startup on.
fn load_state(path: &std::path::Path) -> State {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(state) => {
                println!("alerter: resumed tracking state from {}", path.display());
                state
            }
            Err(e) => {
                eprintln!(
                    "alerter: {} exists but couldn't be parsed ({e}) - starting fresh",
                    path.display()
                );
                State::default()
            }
        },
        Err(_) => State::default(),
    }
}

/// Write-to-temp-then-rename: a crash or power loss mid-write leaves
/// the previous checkpoint intact (a partially-written `path` itself
/// would otherwise corrupt the next `load_state`) - `rename` within the
/// same directory is atomic on the filesystems this runs on. Errors are
/// logged, not propagated: a failed checkpoint shouldn't take the whole
/// poll loop down, only degrade back to this tick's state being lost on
/// the next restart.
fn save_state(path: &std::path::Path, state: &State) {
    let tmp = path.with_extension("json.tmp");
    let write = std::fs::write(&tmp, serde_json::to_vec(state).expect("State always serializes"))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = write {
        eprintln!("alerter: couldn't checkpoint state to {}: {e}", path.display());
    }
}

async fn tick(
    client: &reqwest::Client,
    config: &Config,
    state: &mut State,
) -> Result<(), reqwest::Error> {
    // --- rule UrgentTicketNeedsImmediateAttention, plus tracking this
    // ticket's own age for the overdue sweep below ---
    for (_, payload, created_at) in
        consume(client, &config.base_url, &config.ticket_created_token).await?
    {
        match serde_json::from_value::<TicketCreatedPayload>(payload) {
            Ok(p) => {
                state.created_at.insert(p.ticket_id.clone(), created_at);
                state.company_id.insert(p.ticket_id.clone(), p.company_id.clone());
                state.unhandled.insert(p.ticket_id.clone());
                if let Some(alert) = evaluate_ticket_created(&p) {
                    send_alert(client, config.slack_webhook_url.as_deref(), &alert).await;
                }
            }
            Err(e) => eprintln!("alerter: couldn't decode TicketCreated payload: {e}"),
        }
    }

    // --- track state for rule TicketBecomesOverdue ---
    for (_, payload, _) in consume(client, &config.base_url, &config.ticket_resolved_token).await?
    {
        if let Some(ticket_id) = payload["ticket_id"].as_str() {
            state.unhandled.remove(ticket_id);
        }
    }
    for (_, payload, _) in consume(client, &config.base_url, &config.ticket_reopened_token).await?
    {
        if let Some(ticket_id) = payload["ticket_id"].as_str() {
            state.unhandled.insert(ticket_id.to_string());
        }
    }
    for (_, payload, _) in consume(client, &config.base_url, &config.ticket_closed_token).await? {
        if let Some(ticket_id) = payload["ticket_id"].as_str() {
            state.unhandled.remove(ticket_id);
        }
    }
    for (_, payload, _) in consume(client, &config.base_url, &config.ticket_escalated_token).await?
    {
        if let Some(ticket_id) = payload["ticket_id"].as_str() {
            state.escalated.insert(ticket_id.to_string());
        }
    }
    for (_, payload, _) in consume(client, &config.base_url, &config.tickets_merged_token).await? {
        if let Some(duplicate_ticket_id) = payload["duplicate_ticket_id"].as_str() {
            state.unhandled.remove(duplicate_ticket_id);
        }
    }

    // --- act on the deadline: rule TicketBecomesOverdue ---
    let now = Utc::now();
    let due: Vec<String> = state
        .unhandled
        .iter()
        .filter(|ticket_id| !state.escalated.contains(ticket_id.as_str()))
        .filter_map(|ticket_id| {
            let created_at = state.created_at.get(ticket_id)?;
            is_overdue(*created_at, now, config.unhandled_alert_after).then(|| ticket_id.clone())
        })
        .collect();
    for ticket_id in due {
        match submit_command(
            client,
            &config.base_url,
            &config.escalate_ticket_token,
            serde_json::json!({ "ticket_id": ticket_id }),
        )
        .await
        {
            Ok(()) => {
                send_alert(
                    client,
                    config.slack_webhook_url.as_deref(),
                    &skilj_helpdesk::alerting::Alert {
                        ticket_id: ticket_id.clone(),
                        company_id: state.company_id.get(&ticket_id).cloned().unwrap_or_default(),
                        reason: skilj_helpdesk::alerting::AlertReason::Overdue,
                    },
                )
                .await;
                state.escalated.insert(ticket_id);
            }
            Err(e) => {
                eprintln!("alerter: EscalateTicket for ticket {ticket_id} failed: {e}");
                tracing::warn!(error = %e, ticket_id = %ticket_id, "alerter: EscalateTicket rejected/failed");
            }
        }
    }

    Ok(())
}

/// One `GET /v1/events/consume?mode=auto` call for one token, decoded
/// down to what this binary needs: each event's type name (unused by
/// most call sites - each token is already scoped to one event type -
/// kept for parity with `scheduler.rs`'s identical helper), payload, and
/// when skilj itself recorded it. Identical to `scheduler.rs`'s own
/// `consume` - not shared between the two binaries since each is its own
/// deployable unit with no common library boundary between them worth
/// introducing for one helper.
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

/// One `POST /v1/commands/trigger` call - identical shape and "a
/// rejection is logged by the caller, not a transport error" contract as
/// `scheduler.rs`'s own `submit_command`.
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

/// Console output (unconditional) plus, when `webhook_url` is set, a
/// real Slack post - the worked example this file's own module doc
/// comment describes. A rejected/unreachable webhook is logged and
/// swallowed, not propagated: a broken Slack integration should degrade
/// this binary back to console-only, never take the whole poll loop
/// down (same "a failed checkpoint shouldn't stop `tick`" reasoning
/// `save_state` already gets).
async fn send_alert(client: &reqwest::Client, webhook_url: Option<&str>, alert: &skilj_helpdesk::alerting::Alert) {
    println!(
        "ALERT [{:?}]: ticket {} (company {}) needs a lead's attention",
        alert.reason, alert.ticket_id, alert.company_id
    );
    tracing::info!(
        reason = ?alert.reason,
        ticket_id = %alert.ticket_id,
        company_id = %alert.company_id,
        "alerter: paged a lead"
    );

    let Some(webhook_url) = webhook_url else {
        return;
    };
    // Slack's own incoming-webhook contract: a bare `{"text": ...}`
    // POST, no SDK, no auth beyond the URL itself being the secret -
    // see https://api.slack.com/messaging/webhooks. `mrkdwn` (Slack's
    // own dialect, not real Markdown) for the ticket id, so it renders
    // as inline code in the channel rather than plain text.
    let text = format!(
        "*[{:?}]* ticket `{}` (company `{}`) needs a lead's attention",
        alert.reason, alert.ticket_id, alert.company_id
    );
    let result = client
        .post(webhook_url)
        .json(&serde_json::json!({ "text": text }))
        .send()
        .await
        .and_then(|response| response.error_for_status());
    if let Err(e) = result {
        eprintln!("alerter: failed to post Slack alert: {e}");
        tracing::warn!(error = %e, ticket_id = %alert.ticket_id, "alerter: failed to post Slack alert");
    }
}
