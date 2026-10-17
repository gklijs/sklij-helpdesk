//! Failure tests against the real binaries (issue #35): the compiled
//! `server` is SIGKILLed in the middle of a command batch, and the
//! Postgres underneath a running server, `alerter` and
//! `engagement-watcher` is restarted while traffic keeps coming.
//!
//! Both drive real processes over real sockets, the way
//! `tests/alerter_restart_safety.rs` does, but here `server` is a child
//! process too (`CARGO_BIN_EXE_server`): an in-process router dies with
//! the test, so it can't be killed half-way through a commit. Every
//! credential comes from what `server` prints at startup - the same
//! lines an operator pastes - so nothing here seeds the server's own
//! database behind its back except one extra event read token (test 2).
//!
//! - `sigkill_mid_batch_commits_nothing_partial_and_retries_land_once`:
//!   two `server` instances share one database (a rolling deploy, the
//!   same-version half of `tests/application_version.rs`'s side-by-side
//!   builds). Instance A is SIGKILLed while 32 clients keep
//!   `CreateTicket`s in flight; skilj's `CommandBatcher` commits each
//!   batch in one transaction, so the events, commands and idempotency
//!   keys must all agree afterwards, with the sequence still gapless.
//!   Every command is then retried with its `Idempotency-Key` against
//!   instance B: each one is accepted, the ones A acknowledged come back
//!   `deduplicated` with the same sequences, and every ticket exists
//!   exactly once. A then restarts next to B and serves again.
//!   Runs under CI's shared `DATABASE_URL` too: it uses
//!   `support::fresh_database`.
//! - `postgres_restart_under_load_recovers_and_consumers_resume_from_cursors`:
//!   needs a Postgres it may stop, so it always brings up its own
//!   embedded cluster, even under CI's `DATABASE_URL` (CI passes
//!   `GITHUB_TOKEN` so the binary download isn't rate limited). Postgres is stopped (fast
//!   shutdown, every connection cut) and started again while tickets
//!   keep being created; the server must accept commands again within
//!   `RECOVERY_BOUND`, the alerter must escalate every ticket created
//!   before, during and after the restart exactly once, the
//!   engagement-watcher must flag a company recorded before and one
//!   recorded after it, and a `mode=auto` reader running throughout must
//!   receive every `TicketCreated` exactly once - no gap, no duplicate.
//!
//! `cargo test --test failure_recovery` runs both, in well under a
//! minute in a debug build (the `[profile.dev.package.*]` entries in
//! `Cargo.toml` keep `server`'s RSA key generation from dominating).

mod support;

use serde_json::json;
use skilj_core::access_control::AccessLevel;
use skilj_core::db::{self, Pool};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use support::{
    fresh_database, mint_event_read_token, runtime, seed_mapping_for, seed_role, spawn_reaper,
    unique_name, wait_until, KillOnDrop,
};

/// How long the server may take to accept commands again once Postgres
/// is back.
const RECOVERY_BOUND: Duration = Duration::from_secs(30);

// --- the real `server` binary ---

/// A running `server` child, with everything it printed at startup.
struct Server {
    process: Option<KillOnDrop>,
    base_url: String,
    /// Every `Name: credential` / `export KEY=value` line, by the
    /// heading it was printed under (`"command tokens"`, `"to run the
    /// alerter against this server"`, ...).
    sections: HashMap<String, HashMap<String, String>>,
    startup: String,
    /// Kept for its `Drop`: deleted with the server unless a test fails.
    _logs: Logs,
}

impl Server {
    fn section(&self, heading_prefix: &str) -> &HashMap<String, String> {
        self.sections
            .iter()
            .find(|(heading, _)| heading.starts_with(heading_prefix))
            .map(|(_, entries)| entries)
            .unwrap_or_else(|| panic!("server printed no {heading_prefix:?} section"))
    }

    fn command_token(&self, command_type: &str) -> String {
        self.section("command tokens")[command_type].clone()
    }

    /// SIGKILL - no signal handler runs, nothing is flushed.
    fn kill(&mut self) {
        drop(self.process.take());
    }
}

/// Log files for a child process, deleted unless the test is failing.
struct Logs(Vec<PathBuf>);

impl Logs {
    fn new(label: &str, names: &[&str]) -> Self {
        let dir = std::env::temp_dir();
        Logs(
            names
                .iter()
                .map(|name| dir.join(format!("{}.log", unique_name(&format!("{label}-{name}")))))
                .collect(),
        )
    }
}

impl Drop for Logs {
    fn drop(&mut self) {
        if std::thread::panicking() {
            eprintln!("logs kept for debugging: {:?}", self.0);
            return;
        }
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// Starts the compiled `server` against `database_url` and waits until
/// it listens. Its stdout is read line by line (and copied to a log
/// file) so the credentials it prints can be picked up.
async fn spawn_server(database_url: &str, label: &str) -> Server {
    let port = free_port();
    let logs = Logs::new(label, &["stdout", "stderr"]);
    let mut child = Command::new(env!("CARGO_BIN_EXE_server"))
        .env("DATABASE_URL", database_url)
        // `support::test_master_key()`'s bytes, hex-encoded.
        .env("ENCRYPTION_MASTER_KEY", "07".repeat(32))
        .env("PORT", port.to_string())
        // Small pools: under CI two servers share one Postgres with every
        // other test binary.
        .env("DATABASE_MAX_CONNECTIONS", "16")
        .env("DATABASE_APP_MAX_CONNECTIONS", "4")
        .env("HTTP_MAX_IN_FLIGHT_REQUESTS", "200")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("OIDC_ISSUER_URL")
        .env_remove("SEED_DEMO_TRAFFIC")
        .env("TICKET_ROUTING", "shared")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(std::fs::File::create(&logs.0[1]).unwrap())
        .spawn()
        .expect("failed to spawn the server binary (CARGO_BIN_EXE_server)");
    let stdout = child.stdout.take().unwrap();
    let process = KillOnDrop(child);

    let lines = Arc::new(Mutex::new(Vec::<String>::new()));
    {
        let lines = lines.clone();
        let mut file = std::fs::File::create(&logs.0[0]).unwrap();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let _ = writeln!(file, "{line}");
                lines.lock().unwrap().push(line);
            }
        });
    }

    let mut process = process;
    let start = Instant::now();
    loop {
        if lines
            .lock()
            .unwrap()
            .iter()
            .any(|line| line.contains("skilj-helpdesk listening on"))
        {
            break;
        }
        if let Ok(Some(status)) = process.0.try_wait() {
            panic!(
                "server {label} exited during startup ({status}); see {:?}",
                logs.0
            );
        }
        if start.elapsed() > Duration::from_secs(90) {
            panic!(
                "server {label} didn't start listening in 90s; see {:?}",
                logs.0
            );
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let startup = lines.lock().unwrap().join("\n");
    Server {
        process: Some(process),
        base_url: format!("http://127.0.0.1:{port}"),
        sections: parse_sections(&startup),
        startup,
        _logs: logs,
    }
}

fn parse_sections(stdout: &str) -> HashMap<String, HashMap<String, String>> {
    let mut sections: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut heading = String::new();
    for line in stdout.lines() {
        if line.is_empty() {
            continue;
        }
        let Some(entry) = line.strip_prefix("  ") else {
            heading = line.trim_end_matches(':').to_string();
            continue;
        };
        let pair = match entry.strip_prefix("export ") {
            Some(export) => export.split_once('='),
            None => entry
                .split_once(": ")
                .filter(|(name, value)| !name.contains(' ') && !value.contains(' ')),
        };
        if let Some((name, value)) = pair {
            sections
                .entry(heading.clone())
                .or_default()
                .insert(name.to_string(), value.to_string());
        }
    }
    sections
}

/// Starts one of the consumer binaries with the env block `server`
/// printed for it, plus `extra`.
fn spawn_consumer(
    binary: &str,
    exports: &HashMap<String, String>,
    extra: &[(&str, &str)],
    logs: &Logs,
) -> KillOnDrop {
    let mut command = Command::new(binary);
    command
        .envs(exports)
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env("TICKET_ROUTING", "shared")
        .env_remove("SLACK_WEBHOOK_URL")
        .stdin(Stdio::null())
        .stdout(std::fs::File::create(&logs.0[0]).unwrap())
        .stderr(std::fs::File::create(&logs.0[1]).unwrap());
    for (key, value) in extra {
        command.env(key, value);
    }
    KillOnDrop(command.spawn().expect("failed to spawn a consumer binary"))
}

// --- REST over a real socket ---

/// `POST /v1/commands/trigger`; `Err` for anything but a `200` with a
/// JSON body (a refused connection, a reset, a 5xx during an outage).
async fn trigger(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    payload: serde_json::Value,
    idempotency_key: Option<&str>,
) -> Result<serde_json::Value, String> {
    let mut request = client
        .post(format!("{base_url}/v1/commands/trigger"))
        .bearer_auth(token)
        .timeout(Duration::from_secs(20))
        .json(&json!({ "payload": payload }));
    if let Some(key) = idempotency_key {
        request = request.header("Idempotency-Key", key);
    }
    let response = request.send().await.map_err(|e| e.to_string())?;
    let status = response.status();
    if !status.is_success() {
        return Err(format!("HTTP {status}"));
    }
    response.json().await.map_err(|e| e.to_string())
}

/// `trigger`, retried until the server answers - for setup steps that
/// may race a recovering server.
async fn trigger_accepted(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    payload: serde_json::Value,
) {
    let start = Instant::now();
    loop {
        match trigger(client, base_url, token, payload.clone(), None).await {
            Ok(response) => {
                assert_eq!(response["accepted"], true, "{payload} -> {response}");
                return;
            }
            Err(e) if start.elapsed() < RECOVERY_BOUND => {
                eprintln!("retrying {payload}: {e}");
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => panic!("{payload} never got an answer: {e}"),
        }
    }
}

fn create_ticket_payload(ticket_id: &str, company_id: &str) -> serde_json::Value {
    json!({
        "ticket_id": ticket_id, "company_id": company_id, "requester_id": "customer-1",
        "logged_by_staff_id": null, "title": "failure test", "description": "d", "priority": "low",
    })
}

// --- reading a bounded context's tables directly ---

/// The bounded context's events must be numbered 0..n without a hole: a
/// batch that half-committed, or a sequence handed out by a transaction
/// that then died, would leave one.
async fn assert_gapless(pool: &Pool, bounded_context: &str) {
    let (count, max): (i64, i64) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT count(*), coalesce(max(sequence), -1) FROM \"bc_{bounded_context}\".events"
    )))
    .fetch_one(pool)
    .await
    .unwrap();
    assert_eq!(
        count,
        max + 1,
        "{bounded_context}'s event sequence has a gap"
    );
}

/// `payload ->> key` for every `event_type` event, with how often each
/// value occurs.
async fn event_counts_by(
    pool: &Pool,
    bounded_context: &str,
    event_type: &str,
    key: &str,
) -> BTreeMap<String, i64> {
    sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(format!(
        "SELECT payload::jsonb ->> '{key}', count(*) FROM \"bc_{bounded_context}\".events \
         WHERE event_type_name = '{event_type}' GROUP BY 1"
    )))
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .collect()
}

// --- test 1: SIGKILL mid-batch, rolling deploy ---

#[test]
fn sigkill_mid_batch_commits_nothing_partial_and_retries_land_once() {
    runtime().block_on(async {
        let Some((database_url, pool)) = fresh_database("failure_sigkill").await else {
            return;
        };
        // Rolling deploy: two instances of the same build, one database.
        let mut a = spawn_server(&database_url, "server-a").await;
        let b = spawn_server(&database_url, "server-b").await;
        assert!(
            !b.startup.contains("kept registrations from a newer version"),
            "a same-version instance found newer registrations:\n{}",
            b.startup
        );

        let client = reqwest::Client::new();
        let company_id = unique_name("company");
        trigger_accepted(
            &client,
            &a.base_url,
            &a.command_token("SignUpCompany"),
            json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
        )
        .await;
        // A's token: tokens live in the database, so B accepts it too.
        let create = a.command_token("CreateTicket");

        // 32 clients, each sending its next CreateTicket as soon as the
        // previous one is answered, so A always has commands queued in
        // its batcher. Each ticket's Idempotency-Key is its id.
        const CLIENTS: usize = 32;
        const KILL_AFTER_ACKS: usize = 60;
        let ticket_ids: Vec<String> = (0..4000).map(|i| format!("{company_id}-t{i}")).collect();
        let next = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let mut workers = Vec::new();
        for _ in 0..CLIENTS {
            let (client, base_url, create, company_id) =
                (client.clone(), a.base_url.clone(), create.clone(), company_id.clone());
            let (ticket_ids, next, tx) = (ticket_ids.clone(), next.clone(), tx.clone());
            workers.push(tokio::spawn(async move {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(ticket_id) = ticket_ids.get(i) else { return };
                    let payload = create_ticket_payload(ticket_id, &company_id);
                    let result = trigger(&client, &base_url, &create, payload, Some(ticket_id)).await;
                    let failed = result.is_err();
                    let _ = tx.send((ticket_id.clone(), result));
                    if failed {
                        return;
                    }
                }
            }));
        }
        drop(tx);

        let mut acked: HashMap<String, serde_json::Value> = HashMap::new();
        let mut unanswered: Vec<String> = Vec::new();
        while acked.len() < KILL_AFTER_ACKS {
            let (ticket_id, result) = rx.recv().await.expect("workers stopped before the kill");
            let response = result.unwrap_or_else(|e| panic!("{ticket_id} failed before the kill: {e}"));
            assert_eq!(response["accepted"], true, "{response}");
            acked.insert(ticket_id, response);
        }
        a.kill();
        while let Some((ticket_id, result)) = rx.recv().await {
            match result {
                Ok(response) if response["accepted"] == true => {
                    acked.insert(ticket_id, response);
                }
                Ok(response) => panic!("{ticket_id} rejected: {response}"),
                Err(_) => unanswered.push(ticket_id),
            }
        }
        for worker in workers {
            worker.await.unwrap();
        }
        assert!(
            !unanswered.is_empty(),
            "the kill should have cut off commands in flight"
        );

        // Whatever A committed, it committed whole: every command has its
        // event, every idempotency key names events that exist, no
        // sequence is missing, and every acknowledged ticket is there.
        assert_gapless(&pool, "helpdesk").await;
        let orphaned_commands: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM bc_helpdesk.commands c WHERE NOT EXISTS \
             (SELECT 1 FROM bc_helpdesk.events e WHERE e.origin_command_id = c.id)",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(orphaned_commands, 0, "a command was stored without its events");
        let dangling_keys: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM bc_helpdesk.idempotency_keys k WHERE EXISTS \
             (SELECT 1 FROM unnest(k.triggered_event_sequences) s \
              WHERE NOT EXISTS (SELECT 1 FROM bc_helpdesk.events e WHERE e.sequence = s))",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(dangling_keys, 0, "an idempotency key outlived its events");
        let keyed: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM bc_helpdesk.idempotency_keys WHERE command_type_name = 'CreateTicket'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let created = event_counts_by(&pool, "helpdesk", "TicketCreated", "ticket_id").await;
        assert_eq!(keyed, created.len() as i64, "tickets and idempotency keys disagree");
        assert!(created.values().all(|n| *n == 1), "{created:?}");
        for ticket_id in acked.keys() {
            assert!(created.contains_key(ticket_id), "acknowledged {ticket_id} is missing");
        }
        let committed_unanswered = unanswered.iter().filter(|t| created.contains_key(*t)).count();
        eprintln!(
            "killed with {} acknowledged, {} unanswered ({committed_unanswered} of those committed anyway)",
            acked.len(),
            unanswered.len()
        );

        // Every client retries against the surviving instance, same key.
        let attempted: Vec<&String> = acked.keys().chain(unanswered.iter()).collect();
        for round in 0..2 {
            for ticket_id in &attempted {
                let payload = create_ticket_payload(ticket_id, &company_id);
                let response = trigger(&client, &b.base_url, &create, payload, Some(ticket_id))
                    .await
                    .unwrap_or_else(|e| panic!("retrying {ticket_id} on B: {e}"));
                assert_eq!(response["accepted"], true, "retry of {ticket_id}: {response}");
                let committed_before =
                    round == 1 || acked.contains_key(*ticket_id) || created.contains_key(*ticket_id);
                assert_eq!(
                    response["deduplicated"], committed_before,
                    "retry {round} of {ticket_id}: {response}"
                );
                if let Some(original) = acked.get(*ticket_id) {
                    assert_eq!(
                        response["triggeredEventSequences"], original["triggeredEventSequences"],
                        "{ticket_id}'s retry names different events"
                    );
                }
            }
        }
        let created = event_counts_by(&pool, "helpdesk", "TicketCreated", "ticket_id").await;
        let expected: HashSet<&str> = attempted.iter().map(|t| t.as_str()).collect();
        let actual: HashSet<&str> = created.keys().map(String::as_str).collect();
        assert_eq!(actual, expected);
        assert!(created.values().all(|n| *n == 1), "{created:?}");
        assert_gapless(&pool, "helpdesk").await;

        // A comes back next to B, and serves.
        let a2 = spawn_server(&database_url, "server-a2").await;
        assert!(!a2.startup.contains("kept registrations from a newer version"));
        let response = trigger(
            &client,
            &a2.base_url,
            &create,
            create_ticket_payload(attempted[0], &company_id),
            Some(attempted[0]),
        )
        .await
        .unwrap();
        assert_eq!(response["deduplicated"], true, "{response}");
        let fresh = format!("{company_id}-after-restart");
        trigger_accepted(&client, &a2.base_url, &create, create_ticket_payload(&fresh, &company_id))
            .await;
        assert_gapless(&pool, "helpdesk").await;
        drop((a, b, a2));
    });
}

// --- test 2: Postgres restart under load ---

/// A `mode=auto` reader of `TicketCreated` running next to the binaries,
/// recording every sequence it is served.
async fn read_ticket_created(
    client: reqwest::Client,
    base_url: String,
    token: String,
    stop: Arc<AtomicBool>,
    seen: Arc<Mutex<Vec<i64>>>,
) {
    while !stop.load(Ordering::SeqCst) {
        let response = client
            .get(format!("{base_url}/v1/events/consume"))
            .query(&[("mode", "auto")])
            .bearer_auth(&token)
            .timeout(Duration::from_secs(20))
            .send()
            .await;
        if let Ok(response) = response {
            if response.status().is_success() {
                let body: serde_json::Value = response.json().await.unwrap();
                let mut seen = seen.lock().unwrap();
                for event in body["events"].as_array().unwrap() {
                    seen.push(event["sequence"].as_i64().unwrap());
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[derive(Default)]
struct Load {
    /// Ticket id, and when it was accepted.
    accepted: Vec<(String, Instant)>,
    failed_attempts: usize,
}

/// Creates tickets one after another until `stop`, retrying each with
/// its own Idempotency-Key until it is accepted - a client riding out
/// the outage.
async fn create_tickets(
    client: reqwest::Client,
    base_url: String,
    token: String,
    company_id: String,
    stop: Arc<AtomicBool>,
    load: Arc<Mutex<Load>>,
) {
    let mut i = 0;
    while !stop.load(Ordering::SeqCst) {
        let ticket_id = format!("{company_id}-t{i}");
        let payload = create_ticket_payload(&ticket_id, &company_id);
        match trigger(&client, &base_url, &token, payload, Some(&ticket_id)).await {
            Ok(response) => {
                assert_eq!(response["accepted"], true, "{ticket_id}: {response}");
                load.lock()
                    .unwrap()
                    .accepted
                    .push((ticket_id, Instant::now()));
                i += 1;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
            Err(_) => {
                load.lock().unwrap().failed_attempts += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

#[test]
fn postgres_restart_under_load_recovers_and_consumers_resume_from_cursors() {
    runtime().block_on(async {
        // Its own cluster, not `support::test_db()`'s, and deliberately
        // not `DATABASE_URL` either: that one is shared with every other
        // test binary, which mustn't see Postgres go away.
        let mut postgres = postgresql_embedded::PostgreSQL::default();
        if let Err(e) = postgres.setup().await {
            eprintln!("skipping: embedded PostgreSQL setup failed: {e}");
            return;
        }
        if let Err(e) = postgres.start().await {
            eprintln!("skipping: embedded PostgreSQL failed to start: {e}");
            return;
        }
        spawn_reaper(postgres.settings());
        postgres.create_database("failure_restart").await.unwrap();
        let database_url = postgres.settings().url("failure_restart");

        let server = spawn_server(&database_url, "server").await;
        let pool = db::connect(&database_url).await.unwrap();
        let client = reqwest::Client::new();
        let base_url = server.base_url.clone();

        // The extra reader's own token, on its own Role.
        let role = seed_role(&pool, "failure-reader").await;
        let mapping = seed_mapping_for(&pool, &role, "helpdesk", AccessLevel::Admin, None).await;
        let reader_token = mint_event_read_token(&pool, &mapping, "helpdesk", "TicketCreated").await;

        let company_id = unique_name("company");
        trigger_accepted(
            &client,
            &base_url,
            &server.command_token("SignUpCompany"),
            json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
        )
        .await;
        let record_activity = server.section("activity's own command tokens")["RecordDailyActivity"].clone();
        let long_ago = (chrono::Utc::now() - chrono::Duration::days(30))
            .date_naive()
            .and_hms_opt(0, 0, 0)
            .unwrap()
            .and_utc()
            .to_rfc3339();
        let quiet_company = |id: &str| {
            json!({ "company_id": id, "person_kind": "customer", "person_subject": "c-1", "day": long_ago })
        };
        let quiet_before = unique_name("quiet-before");
        trigger_accepted(&client, &base_url, &record_activity, quiet_company(&quiet_before)).await;

        // The consumers, checkpointing to files as in production.
        let alerter_logs = Logs::new("alerter", &["stdout", "stderr", "state"]);
        let watcher_logs = Logs::new("engagement-watcher", &["stdout", "stderr", "state"]);
        let _alerter = spawn_consumer(
            env!("CARGO_BIN_EXE_alerter"),
            server.section("to run the alerter"),
            &[
                ("ALERTER_STATE_FILE", alerter_logs.0[2].to_str().unwrap()),
                ("UNHANDLED_ALERT_AFTER_HOURS", "0"),
                ("ALERTER_POLL_INTERVAL_MS", "300"),
            ],
            &alerter_logs,
        );
        let _watcher = spawn_consumer(
            env!("CARGO_BIN_EXE_engagement-watcher"),
            server.section("to run engagement-watcher"),
            &[
                ("ENGAGEMENT_WATCHER_STATE_FILE", watcher_logs.0[2].to_str().unwrap()),
                ("QUIET_AFTER_DAYS", "1"),
                ("ENGAGEMENT_WATCHER_POLL_INTERVAL_MS", "300"),
            ],
            &watcher_logs,
        );

        let stop = Arc::new(AtomicBool::new(false));
        let stop_reader = Arc::new(AtomicBool::new(false));
        let load = Arc::new(Mutex::new(Load::default()));
        let seen = Arc::new(Mutex::new(Vec::new()));
        let load_task = tokio::spawn(create_tickets(
            client.clone(),
            base_url.clone(),
            server.command_token("CreateTicket"),
            company_id.clone(),
            stop.clone(),
            load.clone(),
        ));
        let reader_task = tokio::spawn(read_ticket_created(
            client.clone(),
            base_url.clone(),
            reader_token,
            stop_reader.clone(),
            seen.clone(),
        ));

        // Everything is moving before the restart.
        wait_until(Duration::from_secs(30), "traffic and both consumers to get going", || {
            let pool = pool.clone();
            let (load, quiet_before) = (load.clone(), quiet_before.clone());
            async move {
                load.lock().unwrap().accepted.len() >= 5
                    && !event_counts_by(&pool, "helpdesk", "TicketEscalated", "ticket_id").await.is_empty()
                    && event_counts_by(&pool, "activity", "CompanyEngagementDeclined", "company_id")
                        .await
                        .contains_key(&quiet_before)
            }
        })
        .await;
        pool.close().await;

        // Fast shutdown: every backend is terminated, in-flight
        // transactions roll back.
        postgres.stop().await.unwrap();
        tokio::time::sleep(Duration::from_secs(2)).await;
        postgres.start().await.unwrap();
        let restarted = Instant::now();
        let pool = db::connect(&database_url).await.unwrap();

        wait_until(RECOVERY_BOUND, "the server to accept commands again", || {
            std::future::ready(load.lock().unwrap().accepted.iter().any(|(_, at)| *at > restarted))
        })
        .await;
        eprintln!("server recovered {:?} after Postgres came back", restarted.elapsed());

        let quiet_after = unique_name("quiet-after");
        trigger_accepted(&client, &base_url, &record_activity, quiet_company(&quiet_after)).await;
        wait_until(Duration::from_secs(20), "more tickets after the restart", || {
            std::future::ready(
                load.lock().unwrap().accepted.iter().filter(|(_, at)| *at > restarted).count() >= 5,
            )
        })
        .await;
        stop.store(true, Ordering::SeqCst);
        load_task.await.unwrap();
        let tickets: HashSet<String> =
            load.lock().unwrap().accepted.iter().map(|(t, _)| t.clone()).collect();
        eprintln!(
            "{} tickets accepted, {} attempts failed during the outage",
            tickets.len(),
            load.lock().unwrap().failed_attempts
        );

        // The alerter: every ticket, before, during and after, escalated
        // once - its cursor neither skipped nor replayed a TicketCreated.
        wait_until(Duration::from_secs(30), "the alerter to escalate every ticket", || {
            let (pool, tickets) = (pool.clone(), tickets.clone());
            async move {
                let escalated = event_counts_by(&pool, "helpdesk", "TicketEscalated", "ticket_id").await;
                tickets.iter().all(|t| escalated.contains_key(t))
            }
        })
        .await;
        let escalated = event_counts_by(&pool, "helpdesk", "TicketEscalated", "ticket_id").await;
        assert_eq!(escalated.keys().cloned().collect::<HashSet<_>>(), tickets);
        assert!(escalated.values().all(|n| *n == 1), "{escalated:?}");

        // The engagement-watcher: both companies, each flagged once.
        wait_until(Duration::from_secs(20), "the watcher to flag the second company", || {
            let (pool, quiet_after) = (pool.clone(), quiet_after.clone());
            async move {
                event_counts_by(&pool, "activity", "CompanyEngagementDeclined", "company_id")
                    .await
                    .contains_key(&quiet_after)
            }
        })
        .await;
        let flagged = event_counts_by(&pool, "activity", "CompanyEngagementDeclined", "company_id").await;
        assert_eq!(flagged.get(&quiet_before), Some(&1), "{flagged:?}");
        assert_eq!(flagged.get(&quiet_after), Some(&1), "{flagged:?}");

        // The extra reader: exactly the committed TicketCreated
        // sequences, each once, in order.
        let committed: Vec<i64> = sqlx::query_scalar(
            "SELECT sequence FROM bc_helpdesk.events WHERE event_type_name = 'TicketCreated' \
             ORDER BY sequence",
        )
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(committed.len(), tickets.len());
        wait_until(Duration::from_secs(10), "the reader to catch up", || {
            std::future::ready(seen.lock().unwrap().len() >= committed.len())
        })
        .await;
        stop_reader.store(true, Ordering::SeqCst);
        reader_task.await.unwrap();
        assert_eq!(*seen.lock().unwrap(), committed, "the reader's cursor skipped or replayed");

        assert_gapless(&pool, "helpdesk").await;
        assert_gapless(&pool, "activity").await;
        pool.close().await;
        drop(server);
    });
}
