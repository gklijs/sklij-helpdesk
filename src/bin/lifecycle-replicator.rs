//! The tenant-lifecycle mirroring reactor: the second half of what
//! `src/helpdesk.rs`'s `CompanyLifecycleMirrored`/`RecordTenantLifecycle`
//! pair describes, and the first thing in this crate to route a command
//! *into* a tenant.
//!
//! **The problem it exists to solve.** `CreateTicket` (and every other
//! Ticket command) opens with a `company_status` guard read, and skilj
//! hands `decide()` only the events carrying the command's own tags,
//! read from the bounded context the command is dispatched *into*
//! (`CommandType::decide`'s own doc comment). A company's signup happens
//! in the shared `helpdesk` context, so a tenant's own history contains
//! no trace of its company's lifecycle - and a Ticket command routed at a
//! tenant would be rejected `company_not_found` for a company that
//! demonstrably exists. This binary closes that gap by reading lifecycle
//! events off the shared context and re-recording the equivalent fact in
//! the right tenant.
//!
//! **Why this is a separate binary and not a `CrossContextRoute`.** It
//! is the shape `provisioner.rs`'s own module doc comment already
//! describes for this crate's other cross-context side effect (I/O out
//! of a pure `decide()`, driven by a reactor on the REST event feed), and
//! it is not a choice: skilj's `CrossContextRoute` cannot express this
//! fan-out. `Target: CommandType` is one type whose `BOUNDED_CONTEXT` is
//! a compile-time const, and the route catch-up loop reads its route
//! list once at startup with no runtime registration surface, so a route
//! can target a *named* context but never the set of contexts a
//! provisioner creates at runtime. (`ScheduleDeadline` does fan out over
//! every bounded context dynamically - `deadline_fire_tick` re-lists
//! them each tick - but its source is likewise a static type, so it
//! can't react to *another* context's events either.)
//!
//! **Read from the shared context, write to the tenant.** Deliberately
//! one-directional: the shared `helpdesk` context stays the only place a
//! company's status actually changes, and this binary never writes a
//! transition there. `RecordTenantLifecycle`'s own `decide()` is what
//! makes that safe to lean on - it refuses to move a tenant's mirrored
//! state backwards, so out-of-order or redelivered events (each of the
//! three feeds below has its own cursor, so there is no ordering
//! *between* them) converge on the right answer rather than latching a
//! stale one.
//!
//! **Tenant resolution.** `company_id -> tenant_name` comes from the
//! shared context's own `TenantDirectory` projection
//! (`helpdesk.rs`), read over GraphQL. That is the same mapping
//! `RecordCompanyTenant` recorded, projected into a queryable read model
//! rather than re-derived here - one source of truth, so this binary
//! cannot disagree with the provisioner about which tenant a company
//! belongs to.
//!
//! **Per-tenant tokens, minted on demand.** A `CommandToken` is scoped
//! to one bounded context, and there is no token that spans every tenant,
//! so this binary mints one per tenant on first use, via skilj's own
//! `createCommandToken` GraphQL mutation, and caches it in memory for the
//! process's lifetime.
//!
//! `createCommandToken` derives its authority from the *caller's* own
//! `RoleAccessMapping` on the named bounded context (`require_admin_mapping`
//! inside that resolver), so this binary's configured subject must itself
//! hold an Admin mapping on each tenant - which it does, by construction:
//! it is the same identity `createBoundedContextFromTemplate` granted that
//! mapping to at tenant-creation time (`PROVISIONER_TENANT_ADMIN_ROLE_ID`
//! in `provisioner.rs`'s own config, `@guarantee AccessGrantedWithCreation`).
//! Hence no role-id config here: the role is implied by the subject, and a
//! separately-configured role id that disagreed with the subject would be
//! silently ignored by the resolver rather than caught.
//!
//! Mints are cached because each is one HTTP round trip and one durable
//! `CommandToken` row: an uncached mint per event would grow that table
//! without bound at a company's lifecycle-event rate.
//!
//! **Restart safety, deliberately the simpler tradeoff.** No checkpoint
//! file, unlike `alerter.rs`'s own overdue-ticket sweep - which needs one
//! because losing its in-memory state there means losing every currently
//! -open ticket. Here, a restart means this binary's three `mode=auto`
//! event cursors resume from where the server left them, and the worst a
//! crash mid-tick does is skip events for this process's own crash window,
//! the same bounded "occasional missed events on restart is acceptable"
//! tradeoff `specs/skilj-helpdesk.allium`'s own resolved alerting design
//! note already accepts. What that tradeoff costs is a tenant whose
//! lifecycle was never mirrored staying `company_not_found` until the next
//! lifecycle event - bounded, visible (the guard's own rejection says so),
//! and recoverable by resubmitting; a stricter guarantee would mean
//! re-deriving which companies still lack a mirror, which is a per-company
//! scan this binary has no cheap way to do.
//!
//! **Untrusted input, handled as such.** `company_id` arrives from the
//! event feed and flows into a GraphQL `variables`-bound tenant lookup and
//! token mint - both sent as real `variables`, never interpolated into a
//! query document, the same discipline `provisioner.rs`'s own module doc
//! comment states for the identically caller-controlled `tenant_name`.
//!
//! Configuration (env vars, same minimal convention as the other
//! reactors here):
//!   SKILJ_BASE_URL                  - default "http://localhost:3000"
//!   COMPANY_SIGNED_UP_TOKEN         - EventReadToken for CompanySignedUp
//!   COMPANY_ACTIVATED_TOKEN         - EventReadToken for CompanyActivated
//!   COMPANY_EXPIRED_TOKEN           - EventReadToken for CompanyExpired
//!   REPLICATOR_SUPERADMIN_SUBJECT   - the bootstrap admin Role's own
//!                                     `external_subject` (server.rs prints
//!                                     it), which must be the *same*
//!                                     identity `provisioner.rs` was
//!                                     configured with, since that is the
//!                                     one holding the per-tenant Admin
//!                                     mapping `createCommandToken`
//!                                     requires - this binary signs its own
//!                                     short-lived JWT for it,
//!                                     local-JWKS-shortcut only (see the
//!                                     note at `sign_jwt` below)
//!
//! Telemetry: `skilj_helpdesk::telemetry::init` as service
//! `"skilj-helpdesk-lifecycle-replicator"` - same OTLP opt-in as every
//! other binary here.

use chrono::Utc;
use jsonwebtoken::{EncodingKey, Header};
use serde_json::json;
// For `<TenantDirectory as Projection>::NAME` below - the same trait the
// GraphQL `projection` query's `name` argument is a plain string for, so
// this binary can't spell it without the trait in scope.
use skilj::Projection;
use std::collections::HashMap;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// One shared-context event type this binary mirrors, and the lifecycle
/// state it implies. The mapping is the whole content of this binary:
/// `CompanySignedUp` means trialing, `CompanyActivated` means active,
/// `CompanyExpired` means expired. Kept as data rather than a chain of
/// `if event_type == ...` branches so the set of mirrored events is
/// readable in one place, and so adding one is a single-row change.
const MIRRORED_EVENTS: &[(&str, &str)] = &[
    ("CompanySignedUp", "trialing"),
    ("CompanyActivated", "active"),
    ("CompanyExpired", "expired"),
];

/// Local JWKS/JWT shortcut - this binary's own copy, same "duplicated
/// rather than shared across the test/binary boundary" convention
/// `provisioner.rs` and `server.rs` both use; never a real secret.
/// Identical key material to theirs, because all three sign against the
/// same local JWKS endpoint the server serves in that mode.
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
const TEST_KID: &str = "test-key-1";
const TEST_ISSUER: &str = "https://idp.example.test/";
/// The `aud` src/bin/server.rs's own local JWKS shortcut's IdpConfig
/// accepts (skilj 0.0.9 requires an explicit audience on every verified
/// JWT, docs/architecture.md §81) - the same value provisioner.rs's own
/// copy uses, so these self-signed JWTs verify there too.
const TEST_AUDIENCE: &str = "skilj-helpdesk-test-client";

fn sign_jwt(subject: &str) -> String {
    let mut header = Header::new(jsonwebtoken::Algorithm::RS256);
    header.kid = Some(TEST_KID.to_string());
    let claims = json!({
        "sub": subject,
        "iss": TEST_ISSUER,
        "aud": TEST_AUDIENCE,
        "exp": (Utc::now() + chrono::Duration::hours(1)).timestamp(),
    });
    let key = EncodingKey::from_rsa_pem(TEST_PRIVATE_KEY_PEM.as_bytes())
        .expect("the test private key PEM is well-formed");
    jsonwebtoken::encode(&header, &claims, &key).expect("signing a well-formed JWT never fails")
}

struct Config {
    base_url: String,
    /// One `EventReadToken` per entry in `MIRRORED_EVENTS`, by event type
    /// name - a `HashMap` rather than a fixed struct because the mirrored
    /// set is the `MIRRORED_EVENTS` table above and duplicating that list
    /// a third time as struct fields would be exactly the kind of
    /// independent-literal drift this crate keeps avoiding (see
    /// `helpdesk.rs`'s own `STAFF_TEAM` note).
    event_tokens: HashMap<String, String>,
    superadmin_subject: String,
}

impl Config {
    fn from_env() -> Self {
        let required = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| {
                eprintln!("{name} must be set");
                std::process::exit(1);
            })
        };
        let event_tokens = MIRRORED_EVENTS
            .iter()
            .map(|(event_type, _)| {
                (
                    event_type.to_string(),
                    // `COMPANY_SIGNED_UP_TOKEN` for `CompanySignedUp`, and
                    // so on - one env var per mirrored event type, named
                    // after it, so `MIRRORED_EVENTS` staying the single
                    // list of what's mirrored also stays the single list
                    // of what's configured.
                    required(&format!("{}_TOKEN", event_type.to_uppercase())),
                )
            })
            .collect();
        Config {
            base_url: std::env::var("SKILJ_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:3000".to_string()),
            event_tokens,
            superadmin_subject: required("REPLICATOR_SUPERADMIN_SUBJECT"),
        }
    }
}

/// Per-tenant `CommandToken` cache, keyed by tenant bounded-context name.
/// Process-local by design - see the module doc comment on why an
/// uncached mint per event is the wrong default. Not persisted: a
/// restart re-mints, which costs one row per tenant per restart and
/// nothing else.
#[derive(Default)]
struct TokenCache {
    tokens: HashMap<String, String>,
}

impl TokenCache {
    /// `Ok(())` and `Err` both leave the cache untouched, so a failed
    /// mint is retried on the next event for that tenant rather than
    /// cached as a permanent failure.
    async fn get_or_mint(
        &mut self,
        client: &reqwest::Client,
        config: &Config,
        tenant_name: &str,
    ) -> Result<String, String> {
        if let Some(token) = self.tokens.get(tenant_name) {
            return Ok(token.clone());
        }
        let query = r#"
            mutation MintTenantToken($bc: String!, $commandType: String!) {
                createCommandToken(
                    boundedContext: $bc
                    commandTypeName: $commandType
                ) {
                    id
                    secret
                }
            }
        "#;
        let jwt = sign_jwt(&config.superadmin_subject);
        let response = client
            .post(format!("{}/graphql", config.base_url))
            .bearer_auth(jwt)
            .json(&json!({
                "query": query,
                "variables": {
                    "bc": tenant_name,
                    "commandType": skilj_helpdesk::helpdesk::RECORD_TENANT_LIFECYCLE_COMMAND,
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
                return Err(format!("{errors:?}"));
            }
        }
        let minted = &body["data"]["createCommandToken"];
        let id = minted["id"].as_str().ok_or_else(|| format!("{body:?}"))?;
        let secret = minted["secret"]
            .as_str()
            .ok_or_else(|| format!("{body:?}"))?;
        // REST's own credential format, identical to what `provisioner.rs`
        // and `server.rs` already assemble from a minted token.
        let credential = format!("{id}.{secret}");
        self.tokens
            .insert(tenant_name.to_string(), credential.clone());
        Ok(credential)
    }
}

#[tokio::main]
async fn main() {
    let _telemetry = skilj_helpdesk::telemetry::init("skilj-helpdesk-lifecycle-replicator");

    let config = Config::from_env();
    let client = reqwest::Client::new();
    let mut tokens = TokenCache::default();
    println!(
        "lifecycle-replicator: polling {} every {POLL_INTERVAL:?}, mirroring {}",
        config.base_url,
        MIRRORED_EVENTS
            .iter()
            .map(|(event_type, _)| *event_type)
            .collect::<Vec<_>>()
            .join(", ")
    );

    loop {
        if let Err(e) = tick(&client, &config, &mut tokens).await {
            eprintln!("lifecycle-replicator: poll failed, will retry: {e}");
            tracing::warn!(error = %e, "lifecycle-replicator: poll failed, will retry");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn tick(
    client: &reqwest::Client,
    config: &Config,
    tokens: &mut TokenCache,
) -> Result<(), reqwest::Error> {
    for (event_type, status) in MIRRORED_EVENTS {
        let Some(token) = config.event_tokens.get(*event_type) else {
            eprintln!("lifecycle-replicator: no token configured for {event_type}");
            continue;
        };
        for (payload, _) in consume(client, &config.base_url, token).await? {
            let Some(company_id) = payload["company_id"].as_str() else {
                eprintln!(
                    "lifecycle-replicator: {event_type} payload missing company_id: {payload}"
                );
                continue;
            };
            if let Err(e) = mirror_one(client, config, tokens, company_id, event_type, status).await
            {
                // Logged, not propagated: one company's failed mirror
                // must not stop the other companies (or the other two
                // event feeds) in this same tick.
                eprintln!(
                    "lifecycle-replicator: mirroring {company_id} ({event_type}) failed: {e}"
                );
                tracing::warn!(error = %e, company_id, event_type, "lifecycle-replicator: mirror failed");
            }
        }
    }
    Ok(())
}

/// Resolve the company's tenant, then record the mirrored lifecycle fact
/// in it - the whole per-event unit of work, split out from `tick` so
/// each step's failure is attributable in a log line rather than
/// collapsed into one opaque error.
async fn mirror_one(
    client: &reqwest::Client,
    config: &Config,
    tokens: &mut TokenCache,
    company_id: &str,
    source_event_type: &str,
    status: &str,
) -> Result<(), String> {
    let tenant_name = resolve_tenant(client, config, company_id).await?;
    let credential = tokens.get_or_mint(client, config, &tenant_name).await?;
    // `POST /v1/commands/trigger` against a token minted for *this*
    // tenant, which is what makes the command land in the tenant rather
    // than the shared context: the REST handler derives its target
    // bounded context from the token's own `command_type.bounded_context`
    // (`skilj-rest`'s own `post_commands_trigger`), so the routing is the
    // credential's, not anything this binary states in its request.
    submit_command(
        client,
        &config.base_url,
        &credential,
        json!({
            "company_id": company_id,
            "status": status,
            "source_event_type": source_event_type,
        }),
    )
    .await
}

/// `company_id -> tenant_name` via the shared context's own
/// `TenantDirectory` projection - one round trip, and the same recorded
/// mapping any other router would read.
async fn resolve_tenant(
    client: &reqwest::Client,
    config: &Config,
    company_id: &str,
) -> Result<String, String> {
    let query = r#"
        query ResolveTenant($bc: String!, $name: String!, $key: String!) {
            projection(boundedContext: $bc, name: $name, key: $key) {
                ... on helpdesk_TenantDirectory {
                    tenantName
                }
            }
        }
    "#;
    let jwt = sign_jwt(&config.superadmin_subject);
    let response = client
        .post(format!("{}/graphql", config.base_url))
        .bearer_auth(jwt)
        .json(&json!({
            "query": query,
            "variables": {
                "bc": skilj_helpdesk::helpdesk::BOUNDED_CONTEXT,
                "name": skilj_helpdesk::helpdesk::TenantDirectory::NAME,
                "key": company_id,
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
            return Err(format!("{errors:?}"));
        }
    }
    body["data"]["projection"]["tenantName"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "no TenantDirectory entry for company {company_id:?} - not provisioned yet, \
                 or the provisioner hasn't recorded it"
            )
        })
}

/// `GET /v1/events/consume?mode=auto` - byte-for-byte the same shape as
/// `provisioner.rs`'s own `consume` helper (deliberately duplicated
/// rather than shared across these two binaries, the same
/// "duplicated rather than shared across the test/binary boundary"
/// convention the test key material above follows). The event type name
/// is unused here because the caller already knows which feed it's
/// reading, and `MIRRORED_EVENTS` pairs it with the status up front.
async fn consume(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<(serde_json::Value, String)>, reqwest::Error> {
    #[derive(serde::Deserialize)]
    struct ConsumeResponse {
        events: Vec<EventDto>,
    }

    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct EventDto {
        event_type: String,
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
        .map(|e| (e.payload, e.event_type))
        .collect())
}

/// `POST /v1/commands/trigger` - identical shape and "a rejection is
/// logged by the caller, not a transport error" contract as
/// `provisioner.rs`'s own `submit_command`.
async fn submit_command(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    payload: serde_json::Value,
) -> Result<(), String> {
    let response = client
        .post(format!("{base_url}/v1/commands/trigger"))
        .bearer_auth(token)
        .json(&json!({ "payload": payload }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if body["accepted"].as_bool() == Some(true) {
        Ok(())
    } else {
        // `lifecycle_already_mirrored` is the expected shape of a
        // redelivered event, not a failure - reported as a normal
        // rejection rather than an error string so it isn't logged as
        // one (see `RecordTenantLifecycle`'s own guard).
        Err(format!(
            "rejected: {}",
            body["rejectionKind"].as_str().unwrap_or("unknown")
        ))
    }
}
