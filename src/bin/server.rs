//! A real, runnable server for `skilj_helpdesk::helpdesk` - `cargo run
//! --bin server`. Not a test: boots an actual `axum` process serving
//! both REST and GraphQL, prints every credential `alerter`/`scheduler`
//! need as ready-to-export env vars, then serves until killed. Modelled
//! closely on `skilj-demo/src/bin/server.rs` - trimmed of the
//! OpenTelemetry wiring (a reference example for that isn't this
//! crate's own concern; see that file's own doc comment if a real
//! deployment needs it).
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
use serde_json::json;
use skilj::{IdpConfig, SigningAlgorithm, Skilj};
use skilj_core::access_control::{self, AccessLevel, Role, RoleAccessMapping, RoleStatus};
use skilj_core::bootstrap::ContextCreator;
use skilj_core::db;
use skilj_core::event_store::{BoundedContext, BoundedContextStatus};
use skilj_core::shared::{generate_token_id, generate_token_secret};
use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use std::collections::HashMap;

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
];

/// Every `event_read_allowed` event type - see `helpdesk.rs`'s own
/// `event_read_allowed` doc comments for which binary reads which.
const EVENT_TYPES: &[&str] = &[
    "CompanySignedUp",
    "CompanyActivated",
    "CompanyExpired",
    "TicketCreated",
    "TicketResolved",
    "TicketReopened",
    "TicketClosed",
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
        for (label, sub) in [
            ("customer", DEMO_CUSTOMER_SUB),
            ("staff-lead", DEMO_STAFF_LEAD_SUB),
        ] {
            if existing_roles
                .iter()
                .any(|r| r.external_subject == sub && r.status == RoleStatus::Active)
            {
                println!("server: demo Role for {label} already exists (sub {sub:?})");
                continue;
            }
            let demo_role = Role {
                id: generate_token_id(),
                external_subject: sub.to_string(),
                name: format!("skilj-helpdesk demo {label}"),
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
            Utc::now(),
        )?;
        db::insert_command_token(&pool, &token).await?;
        let credential = format!("{}.{}", token.id, token.secret);
        println!("  {command_type_name}: {credential}");
        command_tokens.insert(*command_type_name, credential);
    }

    println!("\nevent read tokens (send as `authorization: Bearer <id>.<secret>` to /v1/events/consume):");
    let mut event_tokens = HashMap::new();
    for event_type_name in EVENT_TYPES {
        let event_type = db::get_event_type(&pool, BOUNDED_CONTEXT, event_type_name)
            .await?
            .unwrap_or_else(|| {
                panic!("{BOUNDED_CONTEXT}/{event_type_name} should have just been registered")
            });
        let token = access_control::create_event_read_token(
            &mapping,
            &event_type,
            generate_token_id(),
            generate_token_secret(),
            Utc::now(),
        )?;
        db::insert_event_read_token(&pool, &token).await?;
        let credential = format!("{}.{}", token.id, token.secret);
        println!("  {event_type_name}: {credential}");
        event_tokens.insert(*event_type_name, credential);
    }

    // Ready-to-paste env vars for the other two binaries this session
    // built - closes the loop between all three.
    println!("\nto run the alerter against this server:");
    println!("  export SKILJ_BASE_URL=http://localhost:{port}");
    println!("  export TICKET_CREATED_TOKEN={}", event_tokens["TicketCreated"]);
    println!("  cargo run --bin alerter");

    println!("\nto run the scheduler against this server:");
    println!("  export SKILJ_BASE_URL=http://localhost:{port}");
    println!("  export COMPANY_SIGNED_UP_TOKEN={}", event_tokens["CompanySignedUp"]);
    println!("  export COMPANY_ACTIVATED_TOKEN={}", event_tokens["CompanyActivated"]);
    println!("  export COMPANY_EXPIRED_TOKEN={}", event_tokens["CompanyExpired"]);
    println!("  export TICKET_RESOLVED_TOKEN={}", event_tokens["TicketResolved"]);
    println!("  export TICKET_REOPENED_TOKEN={}", event_tokens["TicketReopened"]);
    println!("  export TICKET_CLOSED_TOKEN={}", event_tokens["TicketClosed"]);
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

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;

    Ok(())
}
