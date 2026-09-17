//! engagement-watcher: the scheduled binary `specs/activity.allium`'s
//! own resolved note names - sweeps for companies whose customers have
//! gone quiet (`rule EngagementDeclineIsRecorded`'s own external
//! stimulus), the same "external process, not decide() itself, makes
//! the call" shape `src/bin/alerter.rs` already uses for urgent-ticket
//! paging (`helpdesk.rs`'s own trial-deadline/auto-close rules moved off
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
//! **Restart safety**: see `alerter.rs`'s own doc comment - the exact
//! same in-memory-state-plus-non-replayable-`mode=auto`-cursor gap
//! applies here, fixed the identical way (`load_state`/`save_state`
//! below, byte-for-byte the same shape as `alerter.rs`'s own).
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

const POLL_INTERVAL: StdDuration = StdDuration::from_secs(30);

struct Config {
    base_url: String,
    quiet_after: chrono::Duration,
    daily_activity_recorded_token: String,
    record_engagement_decline_token: String,
    /// `None` when `ENGAGEMENT_WATCHER_STATE_FILE` is set to an empty
    /// string - see `alerter.rs`'s identical field for why this exists.
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
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct State {
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
    println!("engagement-watcher: polling {} every {POLL_INTERVAL:?}", config.base_url);

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
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Byte-for-byte the same shape as `alerter.rs`'s own `load_state` -
/// see its doc comment.
fn load_state(path: &std::path::Path) -> State {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(state) => {
                println!("engagement-watcher: resumed tracking state from {}", path.display());
                state
            }
            Err(e) => {
                eprintln!(
                    "engagement-watcher: {} exists but couldn't be parsed ({e}) - starting fresh",
                    path.display()
                );
                State::default()
            }
        },
        Err(_) => State::default(),
    }
}

/// Byte-for-byte the same shape as `alerter.rs`'s own `save_state` -
/// see its doc comment.
fn save_state(path: &std::path::Path, state: &State) {
    let tmp = path.with_extension("json.tmp");
    let write = std::fs::write(&tmp, serde_json::to_vec(state).expect("State always serializes"))
        .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = write {
        eprintln!("engagement-watcher: couldn't checkpoint state to {}: {e}", path.display());
    }
}

async fn tick(
    client: &reqwest::Client,
    config: &Config,
    state: &mut State,
) -> Result<(), reqwest::Error> {
    // --- track state from the event feed ---
    for (_, payload, _) in
        consume(client, &config.base_url, &config.daily_activity_recorded_token).await?
    {
        if payload["person_kind"].as_str() != Some("customer") {
            continue;
        }
        let Some(company_id) = payload["company_id"].as_str() else {
            continue;
        };
        let Some(day) = payload["day"].as_str().and_then(|d| DateTime::parse_from_rfc3339(d).ok())
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
            serde_json::json!({ "company_id": company_id, "flagged_at": now.to_rfc3339() }),
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

/// One `GET /v1/events/consume?mode=auto` call - byte-for-byte the same
/// shape `alerter.rs`'s own `consume` is, duplicated rather than shared
/// for the same no-common-library-boundary reason every other binary's
/// own copy already is.
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
    if body["accepted"].as_bool() == Some(true) {
        Ok(())
    } else {
        Err(format!(
            "rejected: {}",
            body["rejectionReason"].as_str().unwrap_or("unknown")
        ))
    }
}
