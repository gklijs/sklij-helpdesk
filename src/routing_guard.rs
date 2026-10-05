//! The server-side half of the ticket cutover: refusing misrouted traffic.
//!
//! `routing.rs` decides where traffic *should* go. This module is the
//! check that makes that decision binding rather than advisory, because
//! `routing.rs` alone is not enough: the GraphQL surface takes
//! `boundedContext` as a caller-supplied argument
//! (`skilj-graphql`'s own `submit_command_field` and `projection_query`),
//! so a client can name any context it holds a mapping on. That is not a
//! privilege escalation - authorisation still happens per context - but it
//! *is* a way to split one company's ticket history across two bounded
//! contexts, with nothing recording that it happened.
//!
//! ## Why a reject rather than a rewrite
//!
//! The obvious server-side design is a gateway that silently rewrites
//! `boundedContext` to the right tenant. This rejects instead, for one
//! reason: a rewrite has to *derive* the company for every request, and
//! most ticket traffic cannot be attributed. `TicketInternalNotes` and
//! `TicketSummary` are keyed by `ticket_id`, and eleven of the twelve
//! ticket command types carry only a `ticket_id` in their payload. A
//! rewrite would need a `ticket_id -> tenant` index that does not exist
//! (see `routing.rs`'s own `CommandClass::Ticket` and the frontend's
//! note on the same problem). Rejecting needs no derivation: the client
//! already knows which tenant it resolved, so it can name it, and the
//! server only has to notice when it named the shared context instead.
//!
//! ## What this does and does not catch
//!
//! REST needs none of this. `skilj-rest` resolves the destination from
//! the command token rather than from a body field
//! (`skilj-rest/src/routes/mod.rs`, and its own comment explaining that
//! it deliberately does not accept a bare `boundedContext`), so a REST
//! caller cannot name a context at all. This module guards the GraphQL
//! surface, which is the one place where the context is caller-named.
//!
//! Everything here is pure: the caller resolves the company and hands
//! over what it found, so the rules themselves are unit-testable
//! exhaustively and the I/O stays in the middleware.

use crate::helpdesk::{TenantDirectoryState, BOUNDED_CONTEXT};
use crate::routing::{classify, CommandClass, RoutingMode};
use std::sync::Arc;

/// The `TenantDirectory` projection's own name, as it is stored and as a
/// caller must name it in a `projection` query.
const TENANT_DIRECTORY: &str = "TenantDirectory";

/// Every Ticket projection in `helpdesk.rs`, so a read of one of them is
/// recognised as ticket traffic.
///
/// An exhaustive list rather than a prefix rule: `helpdesk.rs` names
/// Ticket projections `TicketSummary`, `CompanyActiveTickets`,
/// `CustomerTickets` and `TicketInternalNotes` - inconsistently, because
/// they're keyed per ticket, per company and per customer. A
/// `name.starts_with("Ticket")` rule would catch two of the four and
/// miss `CompanyActiveTickets`/`CustomerTickets`, precisely the reads the
/// dashboard makes on every load, so the miss would be invisible until a
/// company with a tenant loaded an empty dashboard.
const TICKET_PROJECTIONS: &[&str] = &[
    "TicketSummary",
    "CompanyActiveTickets",
    "CustomerTickets",
    "TicketInternalNotes",
];

/// The one ticket projection keyed by customer - see
/// `Subject::OwnerOfRow`.
const CUSTOMER_TICKETS: &str = "CustomerTickets";

/// What a rejected request was actually trying to do, so the refusal
/// names the traffic rather than just failing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Guarded {
    /// A `submitCommand` naming this command type.
    Command(String),
    /// A `projection` read naming this projection.
    Projection(String),
}

impl Guarded {
    fn name(&self) -> &str {
        match self {
            Guarded::Command(name) | Guarded::Projection(name) => name,
        }
    }
}

/// What class of traffic a name denotes, for the two name spaces the
/// GraphQL surface exposes.
///
/// Projections have no `classify` equivalent in `routing.rs` (which
/// classifies command types), so this is where a projection name is
/// recognised as ticket traffic. `Unknown` rather than a guess: a
/// projection this list doesn't know about is not refused, because
/// refusing an unrecognised name would let a new projection in
/// `helpdesk.rs` break reads the moment the cutover is switched on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameClass {
    Ticket,
    Other,
    Unknown,
}

/// Classify a `submitCommand`'s `commandTypeName`.
///
/// `classify` returns an error for an unrecognised name, which is the
/// right behaviour for a router deciding where to send traffic but the
/// wrong one for a guard deciding whether to *refuse* it: a typo'd or
/// newly-added command type is not ticket traffic this guard can speak
/// about, so it passes through to skilj's own authoritative handling
/// rather than being refused on a guess.
pub fn classify_command_name(name: &str) -> NameClass {
    match classify(name) {
        Ok(CommandClass::Ticket) => NameClass::Ticket,
        _ => NameClass::Other,
    }
}

/// Classify a `projection`'s `name`.
pub fn classify_projection_name(name: &str) -> NameClass {
    if TICKET_PROJECTIONS.contains(&name) {
        NameClass::Ticket
    } else {
        NameClass::Unknown
    }
}

/// Which company a request belongs to, as far as the server can tell.
///
/// The distinction is the whole point of the module, so it is modelled
/// explicitly rather than collapsed into an `Option<String>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Subject {
    /// The request names a company directly - `CreateTicket`'s payload
    /// carries `company_id`, and `CompanyActiveTickets` is keyed by it.
    Company(String),
    /// A read of this projection row, whose company is the row's owner.
    ///
    /// `CustomerTickets` is keyed by customer, which names no company, but
    /// skilj records each row's owner (`OWNER_TAG_KEY = "company"`), so
    /// unlike a `ticket_id` this lookup does exist. `check_request`
    /// resolves it to `Company` before `check` sees it.
    OwnerOfRow { projection: String, key: String },
    /// The request only names a `ticket_id`, so the company behind it
    /// cannot be determined without a lookup that does not exist.
    Unattributed,
}

/// The outcome of checking one request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// Let it through. Either the traffic is not ticket traffic, or the
    /// shared context is the correct destination for this company.
    Allow,
    /// Refuse it: it is ticket traffic naming the shared context for a
    /// company that has a tenant of its own.
    Misrouted {
        traffic: Guarded,
        company_id: Option<String>,
    },
}

/// Whether `class` is traffic this module has an opinion about at all.
///
/// Lifecycle traffic is *supposed* to live in the shared context
/// (`routing.rs`'s own `CommandClass::CompanyLifecycle` and the
/// authority argument in its module docs), so refusing shared lifecycle
/// traffic would break the one thing that must never move. The
/// per-tenant infrastructure commands are addressed at a tenant by
/// construction and are likewise not this guard's business.
pub fn is_routable(class: CommandClass) -> bool {
    matches!(class, CommandClass::Ticket)
}

/// Decide whether a request may proceed.
///
/// `tenant_of_company` is what the caller learned from `TenantDirectory`
/// for the company named by `subject`, and is `None` for a company with
/// no tenant recorded.
///
/// The `Subject::Unattributed` arm is the coarse one, and it is
/// deliberately stricter than `Subject::Company`: with no company to
/// check, the request cannot prove it belongs to a company that has no
/// tenant, so it is refused. In practice this is correct rather than
/// merely convenient - once the cutover is on, unattributed ticket
/// traffic belongs to a company that has a tenant, because a client
/// that resolved no tenant would have had nothing to name but the shared
/// context for an *attributable* request. See the module docs for the
/// one case this costs: a company with no tenant of its own can still
/// file and read its tickets, but its `ticket_id`-keyed reads are refused
/// until it is provisioned.
pub fn check(
    mode: RoutingMode,
    traffic: &Guarded,
    class: CommandClass,
    subject: &Subject,
    tenant_of_company: Option<&str>,
) -> Verdict {
    // Off: this is exactly the pre-cutover behaviour, so there is
    // nothing to refuse. Checked first so that a deployment which has
    // not opted in is unaffected even if it has tenants lying around.
    if mode == RoutingMode::Shared {
        return Verdict::Allow;
    }
    if !is_routable(class) {
        return Verdict::Allow;
    }
    let company_id = match subject {
        Subject::Company(company_id) => company_id,
        // Refused regardless of `tenant_of_company`, which is `None` here
        // by construction - there is no company to have looked one up for.
        // An `OwnerOfRow` should have been resolved by the caller; one that
        // wasn't is no better attributed than a bare `ticket_id`.
        Subject::Unattributed | Subject::OwnerOfRow { .. } => {
            return Verdict::Misrouted {
                traffic: traffic.clone(),
                company_id: None,
            }
        }
    };
    match tenant_of_company {
        // The company has a tenant, so the shared context is the wrong
        // place for its tickets and this request is a bypass attempt.
        Some(_) => Verdict::Misrouted {
            traffic: traffic.clone(),
            company_id: Some(company_id.to_string()),
        },
        // No tenant: shared is the documented fallback, and
        // `routing.rs`'s own module docs make the promise that this can
        // only preserve working behaviour, never introduce a new failure.
        None => Verdict::Allow,
    }
}

/// The client-facing message for a refusal.
///
/// Names the tenant-free company explicitly: a caller that hit this needs
/// to know it is a routing problem and not a permissions problem, since
/// it very likely *does* hold a valid mapping on the shared context.
pub fn rejection_message(traffic: &Guarded, company_id: Option<&str>) -> String {
    let what = match traffic {
        Guarded::Command(name) => format!("command type {name:?}"),
        Guarded::Projection(name) => format!("projection {name:?}"),
    };
    let whose = match company_id {
        Some(company_id) => format!("company {company_id:?}"),
        None => "this ticket".to_string(),
    };
    format!(
        "{what} names the shared {BOUNDED_CONTEXT:?} context, but {whose} is served from its own \
         tenant. Resolve the company's tenant and name that instead - this is a routing error, not \
         an authorization one ({} was a valid destination).",
        traffic.name()
    )
}

/// Why a tenant lookup failed, as distinct from a lookup that succeeded
/// and found nothing.
///
/// The distinction is the point: "no tenant" is an answer that lets the
/// request through to the shared context, so a failure must never be
/// flattened into it. `skilj_core::error::Error` has no
/// `From<serde_json::Error>`, and decoding a stored `TenantDirectoryState`
/// is not any of its other variants' business either, so this is a local
/// type rather than a forced fit.
#[derive(Debug)]
pub enum LookupError {
    /// The database call itself failed.
    Db(skilj_core::error::Error),
    /// The stored state was not a `TenantDirectoryState`. Cannot happen
    /// for a row written by this projection's own `project()`, which is
    /// why it is a variant and not a panic - but a caller still has to
    /// decide what to do, and treating it as "no tenant" would send this
    /// company's traffic to the shared context.
    UndecodableState(String),
}

impl std::fmt::Display for LookupError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            LookupError::Db(e) => write!(f, "couldn't read the tenant directory: {e}"),
            LookupError::UndecodableState(e) => {
                write!(f, "tenant directory state is not decodable: {e}")
            }
        }
    }
}

impl std::error::Error for LookupError {}

impl From<skilj_core::error::Error> for LookupError {
    fn from(e: skilj_core::error::Error) -> Self {
        LookupError::Db(e)
    }
}

/// Resolve a company's tenant from the `TenantDirectory` projection.
///
/// Returns the *recorded* name, never a re-derived one, so a company
/// provisioned before `routing::tenant_name_for` existed keeps the
/// context it already has (`routing.rs`'s own note on the same point).
pub async fn tenant_for_company(
    pool: &skilj_core::db::Pool,
    company_id: &str,
) -> Result<Option<String>, LookupError> {
    let Some((state, _owner)) = skilj_core::db::get_projection_state_and_owner(
        pool,
        BOUNDED_CONTEXT,
        TENANT_DIRECTORY,
        company_id,
    )
    .await?
    else {
        return Ok(None);
    };
    let state: TenantDirectoryState =
        serde_json::from_str(&state).map_err(|e| LookupError::UndecodableState(e.to_string()))?;
    Ok(state.tenant_name)
}

/// The company a request is about, as far as the request itself reveals.
///
/// `payload` is the decoded command payload, present for `submitCommand`
/// and absent for `projection` (where the `key` argument carries the
/// subject instead).
///
/// Only `company_id` is looked for, and only at the top level. That is
/// enough for `CreateTicket` - the one command that both names a company
/// and starts a company's ticket history - and for the `CompanyActiveTickets`
/// read. A `CustomerTickets` read reports the row whose owner to look up.
/// Everything else reports `Unattributed`, which the caller then refuses
/// conservatively rather than guessing a company from a `ticket_id`.
pub fn subject_of(
    traffic: &Guarded,
    payload: Option<&serde_json::Value>,
    projection_key: Option<&str>,
) -> Subject {
    match traffic {
        Guarded::Command(_) => payload
            .and_then(|payload| payload["company_id"].as_str())
            .map(|company_id| Subject::Company(company_id.to_string()))
            .unwrap_or(Subject::Unattributed),
        Guarded::Projection(name) if name == "CompanyActiveTickets" => projection_key
            .map(|key| Subject::Company(key.to_string()))
            .unwrap_or(Subject::Unattributed),
        Guarded::Projection(name) if name == CUSTOMER_TICKETS => projection_key
            .map(|key| Subject::OwnerOfRow {
                projection: name.clone(),
                key: key.to_string(),
            })
            .unwrap_or(Subject::Unattributed),
        // Every other ticket projection is keyed by `ticket_id`, which
        // says nothing about which company owns it.
        Guarded::Projection(_) => Subject::Unattributed,
    }
}

/// The arguments of one `submitCommand`/`projection` call, as they
/// arrived on the wire.
///
/// Values are `serde_json::Value` rather than `&str` because a GraphQL
/// client may pass any argument as a `$variable` instead of an inline
/// literal - the frontend inlines them, but a caller need not, and
/// reading only inline literals would let a request opt out of the guard
/// purely by changing how it spells the same call.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallArgs {
    pub fields: Vec<(String, serde_json::Value)>,
}

impl CallArgs {
    fn get(&self, name: &str) -> Option<&serde_json::Value> {
        self.fields
            .iter()
            .find(|(field, _)| field == name)
            .map(|(_, value)| value)
    }
    fn get_str(&self, name: &str) -> Option<&str> {
        self.get(name).and_then(|value| value.as_str())
    }
}

/// Extract the arguments of `operation(` from a GraphQL query string,
/// resolving `$variables` against the request's own `variables` object.
///
/// **A targeted extractor, not a GraphQL parser.** It walks forward from
/// the literal `operation(` to its matching `)`, tracking string literals
/// so a `)` inside an argument value cannot end the scan early, and
/// splits top-level commas. That is enough for the two operations this
/// guard cares about and nothing more - it deliberately does not build an
/// AST, validate the document, or resolve aliases, fragments, or
/// directives.
///
/// **On anything it cannot parse it returns `None`, and the caller lets
/// the request through.** That is a deliberate choice, not an oversight:
/// failing closed would mean a parser gap silently breaks real traffic,
/// including traffic that is routed perfectly correctly, and a guard
/// that intermittently refuses valid requests is one people disable. It
/// is also not a hole worth much - the guard's real backstop is
/// authorisation, which `skilj-graphql` applies per context regardless
/// of what this module decides, so evading the extractor yields a
/// misrouted write rather than an unauthorised one. The extraction is
/// here to stop the ordinary case (a client naming the shared context
/// because it resolved no tenant) from splitting a company's history,
/// which is what actually happens in practice.
pub fn extract_call(
    query: &str,
    operation: &str,
    variables: &serde_json::Value,
) -> Option<CallArgs> {
    let needle = format!("{operation}(");
    let start = query.find(&needle)? + needle.len();
    let bytes = query.as_bytes();
    let mut fields = Vec::new();
    let mut i = start;
    loop {
        // Skip whitespace and commas between arguments.
        while i < bytes.len() && (bytes[i] as char).is_whitespace()
            || i < bytes.len() && bytes[i] == b','
        {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        if bytes[i] == b')' {
            return Some(CallArgs { fields });
        }
        // Argument name.
        let name_start = i;
        while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
            i += 1;
        }
        if i == name_start {
            return None;
        }
        let name = query[name_start..i].to_string();
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b':' {
            return None;
        }
        i += 1;
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            return None;
        }
        let value = if bytes[i] == b'"' {
            // A GraphQL string literal. Handled by hand rather than
            // `serde_json::from_str` because GraphQL's escaping and
            // JSON's differ (`\/` and `\u` handling in particular).
            let (text, next) = read_graphql_string(query, i)?;
            i = next;
            // A block string (`"""..."""`) is not expected for any
            // argument this guard reads, and is not parsed.
            serde_json::Value::String(text)
        } else if bytes[i] == b'$' {
            i += 1;
            let var_start = i;
            while i < bytes.len() && (bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_') {
                i += 1;
            }
            let name = &query[var_start..i];
            variables.get(name)?.clone()
        } else {
            // A number, boolean, null or enum - none of which any
            // argument this guard reads can legitimately be, so a value
            // of the wrong shape stops the scan rather than being
            // coerced into something misleading.
            return None;
        };
        fields.push((name, value));
    }
}

/// Read a `"..."` GraphQL string literal starting at the opening quote.
fn read_graphql_string(query: &str, at: usize) -> Option<(String, usize)> {
    let bytes = query.as_bytes();
    let mut out = String::new();
    let mut i = at + 1;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some((out, i + 1)),
            b'\\' => {
                // The escape must have something after it, or the literal
                // is truncated and this is not a string.
                i += 1;
                let escaped = *bytes.get(i)?;
                out.push(match escaped {
                    b'n' => '\n',
                    b't' => '\t',
                    b'r' => '\r',
                    b'b' => '\u{8}',
                    b'f' => '\u{c}',
                    other => other as char,
                });
                i += 1;
            }
            _ => {
                // Copy one whole character, so multi-byte UTF-8 in a
                // company id is not split into invalid halves.
                let ch = query[i..].chars().next()?;
                out.push(ch);
                i += ch.len_utf8();
            }
        }
    }
    None
}

/// Decode a `payload` argument's text into the payload itself.
///
/// The text's shape depends on how the argument was spelled, and both
/// spellings are legitimate GraphQL:
///
///   - **Inline literal.** Reading the literal has already applied
///     GraphQL's unescaping, so the text *is* the payload's JSON and one
///     decode finishes it.
///   - **`$variable`.** A variable's value comes from the request's own
///     JSON `variables` object, where it is still a JSON string *holding*
///     the escaped text. Decoding that yields a `String`, and the
///     payload's JSON is one decode further in.
///
/// The distinguishing case is a first decode that yields a *string*: that
/// is the `$variable` spelling, so it is decoded again. A first decode
/// that yields an object is already the payload, inline or not. A payload
/// that is itself a bare JSON string would be decoded twice and fail,
/// which costs nothing here - a string payload carries no `company_id`
/// either way, so it ends up unattributed and refused.
fn decode_payload(text: &str) -> Option<serde_json::Value> {
    match serde_json::from_str::<serde_json::Value>(text) {
        // The `$variable` spelling: a string still holding escaped JSON.
        Ok(serde_json::Value::String(inner)) => serde_json::from_str(&inner).ok(),
        Ok(value) => Some(value),
        Err(_) => None,
    }
}

/// What a GraphQL request is asking for, once its arguments are read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intention {
    /// A `submitCommand` with a command type this guard does not
    /// recognise as ticket traffic - nothing to check.
    NotTicketTraffic,
    /// Ticket traffic naming a bounded context other than the shared one,
    /// which is either already correct or somebody else's tenant.
    Elsewhere(String),
    /// Ticket traffic naming the shared context, with whatever the request
    /// revealed about which company it concerns.
    SharedContext { traffic: Guarded, subject: Subject },
    /// The request could not be read.
    Unreadable,
}

/// Read a GraphQL request body's `query` and `variables` and decide what,
/// if anything, it is trying to do with the shared context.
pub fn intention(body: &serde_json::Value) -> Intention {
    let Some(query) = body["query"].as_str() else {
        return Intention::Unreadable;
    };
    let variables = body.get("variables").cloned().unwrap_or_default();
    if let Some(call) = extract_call(query, "submitCommand", &variables) {
        let (Some(context), Some(command_type_name)) = (
            call.get_str("boundedContext"),
            call.get_str("commandTypeName"),
        ) else {
            return Intention::Unreadable;
        };
        if classify_command_name(command_type_name) != NameClass::Ticket {
            return Intention::NotTicketTraffic;
        }
        if context != BOUNDED_CONTEXT {
            return Intention::Elsewhere(context.to_string());
        }
        // `payload` arrives as a GraphQL *string* literal holding the
        // command's JSON (see `skilj-graphql`'s own
        // `submit_command_field` and the frontend's `api::submit_command`,
        // which double-encodes it). A payload that does not decode
        // leaves the subject unattributed, which the caller refuses - the
        // same conservative answer as a ticket-keyed command.
        let payload = call.get_str("payload").and_then(decode_payload);
        let traffic = Guarded::Command(command_type_name.to_string());
        return Intention::SharedContext {
            subject: subject_of(&traffic, payload.as_ref(), None),
            traffic,
        };
    }
    if let Some(call) = extract_call(query, "projection", &variables) {
        let (Some(context), Some(name)) = (call.get_str("boundedContext"), call.get_str("name"))
        else {
            return Intention::Unreadable;
        };
        if classify_projection_name(name) != NameClass::Ticket {
            return Intention::NotTicketTraffic;
        }
        if context != BOUNDED_CONTEXT {
            return Intention::Elsewhere(context.to_string());
        }
        let traffic = Guarded::Projection(name.to_string());
        return Intention::SharedContext {
            subject: subject_of(&traffic, None, call.get_str("key")),
            traffic,
        };
    }
    Intention::Unreadable
}

/// The GraphQL request body as bytes, recovered after inspection.
pub type RewoundBody = axum::body::Body;

/// How large a GraphQL body this guard will buffer in order to read it.
///
/// Generous for the small documents the frontend and the test suite send,
/// and deliberately finite: buffering a body of unbounded size to
/// pattern-match on it would be a denial-of-service surface of this
/// module's own making, sitting in front of a router that otherwise does
/// its own limiting. A body over this is passed through uninspected,
/// which fails open exactly as an unreadable query does.
const MAX_INSPECTED_BODY: usize = 64 * 1024;

/// Enforce the ticket routing decision on inbound GraphQL requests.
///
/// Mounted in front of `skilj`'s own GraphQL router (see `server.rs`),
/// and only when `TICKET_ROUTING=tenant` - with the cutover off this
/// returns the request untouched without even buffering it, so a
/// deployment that has not opted in pays nothing.
///
/// The body has to be buffered to be read and then handed on unchanged,
/// so it is rebuilt from the bytes taken. Only a `POST` to a path
/// containing `graphql` is inspected; everything else (the REST surface,
/// static assets) passes straight through, and REST needs no guard
/// because `skilj-rest` resolves its own destination (see the module
/// docs).
pub async fn enforce_graphql_routing(
    axum::extract::State(state): axum::extract::State<Arc<GuardState>>,
    request: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let mode = state.mode;
    if mode == RoutingMode::Shared {
        return next.run(request).await;
    }

    let is_graphql_post =
        request.method() == axum::http::Method::POST && request.uri().path().contains("graphql");
    if !is_graphql_post {
        return next.run(request).await;
    }

    let (parts, body) = request.into_parts();
    let bytes = match buffer_up_to(body, MAX_INSPECTED_BODY).await {
        Buffered::Whole(bytes) => bytes,
        // Too large to inspect: pass it on uninspected but intact - the
        // prefix already read followed by the rest of the stream. Failing
        // closed here would turn this module into an outage cause for
        // requests it simply cannot see.
        Buffered::Overflow(rest) => {
            return next
                .run(axum::extract::Request::from_parts(parts, rest))
                .await;
        }
        Buffered::Failed => {
            return refusal_response("couldn't read the request body");
        }
    };

    let refused = match serde_json::from_slice::<serde_json::Value>(&bytes) {
        Ok(parsed) => check_request(&state, &parsed).await,
        // Not JSON at all: not a GraphQL request this guard understands.
        Err(_) => None,
    };

    if let Some(response) = refused {
        return response;
    }
    next.run(axum::extract::Request::from_parts(
        parts,
        RewoundBody::from(bytes),
    ))
    .await
}

/// What `buffer_up_to` managed to read.
enum Buffered {
    /// The whole body, within the limit.
    Whole(axum::body::Bytes),
    /// The body exceeded the limit; this is an equivalent body (the bytes
    /// already read, then whatever is left) to forward uninspected.
    Overflow(RewoundBody),
    /// The underlying stream errored.
    Failed,
}

/// Read `body` until it ends or exceeds `limit` bytes.
///
/// Unlike `axum::body::to_bytes`, going over the limit does not lose what
/// was read: the caller gets back a body that still yields every byte.
async fn buffer_up_to(body: axum::body::Body, limit: usize) -> Buffered {
    use futures_util::StreamExt;

    let mut stream = body.into_data_stream();
    let mut buffered = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(chunk) = chunk else {
            return Buffered::Failed;
        };
        buffered.extend_from_slice(&chunk);
        if buffered.len() > limit {
            let prefix = futures_util::stream::once(async move {
                Ok::<_, axum::Error>(axum::body::Bytes::from(buffered))
            });
            return Buffered::Overflow(RewoundBody::from_stream(prefix.chain(stream)));
        }
    }
    Buffered::Whole(buffered.into())
}

/// Check one parsed GraphQL request, returning a refusal response if the
/// traffic is misrouted.
async fn check_request(
    state: &Arc<GuardState>,
    body: &serde_json::Value,
) -> Option<axum::response::Response> {
    let (traffic, subject) = match intention(body) {
        Intention::SharedContext { traffic, subject } => (traffic, subject),
        // Nothing naming the shared context that this guard has an
        // opinion about: already routed correctly, not ticket traffic,
        // or a read it could not parse.
        Intention::NotTicketTraffic | Intention::Elsewhere(_) | Intention::Unreadable => {
            return None;
        }
    };

    // A row's owner is one lookup more. A row that doesn't exist yet
    // (a customer before their first ticket) holds nothing, so reading it
    // from the shared context can neither split nor leak a history: it is
    // let through. A company with a tenant that does this still gets its
    // misrouting refused, on the `CompanyActiveTickets` read the dashboard
    // makes alongside it.
    let subject = match subject {
        Subject::OwnerOfRow { projection, key } => {
            match skilj_core::db::get_projection_state_and_owner(
                &state.pool,
                BOUNDED_CONTEXT,
                &projection,
                &key,
            )
            .await
            {
                Ok(None) => return None,
                Ok(Some((_, Some(owner)))) => Subject::Company(owner),
                Ok(Some((_, None))) => Subject::Unattributed,
                Err(e) => {
                    eprintln!("routing-guard: {e}");
                    return Some(refusal_response(&format!(
                        "couldn't determine which company this {projection} row belongs to, so \
                         the read was refused rather than served from the shared context: {e}"
                    )));
                }
            }
        }
        subject => subject,
    };

    // Only a named company costs a lookup; `Unattributed` is refused
    // without one, so the per-request database hit is confined to the
    // company-attributable operations.
    let tenant = match &subject {
        Subject::Company(company_id) => match tenant_for_company(&state.pool, company_id).await {
            Ok(tenant) => tenant,
            Err(e) => {
                // A lookup failure is not an answer about this company,
                // and treating it as "no tenant" would let its traffic
                // through to the shared context. Refuse instead, with a
                // message that says what actually went wrong.
                eprintln!("routing-guard: {e}");
                return Some(refusal_response(&format!(
                    "couldn't determine this company's tenant, so its ticket traffic was refused \
                     rather than sent to the shared context: {e}"
                )));
            }
        },
        Subject::Unattributed | Subject::OwnerOfRow { .. } => None,
    };

    match check(
        state.mode,
        &traffic,
        CommandClass::Ticket,
        &subject,
        tenant.as_deref(),
    ) {
        Verdict::Allow => None,
        Verdict::Misrouted {
            traffic,
            company_id,
        } => {
            let message = rejection_message(&traffic, company_id.as_deref());
            eprintln!("routing-guard: refusing misrouted ticket traffic: {message}");
            Some(refusal_response(&message))
        }
    }
}

/// A GraphQL-shaped error response, so a client parsing `errors` sees a
/// refusal rather than an opaque HTTP failure.
///
/// 200 with an `errors` array, matching what `skilj-graphql` itself
/// returns for a rejected mutation (its resolvers surface refusals as
/// GraphQL errors, not HTTP status codes) - returning 4xx here would
/// look like a transport failure to the frontend's own `graphql` helper
/// and would report a routing error as a network error.
fn refusal_response(message: &str) -> axum::response::Response {
    let body = serde_json::json!({
        "data": serde_json::Value::Null,
        "errors": [{
            "message": message,
            "extensions": { "code": "ticket_routing_error" },
        }],
    })
    .to_string();
    axum::response::Response::builder()
        .status(axum::http::StatusCode::OK)
        .header(axum::http::header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(body))
        .expect("a static status and content-type are always valid")
}

/// Shared state the middleware needs, injected via `axum::State` so the
/// pool is not re-cloned per request.
pub struct GuardState {
    pub pool: skilj_core::db::Pool,
    pub mode: RoutingMode,
}

/// Read `TICKET_ROUTING` once, at startup.
///
/// Opt-in, exactly as `routing::RoutingMode::from_env_value` defines it:
/// unset, empty or unrecognised all mean the pre-cutover behaviour, so
/// a typo cannot accidentally cut a deployment over.
pub fn mode_from_env() -> RoutingMode {
    RoutingMode::from_env_value(std::env::var("TICKET_ROUTING").ok().as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::routing::classify;

    const COMPANY: &str = "acme";
    const TENANT: &str = "company_acme_0724d383f4f6de0f";

    fn ticket(name: &str) -> Guarded {
        Guarded::Command(name.to_string())
    }

    #[tokio::test]
    async fn a_body_over_the_inspection_limit_is_forwarded_intact() {
        // Two chunks, the first already over the limit, so the overflow
        // path has to stitch the read prefix back onto the unread rest.
        let chunks = vec![
            Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![b'a'; 10])),
            Ok(axum::body::Bytes::from(vec![b'b'; 5])),
        ];
        let body = axum::body::Body::from_stream(futures_util::stream::iter(chunks));
        let Buffered::Overflow(rest) = buffer_up_to(body, 8).await else {
            panic!("a 15-byte body must overflow an 8-byte limit");
        };
        let forwarded = axum::body::to_bytes(rest, usize::MAX).await.unwrap();
        let mut expected = vec![b'a'; 10];
        expected.extend(vec![b'b'; 5]);
        assert_eq!(forwarded.as_ref(), expected.as_slice());
    }

    #[tokio::test]
    async fn a_body_within_the_limit_is_buffered_whole() {
        let body = axum::body::Body::from("{}");
        let Buffered::Whole(bytes) = buffer_up_to(body, 8).await else {
            panic!("a 2-byte body fits an 8-byte limit");
        };
        assert_eq!(bytes.as_ref(), b"{}");
    }

    #[test]
    fn with_the_cutover_off_nothing_is_refused() {
        // The promise that flipping the cutover on is the only thing that
        // changes behaviour: a deployment with tenants already recorded
        // but `TICKET_ROUTING` unset must serve exactly what it did before.
        let verdict = check(
            RoutingMode::Shared,
            &ticket("CreateTicket"),
            CommandClass::Ticket,
            &Subject::Company(COMPANY.into()),
            Some(TENANT),
        );
        assert_eq!(verdict, Verdict::Allow);
    }

    #[test]
    fn a_company_with_a_tenant_cannot_write_tickets_to_the_shared_context() {
        for name in [
            "CreateTicket",
            "AssignTicket",
            "ResolveTicket",
            "AddInternalNote",
            "RateTicket",
        ] {
            let verdict = check(
                RoutingMode::Tenant,
                &ticket(name),
                classify(name).unwrap(),
                &Subject::Company(COMPANY.into()),
                Some(TENANT),
            );
            assert_eq!(
                verdict,
                Verdict::Misrouted {
                    traffic: ticket(name),
                    company_id: Some(COMPANY.into()),
                },
                "{name} for a tenant-backed company must be refused on the shared context"
            );
        }
    }

    #[test]
    fn a_company_with_a_tenant_cannot_read_tickets_from_the_shared_context() {
        // Both of the frontend's company-keyed and ticket-keyed reads.
        for name in [
            "CompanyActiveTickets",
            "CustomerTickets",
            "TicketInternalNotes",
            "TicketSummary",
        ] {
            let verdict = check(
                RoutingMode::Tenant,
                &Guarded::Projection(name.to_string()),
                CommandClass::Ticket,
                &Subject::Company(COMPANY.into()),
                Some(TENANT),
            );
            assert!(matches!(verdict, Verdict::Misrouted { .. }));
        }
    }

    #[test]
    fn a_company_with_no_tenant_is_still_served_from_the_shared_context() {
        // The fallback is the safety property that makes an incremental
        // rollout possible at all: a company that never signed up must
        // keep working whether or not any other company has a tenant.
        let verdict = check(
            RoutingMode::Tenant,
            &ticket("CreateTicket"),
            CommandClass::Ticket,
            &Subject::Company("brand-new".into()),
            None,
        );
        assert_eq!(verdict, Verdict::Allow);
    }

    #[test]
    fn lifecycle_traffic_is_never_refused_on_the_shared_context() {
        // Lifecycle authority lives in the shared context on purpose
        // (`routing.rs`'s own module docs); refusing it there would break
        // the invariant this whole cutover exists to preserve.
        for name in [
            "SignUpCompany",
            "ConvertCompanyTrial",
            "ExpireCompanyTrial",
            "ReactivateCompany",
        ] {
            let class = classify(name).unwrap();
            let verdict = check(
                RoutingMode::Tenant,
                &ticket(name),
                class,
                &Subject::Company(COMPANY.into()),
                Some(TENANT),
            );
            assert_eq!(verdict, Verdict::Allow, "{name} must stay shared");
        }
    }

    #[test]
    fn per_tenant_infrastructure_commands_are_not_this_guards_business() {
        for name in ["RecordCompanyTenant", "RecordTenantLifecycle"] {
            let verdict = check(
                RoutingMode::Tenant,
                &ticket(name),
                classify(name).unwrap(),
                &Subject::Unattributed,
                None,
            );
            assert_eq!(verdict, Verdict::Allow);
        }
    }

    #[test]
    fn unattributed_ticket_traffic_is_refused_because_it_cannot_be_vouched_for() {
        // The coarse arm: `ticket_id`-keyed traffic names no company, so
        // it cannot demonstrate that the shared context is its fallback.
        let verdict = check(
            RoutingMode::Tenant,
            &Guarded::Projection("TicketInternalNotes".into()),
            CommandClass::Ticket,
            &Subject::Unattributed,
            None,
        );
        assert_eq!(
            verdict,
            Verdict::Misrouted {
                traffic: Guarded::Projection("TicketInternalNotes".into()),
                company_id: None,
            }
        );
    }

    #[test]
    fn the_refusal_explains_itself_as_a_routing_error() {
        let message = rejection_message(&ticket("CreateTicket"), Some(COMPANY));
        assert!(message.contains("CreateTicket"));
        assert!(message.contains(COMPANY));
        assert!(
            message.contains("routing error"),
            "a caller holding a valid shared-context mapping must not read this as a permissions \
             problem, got: {message}"
        );

        // And the unattributable case must not claim to know the company.
        let message = rejection_message(&Guarded::Projection("TicketInternalNotes".into()), None);
        assert!(message.contains("TicketInternalNotes"));
        assert!(!message.contains(COMPANY));
    }

    #[test]
    fn it_reads_the_exact_queries_the_frontend_sends() {
        // Copied from `frontend/src/api.rs`'s own `submit_command` and
        // `query_projection` format strings, so this fails if the
        // frontend's spelling ever drifts from what is parsed here.
        let payload = serde_json::json!({ "company_id": COMPANY, "body": "help" });
        let payload_json = serde_json::to_string(&payload).unwrap();
        let payload_literal = serde_json::to_string(&payload_json).unwrap();
        let query = format!(
            "mutation {{ submitCommand(boundedContext: {BOUNDED_CONTEXT:?}, commandTypeName: \
             \"CreateTicket\", payload: {payload_literal}) {{ accepted rejectionReason }} }}"
        );
        let body = serde_json::json!({ "query": query });
        assert_eq!(
            intention(&body),
            Intention::SharedContext {
                traffic: Guarded::Command("CreateTicket".into()),
                subject: Subject::Company(COMPANY.into()),
            }
        );

        let query = format!(
            "query {{ projection(boundedContext: {BOUNDED_CONTEXT:?}, name: \
             \"CompanyActiveTickets\", key: {COMPANY:?}) {{ ... on helpdesk_CompanyActiveTickets \
             {{ tickets }} }} }}"
        );
        assert_eq!(
            intention(&serde_json::json!({ "query": query })),
            Intention::SharedContext {
                traffic: Guarded::Projection("CompanyActiveTickets".into()),
                subject: Subject::Company(COMPANY.into()),
            }
        );
    }

    #[test]
    fn a_parenthesis_inside_an_argument_does_not_end_the_scan() {
        // The reason the extractor tracks string literals: a company id
        // or note containing `)` is ordinary caller input, and stopping
        // the scan there would silently downgrade the request to
        // `Unreadable` - i.e. a guard that stops guarding exactly when a
        // company has a funny name.
        let payload = serde_json::json!({ "company_id": COMPANY, "body": "a) (b (" });
        let payload_literal =
            serde_json::to_string(&serde_json::to_string(&payload).unwrap()).unwrap();
        let query = format!(
            "mutation {{ submitCommand(boundedContext: {BOUNDED_CONTEXT:?}, commandTypeName: \
             \"CreateTicket\", payload: {payload_literal}) {{ accepted }} }}"
        );
        assert_eq!(
            intention(&serde_json::json!({ "query": query })),
            Intention::SharedContext {
                traffic: Guarded::Command("CreateTicket".into()),
                subject: Subject::Company(COMPANY.into()),
            }
        );
    }

    #[test]
    fn a_comma_inside_an_argument_does_not_split_it() {
        let payload = serde_json::json!({ "company_id": COMPANY, "body": "one, two" });
        let payload_literal =
            serde_json::to_string(&serde_json::to_string(&payload).unwrap()).unwrap();
        let query = format!(
            "mutation {{ submitCommand(boundedContext: {BOUNDED_CONTEXT:?}, commandTypeName: \
             \"CreateTicket\", payload: {payload_literal}) {{ accepted }} }}"
        );
        assert!(matches!(
            intention(&serde_json::json!({ "query": query })),
            Intention::SharedContext { .. }
        ));
    }

    #[test]
    fn arguments_passed_as_variables_are_read_too() {
        // Otherwise opting out of the guard would be as simple as
        // spelling the same call differently.
        let body = serde_json::json!({
            "query": "mutation Submit($bc: String!, $cmd: String!, $p: String!) { \
                      submitCommand(boundedContext: $bc, commandTypeName: $cmd, payload: $p) \
                      { accepted } }",
            "variables": {
                "bc": BOUNDED_CONTEXT,
                "cmd": "CreateTicket",
                "p": serde_json::to_string(&serde_json::to_string(
                    &serde_json::json!({ "company_id": COMPANY })).unwrap()).unwrap(),
            },
        });
        assert_eq!(
            intention(&body),
            Intention::SharedContext {
                traffic: Guarded::Command("CreateTicket".into()),
                subject: Subject::Company(COMPANY.into()),
            }
        );
    }

    #[test]
    fn traffic_already_naming_a_tenant_is_left_alone() {
        // The guard only has an opinion about the shared context.
        let query = format!(
            "mutation {{ submitCommand(boundedContext: {TENANT:?}, commandTypeName: \
             \"CreateTicket\", payload: \"{{}}\") {{ accepted }} }}"
        );
        assert_eq!(
            intention(&serde_json::json!({ "query": query })),
            Intention::Elsewhere(TENANT.into())
        );
    }

    #[test]
    fn non_ticket_traffic_and_unreadable_requests_are_not_guarded() {
        // Lifecycle on the shared context: correct, and must pass.
        let query = format!(
            "mutation {{ submitCommand(boundedContext: {BOUNDED_CONTEXT:?}, commandTypeName: \
             \"SignUpCompany\", payload: \"{{}}\") {{ accepted }} }}"
        );
        assert_eq!(
            intention(&serde_json::json!({ "query": query })),
            Intention::NotTicketTraffic
        );
        // TenantDirectory on the shared context: the read a client does
        // precisely to find out where to route, so refusing it would
        // make routing impossible.
        let query = format!(
            "query {{ projection(boundedContext: {BOUNDED_CONTEXT:?}, name: \
             \"TenantDirectory\", key: {COMPANY:?}) {{ x }} }}"
        );
        assert_eq!(
            intention(&serde_json::json!({ "query": query })),
            Intention::NotTicketTraffic
        );
        // Unreadable: no query, a missing argument, an unterminated
        // literal, a value of an impossible type.
        assert_eq!(intention(&serde_json::json!({})), Intention::Unreadable);
        assert_eq!(
            intention(&serde_json::json!({ "query": "query { projection(name: \"X\") { y } }" })),
            Intention::Unreadable
        );
        assert_eq!(
            intention(
                &serde_json::json!({ "query": "mutation { submitCommand(boundedContext: 7) { a } }" })
            ),
            Intention::Unreadable
        );
        assert_eq!(
            intention(
                &serde_json::json!({ "query": "query { projection(boundedContext: \"helpdesk\", key: \"acme\") { y } }" })
            ),
            Intention::Unreadable
        );
    }

    #[test]
    fn an_undecodable_payload_leaves_the_company_unattributed() {
        // Not an error and not an attribution: the conservative answer,
        // which the caller then refuses.
        let query = format!(
            "mutation {{ submitCommand(boundedContext: {BOUNDED_CONTEXT:?}, commandTypeName: \
             \"CreateTicket\", payload: \"not json at all\") {{ accepted }} }}"
        );
        assert_eq!(
            intention(&serde_json::json!({ "query": query })),
            Intention::SharedContext {
                traffic: Guarded::Command("CreateTicket".into()),
                subject: Subject::Unattributed,
            }
        );
    }

    #[test]
    fn a_multi_byte_company_id_survives_the_scan() {
        let company_id = "ünïcödé";
        let payload = serde_json::json!({ "company_id": company_id });
        let payload_literal =
            serde_json::to_string(&serde_json::to_string(&payload).unwrap()).unwrap();
        let query = format!(
            "mutation {{ submitCommand(boundedContext: {BOUNDED_CONTEXT:?}, commandTypeName: \
             \"CreateTicket\", payload: {payload_literal}) {{ accepted }} }}"
        );
        assert_eq!(
            intention(&serde_json::json!({ "query": query })),
            Intention::SharedContext {
                traffic: Guarded::Command("CreateTicket".into()),
                subject: Subject::Company(company_id.into()),
            }
        );
    }

    #[test]
    fn the_company_keyed_read_and_write_are_attributable() {
        // The two cases the dashboard actually exercises on every load.
        let created = subject_of(
            &ticket("CreateTicket"),
            Some(&serde_json::json!({ "company_id": COMPANY })),
            None,
        );
        assert_eq!(created, Subject::Company(COMPANY.into()));

        let listed = subject_of(
            &Guarded::Projection("CompanyActiveTickets".into()),
            None,
            Some(COMPANY),
        );
        assert_eq!(listed, Subject::Company(COMPANY.into()));
    }

    #[test]
    fn a_customer_keyed_read_is_attributed_through_its_rows_owner() {
        assert_eq!(
            subject_of(
                &Guarded::Projection(CUSTOMER_TICKETS.into()),
                None,
                Some("customer-1")
            ),
            Subject::OwnerOfRow {
                projection: CUSTOMER_TICKETS.into(),
                key: "customer-1".into(),
            }
        );
        // Left unresolved, it is refused like any unattributed read.
        assert!(matches!(
            check(
                RoutingMode::Tenant,
                &Guarded::Projection(CUSTOMER_TICKETS.into()),
                CommandClass::Ticket,
                &Subject::OwnerOfRow {
                    projection: CUSTOMER_TICKETS.into(),
                    key: "customer-1".into(),
                },
                None,
            ),
            Verdict::Misrouted { .. }
        ));
    }

    #[test]
    fn ticket_keyed_reads_and_commands_report_no_company_rather_than_guessing() {
        // A `ticket_id` says nothing about its company, so these are
        // refused conservatively instead of being attributed to whatever
        // company happened to be resolved.
        for name in ["TicketInternalNotes", "TicketSummary"] {
            assert_eq!(
                subject_of(&Guarded::Projection(name.into()), None, Some("ticket-1")),
                Subject::Unattributed,
                "{name} is keyed by ticket_id, not company_id"
            );
        }
        assert_eq!(
            subject_of(
                &ticket("AssignTicket"),
                Some(&serde_json::json!({ "ticket_id": "t-1" })),
                None
            ),
            Subject::Unattributed
        );
        // And a missing/odd payload must not panic into an attribution.
        assert_eq!(
            subject_of(&ticket("CreateTicket"), None, None),
            Subject::Unattributed
        );
    }

    #[test]
    fn every_ticket_projection_in_helpdesk_is_recognised() {
        // The list is hand-written, so assert it against the same
        // projection names `helpdesk.rs` declares - a new Ticket
        // projection there that isn't listed here would silently skip the
        // guard and split a company's reads across two contexts.
        for name in [
            "TicketSummary",
            "CompanyActiveTickets",
            "CustomerTickets",
            "TicketInternalNotes",
        ] {
            assert_eq!(
                classify_projection_name(name),
                NameClass::Ticket,
                "{name} should be recognised as ticket traffic"
            );
        }
        // And the shared-context infrastructure read is not ticket
        // traffic, so a client resolving a tenant through it is untouched.
        assert_eq!(
            classify_projection_name(TENANT_DIRECTORY),
            NameClass::Unknown
        );
    }

    #[test]
    fn an_unrecognised_command_name_passes_rather_than_being_refused() {
        // Deliberately the opposite of `routing::classify`, which errors:
        // a guard must not refuse traffic it cannot classify, or adding a
        // command to `helpdesk.rs` would break writes the moment the
        // cutover was switched on.
        assert_eq!(classify_command_name("NotARealCommand"), NameClass::Other);
        assert_eq!(classify_command_name("SignUpCompany"), NameClass::Other);
        assert_eq!(classify_command_name("CreateTicket"), NameClass::Ticket);
    }

    #[test]
    fn every_classified_command_type_is_covered_by_a_verdict() {
        // Guards against a new command type in `helpdesk.rs` being added
        // without deciding how this module treats it. `classify` is the
        // exhaustive match that forces the decision; this asserts the
        // resulting behaviour, so the table and the tests can't drift.
        let all = [
            "CreateTicket",
            "AssignTicket",
            "ResolveTicket",
            "ReopenTicket",
            "RequestInfoFromCustomer",
            "CustomerRespondsToTicket",
            "CloseTicket",
            "EscalateTicket",
            "MergeTickets",
            "RateTicket",
            "AddInternalNote",
            "SignUpCompany",
            "ConvertCompanyTrial",
            "ExpireCompanyTrial",
            "ReactivateCompany",
            "RecordCompanyTenant",
            "RecordTenantLifecycle",
        ];
        for name in all {
            let class = classify(name).unwrap();
            let expected_refused = matches!(class, CommandClass::Ticket);
            let verdict = check(
                RoutingMode::Tenant,
                &ticket(name),
                class,
                &Subject::Company(COMPANY.into()),
                Some(TENANT),
            );
            assert_eq!(
                matches!(verdict, Verdict::Misrouted { .. }),
                expected_refused,
                "{name} ({class:?}) was treated wrongly"
            );
        }
    }
}
