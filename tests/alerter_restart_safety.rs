//! Integration test for `src/bin/alerter.rs`'s own restart-safety
//! checkpoint (`ALERTER_STATE_FILE`/`load_state`/`save_state`) - the
//! bug this guards against, and the fix, are both explained in that
//! file's own "Restart safety" doc comment: a `state`-only-in-memory
//! alerter forgets every already-tracked ticket, permanently, on any
//! restart, because `mode=auto`'s server-tracked event cursor never
//! re-serves an already-consumed `TicketCreated`.
//!
//! `tests/alerting_feed.rs`'s own doc comment (and `src/bin/alerter.rs`'s
//! own module doc comment) both say this binary isn't exercised by
//! `cargo test`, because doing so for real means "spawning and tearing
//! down a real listening server, a different kind of test" than this
//! crate's other integration tests, which drive `Skilj::rest_router()`
//! in-process via `tower::ServiceExt::oneshot`. This file is exactly
//! that different kind: a real `TcpListener`/`axum::serve` (so the
//! *actual compiled* `alerter` binary, run as a real child process, has
//! a real socket to talk to over `reqwest`) and the real binary itself,
//! killed with `SIGKILL` (not a graceful shutdown - the whole point is
//! proving this survives a crash, not just a clean stop) and restarted.
//!
//! Kept out of `tests/alerting_feed.rs` deliberately: it already covers
//! the alerter's pure decision logic and event-feed path in-process,
//! cheaply; this one is the slow, heavyweight, "spawn a real binary and
//! wait on real timing" kind, and `tests/company.rs`'s own doc comment
//! is why that split is this crate's convention rather than one big file.

mod support;

use skilj_helpdesk::helpdesk::{TicketSummaryState, BOUNDED_CONTEXT};
use std::path::Path;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};
use support::{mint_command_token, mint_event_read_token, projection_state, runtime, setup, test_db, trigger, unique_name};

async fn command_token(
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    command_type_name: &str,
) -> String {
    mint_command_token(pool, mapping, BOUNDED_CONTEXT, command_type_name).await
}

async fn event_token(
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    event_type_name: &str,
) -> String {
    mint_event_read_token(pool, mapping, BOUNDED_CONTEXT, event_type_name).await
}

/// Every env var `src/bin/alerter.rs`'s own `Config::from_env` requires,
/// minted once and reused across both the pre-restart and post-restart
/// process - real `alerter` deployments would do the same (one set of
/// credentials, not reminted per restart).
struct AlerterTokens {
    ticket_created: String,
    ticket_resolved: String,
    ticket_reopened: String,
    ticket_closed: String,
    ticket_escalated: String,
    tickets_merged: String,
    escalate_ticket: String,
}

/// Kills (`SIGKILL`) and reaps the wrapped child unconditionally when
/// dropped - including on a panic unwind, e.g. `wait_until` below
/// timing out. `std::process::Child` does *not* do this itself: a
/// plain `Child` going out of scope, panic or not, leaves the real OS
/// process running. Found the hard way: an earlier version of this
/// test without this guard, run once deliberately against a
/// reintroduced version of the bug this test exists to catch, left an
/// orphaned `alerter` process behind after its own escalation check
/// timed out and panicked before reaching the manual `.kill()` call
/// that used to be the only thing stopping it - still polling a test
/// server that no longer existed, still re-writing its own checkpoint
/// file forever, invisible until a `pgrep` turned it up well after the
/// test binary itself had exited. Exactly the kind of leak this
/// project's own session history already had to clean up once for
/// real (leaked `postgresql_embedded` clusters) - this guard is that
/// lesson applied here before it could repeat.
struct KillOnDrop(Child);
impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// Spawns the *actual compiled* `alerter` binary (`CARGO_BIN_EXE_alerter`,
/// set by Cargo for integration tests in a crate whose own `Cargo.toml`
/// registers a `[[bin]] name = "alerter"`, guaranteeing it's built
/// before this test runs) against `base_url`, checkpointing to
/// `state_file`. `unhandled_alert_after_hours` is the same knob
/// `src/bin/alerter.rs`'s own doc comment describes for a demo: `0` to
/// escalate an already-overdue ticket on the very first tick.
fn spawn_alerter(
    base_url: &str,
    tokens: &AlerterTokens,
    state_file: &Path,
    unhandled_alert_after_hours: u32,
    stderr_log: &Path,
) -> KillOnDrop {
    let child = Command::new(env!("CARGO_BIN_EXE_alerter"))
        .env("SKILJ_BASE_URL", base_url)
        .env("UNHANDLED_ALERT_AFTER_HOURS", unhandled_alert_after_hours.to_string())
        .env("ALERTER_STATE_FILE", state_file)
        .env("TICKET_CREATED_TOKEN", &tokens.ticket_created)
        .env("TICKET_RESOLVED_TOKEN", &tokens.ticket_resolved)
        .env("TICKET_REOPENED_TOKEN", &tokens.ticket_reopened)
        .env("TICKET_CLOSED_TOKEN", &tokens.ticket_closed)
        .env("TICKET_ESCALATED_TOKEN", &tokens.ticket_escalated)
        .env("TICKETS_MERGED_TOKEN", &tokens.tickets_merged)
        .env("ESCALATE_TICKET_TOKEN", &tokens.escalate_ticket)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        // Captured to a file, not a pipe left undrained (that risks the
        // child blocking forever once the pipe buffer fills) - only
        // read back if this test fails, to help debug it.
        .stderr(std::fs::File::create(stderr_log).unwrap())
        .spawn()
        .expect("failed to spawn the alerter binary - was it built? (CARGO_BIN_EXE_alerter)");
    KillOnDrop(child)
}

/// Polls `predicate` every 200ms until it's true or `timeout` elapses -
/// this test is inherently about real elapsed time (a real child
/// process's own `POLL_INTERVAL`, a real HTTP round trip), not
/// something `trigger()`'s synchronous in-process request/response can
/// wait on the way every other test in this crate does. `predicate`
/// itself returns a future (rather than being sync) so an async check
/// like a projection read doesn't need a runtime-within-a-runtime to
/// call from here - no `futures` crate dependency needed just for this
/// one test, only what `tokio` (already a real dependency) provides.
async fn wait_until<F, Fut>(timeout: Duration, what: &str, mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let start = Instant::now();
    loop {
        if predicate().await {
            return;
        }
        if start.elapsed() > timeout {
            panic!("timed out after {timeout:?} waiting for: {what}");
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn checkpoint_tracks_ticket(state_file: &Path, ticket_id: &str) -> bool {
    let Ok(contents) = std::fs::read_to_string(state_file) else {
        return false;
    };
    let Ok(json) = serde_json::from_str::<serde_json::Value>(&contents) else {
        return false;
    };
    json["created_at"]
        .as_object()
        .is_some_and(|m| m.contains_key(ticket_id))
}

#[test]
fn alerter_survives_a_hard_restart_without_forgetting_an_already_tracked_ticket() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        // `trigger()` below needs its own handle on the router; the
        // spawned server (real socket, for the alerter subprocess) takes
        // ownership of a clone - same `Skilj`/pool underneath either way.
        let router = skilj.rest_router();
        let router_for_server = router.clone();

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("failed to bind an ephemeral port for the test server");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            axum::serve(listener, router_for_server)
                .await
                .expect("test server failed");
        });

        let sign_up = command_token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = command_token(&pool, &mapping, "CreateTicket").await;
        let tokens = AlerterTokens {
            ticket_created: event_token(&pool, &mapping, "TicketCreated").await,
            ticket_resolved: event_token(&pool, &mapping, "TicketResolved").await,
            ticket_reopened: event_token(&pool, &mapping, "TicketReopened").await,
            ticket_closed: event_token(&pool, &mapping, "TicketClosed").await,
            ticket_escalated: event_token(&pool, &mapping, "TicketEscalated").await,
            tickets_merged: event_token(&pool, &mapping, "TicketsMerged").await,
            escalate_ticket: command_token(&pool, &mapping, "EscalateTicket").await,
        };

        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");
        trigger(
            &router,
            &sign_up,
            serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
        )
        .await;
        trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "restart safety", "description": "d", "priority": "low",
            }),
        )
        .await;

        let tmp = std::env::temp_dir();
        let state_file = tmp.join(format!("{}.json", unique_name("alerter-restart-safety-state")));
        let stderr_1 = tmp.join(format!("{}.log", unique_name("alerter-restart-safety-stderr-1")));
        let stderr_2 = tmp.join(format!("{}.log", unique_name("alerter-restart-safety-stderr-2")));
        // Cleaned up unconditionally at the end via a guard, not just a
        // final line - a panic partway through (a real test failure)
        // would otherwise leak these into /tmp every time this test
        // fails, exactly the kind of leftover this session already had
        // to clean up once for real.
        struct Cleanup<'a>(&'a [&'a Path]);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                for path in self.0 {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        let _cleanup = Cleanup(&[&state_file, &stderr_1, &stderr_2]);

        // --- phase 1: track the ticket, then crash ---
        // A high threshold - this alerter must not escalate anything
        // yet, only observe and checkpoint the still-open ticket.
        let first = spawn_alerter(&base_url, &tokens, &state_file, 999, &stderr_1);
        wait_until(Duration::from_secs(20), "checkpoint file to track our ticket", || {
            std::future::ready(checkpoint_tracks_ticket(&state_file, &ticket_id))
        })
        .await;

        // SIGKILL (via `KillOnDrop`, not a graceful shutdown): proving
        // this survives an actual crash (no signal handler, no final
        // flush), not a clean stop - see `src/bin/alerter.rs`'s own
        // doc comment on why it checkpoints every tick rather than
        // relying on one at shutdown. Dropped explicitly right here,
        // not left to fall out of scope at the end of the function, so
        // it's SIGKILL'd *before* phase 2 starts, not just eventually.
        drop(first);

        // --- phase 2: a fresh process, same checkpoint, threshold now 0 ---
        // This is the actual regression check: a fresh `alerter`
        // process's own event-feed cursor is already past our ticket's
        // `TicketCreated` (auto-advance, non-replayable - the first
        // alerter already consumed it) - so if the checkpoint didn't
        // carry `created_at`/`unhandled` through, this second process
        // would never know this ticket exists at all, and nothing below
        // would ever happen, no matter how long this waits.
        let second = spawn_alerter(&base_url, &tokens, &state_file, 0, &stderr_2);

        wait_until(Duration::from_secs(20), "the restarted alerter to escalate our ticket", || {
            let pool = pool.clone();
            let ticket_id = ticket_id.clone();
            async move {
                projection_state::<TicketSummaryState>(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id)
                    .await
                    .escalated
            }
        })
        .await;

        drop(second);

        let final_state: TicketSummaryState =
            projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert!(
            final_state.escalated,
            "the ticket created before the crash should end up escalated by the restarted alerter"
        );
    });
}
