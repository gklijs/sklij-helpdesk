//! Mirrors `skilj_helpdesk::helpdesk::TicketQueueEntry`/
//! `CustomerTicketContent`/`TicketMessage`, duplicated rather than shared:
//! this crate has no dependency on the backend crate at all, deliberately
//! - see `Cargo.toml`'s own doc comment.
//!
//! A ticket is two reads joined by `ticket_id`: its lifecycle from the
//! company's `CompanyTicketQueue`, and what its customer wrote from that
//! customer's own `CustomerTickets` row. The second is encrypted per
//! customer, which is why it can't live in the company-keyed first one
//! (see `CompanyTicketQueue`'s doc comment on the backend).
//!
//! No `…State` wrappers here, on purpose: `api::query_projection` already
//! resolves down to the `tickets` field's own inner JSON (a plain
//! `ticket_id -> entry` map), so this crate only ever deserializes into
//! `HashMap<String, _>` directly - see that function's own doc comment,
//! and `pages::dashboard`'s comment on the bug this shape avoided
//! re-introducing.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct TicketMessage {
    pub author_id: String,
    pub from_staff: bool,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TicketQueueEntry {
    pub ticket_id: String,
    pub status: String,
    pub priority: String,
    pub requester_id: String,
    pub assigned_staff_id: Option<String>,
    pub escalated: bool,
    pub rating: Option<u8>,
}

/// Plaintext only to the customer themselves and to staff; anyone else,
/// and everyone once the customer has been forgotten, gets ciphertext
/// here, which nothing in this crate can tell apart from text.
#[derive(Debug, Clone, Deserialize)]
pub struct CustomerTicketContent {
    pub title: String,
    pub description: String,
    pub messages: Vec<TicketMessage>,
    pub rating_comment: Option<String>,
}

/// Mirrors `skilj_helpdesk::helpdesk::TicketInternalNote` - fetched
/// separately, on demand (`pages::dashboard`'s own "Notes" toggle), never
/// as part of the eager ticket fetch - see that projection's own doc
/// comment for why internal notes live in their own projection at all.
#[derive(Debug, Clone, Deserialize)]
pub struct TicketInternalNote {
    pub staff_id: String,
    pub note: String,
}
