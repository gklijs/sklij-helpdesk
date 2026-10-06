//! Inbound email channel (issue #24): customers email the helpdesk, and
//! each email becomes a `CreateTicket` or `CustomerRespondsToTicket`
//! through skilj's NATS bridge (`skilj-nats`).
//!
//! # The pipeline
//!
//! Everything lives in one JetStream stream, [`STREAM`]:
//!
//! 1. A mail gateway (outside this repo: whatever receives SMTP and
//!    checks SPF/DKIM) publishes each email as an [`InboundEmail`] to
//!    [`RECEIVED_SUBJECT`].
//! 2. [`run_translator_until`] decides what the email is ([`route`]) and
//!    republishes the command payload to [`CREATE_TICKET_SUBJECT`] or
//!    [`CUSTOMER_REPLY_SUBJECT`]. An email it can't place goes to
//!    [`UNROUTABLE_SUBJECT`] and stays in the stream for an operator.
//! 3. `skilj_nats::run_inbound_until`, once per command subject, posts
//!    each payload to `POST /v1/commands/trigger` with that command's
//!    `CommandToken`. It retries a failing message with backoff and then
//!    parks it in skilj's `parked_deliveries`, where the
//!    `skilj_helpdesk.parked_deliveries` gauge (`parked_deliveries.rs`)
//!    sees it.
//!
//! The translation step is needed because a `Trigger` mapping passes
//! the message body to skilj unchanged as the command payload, and one
//! mapping serves exactly one command type. Its other job is making the
//! IDs deterministic.
//!
//! # Addresses
//!
//! Plus-addressing on the helpdesk's own domain says what an email is:
//!
//! - `support+<company_id>@...`: a new ticket for that company.
//! - `ticket+<ticket_id>@...`: the customer answering that ticket. The
//!   helpdesk would set this as the reply-to on mail it sends; it sends
//!   none yet.
//!
//! The domain isn't checked. The gateway only hands over mail for the
//! helpdesk's own domains.
//!
//! # Redelivery safety
//!
//! Everything is keyed on [`email_key`], a hash of the email's
//! `Message-ID`:
//!
//! - the ticket id: `email-<first 16 hex digits>`, so a second copy of
//!   the same email is a `ticket_already_exists` rejection, not a second
//!   ticket;
//! - the `Nats-Msg-Id` of the translated message, so JetStream drops a
//!   republish inside its duplicate window;
//! - and through that, the `Idempotency-Key` `skilj-nats` sends, plus
//!   the `Skilj-Correlation-Id` header, which skilj records as the
//!   command's and events' correlation id.
//!
//! A hash, not the `Message-ID` itself: those can be long enough to break
//! skilj's 255-character idempotency-key and 200-character correlation-id
//! limits.
//!
//! # Who the requester is
//!
//! An email customer is identified by address alone: `requester_id` is
//! `email:<lowercased address>`. That's a different id from the same
//! person's portal login (a Dex `sub`), so their email tickets and portal
//! tickets don't show up together. A real deployment would look the
//! address up in a contact directory. GDPR erasure works the same as for
//! any requester: `forgetSubject` with that `requester_id`.
//!
//! # Known gaps
//!
//! - A reply to a ticket that isn't `waiting_on_customer` is rejected by
//!   `CustomerRespondsToTicket` (`rule CustomerReplies`). The bridge
//!   counts a rejection as delivered, so that email ends there. It is
//!   still recorded as a rejected command.
//! - Only the shared `helpdesk` context. A `Trigger` mapping carries one
//!   `CommandToken`, and a token names one bounded context, so per-company
//!   tenant contexts (`TICKET_ROUTING=tenant`) would need a mapping per
//!   tenant. `src/bin/email-bridge.rs` refuses to start in that mode
//!   rather than splitting a company's tickets across two contexts.
//! - The body is taken as-is: quoted earlier messages aren't stripped.

use crate::helpdesk::{CreateTicketPayload, CustomerRespondsToTicketPayload, TicketPriority};
use async_nats::jetstream::{self, consumer::pull, context::Context as Jetstream};
use futures_util::TryStreamExt;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// The one stream holding every subject below.
pub const STREAM: &str = "HELPDESK_EMAIL";
/// Where the mail gateway publishes [`InboundEmail`]s.
pub const RECEIVED_SUBJECT: &str = "helpdesk.email.received";
/// Translated `CreateTicket` payloads, read by `skilj-nats`.
pub const CREATE_TICKET_SUBJECT: &str = "helpdesk.email.create-ticket";
/// Translated `CustomerRespondsToTicket` payloads, read by `skilj-nats`.
pub const CUSTOMER_REPLY_SUBJECT: &str = "helpdesk.email.customer-reply";
/// Emails [`route`] couldn't place, kept for an operator. Each carries
/// the reason in a [`UNROUTABLE_REASON_HEADER`] header.
pub const UNROUTABLE_SUBJECT: &str = "helpdesk.email.unroutable";
pub const UNROUTABLE_REASON_HEADER: &str = "Helpdesk-Unroutable-Reason";

/// Durable pull consumer names, one per reader.
pub const TRANSLATOR_CONSUMER: &str = "email-translator";
pub const CREATE_TICKET_CONSUMER: &str = "email-create-ticket";
pub const CUSTOMER_REPLY_CONSUMER: &str = "email-customer-reply";

/// One received email, as the mail gateway publishes it. Only what the
/// helpdesk uses: attachments, CCs and threading headers are left out.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct InboundEmail {
    /// The `Message-ID` header. Everything downstream is keyed on it
    /// (see [`email_key`]), so a gateway that gets an email without one
    /// should make one up and reuse it on every retry.
    pub message_id: String,
    pub from_address: String,
    pub from_name: Option<String>,
    /// The helpdesk address it was sent to (see "Addresses" above).
    pub to_address: String,
    pub subject: String,
    pub body: String,
}

/// What an email becomes.
#[derive(Debug, Clone)]
pub enum EmailCommand {
    CreateTicket(CreateTicketPayload),
    CustomerReply(CustomerRespondsToTicketPayload),
}

impl EmailCommand {
    pub fn subject(&self) -> &'static str {
        match self {
            EmailCommand::CreateTicket(_) => CREATE_TICKET_SUBJECT,
            EmailCommand::CustomerReply(_) => CUSTOMER_REPLY_SUBJECT,
        }
    }

    pub fn payload_json(&self) -> serde_json::Value {
        match self {
            EmailCommand::CreateTicket(payload) => serde_json::to_value(payload),
            EmailCommand::CustomerReply(payload) => serde_json::to_value(payload),
        }
        .expect("command payloads are plain structs and always serialize")
    }
}

/// Why [`route`] couldn't place an email.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Unroutable {
    MissingMessageId,
    InvalidSender(String),
    /// Not a `support+<company_id>` or `ticket+<ticket_id>` address.
    UnknownRecipient(String),
}

impl std::fmt::Display for Unroutable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Unroutable::MissingMessageId => write!(f, "the email has no Message-ID"),
            Unroutable::InvalidSender(address) => {
                write!(f, "sender {address:?} is not an email address")
            }
            Unroutable::UnknownRecipient(address) => write!(
                f,
                "recipient {address:?} is neither support+<company_id> nor ticket+<ticket_id>"
            ),
        }
    }
}

/// `email-<sha256 of the Message-ID, hex>`: the Nats-Msg-Id,
/// Idempotency-Key and correlation id of everything this email causes.
pub fn email_key(message_id: &str) -> String {
    let digest = Sha256::digest(message_id.trim().as_bytes());
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    format!("email-{hex}")
}

/// The ticket a new-ticket email creates: the first 16 hex digits of
/// [`email_key`], plenty to keep two emails apart.
pub fn ticket_id_for(message_id: &str) -> String {
    let key = email_key(message_id);
    key[..("email-".len() + 16)].to_string()
}

/// `email:<address>`, lowercased: domains are case-insensitive, and in
/// practice so are mailboxes.
pub fn requester_id_for(address: &str) -> String {
    format!("email:{}", address.trim().to_lowercase())
}

/// Decides what `email` is. Pure: no I/O, so every rule is unit-tested
/// below.
pub fn route(email: &InboundEmail) -> Result<EmailCommand, Unroutable> {
    if email.message_id.trim().is_empty() {
        return Err(Unroutable::MissingMessageId);
    }
    let from = email.from_address.trim();
    if split_address(from).is_none() {
        return Err(Unroutable::InvalidSender(email.from_address.clone()));
    }
    let requester_id = requester_id_for(from);

    let unknown = || Unroutable::UnknownRecipient(email.to_address.clone());
    let (local, _domain) = split_address(email.to_address.trim()).ok_or_else(unknown)?;
    let (mailbox, tag) = local.split_once('+').ok_or_else(unknown)?;
    if !is_identifier(tag) {
        return Err(unknown());
    }
    match mailbox.to_ascii_lowercase().as_str() {
        "support" => Ok(EmailCommand::CreateTicket(CreateTicketPayload {
            ticket_id: ticket_id_for(&email.message_id),
            company_id: tag.to_string(),
            requester_id,
            logged_by_staff_id: None,
            title: match email.subject.trim() {
                "" => "(no subject)".to_string(),
                subject => subject.to_string(),
            },
            description: email.body.clone(),
            priority: TicketPriority::Medium,
            requester_name: email
                .from_name
                .as_deref()
                .map(str::trim)
                .filter(|name| !name.is_empty())
                .map(str::to_string),
            requester_email: Some(from.to_string()),
        })),
        "ticket" => Ok(EmailCommand::CustomerReply(
            CustomerRespondsToTicketPayload {
                ticket_id: tag.to_string(),
                requester_id,
                message: email.body.clone(),
            },
        )),
        _ => Err(unknown()),
    }
}

/// `(local part, domain)` when `address` looks like one bare address.
fn split_address(address: &str) -> Option<(&str, &str)> {
    let (local, domain) = address.split_once('@')?;
    let valid = !local.is_empty()
        && !domain.is_empty()
        && !domain.contains('@')
        && !address
            .chars()
            .any(|c| c.is_whitespace() || c == '<' || c == '>');
    valid.then_some((local, domain))
}

/// Company and ticket ids are slugs: letters, digits, `-` and `_`.
fn is_identifier(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

// --- JetStream setup and the translator loop ---

/// Creates [`STREAM`] over `helpdesk.email.>` if it doesn't exist yet.
/// Default duplicate window (2 minutes) and limits.
pub async fn ensure_stream(
    jetstream: &Jetstream,
) -> Result<jetstream::stream::Stream, async_nats::Error> {
    Ok(jetstream
        .get_or_create_stream(jetstream::stream::Config {
            name: STREAM.to_string(),
            subjects: vec!["helpdesk.email.>".to_string()],
            ..Default::default()
        })
        .await?)
}

/// A durable pull consumer on [`STREAM`], reading only `subject`. Created
/// if missing, so every reader resumes where it left off after a restart.
pub async fn pull_consumer(
    stream: &jetstream::stream::Stream,
    name: &str,
    subject: &str,
) -> Result<skilj_nats::PullConsumer, async_nats::Error> {
    Ok(stream
        .get_or_create_consumer(
            name,
            pull::Config {
                durable_name: Some(name.to_string()),
                filter_subject: subject.to_string(),
                ..Default::default()
            },
        )
        .await?)
}

/// Reads [`RECEIVED_SUBJECT`] through `consumer` until `stop` resolves,
/// republishing each email as its command (or to [`UNROUTABLE_SUBJECT`])
/// and acking it only once JetStream has acknowledged that publish.
///
/// A failed publish leaves the email unacked: JetStream redelivers it
/// after the consumer's `ack_wait`, and [`email_key`] as the Nats-Msg-Id
/// makes a second publish harmless. `stop` is checked only between
/// messages, never mid-publish.
pub async fn run_translator_until(
    consumer: &skilj_nats::PullConsumer,
    jetstream: &Jetstream,
    stop: impl std::future::Future<Output = ()>,
) {
    let mut stop = std::pin::pin!(stop);
    loop {
        let subscribed = tokio::select! {
            biased;
            () = &mut stop => return,
            subscribed = consumer.messages() => subscribed,
        };
        let mut messages = match subscribed {
            Ok(messages) => messages,
            Err(e) => {
                tracing::error!(error = %e, "email translator: subscribing failed, retrying");
                tokio::select! {
                    biased;
                    () = &mut stop => return,
                    () = tokio::time::sleep(Duration::from_secs(1)) => {}
                }
                continue;
            }
        };
        loop {
            let next = tokio::select! {
                biased;
                () = &mut stop => return,
                next = messages.try_next() => next,
            };
            let message = match next {
                Ok(Some(message)) => message,
                Ok(None) => break,
                Err(e) => {
                    tracing::warn!(error = %e, "email translator: reading the next message failed");
                    break;
                }
            };
            match translate(jetstream, &message).await {
                Ok(()) => {
                    if let Err(e) = message.ack().await {
                        tracing::warn!(error = %e, "email translator: acking failed");
                    }
                }
                Err(e) => tracing::error!(
                    error = %e,
                    "email translator: publishing failed - leaving the email unacked, \
                     JetStream will redeliver it"
                ),
            }
        }
    }
}

/// Publishes what one received message becomes and waits for JetStream's
/// acknowledgement.
async fn translate(
    jetstream: &Jetstream,
    message: &jetstream::Message,
) -> Result<(), async_nats::Error> {
    let email: InboundEmail = match serde_json::from_slice(&message.payload) {
        Ok(email) => email,
        Err(e) => {
            let reason = format!("not an InboundEmail: {e}");
            return publish_unroutable(jetstream, message, &reason).await;
        }
    };
    let command = match route(&email) {
        Ok(command) => command,
        Err(reason) => return publish_unroutable(jetstream, message, &reason.to_string()).await,
    };
    let key = email_key(&email.message_id);
    let publish = jetstream::message::PublishMessage::build()
        .payload(command.payload_json().to_string().into())
        .message_id(key.as_str())
        .header("Skilj-Correlation-Id", key.as_str());
    let ack = jetstream
        .send_publish(command.subject(), publish)
        .await?
        .await?;
    tracing::info!(
        subject = command.subject(),
        email_key = %key,
        duplicate = ack.duplicate,
        "email translator: routed an email"
    );
    Ok(())
}

async fn publish_unroutable(
    jetstream: &Jetstream,
    message: &jetstream::Message,
    reason: &str,
) -> Result<(), async_nats::Error> {
    // Keyed on where the original sits in the stream: an unroutable
    // email may have no usable Message-ID at all.
    let info = message.info()?;
    let publish = jetstream::message::PublishMessage::build()
        .payload(message.payload.clone())
        .message_id(format!("unroutable-{}", info.stream_sequence))
        .header(UNROUTABLE_REASON_HEADER, reason);
    jetstream
        .send_publish(UNROUTABLE_SUBJECT, publish)
        .await?
        .await?;
    tracing::warn!(
        reason,
        "email translator: email is unroutable, kept on {UNROUTABLE_SUBJECT}"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn email(to: &str) -> InboundEmail {
        InboundEmail {
            message_id: "<abc123@mail.example>".into(),
            from_address: "Alice@Example.com".into(),
            from_name: Some("Alice".into()),
            to_address: to.into(),
            subject: "Printer on fire".into(),
            body: "It is on fire.".into(),
        }
    }

    #[test]
    fn a_support_address_creates_a_ticket_for_that_company() {
        let EmailCommand::CreateTicket(payload) =
            route(&email("support+stark-labs@help.example")).unwrap()
        else {
            panic!("expected CreateTicket");
        };
        assert_eq!(payload.company_id, "stark-labs");
        assert_eq!(payload.ticket_id, ticket_id_for("<abc123@mail.example>"));
        assert_eq!(payload.requester_id, "email:alice@example.com");
        assert_eq!(
            payload.requester_email.as_deref(),
            Some("Alice@Example.com")
        );
        assert_eq!(payload.requester_name.as_deref(), Some("Alice"));
        assert_eq!(payload.title, "Printer on fire");
        assert_eq!(payload.description, "It is on fire.");
        assert_eq!(payload.logged_by_staff_id, None);
    }

    #[test]
    fn a_ticket_address_is_a_reply_from_the_same_requester() {
        let EmailCommand::CustomerReply(payload) =
            route(&email("ticket+email-0011223344556677@help.example")).unwrap()
        else {
            panic!("expected CustomerReply");
        };
        assert_eq!(payload.ticket_id, "email-0011223344556677");
        assert_eq!(payload.requester_id, "email:alice@example.com");
        assert_eq!(payload.message, "It is on fire.");
    }

    #[test]
    fn the_same_message_id_always_gives_the_same_ticket_and_key() {
        let a = email_key("<abc123@mail.example>");
        assert_eq!(a, email_key(" <abc123@mail.example> "));
        assert_ne!(a, email_key("<abc124@mail.example>"));
        assert_eq!(a.len(), "email-".len() + 64);
        assert!(ticket_id_for("<abc123@mail.example>").len() == "email-".len() + 16);
    }

    #[test]
    fn keys_stay_inside_skiljs_limits_whatever_the_message_id() {
        let long = format!("<{}@mail.example>", "x".repeat(900));
        assert!(email_key(&long).len() <= 200);
    }

    #[test]
    fn an_empty_subject_gets_a_placeholder_title() {
        let mut e = email("support+hooli@help.example");
        e.subject = "  ".into();
        let EmailCommand::CreateTicket(payload) = route(&e).unwrap() else {
            panic!("expected CreateTicket");
        };
        assert_eq!(payload.title, "(no subject)");
    }

    #[test]
    fn unknown_or_malformed_recipients_are_unroutable() {
        for to in [
            "support@help.example",
            "sales+hooli@help.example",
            "support+@help.example",
            "support+hoo li@help.example",
            "support+hooli/../x@help.example",
            "not-an-address",
        ] {
            assert_eq!(
                route(&email(to)).unwrap_err(),
                Unroutable::UnknownRecipient(to.into()),
                "{to}"
            );
        }
    }

    #[test]
    fn a_missing_message_id_or_bad_sender_is_unroutable() {
        let mut e = email("support+hooli@help.example");
        e.message_id = " ".into();
        assert_eq!(route(&e).unwrap_err(), Unroutable::MissingMessageId);

        let mut e = email("support+hooli@help.example");
        e.from_address = "Alice <alice@example.com>".into();
        assert!(matches!(route(&e), Err(Unroutable::InvalidSender(_))));
    }
}
