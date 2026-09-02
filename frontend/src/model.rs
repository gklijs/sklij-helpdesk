//! Mirrors `skilj_helpdesk::helpdesk::TicketListEntry`/`TicketMessage`,
//! duplicated rather than shared: this crate has no dependency on the
//! backend crate at all, deliberately - see `Cargo.toml`'s own doc
//! comment.
//!
//! No `CompanyTicketListState` wrapper here, on purpose:
//! `api::query_projection` already resolves down to the `tickets`
//! field's own inner JSON (a plain `ticket_id -> entry` map), so this
//! crate only ever deserializes into `HashMap<String, TicketListEntry>`
//! directly - see that function's own doc comment, and `pages::dashboard`'s
//! comment on the bug this shape avoided re-introducing.

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct TicketMessage {
    pub author_id: String,
    pub from_staff: bool,
    pub text: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct TicketListEntry {
    pub ticket_id: String,
    pub title: String,
    pub description: String,
    pub status: String,
    pub priority: String,
    pub requester_id: String,
    pub assigned_staff_id: Option<String>,
    pub messages: Vec<TicketMessage>,
    pub escalated: bool,
    pub rating: Option<u8>,
}

/// Mirrors `skilj_helpdesk::helpdesk::TicketInternalNote` - fetched
/// separately from `TicketListEntry` above, on demand
/// (`pages::dashboard`'s own "Notes" toggle), never as part of the one
/// eager `CompanyTicketList` fetch - see that projection's own doc
/// comment for why internal notes live in their own projection at all.
#[derive(Debug, Clone, Deserialize)]
pub struct TicketInternalNote {
    pub staff_id: String,
    pub note: String,
}
