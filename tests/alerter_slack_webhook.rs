//! Integration test for `src/bin/alerter.rs`'s own Slack integration
//! (`SLACK_WEBHOOK_URL`/`send_alert`) - see that file's own module doc
//! comment for what this proves: the "swap `send_alert` for a real
//! channel" seam `specs/skilj-helpdesk.allium`'s Excludes section left
//! open actually works, not just that it compiles. No real Slack
//! workspace involved - a tiny in-process HTTP server standing in for
//! Slack's own incoming-webhook endpoint, which is all `send_alert`
//! itself talks to (a bare `POST {"text": ...}`, no Slack SDK, nothing
//! else to fake).
//!
//! Same "real binary, real socket" shape `tests/alerter_restart_safety.rs`
//! uses (see that file's own doc comment for why this needs to be a
//! different kind of test than `tests/alerting_feed.rs`'s in-process
//! ones) - shares its `tests/support/mod.rs` plumbing
//! (`spawn_alerter`/`KillOnDrop`, `serve_for_real`, `wait_until`), kept
//! in its own file for the same reason `tests/company.rs`'s own doc
//! comment gives for this crate's test-splitting convention generally.

mod support;

use axum::extract::State;
use axum::routing::post;
use axum::Json;
use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{mint_alerter_tokens, mint_command_token, runtime, serve_for_real, setup, spawn_alerter, test_db, trigger, unique_name, wait_until};

#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<serde_json::Value>>>);

async fn capture(State(captured): State<Captured>, Json(body): Json<serde_json::Value>) -> &'static str {
    captured.0.lock().unwrap().push(body);
    "ok"
}

/// A fake Slack incoming-webhook endpoint: accepts any POST to
/// `/webhook`, records the JSON body, always answers 200 (Slack's own
/// webhooks always do too, on success) - `send_alert` never needs to
/// tell the difference between this and the real thing.
async fn spawn_fake_slack_webhook() -> (String, Captured) {
    let captured = Captured::default();
    let router = axum::Router::new()
        .route("/webhook", post(capture))
        .with_state(captured.clone());
    let base_url = serve_for_real(router).await;
    (format!("{base_url}/webhook"), captured)
}

#[test]
fn an_urgent_ticket_posts_a_real_slack_alert() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let base_url = serve_for_real(router.clone()).await;
        let (webhook_url, captured) = spawn_fake_slack_webhook().await;

        let sign_up = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
        let create_ticket = mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "CreateTicket").await;
        let tokens = mint_alerter_tokens(&pool, &mapping, BOUNDED_CONTEXT).await;

        let stderr_log = std::env::temp_dir().join(format!("{}.log", unique_name("alerter-slack-stderr")));
        struct Cleanup<'a>(&'a std::path::Path);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(self.0);
            }
        }
        let _cleanup = Cleanup(&stderr_log);

        // No checkpointing needed for this test - disabled outright
        // (see src/bin/alerter.rs's own ALERTER_STATE_FILE doc) rather
        // than leaving the default relative filename to land wherever
        // `cargo test`'s own CWD happens to be.
        let _alerter = spawn_alerter(
            &base_url,
            &tokens,
            &stderr_log,
            &[("ALERTER_STATE_FILE", ""), ("SLACK_WEBHOOK_URL", &webhook_url)],
        );

        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");
        trigger(
            &router,
            &sign_up,
            serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
        )
        .await;
        // `rule UrgentTicketNeedsImmediateAttention` - alerts straight
        // off `TicketCreated`, no deadline to wait out, unlike
        // `tests/alerter_restart_safety.rs`'s own overdue-escalation path.
        trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "Site is down", "description": "500s everywhere", "priority": "urgent",
            }),
        )
        .await;

        // Waits for *our own* ticket specifically, not just "a post
        // arrived" - this test's own token starts consuming from the
        // beginning of `TicketCreated`'s whole history (like
        // `tests/alerting_feed.rs`'s own doc comment describes for the
        // same reason), so a shared database with other urgent tickets
        // already on it (another test run, another test file) means the
        // very first post to land is often *not* ours - a bare
        // "non-empty" check here would pass on someone else's alert
        // before ours has even been posted yet. Found exactly that way,
        // once: this test flaked green on the wrong ticket.
        wait_until(Duration::from_secs(20), "the fake Slack webhook to receive our own ticket's post", || {
            let captured = captured.clone();
            let ticket_id = ticket_id.clone();
            async move {
                captured
                    .0
                    .lock()
                    .unwrap()
                    .iter()
                    .any(|p| p["text"].as_str().is_some_and(|t| t.contains(&ticket_id)))
            }
        })
        .await;

        let posts = captured.0.lock().unwrap();
        let post = posts
            .iter()
            .find(|p| p["text"].as_str().is_some_and(|t| t.contains(&ticket_id)))
            .unwrap_or_else(|| panic!("no Slack post mentioned our own ticket {ticket_id}: {posts:?}"));
        let text = post["text"].as_str().unwrap();
        assert!(text.contains("Urgent"), "expected the Urgent reason in the Slack text, got: {text}");
        assert!(text.contains(&company_id), "expected our own company id in the Slack text, got: {text}");
    });
}
