//! Mirrors `skilj_helpdesk::helpdesk::TicketQueueEntry`/
//! `CustomerTicketContent`/`TicketMessage`, duplicated rather than shared:
//! this crate has no dependency on the backend crate at all, deliberately
//! - see `Cargo.toml`'s own doc comment.
//!
//! The authorized ticket adapter joins company lifecycle metadata with
//! per-customer encrypted content on the server. The browser receives a
//! bounded page of summaries and only the details it has explicitly opened.
//! It receives plaintext or an explicit unavailable state, never ciphertext.

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

/// The authorized application adapter returns plaintext or an explicit
/// unavailable placeholder. It never sends ciphertext to the dashboard.
#[derive(Debug, Clone, Deserialize)]
pub struct CustomerTicketContent {
    pub title: String,
    pub description: String,
    pub messages: Vec<TicketMessage>,
    pub rating_comment: Option<String>,
    pub available: bool,
    pub details_loaded: bool,
    pub message_count: usize,
    pub truncated: bool,
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

/// Mirrors `skilj_helpdesk::marketing::MarketingSignalsState`, as GraphQL
/// returns it (camelCase) - minus its `engagement_declined_at`, which the
/// panel shows from `CompanyEngagement` instead.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MarketingSignals {
    pub trial_status: Option<String>,
    pub last_lapsed_at: Option<String>,
    pub lapse_count: u32,
    pub last_converted_at: Option<String>,
    pub conversion_count: u32,
}

impl MarketingSignals {
    pub const FIELDS: &str = "trialStatus lastLapsedAt lapseCount lastConvertedAt conversionCount";
}

/// Mirrors `skilj_helpdesk::activity::CompanyEngagementState`, as GraphQL
/// returns it (camelCase).
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanyEngagement {
    pub customer_active_days: u32,
    pub staff_active_days: u32,
    pub last_customer_day: Option<String>,
    pub last_staff_day: Option<String>,
    pub engagement_declined_at: Option<String>,
}

impl CompanyEngagement {
    pub const FIELDS: &str =
        "customerActiveDays staffActiveDays lastCustomerDay lastStaffDay engagementDeclinedAt";
}
