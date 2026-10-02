//! Which bounded context this company's Ticket traffic belongs in.
//!
//! ## The shape of the problem
//!
//! The backend splits helpdesk traffic in two: company *lifecycle* is
//! authoritative in one shared `helpdesk` context, while *Ticket* traffic
//! is per-company and moves into that company's own tenant once the
//! cutover is switched on server-side (`TICKET_ROUTING=tenant`). The
//! backend's own `routing.rs` owns that policy; this is the client half.
//!
//! ## Why the policy is duplicated rather than shared
//!
//! `frontend/` is a separate wasm crate with no dependency on the backend
//! library, and adding one would pull the entire server-side dependency
//! tree - database, tokio, OpenTelemetry - into a browser bundle. So the
//! one thing duplicated here is deliberately the *smallest possible*
//! piece of the policy: not a re-implementation of the command
//! classification, which `api.rs` shows this app does not need (it only
//! ever sends `CreateTicket`/`AddInternalNote`, plus a separate
//! `RecordDailyActivity` to the unrelated `activity` context).
//!
//! What is duplicated is the fallback: a company with no tenant is served
//! from the shared context, which is also this app's entire pre-cutover
//! behaviour. Duplicating *that* is safe in a way duplicating the
//! classification would not be - there is no way for this app to get it
//! wrong by sending a lifecycle command to a tenant, because it never
//! sends one.
//!
//! ## What enforces it
//!
//! Nothing here is trusted, and this module does not pretend otherwise:
//! a caller could still name the shared context. The server refuses that
//! (see the backend's `routing_guard` module), which is why the fallback
//! below is a graceful degradation rather than a security decision.

use crate::api;
use crate::config::{BOUNDED_CONTEXT, DEMO_COMPANY_ID};

/// Where this company's Ticket traffic should be sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TicketContext {
    /// Still resolving. Callers must not send Ticket traffic yet: the
    /// first write of a session could otherwise land in the shared
    /// context before resolution finished, and be refused by the server
    /// as misrouted - or worse, in a deployment without enforcement,
    /// split a company's history.
    Resolving,
    /// The company has no tenant, so its tickets live in the shared
    /// context. This is also the whole pre-cutover behaviour.
    Shared,
    /// The company has a tenant of its own.
    Tenant(String),
}

impl TicketContext {
    /// The bounded-context name to put in a `submitCommand`/
    /// `projection` call.
    ///
    /// `Resolving` deliberately reports the shared context: it is only
    /// ever read by a caller that has already checked it is not
    /// `Resolving`, so this arm exists to satisfy the type rather than
    /// to be a decision.
    pub fn bounded_context(&self) -> &str {
        match self {
            TicketContext::Tenant(name) => name,
            TicketContext::Shared | TicketContext::Resolving => BOUNDED_CONTEXT,
        }
    }

    /// The GraphQL type name a `projection` response is queried under.
    ///
    /// skilj builds these as `<bounded_context>_<projection>`
    /// (`skilj-graphql`'s own `graphql_type_name`), so a tenant read has
    /// to ask for `company_acme_…_CompanyTicketList`, not
    /// `helpdesk_CompanyTicketList`. Getting this wrong does not fail
    /// loudly - the inline fragment simply matches no type and the
    /// response comes back without the field - so it is derived from the
    /// same `bounded_context` rather than hard-coded at the call site.
    pub fn graphql_type(&self, projection_name: &str) -> String {
        format!("{}_{}", self.bounded_context(), projection_name)
    }
}

/// Resolve the demo company's ticket context.
///
/// Reads the shared context's `TenantDirectory` projection - the one read
/// that deliberately stays shared, since it is the lookup every piece of
/// routing needs (see its own doc comment in the backend's `helpdesk.rs`).
///
/// A failure to resolve falls back to the shared context rather than
/// surfacing an error: this is the same fallback the backend's own
/// `routing.rs` documents, and it can only preserve working behaviour. If
/// the deployment has enforcement on, the server will refuse a
/// company-backed write anyway with a message naming the real problem,
/// which is a better error than a blank dashboard.
pub async fn resolve(token: &str) -> TicketContext {
    let result = api::query_projection(
        token,
        BOUNDED_CONTEXT,
        "TenantDirectory",
        DEMO_COMPANY_ID,
        &TicketContext::Shared.graphql_type("TenantDirectory"),
        "tenantName",
    )
    .await;
    match result {
        // A company with no tenant reads as a projection whose
        // `tenantName` is null, which arrives as an absent field.
        Ok(json) => match json.get("tenantName").and_then(|v| v.as_str()) {
            Some(tenant_name) => TicketContext::Tenant(tenant_name.to_string()),
            None => TicketContext::Shared,
        },
        Err(_) => TicketContext::Shared,
    }
}
