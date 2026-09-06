//! A real, runnable server for `skilj_helpdesk::helpdesk` - `cargo run
//! --bin server`. Not a test: boots an actual `axum` process serving
//! both REST and GraphQL, prints every credential `alerter`/`scheduler`
//! need as ready-to-export env vars, then serves until killed. Modelled
//! closely on `skilj-demo/src/bin/server.rs`, including its telemetry
//! wiring now (`skilj_helpdesk::telemetry::init` - this crate's own copy
//! of that file's `init_telemetry`; see the module's own doc comment)
//! rather than the earlier pass's decision to trim it out.
//!
//! **Optional fake traffic**: `SEED_DEMO_TRAFFIC=1` signs up a small
//! cast of fake companies once (`sign_up_demo_companies` below), then
//! spawns `SEED_DEMO_CONCURRENCY` (default `1`) independent workers
//! (`run_demo_seed_loop`, driven by the pure decisions in
//! `skilj_helpdesk::demo_seed`), each creating/assigning/resolving fake
//! tickets against this server's own REST surface every
//! `SEED_DEMO_INTERVAL_MS` (default `4000`) - so a dashboard pointed at
//! this process's telemetry has something moving without a person
//! driving curl by hand. `SEED_DEMO_CONCURRENCY` is the load dial: turn
//! it up (or shrink the interval) for a heavier, more dashboard-visible
//! load - each worker paces itself independently and staggers its first
//! tick, so `concurrency` workers is roughly `concurrency`x one
//! worker's own request rate, spread smoothly rather than bursting in
//! lockstep. Unset (the default), nothing about this file's behaviour
//! changes.
//!
//! Needs `DATABASE_URL` pointing at a real Postgres (`PORT` optionally
//! overrides the default `8080`). Every run is safe to repeat against
//! the same database: the bounded context is only created if it doesn't
//! exist yet, and each run mints its own fresh admin `Role` and tokens
//! rather than reusing a previous run's.
//!
//! **The bootstrap below is a shortcut, not the intended production
//! flow** - see `skilj-demo/src/bin/server.rs`'s own doc comment for the
//! full reasoning (seeding a Role directly via `skilj_core::db` is a
//! `cargo run` convenience, not what a real deployment does).
//!
//! **Identity provider: real by default now, not the local JWKS
//! stand-in.** Set `OIDC_ISSUER_URL` to a real running Dex instance
//! (`dex serve dex/config.yaml` - see that file's own doc comment) and
//! this points `IdpConfig` at Dex's own real `/keys` JWKS endpoint,
//! verifying real, browser-flow-issued JWTs - proven end to end against
//! a real Authorization Code + PKCE exchange, not just assumed to work.
//! Leave it unset and this falls back to the same local JWKS/JWT
//! shortcut `skilj-demo`'s own server uses, so `cargo run --bin server`
//! alone still works with zero extra setup - the frontend's own login
//! flow is what actually needs `OIDC_ISSUER_URL` set.
//!
//! The two demo identities `frontend/`'s login page offers
//! (`customer@acme.example` / `customer-demo-pw`, `lead@acme.example` /
//! `staff-demo-pw` - see `dex/config.yaml`) get their own seeded `Role`s
//! here, at the real `sub` Dex's local-password connector actually
//! issues for each (captured once via a real login flow against that
//! exact config - `DEMO_CUSTOMER_SUB`/`DEMO_STAFF_LEAD_SUB` below -
//! deterministic for that config, not something computed at runtime).

use chrono::Utc;
use jsonwebtoken::{EncodingKey, Header};
use opentelemetry::metrics::Counter;
use opentelemetry::KeyValue;
use serde_json::json;
use skilj::{IdpConfig, SigningAlgorithm, Skilj};
use skilj_core::access_control::{self, AccessLevel, Role, RoleAccessMapping, RoleStatus};
use skilj_core::bootstrap::ContextCreator;
use skilj_core::db;
use skilj_core::event_store::{BoundedContext, BoundedContextStatus};
use skilj_core::shared::{generate_token_id, generate_token_secret};
use skilj_helpdesk::demo_seed::{self, Rng, SeedAction, SeedState, DEMO_COMPANIES};
use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use std::collections::HashMap;
use std::sync::LazyLock;
use std::time::Duration;

// --- CSAT metric - see run_csat_metrics_loop's own doc comment ---
//
// Same `LazyLock`/`opentelemetry::global::meter()` shape
// `skilj-core::db`'s own `COMMANDS_PROCESSED`/`EVENTS_APPENDED` use (see
// that module's own doc comment on why this only actually exports once
// `telemetry::init` has already run) - `"skilj-helpdesk"` as the meter's
// own scope name, not `"skilj-core"`, since this metric is this
// application's own domain fact, not a library-level one.
//
// A labelled counter, not a histogram: a rating is one of exactly five
// values, not a continuous measurement - `rating="5"` as an attribute
// gives a clean per-value breakdown in Prometheus (`sum by (rating)
// (...)`) without needing histogram bucket boundaries tuned to a 1-5
// scale (the default ones aren't), and the same series still answers
// "what's the average" just as well (`sum(rating * value) / sum(value)`
// summed by hand in PromQL, or read straight off the distribution panel).
static TICKET_RATINGS: LazyLock<Counter<u64>> = LazyLock::new(|| {
    opentelemetry::global::meter("skilj-helpdesk")
        .u64_counter("skilj_helpdesk.ticket.ratings")
        .with_description("CSAT ratings recorded via RateTicket, by rating value (1-5).")
        .build()
});

// --- local JWKS/IdP shortcut - see this file's own doc comment above ---
//
// The same fixed test RSA keypair `tests/support/mod.rs` already uses
// (itself adapted from `skilj-demo/tests/graphql_auth.rs`) - never a
// real secret, so reusing it here rather than generating a fresh one is
// simpler with no downside. Duplicated rather than shared across the
// test/binary boundary, same reasoning as everywhere else this keypair
// appears in this project.

const TEST_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDPHVFsUHiWXSbG
/TCig1cTQHNT6FnoYoZtMEjvDiQArsOL/dFoM9pmGRM9CfEtQGNum4TsimPtgJec
awfdPnW0uJCRlIF9wGmYdh2mYNBKw8jqxwp664Gd5uqH5L6A4pN8bfGO7+2niD6p
8t0cNeyYOd0PusbAEDcpzCUZmr6KQyM5i8/wk5oO98gntp+ZpMjUZabAD6R8DyhM
IZmV645jo5NPJG7zuSz+3dmKkNY0/GXz8YwvZ2swqmmOANRZHHfN1vgP2ycK02WZ
4yihx6EiuQCDseddBw+xit9KSvSq6GwmwnV1qVpMVNlSGGOeVX7v7JQ3z/BNbQ85
5p6s/FjhAgMBAAECggEAFu8fKghLIhNUjOpSbVxv0vDrFFqBQitOyV50ZQxCzlSL
0L+dZZWAVJfoOnUUYLdli0TrVioI4K7Bmw97AnO9IvLhB03TfPJGfxxtMhQ8XFsL
r3u03GGhq7N7OusIcUslm7ys5/AHd+qtTbJX65zJAx49LVW4VmI1SYqSfSBWgway
8uGYaXyCfwuxQ+xB4fQd6llm/+9dqS+U36LVSMWgEmVjceorYFhPVLfuX4A1wHjF
mDl40AwPBqzVbOIzFDMDikk4heFi6wlt6N3LGDtyBUUuzEg5TBhyiirvNvTjW+4V
Z4MZs3tez+IqM0+F4EsgAEQUU12YQxa4lobm8/zgZQKBgQD81FMzymNR6xWhUSwY
4RtkVntfMBOMp1rVGcVyBxOLKxEXF6ctk2rV38krfUI50h/lWzrbpl+zJvEe8D1H
vZjYj28sL3wf0CSnPYUeGANTxrW1dTiz1HVzzChfbAEWj3fsVrlghNcnHBkDDhqz
L/rPEfp//fB0SyLAEAJt87cgFwKBgQDRtjtH1gIkGn5GCS3u0FAbxV+qrUlTvu4t
Di1GcEw32jootQQSMZN1PxEvLuehaBlaASEL2OZzZlQ4q60LV1Jisvd7wqv5EYnG
o+sKtrCS5iXKfkxqTmg+JS7OZazggyvgBnv4GXT0US6/G4nw7C9JaS2jyOvPGIPS
K8dsWDIxxwKBgQCgr4FBxTticPqKUECqf0cdeilm0fNazXJZRcvLMNwm8vQlrQ6/
VJXt4BDG5xEUFovXBShfOVpRTkqo0x7fXYyq9l49wuAsh+kDsYHNIo3azMvny9yB
zmHnerWeD9KROBWLy4J96W+kl6L94hTuFWxd9psyhX4xKx+m2YXxw5d7eQKBgFB2
I86PHOkvRQ2oDfiX8nSFSQxaSk0Yb5fX3aUuBwBS+YeO1E4KuXH9zaEV1QeHwlpX
Ho/GG71hIKVRsSYtzc1Sr0PL0GHSydLuJ4tHxv3F0fAcf0M2bCaT656DQk4t5dKh
ikUJt2baEx59+XH3nLkE4t75gwhFdqZX5775I+EXAoGAfnpHlLZdGW48rl9Cl887
hRDjXDm/gP/ljCrvxxiWselEgaLj2o4NiT28QAfq7KgtOIpAeLAGzIBP6vkE7KFp
nAF+t4gRpooXXSI5oXCBcGI9a26q68UV3iDEmQGiP8kVHOsdzcOKY0qk1ulNAIV4
fU919gnTKorSq3FdV6zGZ8s=
-----END PRIVATE KEY-----
";
const TEST_MODULUS_N: &str = "zx1RbFB4ll0mxv0wooNXE0BzU-hZ6GKGbTBI7w4kAK7Di_3RaDPaZhkTPQnxLUBjbpuE7Ipj7YCXnGsH3T51tLiQkZSBfcBpmHYdpmDQSsPI6scKeuuBnebqh-S-gOKTfG3xju_tp4g-qfLdHDXsmDndD7rGwBA3KcwlGZq-ikMjOYvP8JOaDvfIJ7afmaTI1GWmwA-kfA8oTCGZleuOY6OTTyRu87ks_t3ZipDWNPxl8_GML2drMKppjgDUWRx3zdb4D9snCtNlmeMoocehIrkAg7HnXQcPsYrfSkr0quhsJsJ1dalaTFTZUhhjnlV-7-yUN8_wTW0POeaerPxY4Q";
const TEST_EXPONENT_E: &str = "AQAB";
const TEST_KID: &str = "test-key-1";
const TEST_ISSUER: &str = "https://idp.example.test/";

// --- real IdP demo identities - see this file's own module doc comment ---
//
// Dex's local-password connector's own `sub` claim isn't the plain
// `userID` from `dex/config.yaml` - it's an opaque, connector-scoped
// encoding (`base64(protobuf{connector_id, user_id})`), deterministic
// for a given connector id + userID but not worth reverse-engineering
// here. Captured once by actually running the real Authorization Code +
// PKCE flow against `dex/config.yaml` and decoding the resulting
// `id_token`'s own `sub` claim - not computed, not guessed.
const DEMO_CUSTOMER_SUB: &str = "Cg1jdXN0b21lci1kZW1vEgVsb2NhbA";
const DEMO_STAFF_LEAD_SUB: &str = "Cg9zdGFmZi1sZWFkLWRlbW8SBWxvY2Fs";
// `frontend/src/config.rs`'s own `DEMO_COMPANY_ID` - the walkthrough
// company README.md's own "sign up the demo company" step creates.
// This binary needs its own copy (no shared crate boundary between
// frontend/ and the backend - see frontend/Cargo.toml's own doc
// comment) to scope the demo customer Role's own RoleAccessMapping
// below to it.
const DEMO_COMPANY_ID: &str = "acme";

async fn serve_local_jwks() -> String {
    let jwks = json!({
        "keys": [{
            "kty": "RSA",
            "use": "sig",
            "alg": "RS256",
            "kid": TEST_KID,
            "n": TEST_MODULUS_N,
            "e": TEST_EXPONENT_E,
        }]
    });
    let app = axum::Router::new().route(
        "/jwks.json",
        axum::routing::get(move || {
            let jwks = jwks.clone();
            async move { axum::Json(jwks) }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("failed to bind an ephemeral port for the local JWKS server");
    let addr = listener
        .local_addr()
        .expect("a bound listener always has a local address");
    tokio::spawn(async move {
        axum::serve(listener, app)
            .await
            .expect("the local JWKS server stopped unexpectedly");
    });
    format!("http://{addr}/jwks.json")
}

fn sign_jwt(subject: &str) -> String {
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(TEST_KID.to_string());
    let claims = json!({
        "sub": subject,
        "iss": TEST_ISSUER,
        "exp": (Utc::now() + chrono::Duration::hours(1)).timestamp(),
    });
    let key = EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY_PEM.as_bytes())
        .expect("the test private key PEM is well-formed");
    jsonwebtoken::encode(&header, &claims, &key).expect("signing a well-formed JWT never fails")
}

/// Every `rest_trigger_allowed` command type in `helpdesk.rs`.
const COMMAND_TYPES: &[&str] = &[
    "SignUpCompany",
    "ConvertCompanyTrial",
    "ExpireCompanyTrial",
    "ReactivateCompany",
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
];

/// `src/bin/alerter.rs`'s own event types - `TicketResolved`/
/// `TicketReopened`/`TicketClosed` overlap with `SCHEDULER_EVENT_TYPES`
/// below on purpose: `skilj-rest`'s `mode=auto` cursor is tracked
/// per-token (`db::get_read_cursor`), so alerter and scheduler each need
/// their *own* token for a type both read - sharing one would have each
/// steal the other's cursor position. That's why event-token minting
/// below is two separate loops, not one list shared by both binaries.
const ALERTER_EVENT_TYPES: &[&str] = &[
    "TicketCreated",
    "TicketResolved",
    "TicketReopened",
    "TicketClosed",
    "TicketEscalated",
    "TicketsMerged",
];

/// `src/bin/scheduler.rs`'s own event types - see `ALERTER_EVENT_TYPES`'s
/// own doc comment for why this is a separate list rather than a shared
/// one, despite the overlap.
const SCHEDULER_EVENT_TYPES: &[&str] = &[
    "CompanySignedUp",
    "CompanyActivated",
    "CompanyExpired",
    "TicketResolved",
    "TicketReopened",
    "TicketClosed",
    "TicketsMerged",
];

async fn shutdown_signal() {
    let ctrl_c = async {
        tokio::signal::ctrl_c()
            .await
            .expect("failed to install a Ctrl+C handler");
    };
    #[cfg(unix)]
    let terminate = async {
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .expect("failed to install a SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
    println!("server: shutdown signal received");
}

/// Mints one fresh `EventReadToken` per `event_type_name`, printing each
/// as it goes (`send as authorization: Bearer <id>.<secret>` to
/// `/v1/events/consume`) - called once per consumer
/// (`ALERTER_EVENT_TYPES`/`SCHEDULER_EVENT_TYPES`), never once per event
/// type overall, so two consumers reading the same event type each get
/// their own independent token/cursor - see `ALERTER_EVENT_TYPES`'s own
/// doc comment for why that matters.
async fn mint_event_tokens(
    pool: &db::Pool,
    mapping: &RoleAccessMapping,
    event_type_names: &[&'static str],
) -> Result<HashMap<&'static str, String>, Box<dyn std::error::Error>> {
    let mut tokens = HashMap::new();
    for event_type_name in event_type_names {
        let event_type = db::get_event_type(pool, BOUNDED_CONTEXT, event_type_name)
            .await?
            .unwrap_or_else(|| {
                panic!("{BOUNDED_CONTEXT}/{event_type_name} should have just been registered")
            });
        let token = access_control::create_event_read_token(
            mapping,
            &event_type,
            generate_token_id(),
            generate_token_secret(),
            // Unrestricted: alerter/scheduler are operational consumers
            // watching every company's own events by design (paging a
            // lead, auto-closing a ticket - neither is "acting as" any
            // one company) - the same reasoning `mapping`'s own `scope`
            // below gets for the same reason.
            None,
            Utc::now(),
        )?;
        db::insert_event_read_token(pool, &token).await?;
        let credential = format!("{}.{}", token.id, token.secret);
        println!("  {event_type_name}: {credential}");
        tokens.insert(*event_type_name, credential);
    }
    Ok(tokens)
}

/// A `TicketRated` payload carries the actual rating - `skilj-core`'s
/// own generic `skilj_commands_processed_total`/`skilj_events_appended_total`
/// (what the dashboard's own "Tickets rated / min" panel already uses)
/// only ever see *that* a `RateTicket` happened, never the 1-5 value
/// itself, since neither is domain-aware. This is that missing piece: a
/// small consumer of this server's own real event feed - the identical
/// "separately-deployable consumer" shape `src/bin/alerter.rs`'s own
/// module doc comment describes, just spawned inline here rather than
/// as its own binary, since one counter doesn't earn a whole deployable
/// unit of its own. Gated on telemetry actually being configured
/// (`main`'s own `telemetry.is_some()`) - with no `MeterProvider`
/// installed, `TICKET_RATINGS` already records into a harmless no-op
/// meter, but there is no reason to keep a poll loop and its own token
/// alive for that.
async fn run_csat_metrics_loop(client: &reqwest::Client, base_url: &str, token: &str) {
    const POLL_INTERVAL: Duration = Duration::from_secs(5);
    loop {
        match consume_ticket_rated(client, base_url, token).await {
            Ok(ratings) => {
                for rating in ratings {
                    TICKET_RATINGS.add(1, &[KeyValue::new("rating", i64::from(rating))]);
                }
            }
            Err(e) => eprintln!("csat metrics: poll failed, will retry: {e}"),
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// One `GET /v1/events/consume?mode=auto` call, decoded down to just the
/// `rating` field this loop needs - the identical shape
/// `src/bin/alerter.rs`'s own `consume` has, duplicated rather than
/// shared for the same "no common library boundary worth introducing
/// for one helper" reason that file's own doc comment gives.
async fn consume_ticket_rated(client: &reqwest::Client, base_url: &str, token: &str) -> Result<Vec<u8>, reqwest::Error> {
    #[derive(serde::Deserialize)]
    struct ConsumeResponse {
        events: Vec<EventDto>,
    }
    #[derive(serde::Deserialize)]
    struct EventDto {
        payload: serde_json::Value,
    }
    let response = client
        .get(format!("{base_url}/v1/events/consume"))
        .query(&[("mode", "auto")])
        .bearer_auth(token)
        .send()
        .await?
        .error_for_status()?;
    let body: ConsumeResponse = response.json().await?;
    Ok(body
        .events
        .into_iter()
        .filter_map(|e| e.payload["rating"].as_u64())
        .map(|r| r as u8)
        .collect())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Must be the very first thing this binary does - see
    // skilj_helpdesk::telemetry's own doc comment on why every
    // skilj-core/skilj-rest counter/histogram (LazyLock, first touched
    // the first time a command/event/request actually happens) needs
    // the global MeterProvider set before that first touch.
    let telemetry = skilj_helpdesk::telemetry::init("skilj-helpdesk-server");

    let database_url = std::env::var("DATABASE_URL")
        .map_err(|_| "DATABASE_URL must be set (a real Postgres, not embedded)")?;
    let port: u16 = std::env::var("PORT")
        .ok()
        .and_then(|p| p.parse().ok())
        .unwrap_or(8080);

    let pool = db::connect(&database_url).await?;
    db::migrate(&pool).await?;

    if db::get_bounded_context(&pool, BOUNDED_CONTEXT).await?.is_none() {
        db::insert_bounded_context(
            &pool,
            &BoundedContext {
                name: BOUNDED_CONTEXT.to_string(),
                status: BoundedContextStatus::Active,
                created_at: Utc::now(),
                created_by: ContextCreator::SystemCreator,
                template: None,
            },
        )
        .await?;
        println!("server: created bounded context {BOUNDED_CONTEXT:?}");
    }

    let external_subject = format!("skilj-helpdesk-admin-{}", generate_token_id());
    let role = Role {
        id: generate_token_id(),
        external_subject: external_subject.clone(),
        name: "skilj-helpdesk admin".into(),
        superadmin: false,
        status: RoleStatus::Active,
        created_at: Utc::now(),
        revoked_at: None,
    };
    db::insert_role(&pool, &role).await?;

    let bounded_context = db::get_bounded_context(&pool, BOUNDED_CONTEXT)
        .await?
        .expect("just ensured it exists above");
    let mapping = RoleAccessMapping {
        role: role.clone(),
        bounded_context,
        level: AccessLevel::Admin,
        can_read_sensitive: false,
        // Unrestricted - this is the system's own type-registration/
        // reconciliation bootstrap Role (see `.reconciliation_role`
        // below), not a caller acting on behalf of any one company.
        scope: None,
        status: RoleStatus::Active,
        created_at: Utc::now(),
        revoked_at: None,
    };
    db::insert_role_access_mapping(&pool, &mapping).await?;

    // Real IdP when OIDC_ISSUER_URL is set (a running Dex instance - see
    // this file's own module doc comment), the local JWKS/JWT shortcut
    // otherwise. Either way `IdpConfig` is what skilj-graphql actually
    // verifies every GraphQL request's JWT against.
    let oidc_issuer_url = std::env::var("OIDC_ISSUER_URL").ok();
    let (issuer, jwks_url) = match &oidc_issuer_url {
        Some(url) => (url.clone(), format!("{url}/keys")),
        None => (TEST_ISSUER.to_string(), serve_local_jwks().await),
    };

    // The two demo identities the frontend's login page offers, seeded
    // with Write access the same way the bootstrap admin Role above is -
    // only meaningful against a real Dex (the local shortcut can sign a
    // JWT for any subject on demand, so it never needed pre-seeded
    // Roles the way real, IdP-issued `sub`s do).
    if oidc_issuer_url.is_some() {
        // Unlike the bootstrap admin Role above (a fresh random
        // external_subject every run, so it can never collide), these
        // two use fixed, deterministic subs - re-running against the
        // same database without this check violates
        // `roles_unique_active_external_subject` outright. Found by
        // actually restarting this binary twice against one database,
        // not assumed - this module's own doc comment's "every run is
        // safe to repeat" claim needed to be true here too, not just for
        // the bounded context.
        let existing_roles = db::list_roles(&pool).await?;
        // The actual fix (see this file's own module doc comment on the
        // cross-tenant read gap a security review found, and skilj's own
        // `docs/architecture.md` §23 for the mechanism): the demo
        // customer's own grant is scoped to its own company, so
        // `TicketSummary`/`CompanyTicketList`/`TicketInternalNotes` -
        // every projection that declares `OWNER_TAG_KEY` - now rejects
        // any instance whose derived owner isn't `DEMO_COMPANY_ID`, not
        // just "this Role has some mapping on the bounded context."
        // staff-lead stays unrestricted (`None`) on purpose: real
        // support staff serve every company sharing this one bounded
        // context, not just one.
        //
        // `name` is `"staff"`/`"customer"` (`helpdesk::STAFF_TEAM` for
        // the former) rather than the older `"skilj-helpdesk demo
        // {label}"` prose because `Role` has no separate "team" field -
        // `TicketInternalNotes`'s own `Projection::TEAM_ONLY` and
        // `AddInternalNote`/`TicketInternalNoteAdded`'s own
        // `private_fields()` (see `src/helpdesk.rs`, skilj 0.0.4)
        // compare against `name` directly, so the staff-lead Role's
        // `name` must literally be `"staff"` for it to still read
        // those. staff-lead stays unrestricted (`scope: None`) on
        // purpose: real support staff serve every company sharing this
        // one bounded context, not just one.
        for (label, sub, scope, name) in [
            (
                "customer",
                DEMO_CUSTOMER_SUB,
                Some(DEMO_COMPANY_ID.to_string()),
                "customer",
            ),
            (
                "staff-lead",
                DEMO_STAFF_LEAD_SUB,
                None,
                skilj_helpdesk::helpdesk::STAFF_TEAM,
            ),
        ] {
            if let Some(existing) = existing_roles
                .iter()
                .find(|r| r.external_subject == sub && r.status == RoleStatus::Active)
            {
                // A Role seeded by a server binary built before the
                // `TEAM_ONLY`/`private_fields` gates above existed
                // still carries the old `"skilj-helpdesk demo {label}"`
                // prose - found in review: without this, `name` (and
                // every gate comparing against it) would stay wrong
                // forever on any database that already had this Role,
                // silently violating this module's own "every run is
                // safe to repeat" claim on exactly the upgrade path
                // that claim exists for.
                if existing.name != name {
                    let mut renamed = existing.clone();
                    renamed.name = name.to_string();
                    db::update_role(&pool, &renamed).await?;
                    println!(
                        "server: renamed demo Role for {label} (sub {sub:?}) from {:?} to {name:?} - pre-existing Role from before TEAM_ONLY/private_fields",
                        existing.name
                    );
                } else {
                    println!("server: demo Role for {label} already exists (sub {sub:?})");
                }
                continue;
            }
            let demo_role = Role {
                id: generate_token_id(),
                external_subject: sub.to_string(),
                name: name.to_string(),
                superadmin: false,
                status: RoleStatus::Active,
                created_at: Utc::now(),
                revoked_at: None,
            };
            db::insert_role(&pool, &demo_role).await?;
            let demo_mapping = RoleAccessMapping {
                role: demo_role,
                bounded_context: mapping.bounded_context.clone(),
                level: AccessLevel::Write,
                can_read_sensitive: false,
                scope,
                status: RoleStatus::Active,
                created_at: Utc::now(),
                revoked_at: None,
            };
            db::insert_role_access_mapping(&pool, &demo_mapping).await?;
            println!("server: seeded demo Role for {label} (sub {sub:?})");
        }
    }

    let (skilj, report) = skilj_helpdesk::register(Skilj::builder(database_url))
        .reconciliation_role(external_subject.clone())
        .identity_provider(IdpConfig::new(
            jwks_url
                .parse()
                .unwrap_or_else(|e| panic!("{jwks_url:?} is not a well-formed URL: {e}")),
            issuer,
            SigningAlgorithm::Rs256,
        ))
        .build()
        .await?;
    println!("server: reconciliation complete, registered {:?}", report.registered);
    if !report.skipped_no_access.is_empty() {
        println!("server: reconciliation skipped (no access yet): {:?}", report.skipped_no_access);
    }

    if let Some(url) = &oidc_issuer_url {
        println!("\nreal IdP: {url} - log in as customer@acme.example / customer-demo-pw");
        println!("or lead@acme.example / staff-demo-pw (see dex/config.yaml)");
    } else {
        println!("\nGraphQL Role credential (send as `authorization: Bearer <jwt>`):");
        println!("  {}", sign_jwt(&role.external_subject));
        println!("(local JWKS shortcut in use - set OIDC_ISSUER_URL to a running Dex for a real login flow)");
    }

    println!("\ncommand tokens (send as `authorization: Bearer <id>.<secret>` to /v1/commands/trigger):");
    let mut command_tokens = HashMap::new();
    for command_type_name in COMMAND_TYPES {
        let command_type = db::get_command_type(&pool, BOUNDED_CONTEXT, command_type_name)
            .await?
            .unwrap_or_else(|| {
                panic!("{BOUNDED_CONTEXT}/{command_type_name} should have just been registered")
            });
        let token = access_control::create_command_token(
            &mapping,
            &command_type,
            generate_token_id(),
            generate_token_secret(),
            // Unrestricted - these are this file's own bootstrap
            // command tokens (printed for the demo curl example, and
            // handed to demo_seed's own fake traffic), not scoped to
            // any one company, same reasoning as every other `None`
            // scope in this file.
            None,
            Utc::now(),
        )?;
        db::insert_command_token(&pool, &token).await?;
        let credential = format!("{}.{}", token.id, token.secret);
        println!("  {command_type_name}: {credential}");
        command_tokens.insert(*command_type_name, credential);
    }

    // Two independent mints, not one shared list - see
    // `ALERTER_EVENT_TYPES`'s own doc comment for why alerter and
    // scheduler can't share a token for the event types they both read.
    println!("\nalerter's own event read tokens:");
    let alerter_event_tokens = mint_event_tokens(&pool, &mapping, ALERTER_EVENT_TYPES).await?;
    println!("\nscheduler's own event read tokens:");
    let scheduler_event_tokens = mint_event_tokens(&pool, &mapping, SCHEDULER_EVENT_TYPES).await?;

    // Only if telemetry is actually configured - see
    // run_csat_metrics_loop's own doc comment for why a token and a
    // poll loop otherwise have nothing to record into.
    let csat_metrics_token = if telemetry.is_some() {
        println!("\nCSAT metrics' own event read token:");
        Some(mint_event_tokens(&pool, &mapping, &["TicketRated"]).await?["TicketRated"].clone())
    } else {
        None
    };

    // Ready-to-paste env vars for the other two binaries this session
    // built - closes the loop between all three.
    println!("\nto run the alerter against this server:");
    println!("  export SKILJ_BASE_URL=http://localhost:{port}");
    println!("  export TICKET_CREATED_TOKEN={}", alerter_event_tokens["TicketCreated"]);
    println!("  export TICKET_RESOLVED_TOKEN={}", alerter_event_tokens["TicketResolved"]);
    println!("  export TICKET_REOPENED_TOKEN={}", alerter_event_tokens["TicketReopened"]);
    println!("  export TICKET_CLOSED_TOKEN={}", alerter_event_tokens["TicketClosed"]);
    println!("  export TICKET_ESCALATED_TOKEN={}", alerter_event_tokens["TicketEscalated"]);
    println!("  export TICKETS_MERGED_TOKEN={}", alerter_event_tokens["TicketsMerged"]);
    println!("  export ESCALATE_TICKET_TOKEN={}", command_tokens["EscalateTicket"]);
    println!("  cargo run --bin alerter");

    println!("\nto run the scheduler against this server:");
    println!("  export SKILJ_BASE_URL=http://localhost:{port}");
    println!("  export COMPANY_SIGNED_UP_TOKEN={}", scheduler_event_tokens["CompanySignedUp"]);
    println!("  export COMPANY_ACTIVATED_TOKEN={}", scheduler_event_tokens["CompanyActivated"]);
    println!("  export COMPANY_EXPIRED_TOKEN={}", scheduler_event_tokens["CompanyExpired"]);
    println!("  export TICKET_RESOLVED_TOKEN={}", scheduler_event_tokens["TicketResolved"]);
    println!("  export TICKET_REOPENED_TOKEN={}", scheduler_event_tokens["TicketReopened"]);
    println!("  export TICKET_CLOSED_TOKEN={}", scheduler_event_tokens["TicketClosed"]);
    println!("  export TICKETS_MERGED_TOKEN={}", scheduler_event_tokens["TicketsMerged"]);
    println!("  export CONVERT_COMPANY_TRIAL_TOKEN={}", command_tokens["ConvertCompanyTrial"]);
    println!("  export EXPIRE_COMPANY_TRIAL_TOKEN={}", command_tokens["ExpireCompanyTrial"]);
    println!("  export CLOSE_TICKET_TOKEN={}", command_tokens["CloseTicket"]);
    println!("  cargo run --bin scheduler");

    let rest = skilj.rest_router();
    let graphql = skilj.graphql_router().await?;
    // Permissive: this showcase's whole point is a real browser
    // (frontend/, a different origin) calling this server directly, and
    // skilj's own auth is bearer-token-based (REST's own AccessToken,
    // GraphQL's own JWT), never cookies - so there's no CSRF surface
    // permissive CORS opens up here the way it would for cookie-based
    // auth. A real deployment would still want this restricted to its
    // own known frontend origin(s) rather than left permissive.
    let app = rest.merge(graphql).layer(tower_http::cors::CorsLayer::permissive());

    let listener = tokio::net::TcpListener::bind(("0.0.0.0", port)).await?;
    println!("\nskilj-helpdesk listening on http://localhost:{port} (REST under /v1/..., GraphQL at /graphql)");
    println!("example - sign up a company:");
    println!(
        "  curl -H 'authorization: Bearer {}' -H 'content-type: application/json' \\\n\
         \x20      -d '{{\"payload\":{{\"company_id\":\"acme\",\"name\":\"Acme\",\"contact_email\":\"a@acme.example\"}}}}' \\\n\
         \x20      http://localhost:{port}/v1/commands/trigger",
        command_tokens["SignUpCompany"],
    );

    if let Some(token) = csat_metrics_token {
        println!("\nrecording CSAT ratings as a real metric (skilj_helpdesk_ticket_ratings_total)");
        let base_url = format!("http://localhost:{port}");
        let client = reqwest::Client::new();
        tokio::spawn(async move { run_csat_metrics_loop(&client, &base_url, &token).await });
    }

    // Optional fake traffic - see this file's own module doc comment.
    // Reuses the exact CommandTokens just minted/printed above, so this
    // is a real client of this same process's own REST surface, not a
    // shortcut around it.
    if std::env::var("SEED_DEMO_TRAFFIC")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
    {
        let interval_ms: u64 = std::env::var("SEED_DEMO_INTERVAL_MS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4000);
        // The load dial: each worker paces itself at `interval_ms`, so
        // `SEED_DEMO_CONCURRENCY` workers running at once is roughly
        // `concurrency` times the request rate one alone would produce -
        // turn this up (or shrink SEED_DEMO_INTERVAL_MS) to put real
        // load through the REST surface for a dashboard to show moving.
        let concurrency: usize = std::env::var("SEED_DEMO_CONCURRENCY")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|&n| n > 0)
            .unwrap_or(1);
        println!(
            "\nSEED_DEMO_TRAFFIC=1: starting {concurrency} fake-traffic worker(s), each every \
             {interval_ms}ms, against http://localhost:{port} (see src/demo_seed.rs)"
        );
        let seed_base_url = format!("http://localhost:{port}");
        let seed_tokens = command_tokens.clone();
        tokio::spawn(async move {
            sign_up_demo_companies(&seed_base_url, &seed_tokens).await;
            for worker_index in 0..concurrency {
                let base_url = seed_base_url.clone();
                let tokens = seed_tokens.clone();
                // Staggers each worker's first tick evenly across one
                // interval, rather than every worker firing in lockstep
                // every `interval_ms` - a smoother, more realistic load
                // shape (one steady stream) than `concurrency` synchronised
                // bursts would be.
                let stagger = Duration::from_millis(interval_ms * worker_index as u64 / concurrency as u64);
                tokio::spawn(async move {
                    tokio::time::sleep(stagger).await;
                    run_demo_seed_loop(worker_index, base_url, tokens, Duration::from_millis(interval_ms)).await;
                });
            }
        });
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    if let Some(telemetry) = telemetry {
        telemetry.shutdown();
    }

    Ok(())
}

// --- optional fake traffic (SEED_DEMO_TRAFFIC=1) - see this file's own
// module doc comment; the actual decisions are src/demo_seed.rs's own
// pure `next_action`/`apply_outcome`, this is just the I/O loop around
// them ---

/// Signs up `demo_seed::DEMO_COMPANIES` once - tolerating
/// `already_signed_up` (the same idempotent treatment this file's own
/// demo-Role seeding above already gets), which now matters twice over:
/// a repeat run of `server` itself, and every `SEED_DEMO_CONCURRENCY`
/// worker beyond the first racing to sign up the same three companies
/// concurrently the moment this loop starts (harmless, since it's just
/// this - see `run_demo_seed_loop`'s own doc comment for why per-worker
/// *ticket* state doesn't get the same "just let it collide" treatment).
async fn sign_up_demo_companies(base_url: &str, command_tokens: &HashMap<&'static str, String>) {
    let client = reqwest::Client::new();
    for company_id in DEMO_COMPANIES {
        let payload = serde_json::json!({
            "company_id": company_id,
            "name": company_display_name(company_id),
            "contact_email": format!("hello@{company_id}.example"),
        });
        match trigger_command(&client, base_url, &command_tokens["SignUpCompany"], payload).await {
            Ok(_) => tracing::info!(company_id = %company_id, "demo-seed: signed up fake company"),
            Err(e) => {
                tracing::warn!(error = %e, company_id = %company_id, "demo-seed: failed to sign up fake company")
            }
        }
    }
}

/// One `SEED_DEMO_CONCURRENCY` worker: fires one fake command every
/// `interval` forever, against its own independent `demo_seed::SeedState`
/// (`worker_index` becomes that state's own ticket_id prefix - see
/// `SeedState::new`'s own doc comment for why two workers must never
/// share one). Never returns; `tokio::spawn` just leaks it for the
/// process's lifetime, the same "runs until killed" treatment
/// `alerter`/`scheduler` already get as whole separate processes.
async fn run_demo_seed_loop(
    worker_index: usize,
    base_url: String,
    command_tokens: HashMap<&'static str, String>,
    interval: Duration,
) {
    let client = reqwest::Client::new();
    let mut state = SeedState::new(
        DEMO_COMPANIES.iter().map(|s| s.to_string()).collect(),
        format!("seed-ticket-w{worker_index}"),
    );
    let mut rng = Rng::from_clock_and_worker(worker_index);

    loop {
        tokio::time::sleep(interval).await;
        let action = demo_seed::next_action(&state, &mut rng);
        let (command_type_name, payload) = command_and_payload(&action);
        match trigger_command(&client, &base_url, &command_tokens[command_type_name], payload)
            .await
        {
            Ok(accepted) => {
                demo_seed::apply_outcome(&mut state, &action, accepted);
                tracing::info!(?action, accepted, "demo-seed: fired fake command");
            }
            Err(e) => tracing::warn!(error = %e, ?action, "demo-seed: request failed"),
        }
    }
}

/// The REST `CommandType` name (a key into `command_tokens`) and JSON
/// payload for one `SeedAction`.
fn command_and_payload(action: &SeedAction) -> (&'static str, serde_json::Value) {
    match action {
        SeedAction::CreateTicket {
            ticket_id,
            company_id,
            requester_id,
            title,
            description,
            priority,
        } => (
            "CreateTicket",
            serde_json::json!({
                "ticket_id": ticket_id,
                "company_id": company_id,
                "requester_id": requester_id,
                "logged_by_staff_id": null,
                "title": title,
                "description": description,
                "priority": priority,
            }),
        ),
        SeedAction::AssignTicket { ticket_id, staff_id } => (
            "AssignTicket",
            serde_json::json!({ "ticket_id": ticket_id, "staff_id": staff_id }),
        ),
        SeedAction::ResolveTicket { ticket_id } => (
            "ResolveTicket",
            serde_json::json!({ "ticket_id": ticket_id }),
        ),
        SeedAction::RequestInfo {
            ticket_id,
            staff_id,
            message,
        } => (
            "RequestInfoFromCustomer",
            serde_json::json!({ "ticket_id": ticket_id, "staff_id": staff_id, "message": message }),
        ),
        SeedAction::CustomerResponds {
            ticket_id,
            requester_id,
            message,
        } => (
            "CustomerRespondsToTicket",
            serde_json::json!({ "ticket_id": ticket_id, "requester_id": requester_id, "message": message }),
        ),
        SeedAction::ReopenTicket { ticket_id } => (
            "ReopenTicket",
            serde_json::json!({ "ticket_id": ticket_id }),
        ),
        SeedAction::AddInternalNote {
            ticket_id,
            staff_id,
            note,
        } => (
            "AddInternalNote",
            serde_json::json!({ "ticket_id": ticket_id, "staff_id": staff_id, "note": note }),
        ),
        SeedAction::RateTicket {
            ticket_id,
            rating,
            comment,
        } => (
            "RateTicket",
            serde_json::json!({ "ticket_id": ticket_id, "rating": rating, "comment": comment }),
        ),
        SeedAction::MergeTickets {
            primary_ticket_id,
            duplicate_ticket_id,
        } => (
            "MergeTickets",
            serde_json::json!({
                "primary_ticket_id": primary_ticket_id,
                "duplicate_ticket_id": duplicate_ticket_id,
            }),
        ),
    }
}

/// POSTs one command, the same shape every other REST client of this
/// crate uses (`{"payload": ...}`, a `CommandToken` bearer credential) -
/// returns `CommandTriggerResponse.accepted` (`tests/support/mod.rs`'s
/// own `accepted()` reads the identical field). A rejected command is
/// still a normal 200 (business rejections render as 200 - see
/// `skilj-rest`'s own REQUEST_DURATION doc comment); `error_for_status`
/// below only ever fires on a genuine transport/auth-level failure.
async fn trigger_command(
    client: &reqwest::Client,
    base_url: &str,
    credential: &str,
    payload: serde_json::Value,
) -> Result<bool, reqwest::Error> {
    let response = client
        .post(format!("{base_url}/v1/commands/trigger"))
        .header("authorization", format!("Bearer {credential}"))
        .json(&serde_json::json!({ "payload": payload }))
        .send()
        .await?
        .error_for_status()?;
    let body: serde_json::Value = response.json().await?;
    Ok(body["accepted"].as_bool().unwrap_or(false))
}

/// `"wonka-industries"` -> `"Wonka Industries"` - purely cosmetic, for
/// the fake `SignUpCompany.name` this loop mints once at startup.
fn company_display_name(company_id: &str) -> String {
    company_id
        .split('-')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().collect::<String>() + chars.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
