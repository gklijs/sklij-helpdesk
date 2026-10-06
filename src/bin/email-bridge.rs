//! email-bridge: the inbound email channel (issue #24) - customers'
//! emails become tickets and replies through skilj's NATS bridge
//! (`skilj-nats`). The pipeline, address scheme and redelivery story
//! are in `src/email_channel.rs`'s module doc comment.
//!
//! Runs three loops against one JetStream stream until Ctrl-C:
//!
//! - the translator: `helpdesk.email.received` to a command subject (or
//!   `helpdesk.email.unroutable`);
//! - `skilj_nats::run_inbound_until` for `CreateTicket`;
//! - `skilj_nats::run_inbound_until` for `CustomerRespondsToTicket`.
//!
//! A message skilj keeps failing on (not rejecting: an error) is
//! retried with `skilj_retry`'s default backoff, 5 attempts, then parked
//! in skilj's `parked_deliveries` with source `nats-inbound`. The
//! `skilj_helpdesk.parked_deliveries` gauge and its alert pick it up
//! from there.
//!
//! Configuration (env vars):
//!   NATS_URL                          - default "nats://localhost:4222";
//!                                       JetStream must be enabled
//!   SKILJ_BASE_URL                    - default "http://localhost:8080"
//!   CREATE_TICKET_TOKEN               - CommandToken ("id.secret")
//!   CUSTOMER_RESPONDS_TO_TICKET_TOKEN - CommandToken ("id.secret")
//!   Both tokens are printed by `src/bin/server.rs` on every run.
//!   TICKET_ROUTING                    - refuses to start when "tenant";
//!                                       see email_channel.rs's "Known gaps"
//!
//! To try it by hand against `cargo run --bin server` and a local
//! `nats-server -js`:
//!
//! ```sh
//! nats pub helpdesk.email.received '{"message_id":"<1@mail.example>",
//!   "from_address":"alice@example.com","from_name":"Alice",
//!   "to_address":"support+hooli@help.example","subject":"Hi","body":"..."}'
//! ```
//!
//! `tests/email_channel.rs` runs this binary against a real NATS server
//! and a real skilj.

use skilj_helpdesk::email_channel::{self, CREATE_TICKET_CONSUMER, CUSTOMER_REPLY_CONSUMER};
use skilj_helpdesk::routing::RoutingMode;
use skilj_nats::{InboundAction, InboundMapping};

struct Config {
    nats_url: String,
    base_url: String,
    create_ticket_token: String,
    customer_responds_to_ticket_token: String,
}

impl Config {
    fn from_env() -> Result<Self, String> {
        let required = |name: &str| {
            std::env::var(name)
                .map_err(|_| format!("{name} must be set (see this binary's doc comment)"))
        };
        Ok(Config {
            nats_url: std::env::var("NATS_URL")
                .unwrap_or_else(|_| "nats://localhost:4222".to_string()),
            base_url: std::env::var("SKILJ_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:8080".to_string()),
            create_ticket_token: required("CREATE_TICKET_TOKEN")?,
            customer_responds_to_ticket_token: required("CUSTOMER_RESPONDS_TO_TICKET_TOKEN")?,
        })
    }
}

#[tokio::main]
async fn main() -> Result<(), async_nats::Error> {
    let _telemetry = skilj_helpdesk::telemetry::init("skilj-helpdesk-email-bridge");

    if RoutingMode::from_env_value(std::env::var("TICKET_ROUTING").ok().as_deref())
        == RoutingMode::Tenant
    {
        return Err(
            "TICKET_ROUTING=tenant is not supported: this bridge holds one token per \
                    command, which names the shared `helpdesk` context, and would split each \
                    company's tickets across two contexts (src/email_channel.rs, \"Known gaps\")"
                .into(),
        );
    }
    let config = Config::from_env()?;

    let nats = async_nats::connect(&config.nats_url).await?;
    let jetstream = async_nats::jetstream::new(nats);
    let stream = email_channel::ensure_stream(&jetstream).await?;
    let translator = email_channel::pull_consumer(
        &stream,
        email_channel::TRANSLATOR_CONSUMER,
        email_channel::RECEIVED_SUBJECT,
    )
    .await?;
    let create_ticket = email_channel::pull_consumer(
        &stream,
        CREATE_TICKET_CONSUMER,
        email_channel::CREATE_TICKET_SUBJECT,
    )
    .await?;
    let customer_reply = email_channel::pull_consumer(
        &stream,
        CUSTOMER_REPLY_CONSUMER,
        email_channel::CUSTOMER_REPLY_SUBJECT,
    )
    .await?;

    let create_ticket_mapping = InboundMapping {
        credential: config.create_ticket_token,
        action: InboundAction::Trigger {
            command_type: "CreateTicket".to_string(),
        },
    };
    let customer_reply_mapping = InboundMapping {
        credential: config.customer_responds_to_ticket_token,
        action: InboundAction::Trigger {
            command_type: "CustomerRespondsToTicket".to_string(),
        },
    };
    let retry_policy = skilj_retry::RetryPolicy::default();
    let http = skilj_nats::http_client();

    println!(
        "email-bridge: {} -> {} (stream {})",
        config.nats_url,
        config.base_url,
        email_channel::STREAM
    );

    // One shutdown signal, fanned out to all three loops; each finishes
    // the message it's on (see their own doc comments) and returns.
    let (stop_tx, stop_rx) = tokio::sync::watch::channel(false);
    let stopped = |mut rx: tokio::sync::watch::Receiver<bool>| async move {
        let _ = rx.wait_for(|stop| *stop).await;
    };
    tokio::spawn(async move {
        let _ = tokio::signal::ctrl_c().await;
        let _ = stop_tx.send(true);
    });

    tokio::join!(
        email_channel::run_translator_until(&translator, &jetstream, stopped(stop_rx.clone())),
        skilj_nats::run_inbound_until(
            &create_ticket,
            &http,
            &config.base_url,
            &create_ticket_mapping,
            &retry_policy,
            stopped(stop_rx.clone()),
        ),
        skilj_nats::run_inbound_until(
            &customer_reply,
            &http,
            &config.base_url,
            &customer_reply_mapping,
            &retry_policy,
            stopped(stop_rx),
        ),
    );
    println!("email-bridge: stopped");
    Ok(())
}
