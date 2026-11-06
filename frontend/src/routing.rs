//! Which bounded context this company's Ticket traffic belongs in.
//!
//! ## The shape of the problem
//!
//! The backend splits helpdesk traffic in two: company *lifecycle* is
//! authoritative in one shared `helpdesk` context, while *Ticket* traffic
//! is per-company and moves into that company's own tenant by default
//! (unless the demo opts into `TICKET_ROUTING=shared`). The
//! backend's own `routing.rs` owns that policy; this is the client half.
//!
//! The authenticated company directory supplies the destination after checking
//! the caller's grant on that context. Until discovery succeeds the dashboard
//! stays `Resolving` and cannot submit tickets. This WASM crate keeps only the
//! small context/type-name representation; the backend owns routing policy.

use crate::config::BOUNDED_CONTEXT;

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
    /// to ask for `company_acme_…_CompanyActiveTickets`, not
    /// `helpdesk_CompanyActiveTickets`. Getting this wrong does not fail
    /// loudly - the inline fragment simply matches no type and the
    /// response comes back without the field - so it is derived from the
    /// same `bounded_context` rather than hard-coded at the call site.
    pub fn graphql_type(&self, projection_name: &str) -> String {
        format!("{}_{}", self.bounded_context(), projection_name)
    }
}
