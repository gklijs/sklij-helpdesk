//! End to end for the inbound email channel (`src/email_channel.rs`,
//! `src/bin/email-bridge.rs`): a real NATS server with JetStream (Docker,
//! via `testcontainers`), a real skilj served on a real socket, and the
//! actual compiled `email-bridge` binary between them. The only fake is
//! the mail gateway, which is this test publishing `InboundEmail`s.
//!
//! One test, not several: the whole flow is one customer's story, and
//! one NATS container per test would be the slow part.
//!
//! Skips, like the Postgres-backed tests, when Docker isn't reachable.

mod support;

use async_nats::jetstream;
use futures_util::StreamExt;
use skilj_helpdesk::email_channel::{
    email_key, ticket_id_for, InboundEmail, RECEIVED_SUBJECT, STREAM, UNROUTABLE_REASON_HEADER,
    UNROUTABLE_SUBJECT,
};
use skilj_helpdesk::helpdesk::{TicketSummaryState, BOUNDED_CONTEXT};
use std::time::Duration;
use support::{
    accepted, mint_command_token, projection_state, runtime, serve_for_real, setup, test_db,
    trigger, unique_name, wait_until, KillOnDrop,
};
use testcontainers_modules::nats::{Nats, NatsServerCmd};
use testcontainers_modules::testcontainers::runners::AsyncRunner;
use testcontainers_modules::testcontainers::ImageExt;

const WAIT: Duration = Duration::from_secs(30);

async fn ticket_status(pool: &skilj_core::db::Pool, ticket_id: &str) -> Option<String> {
    let state: TicketSummaryState =
        projection_state(pool, BOUNDED_CONTEXT, "TicketSummary", ticket_id).await;
    state.status
}

/// Events in `helpdesk` carrying `correlation_id` - what `email-bridge`
/// sets from the email's own key.
async fn events_correlated_to(pool: &skilj_core::db::Pool, correlation_id: &str) -> Vec<String> {
    sqlx::query_scalar(sqlx::AssertSqlSafe(format!(
        "SELECT event_type_name FROM \"bc_{BOUNDED_CONTEXT}\".events \
         WHERE metadata_correlation_id = $1 ORDER BY sequence"
    )))
    .bind(correlation_id)
    .fetch_all(pool)
    .await
    .unwrap()
}

async fn send(jetstream: &jetstream::Context, email: &InboundEmail) {
    jetstream
        .publish(RECEIVED_SUBJECT, serde_json::to_vec(email).unwrap().into())
        .await
        .unwrap()
        .await
        .unwrap();
}

#[test]
fn emails_become_tickets_and_replies_through_the_nats_bridge() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let nats = match Nats::default()
            .with_cmd(&NatsServerCmd::default().with_jetstream())
            .start()
            .await
        {
            Ok(container) => container,
            Err(e) => {
                eprintln!("skipping: couldn't start a NATS container (is Docker running?): {e}");
                return;
            }
        };
        let nats_url = format!(
            "nats://{}:{}",
            nats.get_host().await.unwrap(),
            nats.get_host_port_ipv4(4222).await.unwrap()
        );

        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let base_url = serve_for_real(router.clone()).await;
        let token = |name: &'static str| mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, name);
        let sign_up = token("SignUpCompany").await;
        let assign = token("AssignTicket").await;
        let request_info = token("RequestInfoFromCustomer").await;
        let create_ticket = token("CreateTicket").await;
        let customer_responds = token("CustomerRespondsToTicket").await;

        let company_id = unique_name("company");
        trigger(
            &router,
            &sign_up,
            serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
        )
        .await;

        let log = std::env::temp_dir().join(format!("{}.log", unique_name("email-bridge")));
        let _bridge = KillOnDrop(
            std::process::Command::new(env!("CARGO_BIN_EXE_email-bridge"))
                .env("NATS_URL", &nats_url)
                .env("SKILJ_BASE_URL", &base_url)
                .env("CREATE_TICKET_TOKEN", &create_ticket)
                .env("CUSTOMER_RESPONDS_TO_TICKET_TOKEN", &customer_responds)
                .env_remove("TICKET_ROUTING")
                .stdin(std::process::Stdio::null())
                // Tracing goes to stdout. Both go to one file, left
                // behind in the temp dir only if the test fails.
                .stdout(std::fs::File::create(&log).unwrap())
                .stderr(std::fs::File::options().append(true).open(&log).unwrap())
                .spawn()
                .expect("failed to spawn email-bridge"),
        );

        let client = async_nats::connect(&nats_url).await.unwrap();
        let jetstream = jetstream::new(client);
        // The bridge creates the stream on startup; publishing before
        // that would go nowhere.
        wait_until(WAIT, "email-bridge to create its stream", || {
            let jetstream = jetstream.clone();
            async move { jetstream.get_stream(STREAM).await.is_ok() }
        })
        .await;

        // 1. A new email to support+<company> opens a ticket.
        let first = InboundEmail {
            message_id: format!("<{}@mail.example>", unique_name("msg")),
            from_address: "Alice@Example.com".into(),
            from_name: Some("Alice".into()),
            to_address: format!("support+{company_id}@help.example"),
            subject: "Printer on fire".into(),
            body: "It is on fire.".into(),
        };
        let ticket_id = ticket_id_for(&first.message_id);
        send(&jetstream, &first).await;
        wait_until(WAIT, "the email's ticket to be open", || async {
            ticket_status(&pool, &ticket_id).await.as_deref() == Some("open")
        })
        .await;
        let first_key = email_key(&first.message_id);
        assert_eq!(
            events_correlated_to(&pool, &first_key).await,
            vec!["TicketCreated"],
            "the ticket's events carry the email's key as correlation id"
        );

        // 2. The gateway delivering the same email twice doesn't make a
        // second ticket. A different email sent after it is the marker:
        // each subject is read in order, so once its ticket exists the
        // duplicate has been through the whole pipeline too.
        send(&jetstream, &first).await;
        let marker = InboundEmail {
            message_id: format!("<{}@mail.example>", unique_name("msg")),
            subject: "Unrelated".into(),
            ..first.clone()
        };
        send(&jetstream, &marker).await;
        let marker_ticket = ticket_id_for(&marker.message_id);
        wait_until(WAIT, "the marker email's ticket", || async {
            ticket_status(&pool, &marker_ticket).await.is_some()
        })
        .await;
        assert_eq!(
            events_correlated_to(&pool, &first_key).await,
            vec!["TicketCreated"],
            "a redelivered email changes nothing"
        );

        // 3. Staff ask a question; the customer answers by email.
        let staff_id = unique_name("staff");
        let requester_id = "email:alice@example.com";
        trigger(&router, &assign, serde_json::json!({ "ticket_id": ticket_id, "staff_id": staff_id })).await;
        let asked = trigger(
            &router,
            &request_info,
            serde_json::json!({ "ticket_id": ticket_id, "staff_id": staff_id, "message": "Which printer?", "requester_id": requester_id }),
        )
        .await;
        assert!(accepted(&asked), "{asked:?}");
        assert_eq!(ticket_status(&pool, &ticket_id).await.as_deref(), Some("waiting_on_customer"));

        let reply = InboundEmail {
            message_id: format!("<{}@mail.example>", unique_name("msg")),
            from_address: "alice@example.com".into(),
            from_name: None,
            to_address: format!("ticket+{ticket_id}@help.example"),
            subject: "Re: Printer on fire".into(),
            body: "The one on floor 2.".into(),
        };
        send(&jetstream, &reply).await;
        wait_until(WAIT, "the emailed reply to move the ticket back to in_progress", || async {
            ticket_status(&pool, &ticket_id).await.as_deref() == Some("in_progress")
        })
        .await;
        assert_eq!(
            events_correlated_to(&pool, &email_key(&reply.message_id)).await,
            vec!["TicketCustomerResponded"]
        );

        // 4. An email to an address that means nothing is kept on the
        // unroutable subject, with the reason, not dropped.
        let stray = InboundEmail {
            to_address: "sales@help.example".into(),
            message_id: format!("<{}@mail.example>", unique_name("msg")),
            ..first.clone()
        };
        send(&jetstream, &stray).await;
        let stream = jetstream.get_stream(STREAM).await.unwrap();
        let unroutable = stream
            .create_consumer(jetstream::consumer::pull::Config {
                filter_subject: UNROUTABLE_SUBJECT.to_string(),
                ..Default::default()
            })
            .await
            .unwrap();
        let mut messages = unroutable.messages().await.unwrap();
        let kept = tokio::time::timeout(WAIT, messages.next())
            .await
            .expect("the stray email should reach the unroutable subject")
            .unwrap()
            .unwrap();
        let kept_email: InboundEmail = serde_json::from_slice(&kept.payload).unwrap();
        assert_eq!(kept_email, stray);
        let reason = kept.headers.as_ref().and_then(|h| h.get(UNROUTABLE_REASON_HEADER));
        assert!(
            reason.is_some_and(|r| r.as_str().contains("sales@help.example")),
            "the reason names the address: {reason:?}"
        );

        let _ = std::fs::remove_file(&log);
    });
}
