//! Fixed configuration for this showcase - see `dex/config.yaml` and
//! `src/bin/server.rs`'s own doc comments for the other halves of each
//! of these. No env-var/build-time configurability this pass: a real
//! deployment would need that, a local showcase doesn't.

pub const DEX_ISSUER: &str = "http://127.0.0.1:5556/dex";
pub const CLIENT_ID: &str = "skilj-helpdesk-frontend";
pub const REDIRECT_URI: &str = "http://127.0.0.1:8081/callback";

/// `src/bin/server.rs`'s own default `PORT` (8080) - if that's
/// overridden, this needs to match.
pub const GRAPHQL_URL: &str = "http://localhost:8080/graphql";

pub const BOUNDED_CONTEXT: &str = "helpdesk";

/// Every ticket in this showcase belongs to one demo company - matches
/// `server.rs`'s own printed `curl` example (`company_id: "acme"`).
/// Sign it up first (that example, or the GraphQL equivalent) before
/// this app has anything to show - see this crate's own README.
pub const DEMO_COMPANY_ID: &str = "acme";

/// The real `sub` Dex's local-password connector issues for each demo
/// identity - see `src/bin/server.rs`'s own `DEMO_CUSTOMER_SUB`/
/// `DEMO_STAFF_LEAD_SUB` doc comment for how these were captured.
/// Duplicated here rather than shared across the backend/frontend crate
/// boundary (they're two independent binaries with no code-sharing
/// relationship, same as every other duplicated constant in this
/// project).
pub const DEMO_CUSTOMER_SUB: &str = "Cg1jdXN0b21lci1kZW1vEgVsb2NhbA";
pub const DEMO_STAFF_LEAD_SUB: &str = "Cg9zdGFmZi1sZWFkLWRlbW8SBWxvY2Fs";
