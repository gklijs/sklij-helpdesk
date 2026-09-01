//! Shared integration-test harness for `tests/helpdesk.rs` - adapted
//! near-verbatim from `skilj-demo/tests/support/mod.rs` (same
//! `DATABASE_URL`-then-embedded-Postgres-then-skip provisioning, same
//! direct-`db::`-seeding bootstrap), trimmed to this crate's one
//! bounded context.
#![allow(dead_code)]

use axum::body::Body;
use axum::http::{Request, StatusCode};
use chrono::{DateTime, SubsecRound, Utc};
use http_body_util::BodyExt;
use jsonwebtoken::{EncodingKey, Header};
use serde::de::DeserializeOwned;
use serde_json::json;
use skilj::{IdpConfig, Skilj, SigningAlgorithm};
use skilj_core::access_control::{self, AccessLevel, Role, RoleAccessMapping, RoleStatus};
use skilj_core::bootstrap::ContextCreator;
use skilj_core::db::{self, Pool};
use skilj_core::event_store::{BoundedContext, BoundedContextStatus};
use skilj_core::shared::{generate_token_id, generate_token_secret};
use tower::ServiceExt;

// --- provisioning: DATABASE_URL, else embedded Postgres, else skip ---

pub struct TestDb {
    pub database_url: String,
    pub pool: Pool,
    _embedded: Option<postgresql_embedded::PostgreSQL>,
}

static TEST_DB: tokio::sync::OnceCell<Option<TestDb>> = tokio::sync::OnceCell::const_new();

pub fn runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Runtime::new()
            .expect("failed to build a tokio runtime for skilj-helpdesk tests")
    })
}

pub async fn test_db() -> Option<(String, Pool)> {
    TEST_DB
        .get_or_init(provision)
        .await
        .as_ref()
        .map(|db| (db.database_url.clone(), db.pool.clone()))
}

async fn connect_and_migrate(database_url: &str, label: &str) -> Option<Pool> {
    let pool = match db::connect(database_url).await {
        Ok(pool) => pool,
        Err(e) => {
            eprintln!("skipping: connecting to {label} failed: {e}");
            return None;
        }
    };
    if let Err(e) = db::migrate(&pool).await {
        eprintln!("skipping: migrating {label} failed: {e}");
        return None;
    }
    Some(pool)
}

async fn provision() -> Option<TestDb> {
    let database_url = if let Ok(database_url) = std::env::var("DATABASE_URL") {
        database_url
    } else {
        let mut server = postgresql_embedded::PostgreSQL::default();
        if let Err(e) = server.setup().await {
            eprintln!(
                "skipping: DATABASE_URL not set and embedded PostgreSQL setup failed \
                 (no network egress to fetch the binary, or a missing system library \
                 like libxml2 it links against): {e}"
            );
            return None;
        }
        if let Err(e) = server.start().await {
            eprintln!("skipping: embedded PostgreSQL failed to start: {e}");
            return None;
        }
        let database_name = "skilj_helpdesk_test";
        if let Err(e) = server.create_database(database_name).await {
            eprintln!("skipping: embedded PostgreSQL create_database failed: {e}");
            return None;
        }
        let url = server.settings().url(database_name);
        let pool = connect_and_migrate(&url, "embedded PostgreSQL").await?;
        return Some(TestDb {
            database_url: url,
            pool,
            _embedded: Some(server),
        });
    };

    let pool = connect_and_migrate(&database_url, "DATABASE_URL").await?;
    Some(TestDb {
        database_url,
        pool,
        _embedded: None,
    })
}

pub fn unique_name(prefix: &str) -> String {
    format!("{prefix}_{}", generate_token_id())
}

pub fn test_now() -> DateTime<Utc> {
    Utc::now().trunc_subsecs(6)
}

// --- bootstrap: seed an admin Role with access to the helpdesk context ---

static BOUNDED_CONTEXT_READY: tokio::sync::OnceCell<()> = tokio::sync::OnceCell::const_new();

async fn ensure_bounded_context(pool: &Pool) {
    BOUNDED_CONTEXT_READY
        .get_or_init(|| async {
            let name = skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
            if db::get_bounded_context(pool, name).await.unwrap().is_none() {
                db::insert_bounded_context(
                    pool,
                    &BoundedContext {
                        name: name.to_string(),
                        status: BoundedContextStatus::Active,
                        created_at: test_now(),
                        created_by: ContextCreator::SystemCreator,
                        template: None,
                    },
                )
                .await
                .unwrap();
            }
        })
        .await;
}

/// A fresh admin `Role`, granted `Admin` access to the helpdesk context.
/// Safe to call once per `#[test]` - each gets its own `Role`.
pub async fn seed_admin(pool: &Pool) -> RoleAccessMapping {
    ensure_bounded_context(pool).await;

    let external_subject = unique_name("subject");
    let role = Role {
        id: generate_token_id(),
        external_subject,
        name: "Test Admin".into(),
        superadmin: false,
        status: RoleStatus::Active,
        created_at: test_now(),
        revoked_at: None,
    };
    db::insert_role(pool, &role).await.unwrap();

    let bounded_context = db::get_bounded_context(pool, skilj_helpdesk::helpdesk::BOUNDED_CONTEXT)
        .await
        .unwrap()
        .expect("ensure_bounded_context just made sure this exists");
    let mapping = RoleAccessMapping {
        role: role.clone(),
        bounded_context,
        level: AccessLevel::Admin,
        can_read_sensitive: false,
        status: RoleStatus::Active,
        created_at: test_now(),
        revoked_at: None,
    };
    db::insert_role_access_mapping(pool, &mapping)
        .await
        .unwrap();
    mapping
}

/// A fully-built `Skilj` with the helpdesk bounded context reconciled -
/// a `#[test]` mints its own `CommandToken`s from the returned mapping.
pub async fn setup() -> (Skilj, Pool, RoleAccessMapping) {
    let (database_url, pool) = test_db()
        .await
        .expect("test_db() must be Some - caller already checked");
    let mapping = seed_admin(&pool).await;
    let external_subject = mapping.role.external_subject.clone();

    let (skilj, report) = skilj_helpdesk::register(Skilj::builder(database_url))
        .reconciliation_role(external_subject)
        .build()
        .await
        .unwrap();
    assert_eq!(report.skipped_no_access, Vec::<String>::new());

    (skilj, pool, mapping)
}

pub async fn mint_command_token(
    pool: &Pool,
    mapping: &RoleAccessMapping,
    bounded_context: &str,
    command_type_name: &str,
) -> String {
    let command_type = db::get_command_type(pool, bounded_context, command_type_name)
        .await
        .unwrap()
        .unwrap_or_else(|| {
            panic!("{bounded_context}/{command_type_name} must already be registered")
        });
    let token = access_control::create_command_token(
        mapping,
        &command_type,
        generate_token_id(),
        generate_token_secret(),
        test_now(),
    )
    .unwrap();
    db::insert_command_token(pool, &token).await.unwrap();
    format!("{}.{}", token.id, token.secret)
}

pub async fn mint_event_read_token(
    pool: &Pool,
    mapping: &RoleAccessMapping,
    bounded_context: &str,
    event_type_name: &str,
) -> String {
    let event_type = db::get_event_type(pool, bounded_context, event_type_name)
        .await
        .unwrap()
        .unwrap_or_else(|| {
            panic!("{bounded_context}/{event_type_name} must already be registered")
        });
    let token = access_control::create_event_read_token(
        mapping,
        &event_type,
        generate_token_id(),
        generate_token_secret(),
        test_now(),
    )
    .unwrap();
    db::insert_event_read_token(pool, &token).await.unwrap();
    format!("{}.{}", token.id, token.secret)
}

// --- HTTP: POST /v1/commands/trigger ---

pub async fn trigger(
    router: &axum::Router,
    credential: &str,
    payload: serde_json::Value,
) -> serde_json::Value {
    let request = Request::builder()
        .method("POST")
        .uri("/v1/commands/trigger")
        .header("authorization", format!("Bearer {credential}"))
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&serde_json::json!({ "payload": payload })).unwrap(),
        ))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "a well-formed CommandTrigger request always renders 200, accepted or not"
    );
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

// --- HTTP: GET /v1/events/consume - src/bin/alerter.rs's own real path ---

pub async fn consume_auto(router: &axum::Router, credential: &str) -> serde_json::Value {
    let request = Request::builder()
        .method("GET")
        .uri("/v1/events/consume?mode=auto")
        .header("authorization", format!("Bearer {credential}"))
        .body(Body::empty())
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK, "a well-formed consume request always renders 200");
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

pub fn accepted(response: &serde_json::Value) -> bool {
    response["accepted"]
        .as_bool()
        .expect("CommandTriggerResponse.accepted is always present")
}

pub fn rejection_kind(response: &serde_json::Value) -> &str {
    response["rejectionKind"]
        .as_str()
        .expect("a rejected response always carries rejectionKind")
}

// --- projection reads ---

pub async fn projection_state<T: DeserializeOwned + Default>(
    pool: &Pool,
    bounded_context: &str,
    projection_name: &str,
    key: &str,
) -> T {
    match db::get_projection_state(pool, bounded_context, projection_name, key)
        .await
        .unwrap()
    {
        Some(json) => serde_json::from_str(&json).unwrap(),
        None => T::default(),
    }
}

// --- GraphQL: JWT-authenticated requests against Skilj::graphql_router() ---
//
// Adapted near-verbatim from skilj-demo/tests/graphql_auth.rs - same
// fixed test RSA keypair (that file's own doc comment explains why
// reusing a fixed, publicly-known key is fine for a test fixture),
// duplicated rather than shared across crate boundaries for the same
// reason skilj-demo's own copy is duplicated rather than imported.

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

pub async fn serve_jwks() -> String {
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
        .expect("failed to bind an ephemeral port for the JWKS test server");
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/jwks.json")
}

pub fn sign_jwt(subject: &str) -> String {
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(TEST_KID.to_string());
    let claims = json!({
        "sub": subject,
        "iss": TEST_ISSUER,
        "exp": (Utc::now() + chrono::Duration::hours(1)).timestamp(),
    });
    let key = EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY_PEM.as_bytes()).unwrap();
    jsonwebtoken::encode(&header, &claims, &key).unwrap()
}

/// Same shape as `setup()`, plus a real local JWKS server and a signed
/// JWT for the seeded admin Role - everything `tests/graphql.rs` needs
/// to authenticate a real GraphQL request the way a real caller would
/// (an IdP-issued JWT, not skilj's own bearer tokens - see
/// `specs/skilj.allium`'s two independent authentication tracks note).
pub async fn setup_graphql() -> (Skilj, Pool, RoleAccessMapping, String) {
    let (database_url, pool) = test_db()
        .await
        .expect("test_db() must be Some - caller already checked");
    let mapping = seed_admin(&pool).await;
    let external_subject = mapping.role.external_subject.clone();
    let jwks_url = serve_jwks().await;

    let (skilj, report) = skilj_helpdesk::register(Skilj::builder(database_url))
        .reconciliation_role(external_subject.clone())
        .identity_provider(IdpConfig::new(
            jwks_url.parse().unwrap(),
            TEST_ISSUER,
            SigningAlgorithm::Rs256,
        ))
        .build()
        .await
        .unwrap();
    assert_eq!(report.skipped_no_access, Vec::<String>::new());

    let jwt = sign_jwt(&external_subject);
    (skilj, pool, mapping, jwt)
}

pub async fn graphql_request(router: &axum::Router, jwt: &str, query: &str) -> serde_json::Value {
    let request = Request::builder()
        .method("POST")
        .uri("/graphql")
        .header("authorization", format!("Bearer {jwt}"))
        .header("content-type", "application/json")
        .body(Body::from(json!({ "query": query }).to_string()))
        .unwrap();

    let response = router.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

/// `submitCommand`'s own payload argument is a raw JSON *string* - see
/// `skilj-graphql`'s own `submit_command_field` doc comment
/// (`payload: String!`, unlike REST's typed JSON body). Builds a
/// `submitCommand` mutation with that payload correctly double-encoded
/// (JSON-inside-a-GraphQL-string-literal).
pub fn submit_command_mutation(
    bounded_context: &str,
    command_type_name: &str,
    payload: &serde_json::Value,
) -> String {
    let payload_json = serde_json::to_string(payload).unwrap();
    let payload_literal = serde_json::to_string(&payload_json).unwrap();
    format!(
        "mutation {{ submitCommand(boundedContext: {bounded_context:?}, commandTypeName: {command_type_name:?}, payload: {payload_literal}) {{ accepted rejectionReason rejectionKind }} }}"
    )
}

pub fn graphql_accepted(response: &serde_json::Value) -> bool {
    assert!(
        response.get("errors").is_none(),
        "expected no GraphQL errors, got {response:?}"
    );
    response["data"]["submitCommand"]["accepted"]
        .as_bool()
        .expect("submitCommand.accepted is always present when there are no GraphQL errors")
}
