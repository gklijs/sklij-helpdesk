//! Live dashboard updates over skilj's `allEvents` subscription (issue #21).
//!
//! The dashboard reads projections, not events, so this feed carries no
//! data the page renders: each ticket event's `sequence` is only the
//! signal to re-read, passed on as the `waitForSequence` that read needs
//! (`CompanyActiveTickets` is async - see `pages::dashboard::AfterWrite`).
//! That keeps the whole thing small, and makes every failure recoverable
//! the same way: a re-read is always a complete answer, whatever was
//! missed.
//!
//! ## The wire protocol
//!
//! `graphql-transport-ws`, the one skilj-graphql serves on `GET /graphql`.
//! There's no header on an already-open websocket, so the bearer token
//! goes in `connection_init`'s payload instead (skilj-graphql's own
//! `resolve_role_from_connection_init`). A subscription that fails is not
//! sent as an `error` message: skilj ends the stream with one `next` that
//! carries `errors`, followed by `complete`, so both are handled here.
//!
//! ## What each failure does
//!
//! - `subscription_lagged`: this client fell behind the broadcast.
//!   Resubscribes with `fromSequence` set to the last sequence seen, so
//!   skilj replays what was missed first.
//! - `resume_span_too_large` / `from_sequence_not_committed`: the gap
//!   can't be replayed (too long, or the sequence isn't this context's).
//!   Subscribes from now and re-reads once instead - the projections
//!   already hold everything the replay would have signalled.
//! - Close code 4403: the token expired (or was refused). Stops, and asks
//!   for a new login; reconnecting with the same token can't succeed.
//! - Any other close: reconnects with backoff, resuming from the last
//!   sequence seen.
//! - Any other subscription error (a revoked grant, say): stops.

use crate::config::GRAPHQL_WS_URL;
use serde_json::{json, Value};
use std::cell::RefCell;
use std::rc::{Rc, Weak};
use wasm_bindgen::{closure::Closure, JsCast};
use web_sys::{CloseEvent, MessageEvent, WebSocket};

/// Every event that changes `CompanyActiveTickets` or `CustomerTickets` -
/// the two reads the ticket table is built from. `TicketEscalated` is the
/// one no click on this page causes (the alerter writes it), and so the
/// main reason the page needs a feed at all. `TicketInternalNoteAdded` is
/// left out: notes are their own on-demand read, not part of the table.
const TICKET_EVENT_TYPES: &[&str] = &[
    "TicketCreated",
    "TicketAssigned",
    "TicketResolved",
    "TicketReopened",
    "TicketInfoRequested",
    "TicketCustomerResponded",
    "TicketClosed",
    "TicketEscalated",
    "TicketsMerged",
    "TicketRated",
];

/// graphql-ws's own "Forbidden" - what skilj-graphql closes with once the
/// token in `connection_init` expires (`CREDENTIAL_EXPIRED_CLOSE_CODE`).
const FORBIDDEN_CLOSE_CODE: u16 = 4403;

/// A burst of events (a merge, a replay after a lag) is one re-read, not
/// one per event.
const COALESCE_MS: i32 = 100;

const FIRST_RETRY_MS: i32 = 1_000;
const MAX_RETRY_MS: i32 = 30_000;

/// What the dashboard is told.
pub enum Update {
    /// Re-read, waiting for this sequence.
    UpTo(i64),
    /// Re-read without waiting: events may have been missed and there is
    /// no sequence to wait for.
    Resync,
}

/// A running feed. Dropping it closes the socket and cancels any pending
/// reconnect, so it can live in the dashboard's own reactive scope.
pub struct LiveFeed(Rc<RefCell<Feed>>);

struct Feed {
    token: String,
    bounded_context: String,
    on_update: Box<dyn Fn(Update)>,
    /// `None` when live, otherwise why not.
    on_status: Box<dyn Fn(Option<String>)>,
    /// The highest sequence received - where a resubscribe resumes from.
    last_sequence: Option<i64>,
    /// Received but not yet passed to `on_update`.
    pending: Option<i64>,
    flush_scheduled: bool,
    /// Only the current subscription's messages are acted on: a
    /// resubscribe leaves the old one's `complete` still in flight.
    subscription_id: u32,
    connected_before: bool,
    retry_ms: i32,
    connection: Option<Connection>,
    stopped: bool,
}

/// The socket and the handlers it calls. Kept together so the handlers
/// are detached before they're dropped - a socket calling a dropped
/// closure throws.
struct Connection {
    socket: WebSocket,
    _on_open: Closure<dyn FnMut()>,
    _on_message: Closure<dyn FnMut(MessageEvent)>,
    _on_close: Closure<dyn FnMut(CloseEvent)>,
}

impl Drop for Connection {
    fn drop(&mut self) {
        self.socket.set_onopen(None);
        self.socket.set_onmessage(None);
        self.socket.set_onclose(None);
        let _ = self.socket.close();
    }
}

impl Drop for LiveFeed {
    fn drop(&mut self) {
        let mut feed = self.0.borrow_mut();
        feed.stopped = true;
        feed.connection = None;
    }
}

impl LiveFeed {
    pub fn start(
        token: String,
        bounded_context: String,
        on_update: impl Fn(Update) + 'static,
        on_status: impl Fn(Option<String>) + 'static,
    ) -> Self {
        let feed = Rc::new(RefCell::new(Feed {
            token,
            bounded_context,
            on_update: Box::new(on_update),
            on_status: Box::new(on_status),
            last_sequence: None,
            pending: None,
            flush_scheduled: false,
            subscription_id: 0,
            connected_before: false,
            retry_ms: FIRST_RETRY_MS,
            connection: None,
            stopped: false,
        }));
        connect(&feed);
        LiveFeed(feed)
    }
}

fn connect(feed: &Rc<RefCell<Feed>>) {
    let socket = match WebSocket::new_with_str(GRAPHQL_WS_URL, "graphql-transport-ws") {
        Ok(socket) => socket,
        Err(e) => {
            disconnected(feed, &format!("couldn't open {GRAPHQL_WS_URL}: {e:?}"));
            return;
        }
    };

    let weak = Rc::downgrade(feed);
    let on_open = Closure::<dyn FnMut()>::new({
        let weak = weak.clone();
        move || {
            with_feed(&weak, |feed| {
                let token = feed.borrow().token.clone();
                send(
                    feed,
                    &json!({
                        "type": "connection_init",
                        "payload": { "Authorization": format!("Bearer {token}") },
                    }),
                );
            })
        }
    });
    let on_message = Closure::<dyn FnMut(MessageEvent)>::new({
        let weak = weak.clone();
        move |event: MessageEvent| {
            let Some(text) = event.data().as_string() else {
                return;
            };
            let Ok(message) = serde_json::from_str::<Value>(&text) else {
                return;
            };
            with_feed(&weak, |feed| on_message(feed, &message));
        }
    });
    let on_close = Closure::<dyn FnMut(CloseEvent)>::new(move |event: CloseEvent| {
        with_feed(&weak, |feed| {
            if event.code() == FORBIDDEN_CLOSE_CODE {
                stop(
                    feed,
                    "your session expired - log in again to keep this page live",
                );
            } else {
                disconnected(feed, "lost the connection to the server");
            }
        })
    });
    socket.set_onopen(Some(on_open.as_ref().unchecked_ref()));
    socket.set_onmessage(Some(on_message.as_ref().unchecked_ref()));
    socket.set_onclose(Some(on_close.as_ref().unchecked_ref()));

    feed.borrow_mut().connection = Some(Connection {
        socket,
        _on_open: on_open,
        _on_message: on_message,
        _on_close: on_close,
    });
}

fn on_message(feed: &Rc<RefCell<Feed>>, message: &Value) {
    let current_id = feed.borrow().subscription_id.to_string();
    let for_current = message["id"].as_str() == Some(current_id.as_str());
    match message["type"].as_str() {
        Some("connection_ack") => {
            let resync = {
                let mut f = feed.borrow_mut();
                f.retry_ms = FIRST_RETRY_MS;
                // The first connection opens alongside the dashboard's own
                // first read, which covers anything before it. A later one
                // with nothing to resume from may have missed events.
                let resync = f.connected_before && f.last_sequence.is_none();
                f.connected_before = true;
                resync
            };
            (feed.borrow().on_status)(None);
            subscribe(feed);
            if resync {
                (feed.borrow().on_update)(Update::Resync);
            }
        }
        Some("ping") => send(feed, &json!({ "type": "pong" })),
        Some("next") if for_current => {
            if let Some(errors) = message["payload"]["errors"].as_array() {
                subscription_failed(feed, errors);
            } else if let Some(sequence) =
                message["payload"]["data"]["allEvents"]["sequence"].as_i64()
            {
                received(feed, sequence);
            }
        }
        Some("error") if for_current => {
            subscription_failed(feed, message["payload"].as_array().unwrap_or(&Vec::new()));
        }
        // Ended without an error: the server is going away. Reconnecting
        // resumes from the last sequence seen.
        Some("complete") if for_current => {
            feed.borrow_mut().connection = None;
            disconnected(feed, "the server ended the live feed");
        }
        _ => {}
    }
}

fn subscribe(feed: &Rc<RefCell<Feed>>) {
    let message = {
        let mut f = feed.borrow_mut();
        f.subscription_id += 1;
        json!({
            "id": f.subscription_id.to_string(),
            "type": "subscribe",
            "payload": {
                "query": "subscription($boundedContext: String!, $eventTypes: [String!], $fromSequence: Int) { \
                          allEvents(boundedContext: $boundedContext, eventTypes: $eventTypes, fromSequence: $fromSequence) { sequence } }",
                "variables": {
                    "boundedContext": f.bounded_context,
                    "eventTypes": TICKET_EVENT_TYPES,
                    "fromSequence": f.last_sequence,
                },
            },
        })
    };
    send(feed, &message);
}

fn subscription_failed(feed: &Rc<RefCell<Feed>>, errors: &[Value]) {
    let code = errors
        .iter()
        .find_map(|e| e["extensions"]["code"].as_str())
        .unwrap_or_default();
    match code {
        "subscription_lagged" => subscribe(feed),
        "resume_span_too_large" | "from_sequence_not_committed" => {
            {
                let mut f = feed.borrow_mut();
                f.last_sequence = None;
                f.pending = None;
            }
            subscribe(feed);
            (feed.borrow().on_update)(Update::Resync);
        }
        _ => {
            let reason = errors
                .first()
                .and_then(|e| e["message"].as_str())
                .unwrap_or("the subscription was refused");
            stop(feed, &format!("live updates stopped: {reason}"));
        }
    }
}

fn received(feed: &Rc<RefCell<Feed>>, sequence: i64) {
    let schedule = {
        let mut f = feed.borrow_mut();
        f.last_sequence = f.last_sequence.max(Some(sequence));
        f.pending = f.pending.max(Some(sequence));
        !std::mem::replace(&mut f.flush_scheduled, true)
    };
    if schedule {
        after(feed, COALESCE_MS, |feed| {
            let pending = {
                let mut f = feed.borrow_mut();
                f.flush_scheduled = false;
                f.pending.take()
            };
            if let Some(sequence) = pending {
                (feed.borrow().on_update)(Update::UpTo(sequence));
            }
        });
    }
}

fn disconnected(feed: &Rc<RefCell<Feed>>, why: &str) {
    let delay = {
        let mut f = feed.borrow_mut();
        if f.stopped {
            return;
        }
        f.connection = None;
        let delay = f.retry_ms;
        f.retry_ms = (delay * 2).min(MAX_RETRY_MS);
        delay
    };
    (feed.borrow().on_status)(Some(format!("{why} - reconnecting in {}s", delay / 1_000)));
    after(feed, delay, connect);
}

fn stop(feed: &Rc<RefCell<Feed>>, why: &str) {
    {
        let mut f = feed.borrow_mut();
        f.stopped = true;
        f.connection = None;
    }
    (feed.borrow().on_status)(Some(why.to_string()));
}

fn send(feed: &Rc<RefCell<Feed>>, message: &Value) {
    if let Some(connection) = &feed.borrow().connection {
        let _ = connection.socket.send_with_str(&message.to_string());
    }
}

/// Runs `f` after `ms`, unless the feed has been dropped or stopped by
/// then.
fn after(feed: &Rc<RefCell<Feed>>, ms: i32, f: impl FnOnce(&Rc<RefCell<Feed>>) + 'static) {
    let weak = Rc::downgrade(feed);
    let callback = Closure::once_into_js(move || with_feed(&weak, f));
    let _ = web_sys::window()
        .expect("browser")
        .set_timeout_with_callback_and_timeout_and_arguments_0(callback.unchecked_ref(), ms);
}

fn with_feed(weak: &Weak<RefCell<Feed>>, f: impl FnOnce(&Rc<RefCell<Feed>>)) {
    if let Some(feed) = weak.upgrade() {
        if !feed.borrow().stopped {
            f(&feed);
        }
    }
}
