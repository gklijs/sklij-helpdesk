//! Application-owned replay cursors. Fetch does not mutate skilj's server
//! cursor: progress is durable only in the same checkpoint as derived state.
use chrono::{DateTime, Utc};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::{collections::HashMap, io::Write, path::Path};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Event {
    pub sequence: i64,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub metadata: Metadata,
}
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    pub created_at: DateTime<Utc>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Page {
    pub events: Vec<Event>,
    pub next_cursor: Option<String>,
}

/// Returns one replayable page. A failed request never changes progress.
pub async fn fetch(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    after: Option<&str>,
) -> Result<Page, reqwest::Error> {
    let mut request = client.get(format!("{base}/v1/events")).bearer_auth(token);
    if let Some(after) = after {
        request = request.query(&[("after", after)]);
    }
    let response = request.send().await?;
    if matches!(response.status().as_u16(), 400 | 401 | 403 | 409) {
        eprintln!("consumer: replay refused ({}); check the token and database epoch; restore a matching checkpoint or remove it to replay the current history", response.status());
        std::process::exit(1);
    }
    let page: Page = response.error_for_status()?.json().await?;
    if !page.events.is_empty() {
        fault_point("after_delivery");
    }
    Ok(page)
}

/// Fetch a bounded page using a stable feed name, independent of token IDs.
/// The caller folds this page and checkpoints positions and state together.
pub async fn replay(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    feed: &str,
    positions: &mut HashMap<String, String>,
) -> Result<Vec<Event>, reqwest::Error> {
    let page = fetch(client, base, token, positions.get(feed).map(String::as_str)).await?;
    if let Some(cursor) = page.next_cursor {
        positions.insert(feed.into(), cursor);
    }
    Ok(page.events)
}

/// A missing checkpoint safely starts a full replay. Corrupt or unreadable
/// checkpoints stop the process: operators can deliberately remove one to
/// rebuild from history, never silently discard a partial state.
pub fn load<T: DeserializeOwned + Default>(path: &Path) -> Result<T, String> {
    match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| {
            format!(
                "invalid checkpoint {}: {e}; restore it or remove it to replay history",
                path.display()
            )
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(T::default()),
        Err(e) => Err(format!("cannot read checkpoint {}: {e}", path.display())),
    }
}

/// Flush data, atomically replace, then flush the directory entry. Callers
/// stop on failure; continuing would make their in-memory progress unsafe.
pub fn save<T: Serialize>(path: &Path, state: &T) -> Result<(), String> {
    let write = || -> Result<(), Box<dyn std::error::Error>> {
        let temp = path.with_extension("json.tmp");
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&temp)?;
        file.write_all(&serde_json::to_vec(state)?)?;
        file.sync_all()?;
        std::fs::rename(temp, path)?;
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        std::fs::File::open(parent)?.sync_all()?;
        Ok(())
    };
    write().map_err(|e| format!("cannot checkpoint {}: {e}", path.display()))
}

/// Deterministic crash windows for the process-level recovery suite. Opt-in
/// through the environment of the disposable test process only.
pub fn fault_point(point: &str) {
    if std::env::var("HELPDESK_CONSUMER_FAULT").as_deref() == Ok(point) {
        eprintln!("consumer fault injection: {point}");
        std::process::exit(86);
    }
}
