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
//! that different kind: a real socket (so the *actual compiled*
//! `alerter` binary, run as a real child process, has something to talk
//! to over `reqwest`) and the real binary itself, killed with `SIGKILL`
//! (not a graceful shutdown - the whole point is proving this survives
//! a crash, not just a clean stop) and restarted. The plumbing for both
//! (`support::serve_for_real`, `support::spawn_alerter`/`KillOnDrop`,
//! `support::wait_until`) lives in `tests/support/mod.rs`, shared with
//! `tests/alerter_slack_webhook.rs` - the other test that needs the
//! real binary rather than its in-process decision logic.
//!
//! Kept out of `tests/alerting_feed.rs` deliberately: it already covers
//! the alerter's pure decision logic and event-feed path in-process,
//! cheaply; this one is the slow, heavyweight, "spawn a real binary and
//! wait on real timing" kind, and `tests/company.rs`'s own doc comment
//! is why that split is this crate's convention rather than one big file.

mod support;

use skilj_helpdesk::helpdesk::{TicketSummaryState, BOUNDED_CONTEXT};
use std::path::Path;
use std::time::Duration;
use support::{mint_alerter_tokens, mint_command_token, projection_state, runtime, serve_for_real, setup, spawn_alerter, test_db, trigger, unique_name, wait_until};

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
        // real server started by `serve_for_real` takes ownership of a
        // clone - same `Skilj`/pool underneath either way.
        let router = skilj.rest_router();
        let base_url = serve_for_real(router.clone()).await;

        let sign_up = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
        let create_ticket = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "CreateTicket").await;
        let tokens = mint_alerter_tokens(&pool, &mapping, BOUNDED_CONTEXT).await;

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
        let state_file_str = state_file.to_str().unwrap();

        // --- phase 1: track the ticket, then crash ---
        // A high threshold - this alerter must not escalate anything
        // yet, only observe and checkpoint the still-open ticket.
        let first = spawn_alerter(
            &base_url,
            &tokens,
            &stderr_1,
            &[("ALERTER_STATE_FILE", state_file_str), ("UNHANDLED_ALERT_AFTER_HOURS", "999")],
        );
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
        let second = spawn_alerter(
            &base_url,
            &tokens,
            &stderr_2,
            &[("ALERTER_STATE_FILE", state_file_str), ("UNHANDLED_ALERT_AFTER_HOURS", "0")],
        );

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
