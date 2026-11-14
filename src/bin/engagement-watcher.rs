//! engagement-watcher: the scheduled binary `specs/activity.allium`'s
//! own resolved note names - sweeps for companies whose customers have
//! gone quiet (`rule EngagementDeclineIsRecorded`'s own external
//! stimulus), the same "external process, not decide() itself, makes
//! the call" shape `src/bin/alerter.rs` already uses for urgent-ticket
//! paging (`helpdesk/`'s own trial-deadline/auto-close rules moved off
//! this shape onto skilj's native `ScheduleDeadline` in 0.0.7 - see
//! `src/activity_scheduling.rs`'s own doc comment for why "gone quiet"
//! doesn't fit that mechanism the same way).
//!
//! Tracks, per company, the most recent customer-kind
//! `DailyActivityRecorded` this binary has read (keyed on that event's
//! own `day` field, not when the event committed - `day`'s the domain
//! concept `config.quiet_after` measures against). A company with zero
//! customer activity ever is not flagged by this first pass - only an
//! already-observed-then-quiet company is (see `specs/activity.allium`'s
//! own resolved note on this scope decision). Once `config.quiet_after`
//! has elapsed since a tracked company's own last customer activity,
//! submits `RecordEngagementDecline` and stops tracking it -
//! `CompanyEngagementDeclined` only ever fires once per company (see
//! `rule EngagementDeclineIsRecorded`'s own `already_flagged` guard), so
//! there's nothing further to watch for once flagged - the same "stop
//! tracking once terminal" fix `src/bin/alerter.rs`'s own
//! `unhandled`/`TicketsMerged` handling already established.
//!
//! **Restart safety:** replay cursors and derived activity state share one
//! atomic checkpoint. Missing state replays history; corrupt/unwritable state
//! stops the process. See `docs/consumer-recovery.md`.
//!
//! Configuration (env vars, same minimal style `alerter.rs` already
//! uses):
//!   SKILJ_BASE_URL                     - default "http://localhost:3000"
//!   QUIET_AFTER_DAYS                   - default 14 (matches
//!                                         specs/activity.allium's own
//!                                         config.quiet_after)
//!   ENGAGEMENT_WATCHER_STATE_FILE      - default
//!                                         "engagement-watcher-state.json";
//!                                         empty string disables
//!                                         checkpointing - see
//!                                         `alerter.rs`'s own
//!                                         ALERTER_STATE_FILE doc.
//!   ENGAGEMENT_WATCHER_POLL_INTERVAL_MS - default 30000. Only
//!                                         tests/failure_recovery.rs
//!                                         lowers it.
//!   One EventReadToken ("id.secret"):
//!     DAILY_ACTIVITY_RECORDED_TOKEN
//!   One CommandToken ("id.secret"):
//!     RECORD_ENGAGEMENT_DECLINE_TOKEN
//!   Both printed by `src/bin/server.rs` on every run - see that file's
//!   own "to run engagement-watcher against this server" block.
//!
//! Not exercised by `cargo test`, for the same reason `alerter.rs` isn't:
//! the pure "gone quiet" logic is `src/activity_scheduling.rs`'s own,
//! tested there without any HTTP or Postgres involved.
//!
//! Telemetry: `skilj_helpdesk::telemetry::init` as service
//! `"skilj-helpdesk-engagement-watcher"` - see `alerter.rs`'s own doc
//! comment for why `_telemetry` below is just held, not explicitly shut
//! down.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use skilj_helpdesk::activity_scheduling::is_quiet;
use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Duration as StdDuration;

/// The default between two ticks; `ENGAGEMENT_WATCHER_POLL_INTERVAL_MS`
/// overrides it.
const POLL_INTERVAL: StdDuration = StdDuration::from_secs(30);

struct Config {
    base_url: String,
    quiet_after: chrono::Duration,
    daily_activity_recorded_token: String,
    record_engagement_decline_token: String,
    /// `None` when `ENGAGEMENT_WATCHER_STATE_FILE` is set to an empty
    /// string - see `alerter.rs`'s identical field for why this exists.
    state_file: Option<PathBuf>,
    poll_interval: StdDuration,
}

impl Config {
    fn from_env() -> Self {
        let required = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| {
                eprintln!("{name} must be set");
                std::process::exit(1);
            })
        };
        let state_file = match std::env::var("ENGAGEMENT_WATCHER_STATE_FILE") {
            Ok(s) if s.is_empty() => None,
            Ok(s) => Some(PathBuf::from(s)),
            Err(_) => Some(PathBuf::from("engagement-watcher-state.json")),
        };
        let quiet_after_days: i64 = std::env::var("QUIET_AFTER_DAYS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(14);
        Config {
            base_url: std::env::var("SKILJ_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:3000".to_string()),
            quiet_after: chrono::Duration::days(quiet_after_days),
            daily_activity_recorded_token: required("DAILY_ACTIVITY_RECORDED_TOKEN"),
            record_engagement_decline_token: required("RECORD_ENGAGEMENT_DECLINE_TOKEN"),
            state_file,
            poll_interval: std::env::var("ENGAGEMENT_WATCHER_POLL_INTERVAL_MS")
                .ok()
                .and_then(|v| v.parse().ok())
                .map_or(POLL_INTERVAL, StdDuration::from_millis),
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    #[serde(default)]
    positions: HashMap<String, String>,
    #[serde(skip)]
    replaying: bool,
    /// company_id -> the most recent customer-kind DailyActivityRecorded
    /// `day` this binary has observed for that company. Always the max
    /// seen so far, not just the latest read - `mode=auto` delivers in
    /// commit order, but this stays correct even if a future change (or
    /// a fresh state file re-reading a backlog) delivers a day out of
    /// order.
    last_customer_activity: HashMap<String, DateTime<Utc>>,
}

#[tokio::main]
async fn main() {
    let _telemetry = skilj_helpdesk::telemetry::init("skilj-helpdesk-engagement-watcher");

    let config = Config::from_env();
    let client = reqwest::Client::new();
    let mut state = match &config.state_file {
        Some(path) => load_state(path),
        None => State::default(),
    };
    if state.positions.is_empty() {
        state = State::default();
    }
    if let Some(path) = &config.state_file {
        save_state(path, &state);
    }

    println!(
        "engagement-watcher: polling {} every {:?}",
        config.base_url, config.poll_interval
    );

    loop {
        if let Err(e) = tick(&client, &config, &mut state).await {
            eprintln!("engagement-watcher: tick failed, will retry: {e}");
            tracing::warn!(error = %e, "engagement-watcher: tick failed, will retry");
        }
        // See `alerter.rs`'s identical checkpoint call for why this
        // happens every tick, success or not.
        if let Some(path) = &config.state_file {
            save_state(path, &state);
        }
        tokio::time::sleep(config.poll_interval).await;
    }
}

/// Byte-for-byte the same shape as `alerter.rs`'s own `load_state` -
/// see its doc comment.
fn load_state(path: &std::path::Path) -> State {
    skilj_helpdesk::consumer::load(path).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    })
}
fn save_state(path: &std::path::Path, state: &State) {
    skilj_helpdesk::consumer::save(path, state).unwrap_or_else(|e| {
        eprintln!("{e}");
        std::process::exit(1)
    });
}

async fn tick(
    client: &reqwest::Client,
    config: &Config,
    state: &mut State,
) -> Result<(), reqwest::Error> {
    state.replaying = false;
    // --- track state from the event feed ---
    for event in consume(
        client,
        &config.base_url,
        &config.daily_activity_recorded_token,
        "activity/DailyActivityRecorded",
        state,
    )
    .await?
    {
        let payload = event.payload;
        if payload["person_kind"].as_str() != Some("customer") {
            continue;
        }
        let Some(company_id) = payload["company_id"].as_str() else {
            continue;
        };
        let Some(day) = payload["day"]
            .as_str()
            .and_then(|d| DateTime::parse_from_rfc3339(d).ok())
        else {
            continue;
        };
        let day = day.with_timezone(&Utc);
        state
            .last_customer_activity
            .entry(company_id.to_string())
            .and_modify(|existing| {
                if day > *existing {
                    *existing = day;
                }
            })
            .or_insert(day);
    }

    // Wait until replay has reached the end of every feed before deadlines.
    if state.replaying {
        return Ok(());
    }
    // --- act on deadlines: rule EngagementDeclineIsRecorded ---
    let now = Utc::now();
    let due_companies: Vec<String> = state
        .last_customer_activity
        .iter()
        .filter(|(_, last_activity)| is_quiet(**last_activity, now, config.quiet_after))
        .map(|(id, _)| id.clone())
        .collect();
    for company_id in due_companies {
        match submit_command(
            client,
            &config.base_url,
            &config.record_engagement_decline_token,
            serde_json::json!({ "company_id": company_id }),
        )
        .await
        {
            Ok(()) => {
                println!("engagement-watcher: RecordEngagementDecline for company {company_id}");
                tracing::info!(company_id = %company_id, "engagement-watcher: submitted command");
                state.last_customer_activity.remove(&company_id);
            }
            Err(e) => {
                eprintln!("engagement-watcher: RecordEngagementDecline for company {company_id} failed: {e}");
                tracing::warn!(error = %e, company_id = %company_id, "engagement-watcher: command rejected/failed");
            }
        }
    }

    Ok(())
}

/// One replayable page, with its position stored in the same state as the
/// activity it derives. A full page defers the quiet-company sweep.
async fn consume(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    feed: &str,
    state: &mut State,
) -> Result<Vec<skilj_helpdesk::consumer::Event>, reqwest::Error> {
    let events =
        skilj_helpdesk::consumer::replay(client, base_url, token, feed, &mut state.positions)
            .await?;
    state.replaying |= events.len() >= skilj_core::event_store::DEFAULT_MAX_EVENTS_PER_READ;
    Ok(events)
}

/// One `POST /v1/commands/trigger` call - byte-for-byte the same shape
/// `alerter.rs`'s own `submit_command` is.
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
    if body["accepted"].as_bool() == Some(true) || body["rejectionKind"] == "already_flagged" {
        skilj_helpdesk::consumer::fault_point("after_effect");
        Ok(())
    } else {
        Err(format!(
            "rejected: {}",
            body["rejectionReason"].as_str().unwrap_or("unknown")
        ))
    }
}
