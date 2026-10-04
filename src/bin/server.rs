//! A real, runnable server for `skilj_helpdesk::helpdesk` - `cargo run
//! --bin server`. Not a test: boots an actual `axum` process serving
//! both REST and GraphQL, prints every credential `alerter`/
//! `engagement-watcher` need as ready-to-export env vars, then serves
//! until killed. Modelled
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
//! changes. When `TICKET_ROUTING=tenant` is also set, each worker routes
//! ticket commands to the company's own tenant (discovered from
//! `CompanyTenantProvisioned` and per-tenant tokens minted via GraphQL),
//! so the demo exercises the same per-tenant path real traffic takes.
//!
//! Needs `DATABASE_URL` pointing at a real Postgres (`PORT` optionally
//! overrides the default `8080`; `BIND_ADDR` the default `127.0.0.1` -
//! set it to `0.0.0.0` to serve beyond this machine, e.g. in a
//! container). Every run is safe to repeat against
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
//! flow is what actually needs `OIDC_ISSUER_URL` set. That shortcut's
//! signing key is generated fresh on every start, so a JWT printed by
//! one run stops verifying once the server restarts.
//!
//! The two demo identities `frontend/`'s login page offers
//! (`customer@acme.example` / `customer-demo-pw`, `lead@acme.example` /
//! `staff-demo-pw` - see `dex/config.yaml`) get their own seeded `Role`s
//! here, at the real `sub` Dex's local-password connector actually
//! issues for each (captured once via a real login flow against that
//! exact config - `DEMO_CUSTOMER_SUB`/`DEMO_STAFF_LEAD_SUB` below -
//! deterministic for that config, not something computed at runtime).

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;
use chrono::Utc;
use jsonwebtoken::{EncodingKey, Header};
use opentelemetry::metrics::Counter;
use opentelemetry::KeyValue;
use rsa::pkcs1::EncodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::RsaPrivateKey;
use serde_json::json;
use skilj::{IdpConfig, SigningAlgorithm, Skilj};
use skilj_core::access_control::{self, AccessLevel, Role, RoleAccessMapping, RoleStatus};
use skilj_core::bootstrap::ContextCreator;
use skilj_core::db;
use skilj_core::event_store::{BoundedContext, BoundedContextStatus};
use skilj_core::shared::{generate_token_id, generate_token_secret};
use skilj_helpdesk::demo_seed::{self, Rng, SeedAction, SeedState, DEMO_COMPANIES};
use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use skilj_helpdesk::routing::RoutingMode;
use skilj_helpdesk::routing_guard::{self, GuardState};
use std::collections::HashMap;
use std::sync::{Arc, LazyLock};
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

const TEST_ISSUER: &str = "https://idp.example.test/";
// skilj 0.0.9 requires an explicit `aud` on every verified JWT
// (IdpConfig::new's `audience`, docs/architecture.md §81) - a token
// issued to some *other* application at the same IdP must not be
// accepted here as its user. The local JWKS shortcut's own signed JWTs
// (sign_jwt below) therefore carry one, exactly like the real Dex-issued
// ones this falls back from do.
const TEST_AUDIENCE: &str = "skilj-helpdesk-test-client";
// The Dex client id `dex/config.yaml`'s own staticClients registers -
// the `aud` Dex puts in every token it issues for this deployment
// (skilj's own §81: "usually its client id there"). Changing that id
// there means changing it here too.
const DEX_AUDIENCE: &str = "skilj-helpdesk-frontend";

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

// --- local JWKS/IdP shortcut - see this file's own doc comment above ---
//
// When `OIDC_ISSUER_URL` is unset, this binary falls back to its own
// self-signed JWKS/JWT shortcut: it spins up a tiny local HTTP server that
// serves a freshly generated RSA public key, and `GeneratedKeyPair::sign_jwt`
// below signs demo JWTs with the matching private key. The key pair is
// generated at startup from a CSPRNG so no secret is ever committed to source
// — each `cargo run` gets a new, unpredictable key.

/// A freshly generated RSA keypair for the local JWKS/JWT shortcut.
/// The private key PEM is used for signing; the JWK representation is served
/// via the local `/jwks.json` endpoint. Generated once per process lifetime.
struct GeneratedKeyPair {
    private_key_pem: String,
    kid: String,
    jwks: serde_json::Value,
}

impl GeneratedKeyPair {
    fn generate() -> Self {
        let mut rng = rsa::rand_core::OsRng;
        let priv_key = RsaPrivateKey::new(&mut rng, 2048)
            .expect("RSA 2048 key generation from the OS RNG should succeed");

        let priv_pem = priv_key
            .to_pkcs1_pem(rsa::pkcs1::LineEnding::LF)
            .expect("PEM encoding a generated RSA key should succeed")
            .to_string();

        let pub_key = priv_key.to_public_key();
        let n_bytes = pub_key.n().to_bytes_be();
        let e_bytes = pub_key.e().to_bytes_be();
        let n_b64 = URL_SAFE_NO_PAD.encode(&n_bytes);
        let e_b64 = URL_SAFE_NO_PAD.encode(&e_bytes);

        let kid = "skilj-helpdesk-rs256-01".to_string();
        let jwks = json!({
            "keys": [{
                "kty": "RSA",
                "use": "sig",
                "alg": "RS256",
                "kid": &kid,
                "n": &n_b64,
                "e": &e_b64,
            }]
        });

        GeneratedKeyPair {
            private_key_pem: priv_pem,
            kid,
            jwks,
        }
    }

    fn sign_jwt(&self, subject: &str) -> String {
        let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
        header.kid = Some(self.kid.clone());
        let claims = json!({
            "sub": subject,
            "iss": TEST_ISSUER,
            "aud": TEST_AUDIENCE,
            "exp": (Utc::now() + chrono::Duration::hours(1)).timestamp(),
        });
        let key = EncodingKey::from_rsa_pem(self.private_key_pem.as_bytes())
            .expect("the generated private key PEM is well-formed");
        jsonwebtoken::encode(&header, &claims, &key).expect("signing a well-formed JWT never fails")
    }
}

async fn serve_local_jwks(jwks: serde_json::Value) -> String {
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

/// CORS policy for the demo server. By default, only the frontend's own dev
/// server origin is allowed (`http://localhost:8081` and `http://127.0.0.1:8081`).
/// `CORS_ALLOWED_ORIGINS` can be set to a comma-separated list to override.
/// This is bearer-token-based (no cookies), so permissive CORS doesn't open
/// a CSRF hole — but it does let any compromised page read the token out of
/// `localStorage` and make authenticated requests. A real deployment should
/// restrict this; the default here already prevents the worst case.
fn cors_layer() -> tower_http::cors::CorsLayer {
    let allowed: Vec<axum::http::HeaderValue> = match std::env::var("CORS_ALLOWED_ORIGINS") {
        Ok(s) if !s.is_empty() => s.split(',').filter_map(|o| o.trim().parse().ok()).collect(),
        _ => ["http://localhost:8081", "http://127.0.0.1:8081"]
            .iter()
            .filter_map(|o| o.parse().ok())
            .collect(),
    };
    tower_http::cors::CorsLayer::new()
        .allow_origin(allowed)
        .allow_methods([
            axum::http::Method::GET,
            axum::http::Method::POST,
            axum::http::Method::OPTIONS,
        ])
        .allow_headers([
            axum::http::header::AUTHORIZATION,
            axum::http::header::CONTENT_TYPE,
            axum::http::header::ACCEPT,
        ])
        .allow_credentials(false)
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
    "RecordCompanyTenant",
];

/// `src/bin/provisioner.rs`'s own event type - one `CompanySignedUp` per
/// company, reacted to by calling `createBoundedContextFromTemplate`
/// and reporting the result back via `RecordCompanyTenant` (in
/// `COMMAND_TYPES` above). Its own token set, same "each consumer gets
/// its own cursor" reasoning `ALERTER_EVENT_TYPES`'s own doc comment
/// gives - nothing else currently reads `CompanySignedUp`.
const PROVISIONER_EVENT_TYPES: &[&str] = &["CompanySignedUp"];

/// `src/bin/alerter.rs`'s own event types - the trial-conversion/
/// auto-close rules used to read `TicketResolved`/`TicketReopened`/
/// `TicketClosed` too, via `src/bin/scheduler.rs`'s own separate token
/// set (`skilj-rest`'s `mode=auto` cursor is tracked per-token, so two
/// independent readers of the same type each needed their own token to
/// avoid stealing each other's cursor position) - that binary's gone
/// now, replaced by skilj's native per-entity deadline mechanism
/// (`helpdesk.rs`'s own `ScheduleTicketAutoClose` etc.), which reads
/// events through its own internal poller, not a minted `EventReadToken`
/// at all.
///
/// `CompanyTenantProvisioned` is in the same set so the alerter can
/// discover tenants when `TICKET_ROUTING=tenant` is on - see that
/// config's own doc comment in `src/bin/alerter.rs`. It is *not* one of
/// the per-tenant event types the alerter mints (the alerter's own
/// `ALERTER_EVENT_TYPES` constant in that binary lists only the six
/// ticket types); discovery stays on the shared context only.
const ALERTER_EVENT_TYPES: &[&str] = &[
    "TicketCreated",
    "TicketResolved",
    "TicketReopened",
    "TicketClosed",
    "TicketEscalated",
    "TicketsMerged",
    "CompanyTenantProvisioned",
];

/// `src/bin/lifecycle-replicator.rs`'s own event types - the three
/// company lifecycle facts it mirrors into each company's tenant, so a
/// Ticket command routed there passes the same `company_status` guard it
/// would pass in the shared context (see `helpdesk.rs`'s
/// `CompanyLifecycleMirrored` doc comment for why that mirror is needed
/// at all). Its own token set, one per event type, for the same
/// "each consumer gets its own cursor" reason `PROVISIONER_EVENT_TYPES`
/// above gives - three independent feeds, three independent read
/// positions, and no ordering *between* them, which is exactly why
/// `RecordTenantLifecycle`'s own guard has to refuse backwards
/// transitions rather than relying on arrival order.
const REPLICATOR_EVENT_TYPES: &[&str] = &["CompanySignedUp", "CompanyActivated", "CompanyExpired"];

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

/// Ensures `bounded_context_name`'s own `BoundedContext` row exists
/// (same idempotent "insert only if missing" shape `helpdesk`'s own
/// bootstrap in `main()` uses), then grants `role` an unrestricted
/// `Admin` mapping onto it. The generalised, reusable counterpart to
/// that inline `helpdesk` bootstrap - extracted once a second and third
/// bounded context (`activity`, `marketing`) needed the identical two
/// steps, rather than a third copy-pasted block.
async fn ensure_bounded_context_and_grant(
    pool: &db::Pool,
    role: &Role,
    bounded_context_name: &str,
) -> Result<RoleAccessMapping, Box<dyn std::error::Error>> {
    if db::get_bounded_context(pool, bounded_context_name)
        .await?
        .is_none()
    {
        db::insert_bounded_context(
            pool,
            &BoundedContext {
                name: bounded_context_name.to_string(),
                status: BoundedContextStatus::Active,
                created_at: Utc::now(),
                created_by: ContextCreator::SystemCreator,
                template: None,
            },
        )
        .await?;
        println!("server: created bounded context {bounded_context_name:?}");
    }
    let bounded_context = db::get_bounded_context(pool, bounded_context_name)
        .await?
        .expect("just ensured it exists above");
    let mapping = RoleAccessMapping {
        role: role.clone(),
        bounded_context,
        level: AccessLevel::Admin,
        can_read_sensitive: false,
        // Unrestricted - same reasoning as the helpdesk bootstrap
        // mapping's own `None` scope in `main()`.
        scope: None,
        status: RoleStatus::Active,
        created_at: Utc::now(),
        revoked_at: None,
    };
    db::insert_role_access_mapping(pool, &mapping).await?;
    Ok(mapping)
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
    bounded_context: &str,
    event_type_names: &[&'static str],
) -> Result<HashMap<&'static str, String>, Box<dyn std::error::Error>> {
    let mut tokens = HashMap::new();
    for event_type_name in event_type_names {
        let event_type = db::get_event_type(pool, bounded_context, event_type_name)
            .await?
            .unwrap_or_else(|| {
                panic!("{bounded_context}/{event_type_name} should have just been registered")
            });
        let token = access_control::create_event_read_token(
            mapping,
            &event_type,
            generate_token_id(),
            generate_token_secret(),
            // Unrestricted: alerter is an operational consumer watching
            // every company's own events by design (paging a lead -
            // not "acting as" any one company) - the same reasoning
            // `mapping`'s own `scope` below gets for the same reason.
            None,
            // `None` - `EventReadStartPosition::Beginning`, matching
            // every one of these tokens' prior behaviour from before
            // this parameter existed (skilj 0.0.5): every consumer here
            // wants its own tenure to read the whole history, not just
            // what's new from the moment it starts.
            None,
            // `start_at_sequence`/`start_at_time` (skilj 0.0.6): both
            // `None` - only meaningful alongside `AtSequence`/`AtTime`
            // above, neither of which this call ever passes.
            None,
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

/// Mints one fresh `CommandToken` per `command_type_name` - the same
/// shape `mint_event_tokens` above is, and the generalised counterpart
/// to what used to be an inline loop in `main()` scoped to `helpdesk`
/// alone (`COMMAND_TYPES`, below), now shared with `engagement-watcher`'s
/// own `RecordEngagementDecline` token against `activity`.
async fn mint_command_tokens(
    pool: &db::Pool,
    mapping: &RoleAccessMapping,
    bounded_context: &str,
    command_type_names: &[&'static str],
) -> Result<HashMap<&'static str, String>, Box<dyn std::error::Error>> {
    let mut tokens = HashMap::new();
    for command_type_name in command_type_names {
        let command_type = db::get_command_type(pool, bounded_context, command_type_name)
            .await?
            .unwrap_or_else(|| {
                panic!("{bounded_context}/{command_type_name} should have just been registered")
            });
        let token = access_control::create_command_token(
            mapping,
            &command_type,
            generate_token_id(),
            generate_token_secret(),
            // Unrestricted - same reasoning as mint_event_tokens' own
            // `None` scope above.
            None,
            Utc::now(),
        )?;
        db::insert_command_token(pool, &token).await?;
        let credential = format!("{}.{}", token.id, token.secret);
        println!("  {command_type_name}: {credential}");
        tokens.insert(*command_type_name, credential);
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
/// One pass of the tenant-access reconciler, on an interval.
///
/// Runs unconditionally, including while the cutover is off, because it
/// is pure preparation: it makes a tenant's grants match the shared
/// context whether or not anything is being routed there yet. Turning it
/// off would only save a couple of index reads per company, and would
/// mean the cutover itself starts from a cold cache - the one moment
/// where a company's first routed command could arrive before its
/// reconciler pass.
///
/// Interval is short-ish (5s, same as the CSAT loop) because the failure
/// this guards against is a *rejected customer command*: a Role granted
/// after provisioning is denied at the tenant until the next pass, and
/// the window is user-visible. The cost when everything is already in
/// sync is two indexed reads and a comparison per company per tick.
async fn run_tenant_access_reconciler(pool: skilj_core::db::Pool, ops_role: Role) {
    const POLL_INTERVAL: Duration = Duration::from_secs(5);
    loop {
        skilj_helpdesk::tenant_access::reconcile_all_tenants(&pool, &ops_role).await;
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

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

/// Phase 4 multi-tenant CSAT config: when `TICKET_ROUTING=tenant` is set,
/// `TicketRated` events live in per-company tenant contexts, not just the
/// shared `helpdesk` one. This struct carries the extra wiring the CSAT loop
/// needs to discover and poll those tenant feeds:
///
/// - `company_tenant_provisioned_token`: an `EventReadToken` for
///   `CompanyTenantProvisioned` on the shared context, so the loop can
///   learn which tenants exist (the same approach `src/bin/alerter.rs`
///   uses).
/// - `superadmin_subject`: the bootstrap admin Role's `external_subject`,
///   which this binary signs its own short-lived JWT for to call
///   `createEventReadToken` on each tenant (the same identity
///   `src/bin/provisioner.rs` used to grant Admin on each tenant via
///   `createBoundedContextFromTemplate`'s `roleId`).
struct MultiTenantCsatConfig {
    company_tenant_provisioned_token: String,
    superadmin_subject: String,
}

/// Mints a per-tenant `TicketRated` `EventReadToken` via skilj's own
/// `createEventReadToken` GraphQL mutation, signed as the superadmin
/// identity. Mirrors `src/bin/alerter.rs`'s own
/// `mint_graphql_event_token` (deliberately duplicated across binaries,
/// same convention the test key material follows) - only the event type
/// differs (`TicketRated` instead of the six alerter event types).
async fn mint_tenant_ticket_rated_token(
    client: &reqwest::Client,
    base_url: &str,
    jwt: &str,
    tenant_name: &str,
) -> Result<String, String> {
    let query = r#"
        mutation MintTicketRatedToken($bc: String!, $eventTypeName: String!) {
            createEventReadToken(
                boundedContext: $bc
                eventTypeName: $eventTypeName
            ) {
                id
                secret
            }
        }
    "#;
    let response = client
        .post(format!("{base_url}/graphql"))
        .bearer_auth(jwt)
        .json(&serde_json::json!({
            "query": query,
            "variables": {
                "bc": tenant_name,
                "eventTypeName": "TicketRated",
            },
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if let Some(errors) = body.get("errors").and_then(|e| e.as_array()) {
        if !errors.is_empty() {
            return Err(format!("createEventReadToken errors: {errors:?}"));
        }
    }
    let id = body["data"]["createEventReadToken"]["id"]
        .as_str()
        .ok_or_else(|| format!("no id in response: {body:?}"))?;
    let secret = body["data"]["createEventReadToken"]["secret"]
        .as_str()
        .ok_or_else(|| format!("no secret in response: {body:?}"))?;
    Ok(format!("{id}.{secret}"))
}

/// Phase 4 multi-tenant helper: mints a per-tenant REST `CommandToken`
/// via skilj's own `createCommandToken` GraphQL mutation, signed as the
/// superadmin identity. Mirrors `src/bin/alerter.rs`'s own
/// `mint_graphql_command_token` (deliberately duplicated across binaries,
/// same convention the test key material follows) - the server's own
/// `mint_command_tokens` works against the DB directly, but that path needs
/// a `RoleAccessMapping` row that only exists once the tenant is
/// provisioned; this GraphQL route resolves that dynamically at runtime.
async fn mint_graphql_command_token(
    client: &reqwest::Client,
    base_url: &str,
    jwt: &str,
    bounded_context: &str,
    command_type_name: &str,
) -> Result<String, String> {
    let query = r#"
        mutation MintCommandToken($bc: String!, $commandType: String!) {
            createCommandToken(
                boundedContext: $bc
                commandTypeName: $commandType
            ) {
                id
                secret
            }
        }
    "#;
    let response = client
        .post(format!("{base_url}/graphql"))
        .bearer_auth(jwt)
        .json(&serde_json::json!({
            "query": query,
            "variables": {
                "bc": bounded_context,
                "commandType": command_type_name,
            },
        }))
        .send()
        .await
        .map_err(|e| e.to_string())?
        .error_for_status()
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if let Some(errors) = body.get("errors").and_then(|e| e.as_array()) {
        if !errors.is_empty() {
            return Err(format!("createCommandToken errors: {errors:?}"));
        }
    }
    let id = body["data"]["createCommandToken"]["id"]
        .as_str()
        .ok_or_else(|| format!("no id in response: {body:?}"))?;
    let secret = body["data"]["createCommandToken"]["secret"]
        .as_str()
        .ok_or_else(|| format!("no secret in response: {body:?}"))?;
    Ok(format!("{id}.{secret}"))
}

/// Discovers tenants from `CompanyTenantProvisioned` events on the shared
/// context and returns the tenant name, using the same `mode=auto` consume
/// shape `src/bin/alerter.rs`'s own `consume` helper uses.
async fn discover_tenants(
    client: &reqwest::Client,
    base_url: &str,
    company_tenant_provisioned_token: &str,
) -> Result<Vec<String>, reqwest::Error> {
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
        .bearer_auth(company_tenant_provisioned_token)
        .send()
        .await?
        .error_for_status()?;
    let body: ConsumeResponse = response.json().await?;
    Ok(body
        .events
        .into_iter()
        .filter_map(|e| e.payload["tenant_name"].as_str().map(str::to_string))
        .collect())
}

/// Phase 4 multi-tenant CSAT loop. In addition to the shared-context
/// `TicketRated` polling `run_csat_metrics_loop` already does, this discovers
/// per-company tenants and polls each tenant's own `TicketRated` feed - so
/// a `RateTicket` command routed into a tenant (the Phase 4 cutover) still
/// records its rating as a metric, not just in the shared context.
struct MultiTenantCsatLoop {
    base_url: String,
    key_pair: Arc<GeneratedKeyPair>,
    superadmin_subject: String,
    company_tenant_provisioned_token: String,
    /// tenant_name -> TicketRated EventReadToken
    tenant_tokens: HashMap<String, String>,
}

impl MultiTenantCsatLoop {
    fn new(
        config: &MultiTenantCsatConfig,
        base_url: &str,
        key_pair: Arc<GeneratedKeyPair>,
    ) -> Self {
        MultiTenantCsatLoop {
            base_url: base_url.to_string(),
            key_pair,
            superadmin_subject: config.superadmin_subject.clone(),
            company_tenant_provisioned_token: config.company_tenant_provisioned_token.clone(),
            tenant_tokens: HashMap::new(),
        }
    }

    /// One tick: discover new tenants, mint tokens for any unseen ones,
    /// then consume `TicketRated` from each tenant's own feed.
    async fn tick(&mut self, client: &reqwest::Client) -> Result<(), reqwest::Error> {
        // Discover new tenants from the shared context.
        let discovered = discover_tenants(
            client,
            &self.base_url,
            &self.company_tenant_provisioned_token,
        )
        .await?;
        let jwt = self.key_pair.sign_jwt(&self.superadmin_subject);
        for tenant_name in discovered {
            if !self.tenant_tokens.contains_key(&tenant_name) {
                match mint_tenant_ticket_rated_token(client, &self.base_url, &jwt, &tenant_name)
                    .await
                {
                    Ok(token) => {
                        println!("csat metrics: minted TicketRated token for tenant {tenant_name}");
                        self.tenant_tokens.insert(tenant_name.clone(), token);
                    }
                    Err(e) => eprintln!(
                        "csat metrics: minting TicketRated for tenant {tenant_name} failed: {e}"
                    ),
                }
            }
        }

        // Consume TicketRated from each tenant's own feed.
        for (tenant_name, token) in &self.tenant_tokens {
            match consume_ticket_rated(client, &self.base_url, token).await {
                Ok(ratings) => {
                    for rating in ratings {
                        TICKET_RATINGS.add(
                            1,
                            &[opentelemetry::KeyValue::new("rating", i64::from(rating))],
                        );
                    }
                }
                Err(e) => {
                    eprintln!("csat metrics: poll failed for tenant {tenant_name}: {e}");
                }
            }
        }

        Ok(())
    }
}

/// `rating` field this loop needs - the identical shape
/// `src/bin/alerter.rs`'s own `consume` has, duplicated rather than
/// shared for the same "no common library boundary worth introducing
/// for one helper" reason that file's own doc comment gives.
async fn consume_ticket_rated(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<u8>, reqwest::Error> {
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

/// Phase 4 multi-tenant CSAT loop: discovers tenants from
/// `CompanyTenantProvisioned` events, mints a per-tenant `TicketRated`
/// `EventReadToken` via GraphQL, and polls that tenant's own feed - so a
/// `RateTicket` command routed into a tenant (Phase 4 cutover) still records
/// its rating as a metric, not just in the shared context.
async fn run_multi_tenant_csat_loop(
    client: &reqwest::Client,
    base_url: &str,
    config: MultiTenantCsatConfig,
    key_pair: Arc<GeneratedKeyPair>,
) {
    const POLL_INTERVAL: Duration = Duration::from_secs(5);
    let mut loop_state = MultiTenantCsatLoop::new(&config, base_url, key_pair);
    loop {
        if let Err(e) = loop_state.tick(client).await {
            eprintln!("csat metrics: multi-tenant poll failed, will retry: {e}");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

/// Phase 4 multi-tenant demo seed tokens: discovers per-company tenants
/// (consuming `CompanyTenantProvisioned` from the shared context) and mints
/// per-tenant REST `createCommandToken` values for each tenant, so the demo
/// seed loop can route ticket commands into the right tenant when
/// `TICKET_ROUTING=tenant`. Follows the same lazy-discover-and-cache shape
/// `src/bin/alerter.rs`'s own `TenantTokenCache` uses (deliberately
/// duplicated across binaries, same convention the test key material
/// follows) - REST routes by token, so each tenant needs its own token set,
/// not just the shared one.
struct DemoSeedTokenCache {
    base_url: String,
    key_pair: Arc<GeneratedKeyPair>,
    superadmin_subject: String,
    company_tenant_provisioned_token: String,
    /// tenant (company_id -> tenant_name) mapping, refreshed each discover
    company_to_tenant: HashMap<String, String>,
    /// (company_id, command_type_name) -> REST command token, for ticket
    /// commands that must hit the tenant context
    command_tokens: HashMap<(String, &'static str), String>,
}

/// The ticket command types the demo seed loop fires - all of which must
/// route to the tenant, not the shared context, when `TICKET_ROUTING=tenant`.
/// Used to pre-mint per-tenant tokens for each discovered tenant in one
/// batch (one GraphQL `createCommandToken` mutation per type per tenant),
/// rather than minting them one at a time on first use inside the seed loop.
const DEMO_TENANT_COMMAND_TYPES: &[&str] = &[
    "CreateTicket",
    "AssignTicket",
    "ResolveTicket",
    "ReopenTicket",
    "RequestInfoFromCustomer",
    "CustomerRespondsToTicket",
    "EscalateTicket",
    "MergeTickets",
    "RateTicket",
    "AddInternalNote",
];

impl DemoSeedTokenCache {
    fn new(
        base_url: &str,
        key_pair: Arc<GeneratedKeyPair>,
        superadmin_subject: &str,
        company_tenant_provisioned_token: &str,
    ) -> Self {
        DemoSeedTokenCache {
            base_url: base_url.to_string(),
            key_pair,
            superadmin_subject: superadmin_subject.to_string(),
            company_tenant_provisioned_token: company_tenant_provisioned_token.to_string(),
            company_to_tenant: HashMap::new(),
            command_tokens: HashMap::new(),
        }
    }

    /// Refresh the company -> tenant_name map from `CompanyTenantProvisioned`,
    /// and pre-mint per-tenant command tokens for any newly discovered
    /// tenant (so the seed loop's first `CreateTicket` for that company
    /// doesn't block on a GraphQL round-trip per command type).
    async fn refresh_tenant_map(&mut self, client: &reqwest::Client) {
        // Reuses the same `CompanyTenantProvisioned` consume that the CSAT
        // loop above does. The payload carries `company_id` -> `tenant_name`.
        #[derive(serde::Deserialize)]
        struct EventDto {
            payload: serde_json::Value,
        }
        #[derive(serde::Deserialize)]
        struct ConsumeResponse {
            events: Vec<EventDto>,
        }
        let response = match client
            .get(format!("{}/v1/events/consume", self.base_url))
            .query(&[("mode", "auto")])
            .bearer_auth(&self.company_tenant_provisioned_token)
            .send()
            .await
        {
            Ok(r) => r,
            Err(e) => {
                eprintln!("demo-seed: tenant discovery failed, will retry: {e}");
                return;
            }
        };
        let body: ConsumeResponse = match response.json().await {
            Ok(b) => b,
            Err(e) => {
                eprintln!("demo-seed: tenant discovery response decode failed: {e}");
                return;
            }
        };
        let jwt = self.key_pair.sign_jwt(&self.superadmin_subject);
        for event in body.events {
            if let (Some(cid), Some(tn)) = (
                event.payload["company_id"].as_str(),
                event.payload["tenant_name"].as_str(),
            ) {
                // Skip if already known - avoids re-minting tokens for
                // tenants discovered in a previous tick.
                if self.company_to_tenant.contains_key(cid) {
                    continue;
                }
                self.company_to_tenant
                    .insert(cid.to_string(), tn.to_string());
                // Pre-mint all demo tenant command tokens for this tenant
                // in one batch, so the seed loop can fire immediately.
                for &command_type in DEMO_TENANT_COMMAND_TYPES {
                    match mint_graphql_command_token(client, &self.base_url, &jwt, tn, command_type)
                        .await
                    {
                        Ok(token) => {
                            self.command_tokens
                                .insert((cid.to_string(), command_type), token);
                        }
                        Err(e) => eprintln!(
                            "demo-seed: pre-minting {command_type} for tenant {tn} failed: {e}"
                        ),
                    }
                }
            }
        }
    }

    /// Returns a REST command token for `command_type` in `company_id`'s
    /// tenant, minting it on the first miss. Falls back to `fallback` when
    /// the company has no tenant yet (or minting fails) - which keeps the
    /// seed loop running against the shared context until provisioning
    /// catches up, rather than blocking on tenant existence at startup.
    async fn get_or_mint(
        &mut self,
        client: &reqwest::Client,
        company_id: &str,
        command_type: &'static str,
        fallback: &str,
    ) -> String {
        // Refresh if we don't know this company's tenant yet.
        if !self.company_to_tenant.contains_key(company_id) {
            self.refresh_tenant_map(client).await;
        }
        let tenant_name = self.company_to_tenant.get(company_id);
        if let Some(tenant_name) = tenant_name {
            let key = (company_id.to_string(), command_type);
            if let Some(token) = self.command_tokens.get(&key) {
                return token.clone();
            }
            let jwt = self.key_pair.sign_jwt(&self.superadmin_subject);
            match mint_graphql_command_token(
                client,
                &self.base_url,
                &jwt,
                tenant_name,
                command_type,
            )
            .await
            {
                Ok(token) => {
                    self.command_tokens.insert(key, token.clone());
                    token
                }
                Err(e) => {
                    eprintln!(
                        "demo-seed: minting {command_type} for tenant {tenant_name} failed: {e}"
                    );
                    fallback.to_string()
                }
            }
        } else {
            fallback.to_string()
        }
    }
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
    let bind_addr: std::net::IpAddr = match std::env::var("BIND_ADDR") {
        Ok(addr) => addr
            .parse()
            .map_err(|e| format!("BIND_ADDR {addr:?} is not an IP address: {e}"))?,
        Err(_) => std::net::Ipv4Addr::LOCALHOST.into(),
    };

    // db::connect's bare default (sqlx's own PgPoolOptions::new(), a
    // 10-connection cap) turned out to be the actual ceiling on this
    // server's throughput, not anything in the app's own logic - see
    // docs/load-test-report-2026-09-17.md. DATABASE_MAX_CONNECTIONS lets
    // that be sized for real load instead of silently inheriting
    // whatever sqlx ships with. 90, not just "well above 10": this same
    // pool is also shared by every background loop skilj spawns
    // (cross_context_route_tick, async_projection_tick, scheduler_tick)
    // plus `/v1/events/consume` (which holds its connection for a whole
    // open transaction, not just one query - see skilj-rest's own
    // get_events_consume doc comment on why a concurrent call for the
    // same token genuinely blocks there) - all of that competes with
    // foreground request traffic for the same budget, so sizing this to
    // just the expected foreground concurrency undercounts real demand.
    // 90 leaves headroom under Postgres's own default server-side
    // max_connections (100) for `alerter`/`engagement-watcher`/manual
    // `psql` alongside this pool.
    let db_max_connections: u32 = std::env::var("DATABASE_MAX_CONNECTIONS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(90);
    let pool = db::connect_with(
        &database_url,
        db::PgPoolOptions::new().max_connections(db_max_connections),
    )
    .await?;
    db::migrate(&pool).await?;

    if db::get_bounded_context(&pool, BOUNDED_CONTEXT)
        .await?
        .is_none()
    {
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
        // `true`, not `false`: this is now the identity
        // `src/bin/provisioner.rs` signs its own JWTs for (see this
        // role's own JWT print below) to call skilj's
        // `createBoundedContextFromTemplate` mutation, which is gated on
        // `Role.superadmin` directly, never a `RoleAccessMapping` (see
        // `tests/support::seed_superadmin`'s own doc comment in the
        // sibling test crate) - this is still the one operator-controlled
        // bootstrap identity for this whole deployment, so granting it
        // superadmin too (rather than minting a second Role) keeps this
        // server's own "one admin identity" shape rather than doubling it.
        superadmin: true,
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

    // The same bootstrap admin Role, granted a second and third mapping
    // onto the "activity"/"marketing" bounded contexts -
    // src/activity.rs's/src/marketing.rs's own command/event types live
    // there, not in `helpdesk`, so engagement-watcher's own tokens below
    // (and the three `CrossContextRoute`s lib.rs's own `register()`
    // wires in) need a mapping scoped to each. Without the `marketing`
    // grant specifically, `report.skipped_no_access` below would list
    // every `marketing/*` type and the routes would silently skip every
    // occurrence forever (`target CommandType isn't registered`) -
    // caught by actually running this binary and watching the route's
    // own background task log a warning, not assumed from reading the
    // code. Each bounded context's own type registration happens
    // automatically via `register()`'s `auto_register()` (see lib.rs's
    // own doc comment); only the bounded context row and this grant are
    // this file's own job, the same two steps `helpdesk`'s own bootstrap
    // just did above.
    let activity_mapping =
        ensure_bounded_context_and_grant(&pool, &role, skilj_helpdesk::activity::BOUNDED_CONTEXT)
            .await?;
    let _marketing_mapping =
        ensure_bounded_context_and_grant(&pool, &role, skilj_helpdesk::marketing::BOUNDED_CONTEXT)
            .await?;

    // Real IdP when OIDC_ISSUER_URL is set (a running Dex instance - see
    // this file's own module doc comment), the local JWKS/JWT shortcut
    // otherwise. Either way `IdpConfig` is what skilj-graphql actually
    // verifies every GraphQL request's JWT against.
    let oidc_issuer_url = std::env::var("OIDC_ISSUER_URL").ok();
    let (issuer, jwks_url, audience, key_pair) = match &oidc_issuer_url {
        Some(url) => (url.clone(), format!("{url}/keys"), DEX_AUDIENCE, None),
        None => {
            let kp = Arc::new(GeneratedKeyPair::generate());
            let url = serve_local_jwks(kp.jwks.clone()).await;
            (TEST_ISSUER.to_string(), url, TEST_AUDIENCE, Some(kp))
        }
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
            audience,
            SigningAlgorithm::Rs256,
        ))
        .build()
        .await?;
    println!(
        "server: reconciliation complete, registered {:?}",
        report.registered
    );
    if !report.skipped_no_access.is_empty() {
        println!(
            "server: reconciliation skipped (no access yet): {:?}",
            report.skipped_no_access
        );
    }

    if let Some(url) = &oidc_issuer_url {
        println!("\nreal IdP: {url} - log in as customer@acme.example / customer-demo-pw");
        println!("or lead@acme.example / staff-demo-pw (see dex/config.yaml)");
    } else {
        let key_pair = key_pair
            .as_ref()
            .expect("key_pair is Some when no OIDC issuer is configured");
        println!("\nGraphQL Role credential (send as `authorization: Bearer <jwt>`):");
        println!("  {}", key_pair.sign_jwt(&role.external_subject));
        println!("(local JWKS shortcut in use - set OIDC_ISSUER_URL to a running Dex for a real login flow)");
    }

    println!(
        "\ncommand tokens (send as `authorization: Bearer <id>.<secret>` to /v1/commands/trigger):"
    );
    let command_tokens =
        mint_command_tokens(&pool, &mapping, BOUNDED_CONTEXT, COMMAND_TYPES).await?;

    println!("\nalerter's own event read tokens:");
    let alerter_event_tokens =
        mint_event_tokens(&pool, &mapping, BOUNDED_CONTEXT, ALERTER_EVENT_TYPES).await?;

    println!("\nprovisioner's own event read token:");
    let provisioner_event_tokens =
        mint_event_tokens(&pool, &mapping, BOUNDED_CONTEXT, PROVISIONER_EVENT_TYPES).await?;

    println!("\nlifecycle-replicator's own event read tokens:");
    let replicator_event_tokens =
        mint_event_tokens(&pool, &mapping, BOUNDED_CONTEXT, REPLICATOR_EVENT_TYPES).await?;

    // Only if telemetry is actually configured - see
    // run_csat_metrics_loop's own doc comment for why a token and a
    // poll loop otherwise have nothing to record into.
    let csat_metrics_token = if telemetry.is_some() {
        println!("\nCSAT metrics' own event read token:");
        Some(
            mint_event_tokens(&pool, &mapping, BOUNDED_CONTEXT, &["TicketRated"]).await?
                ["TicketRated"]
                .clone(),
        )
    } else {
        None
    };

    // engagement-watcher's own tokens - against `activity`, via
    // `activity_mapping` (see this file's own bootstrap above), not
    // `helpdesk`/`mapping`.
    println!("\nengagement-watcher's own event read token:");
    let activity_event_tokens = mint_event_tokens(
        &pool,
        &activity_mapping,
        skilj_helpdesk::activity::BOUNDED_CONTEXT,
        &["DailyActivityRecorded"],
    )
    .await?;
    // `RecordDailyActivity` too, not just engagement-watcher's own
    // `RecordEngagementDecline` - same "every rest_trigger_allowed
    // command type gets a demo token" convention `COMMAND_TYPES` above
    // already follows for `helpdesk`; `CustomerActivityPing`/
    // `StaffActivityPing` are real surfaces too, just not yet backed by
    // frontend/.
    println!("\nactivity's own command tokens:");
    let activity_command_tokens = mint_command_tokens(
        &pool,
        &activity_mapping,
        skilj_helpdesk::activity::BOUNDED_CONTEXT,
        &["RecordDailyActivity", "RecordEngagementDecline"],
    )
    .await?;

    // Ready-to-paste env vars for the other two binaries this session
    // built - closes the loop between all three.
    println!("\nto run the alerter against this server:");
    println!("  export SKILJ_BASE_URL=http://localhost:{port}");
    println!(
        "  export TICKET_CREATED_TOKEN={}",
        alerter_event_tokens["TicketCreated"]
    );
    println!(
        "  export TICKET_RESOLVED_TOKEN={}",
        alerter_event_tokens["TicketResolved"]
    );
    println!(
        "  export TICKET_REOPENED_TOKEN={}",
        alerter_event_tokens["TicketReopened"]
    );
    println!(
        "  export TICKET_CLOSED_TOKEN={}",
        alerter_event_tokens["TicketClosed"]
    );
    println!(
        "  export TICKET_ESCALATED_TOKEN={}",
        alerter_event_tokens["TicketEscalated"]
    );
    println!(
        "  export TICKETS_MERGED_TOKEN={}",
        alerter_event_tokens["TicketsMerged"]
    );
    println!(
        "  export ESCALATE_TICKET_TOKEN={}",
        command_tokens["EscalateTicket"]
    );
    println!("  cargo run --bin alerter");
    println!();
    println!("  # Phase 4: multi-tenant mode (TICKET_ROUTING=tenant).");
    println!("  # When the cutover is on, the alerter discovers per-company");
    println!("  # tenants from CompanyTenantProvisioned, then mints its own");
    println!("  # per-tenant tokens via GraphQL using this superadmin subject.");
    println!(
        "  export COMPANY_TENANT_PROVISIONED_TOKEN={}",
        alerter_event_tokens["CompanyTenantProvisioned"]
    );
    println!("  export ALERTER_SUPERADMIN_SUBJECT={external_subject}");

    println!("\nto run the provisioner against this server:");
    println!("  export SKILJ_BASE_URL=http://localhost:{port}");
    println!(
        "  export COMPANY_SIGNED_UP_TOKEN={}",
        provisioner_event_tokens["CompanySignedUp"]
    );
    println!(
        "  export RECORD_COMPANY_TENANT_TOKEN={}",
        command_tokens["RecordCompanyTenant"]
    );
    println!("  export PROVISIONER_SUPERADMIN_SUBJECT={external_subject}");
    println!("  export PROVISIONER_TENANT_ADMIN_ROLE_ID={}", role.id);
    if oidc_issuer_url.is_some() {
        println!(
            "  (provisioner signs its own short-lived JWTs against the local JWKS shortcut's \
             own test key - see provisioner.rs's own doc comment; it can't get a real superadmin \
             JWT out of a real Dex issuer, so it won't work against this OIDC_ISSUER_URL run)"
        );
    }
    println!("  cargo run --bin provisioner");

    println!(
        "\nto run the lifecycle-replicator against this server (mirrors each company's \
         lifecycle into its own tenant, so ticket commands routed there pass the same \
         company_status guard):"
    );
    println!("  export SKILJ_BASE_URL=http://localhost:{port}");
    for event_type in REPLICATOR_EVENT_TYPES {
        println!(
            "  export {}_TOKEN={}",
            event_type.to_uppercase(),
            replicator_event_tokens[*event_type]
        );
    }
    // Deliberately the *same* subject the provisioner was configured with
    // below, and not the provisioner's role id: `createCommandToken`
    // derives its authority from the caller's own RoleAccessMapping on
    // the named context, and this binary only has one on each tenant if
    // it authenticates as the identity that was granted it.
    println!("  export REPLICATOR_SUPERADMIN_SUBJECT={external_subject}");
    println!("  cargo run --bin lifecycle-replicator");

    println!(
        "\ntrial conversion/expiry and ticket auto-close run in-process now (skilj's own native \
         per-entity deadlines, docs/architecture.md §46) - no separate binary or tokens needed. \
         TRIAL_DURATION_DAYS/AUTO_CLOSE_AFTER_DAYS still override the default 30/7-day windows."
    );

    println!("\nto run engagement-watcher against this server:");
    println!("  export SKILJ_BASE_URL=http://localhost:{port}");
    println!(
        "  export DAILY_ACTIVITY_RECORDED_TOKEN={}",
        activity_event_tokens["DailyActivityRecorded"]
    );
    println!(
        "  export RECORD_ENGAGEMENT_DECLINE_TOKEN={}",
        activity_command_tokens["RecordEngagementDecline"]
    );
    println!("  cargo run --bin engagement-watcher");

    let rest = skilj.rest_router();
    let graphql = skilj.graphql_router().await?;
    // Permissive: this showcase's whole point is a real browser
    // (frontend/, a different origin) calling this server directly, and
    // skilj's own auth is bearer-token-based (REST's own AccessToken,
    // GraphQL's own JWT), never cookies - so there's no CSRF surface
    // permissive CORS opens up here the way it would for cookie-based
    // auth. A real deployment would still want this restricted to its
    // own known frontend origin(s) rather than left permissive.
    // Load-shed + concurrency-limit: the other half of the
    // docs/load-test-report-2026-09-17.md fix, alongside
    // DATABASE_MAX_CONNECTIONS above. Even with a right-sized DB pool,
    // nothing previously capped how many requests axum would accept and
    // hold in memory at once while waiting on that pool - a genuine
    // overload just grew unboundedly (measured: 54MB -> 487MB server RSS
    // in 4 minutes) instead of failing fast. Once more than
    // HTTP_MAX_IN_FLIGHT_REQUESTS are already in flight, load_shed()
    // makes any further request fail immediately (503) rather than queue.
    // Deliberately below db_max_connections, not just "some big number":
    // this needs to shed load *before* every pool connection is spoken
    // for, or it never fires and requests still pile up waiting on
    // sqlx's own 30s acquire_timeout instead of getting a fast 503 - the
    // gap between this and db_max_connections is the pool budget left
    // for the background loops/long-poll consumers noted above, which
    // this limit doesn't (and shouldn't) count against.
    let http_max_in_flight: usize = std::env::var("HTTP_MAX_IN_FLIGHT_REQUESTS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(70);
    // Ticket-routing enforcement, in front of the GraphQL router only.
    // The frontend resolves each company's tenant and names it; this
    // refuses ticket traffic that names the shared `helpdesk` context
    // instead, so a client that resolved no tenant - or one deliberately
    // naming shared - can't split a company's history across two
    // contexts. REST needs no equivalent: `skilj-rest` derives its
    // destination from the command token rather than from a body field.
    //
    // A `from_fn_with_state` layer on the GraphQL router rather than on
    // the merged `app`, so it cannot slow down or interfere with the
    // REST surface or anything else merged in later. With the cutover
    // off (the default) it short-circuits without buffering the body at
    // all - see `routing_guard::enforce_graphql_routing`.
    let routing_mode = routing_guard::mode_from_env();
    let graphql = if routing_mode == RoutingMode::Tenant {
        println!(
            "\nticket routing is ON (TICKET_ROUTING=tenant): each company's Ticket traffic is \
             served from that company's own tenant, and GraphQL traffic naming the shared \
             {BOUNDED_CONTEXT:?} context for a company that has a tenant is refused. Company \
             lifecycle traffic stays shared."
        );
        let guard_state = Arc::new(GuardState {
            pool: pool.clone(),
            mode: routing_mode,
        });
        graphql.layer(axum::middleware::from_fn_with_state(
            guard_state,
            routing_guard::enforce_graphql_routing,
        ))
    } else {
        graphql
    };
    let app = rest.merge(graphql).layer(cors_layer()).layer(
        tower::ServiceBuilder::new()
            .layer(axum::error_handling::HandleErrorLayer::new(
                |_: tower::BoxError| async {
                    (
                        axum::http::StatusCode::SERVICE_UNAVAILABLE,
                        "overloaded - too many in-flight requests, try again shortly",
                    )
                },
            ))
            .load_shed()
            .concurrency_limit(http_max_in_flight),
    );

    let listener = tokio::net::TcpListener::bind((bind_addr, port)).await?;
    println!("\nskilj-helpdesk listening on http://{bind_addr}:{port} (REST under /v1/..., GraphQL at /graphql)");
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

        // Phase 4: when TICKET_ROUTING=tenant, TicketRated events also
        // land in per-company tenant contexts. This second loop discovers
        // those tenants from CompanyTenantProvisioned and polls each
        // tenant's own TicketRated feed - so a RateTicket command routed
        // into a tenant still records its rating as a metric.
        //
        // It mints its per-tenant tokens with a JWT this process signs
        // itself, so it only works with the local JWKS shortcut: a real
        // IdP would never verify that JWT.
        if routing_mode == RoutingMode::Tenant && key_pair.is_none() {
            eprintln!(
                "csat metrics: OIDC_ISSUER_URL is set, so tenant TicketRated feeds are not \
                 polled (the per-tenant tokens need a locally signed JWT)"
            );
        }
        if let (RoutingMode::Tenant, Some(mt_key_pair)) = (routing_mode, key_pair.clone()) {
            let mt_config = MultiTenantCsatConfig {
                company_tenant_provisioned_token: alerter_event_tokens["CompanyTenantProvisioned"]
                    .clone(),
                superadmin_subject: external_subject.clone(),
            };
            let mt_base_url = format!("http://localhost:{port}");
            let mt_client = reqwest::Client::new();
            tokio::spawn(async move {
                run_multi_tenant_csat_loop(&mt_client, &mt_base_url, mt_config, mt_key_pair).await;
            });
        }
    }

    // Keep each company's access grants mirrored into its own tenant -
    // `src/tenant_access.rs`'s own module doc comment for why this has to
    // exist before any traffic is routed there (skilj-graphql's
    // `submitCommand` authorizes against the caller's mapping on the
    // *named* context, so a tenant with no grant simply rejects
    // everything).
    //
    // Runs on an interval rather than once at startup because a Role's
    // grant can be added or revoked long after its tenant was
    // provisioned - `SignUpCompany` creates no Roles at all, so at
    // provisioning time there is usually nothing yet to mirror, and the
    // first real grant arrives afterwards.
    let reconciler_pool = pool.clone();
    let reconciler_role = role.clone();
    tokio::spawn(
        async move { run_tenant_access_reconciler(reconciler_pool, reconciler_role).await },
    );

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
        // Phase 4: when TICKET_ROUTING=tenant, the demo seed also needs a
        // per-company tenant token cache so ticket commands route to the
        // right tenant context (REST routes by token, so a shared token
        // would always hit the shared `helpdesk` context).
        // Same local-JWT limitation as the multi-tenant CSAT loop above:
        // with a real IdP the seed falls back to the shared context.
        if routing_mode == RoutingMode::Tenant && key_pair.is_none() {
            eprintln!(
                "demo seed: OIDC_ISSUER_URL is set, so seed traffic is not routed to tenants \
                 (the per-tenant tokens need a locally signed JWT)"
            );
        }
        let tenant_token_cache =
            if let (RoutingMode::Tenant, Some(seed_key_pair)) = (routing_mode, key_pair.clone()) {
                let cache = DemoSeedTokenCache::new(
                    &seed_base_url,
                    seed_key_pair,
                    &external_subject,
                    &alerter_event_tokens["CompanyTenantProvisioned"],
                );
                Some(Arc::new(tokio::sync::Mutex::new(cache)))
            } else {
                None
            };
        tokio::spawn(async move {
            sign_up_demo_companies(&seed_base_url, &seed_tokens).await;
            for worker_index in 0..concurrency {
                let base_url = seed_base_url.clone();
                let tokens = seed_tokens.clone();
                let tenant_cache = tenant_token_cache.clone();
                // Staggers each worker's first tick evenly across one
                // interval, rather than every worker firing in lockstep
                // every `interval_ms` - a smoother, more realistic load
                // shape (one steady stream) than `concurrency` synchronised
                // bursts would be.
                let stagger =
                    Duration::from_millis(interval_ms * worker_index as u64 / concurrency as u64);
                tokio::spawn(async move {
                    tokio::time::sleep(stagger).await;
                    run_demo_seed_loop(
                        worker_index,
                        base_url,
                        tokens,
                        tenant_cache,
                        Duration::from_millis(interval_ms),
                    )
                    .await;
                });
            }
        });
    }

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    // skilj 0.0.9's own graceful stop (docs/architecture.md §123):
    // every background loop this process started - projection/snapshot
    // catch-up, routes, the per-entity deadline reactors this server's
    // own trial-conversion and ticket auto-close rules ride on, scheduled
    // events, the new idempotency-key and resolved-deadline retention
    // sweeps - finishes the tick it is in and stops, then the pool
    // closes. Before this existed, dropping the process killed those
    // loops mid-tick and left the pool to die with the connections
    // (documented as "97 'request failed' lines ... server was torn down
    // mid-request" in docs/load-test-report-2026-09-19.md's own step 4).
    //
    // Only safe once `axum::serve` above has returned: the routers share
    // this pool, so a request still in flight would fail, and the pool
    // can't close while a request holds a connection. Anything still busy
    // at the timeout is aborted exactly like a crash, and recovers on the
    // next start under its own idempotency keys.
    let shutdown_report = skilj.shutdown(Duration::from_secs(10)).await;
    println!(
        "server: background loops stopped cleanly: {:?}; aborted at the timeout: {:?}; pool closed: {}",
        shutdown_report.stopped, shutdown_report.aborted, shutdown_report.pool_closed
    );

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
/// `alerter`/`engagement-watcher` already get as whole separate
/// processes.
async fn run_demo_seed_loop(
    worker_index: usize,
    base_url: String,
    command_tokens: HashMap<&'static str, String>,
    tenant_token_cache: Option<Arc<tokio::sync::Mutex<DemoSeedTokenCache>>>,
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
        // Phase 4: when TICKET_ROUTING=tenant, resolve the per-company
        // tenant token for this action's company instead of the always-
        // shared one. Token resolution is itself async (tenant discovery +
        // mint), so this only happens on the multi-tenant path.
        let credential = if let Some(cache) = &tenant_token_cache {
            let company_id = action.company_id(&state);
            match company_id {
                Some(cid) => {
                    let mut cache = cache.lock().await;
                    let fallback = command_tokens[command_type_name].clone();
                    cache
                        .get_or_mint(&client, cid, command_type_name, &fallback)
                        .await
                }
                None => command_tokens[command_type_name].clone(),
            }
        } else {
            command_tokens[command_type_name].clone()
        };
        match trigger_command(&client, &base_url, &credential, payload).await {
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
        SeedAction::AssignTicket {
            ticket_id,
            staff_id,
        } => (
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
