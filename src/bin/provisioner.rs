//! The tenant-provisioning reactor: a separately-deployable unit, same
//! shape as `alerter.rs` - see that file's own module doc comment for
//! why this project always splits an I/O side effect out of `decide()`
//! into its own binary reacting to skilj's REST event feed, rather than
//! running it inside the command itself.
//!
//! `src/helpdesk.rs`'s own `SignUpCompany`/`RecordCompanyTenant` doc
//! comments describe the split this binary is the missing half of:
//! `SignUpCompany::decide()` stays pure and emits `CompanySignedUp`;
//! this binary reads that event, calls skilj's own
//! `createBoundedContextFromTemplate` GraphQL mutation, superadmin-gated
//! (`tests/multi_tenant_provisioning.rs` proves the exact mechanism
//! first, against a test-seeded superadmin `Role`), to stamp a
//! brand-new tenant from the `helpdesk` bounded context as the
//! template, then submits `RecordCompanyTenant` back so the result is
//! durable.
//!
//! **What this does not do**: reroute any Ticket command or query into
//! the tenant it just created. See `helpdesk.rs`'s own module doc
//! comment for why that's a separate, larger piece of work (Company's
//! own guard reads - `company_status`, read by `CreateTicket` et al. via
//! one same-context DCB query - would need to reach across two bounded
//! contexts instead of one). Every company still gets its tickets
//! handled in the shared `helpdesk` context; this binary's only job is
//! making the tenant itself real and durably recorded.
//!
//! **Restart safety, deliberately the simpler tradeoff.** Unlike
//! `alerter.rs`'s `TicketBecomesOverdue` sweep (which needs a checkpoint
//! file - see that binary's own doc comment - because losing its
//! in-memory state means losing every currently-open ticket, not just a
//! few events), a missed `CompanySignedUp` here is bounded to exactly
//! the companies that signed up during this process's own crash
//! window, the same "occasional missed events on crash is acceptable"
//! tradeoff `specs/skilj-helpdesk.allium`'s own resolved alerting
//! design note already accepts for the identical reason. No state
//! file, no in-memory tracking beyond one HTTP round trip per event.
//!
//! **Idempotent by construction anyway.** `RecordCompanyTenant::decide`
//! rejects a second recording for the same company
//! (`tenant_already_provisioned`), and skilj's own `BoundedContext.name`
//! is unique, so a redelivered `CompanySignedUp` (this token's own
//! `mode=auto` cursor can, in principle, redeliver on a crash mid-tick -
//! see `skilj-rest`'s own `docs/architecture.md` §7.4) fails safely on
//! either side rather than silently double-provisioning.
//!
//! **Untrusted input, handled as such.** `company_id` is caller-supplied
//! (a customer's own `SignUpCompany` payload - see that command's own
//! doc comment) and flows straight into this binary's own derived tenant
//! name. Unlike `tests/multi_tenant_provisioning.rs`'s own mutation
//! string (built by string interpolation - fine there, since every value
//! it interpolates is test-generated via `unique_name`, never caller
//! input), `provision_tenant` below sends `tenant_name`/`role_id` as
//! real GraphQL `variables`, not interpolated into the query document -
//! the same "don't build a query out of untrusted text" reasoning
//! `alerter.rs`'s own `escape_slack_text` doc comment gives for why
//! `send_alert` escapes caller-controlled ticket/company ids before they
//! reach Slack's own markup language.
//!
//! Configuration (env vars, deliberately minimal - same convention as
//! `alerter.rs`'s own module doc comment):
//!   SKILJ_BASE_URL                  - default "http://localhost:3000"
//!   COMPANY_SIGNED_UP_TOKEN         - this binary's own EventReadToken
//!   RECORD_COMPANY_TENANT_TOKEN     - this binary's own CommandToken
//!   PROVISIONER_SUPERADMIN_SUBJECT  - the bootstrap admin Role's own
//!                                     `external_subject` (server.rs
//!                                     prints it) - this binary signs its
//!                                     own short-lived JWT for it, local-
//!                                     JWKS-shortcut only (see this
//!                                     file's own doc comment on why a
//!                                     real Dex issuer can't be satisfied
//!                                     this way).
//!   PROVISIONER_TENANT_ADMIN_ROLE_ID - the `Role.id` granted ADMIN
//!                                     access on every tenant this binary
//!                                     provisions - the same bootstrap
//!                                     admin Role by default (server.rs
//!                                     prints it too), reusing the "one
//!                                     ops identity administers every
//!                                     bounded context" shape
//!                                     `ensure_bounded_context_and_grant`
//!                                     already establishes for `activity`/
//!                                     `marketing`.
//!
//! Telemetry: `skilj_helpdesk::telemetry::init` as service
//! `"skilj-helpdesk-provisioner"` - same OTLP opt-in as every other
//! binary here.

use chrono::Utc;
use jsonwebtoken::{EncodingKey, Header};
use serde_json::json;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

// --- local JWKS/JWT shortcut - this binary's own copy, same
// "duplicated rather than shared across the test/binary boundary"
// convention `server.rs`'s own doc comment on `TEST_PRIVATE_KEY_PEM`
// uses; never a real secret. ---
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

struct Config {
    base_url: String,
    company_signed_up_token: String,
    record_company_tenant_token: String,
    superadmin_subject: String,
    tenant_admin_role_id: String,
}

impl Config {
    fn from_env() -> Self {
        let required = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| {
                eprintln!("{name} must be set");
                std::process::exit(1);
            })
        };
        Config {
            base_url: std::env::var("SKILJ_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:3000".to_string()),
            company_signed_up_token: required("COMPANY_SIGNED_UP_TOKEN"),
            record_company_tenant_token: required("RECORD_COMPANY_TENANT_TOKEN"),
            superadmin_subject: required("PROVISIONER_SUPERADMIN_SUBJECT"),
            tenant_admin_role_id: required("PROVISIONER_TENANT_ADMIN_ROLE_ID"),
        }
    }
}

#[tokio::main]
async fn main() {
    let _telemetry = skilj_helpdesk::telemetry::init("skilj-helpdesk-provisioner");

    let config = Config::from_env();
    let client = reqwest::Client::new();
    println!("provisioner: polling {} every {POLL_INTERVAL:?}", config.base_url);

    loop {
        if let Err(e) = tick(&client, &config).await {
            eprintln!("provisioner: poll failed, will retry: {e}");
            tracing::warn!(error = %e, "provisioner: poll failed, will retry");
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

async fn tick(client: &reqwest::Client, config: &Config) -> Result<(), reqwest::Error> {
    for (payload, _) in consume(client, &config.base_url, &config.company_signed_up_token).await? {
        let Some(company_id) = payload["company_id"].as_str() else {
            eprintln!("provisioner: CompanySignedUp payload missing company_id: {payload}");
            continue;
        };
        // `helpdesk.rs`'s own `BOUNDED_CONTEXT` is the template - every
        // `CommandType`/`EventType`/`Projection` `#[auto_register]`s onto
        // it becomes this new tenant's own starting registrations
        // (`createBoundedContextFromTemplate`'s own contract - see
        // `tests/multi_tenant_provisioning.rs`).
        let tenant_name = format!("company-{company_id}");
        match provision_tenant(client, config, &tenant_name).await {
            Ok(()) => {
                println!("provisioner: provisioned tenant {tenant_name:?} for company {company_id:?}");
            }
            Err(e) => {
                eprintln!("provisioner: createBoundedContextFromTemplate for {company_id} failed: {e}");
                tracing::warn!(error = %e, company_id = %company_id, "provisioner: tenant creation failed");
                continue;
            }
        }
        if let Err(e) = submit_command(
            client,
            &config.base_url,
            &config.record_company_tenant_token,
            serde_json::json!({ "company_id": company_id, "tenant_name": tenant_name }),
        )
        .await
        {
            eprintln!("provisioner: RecordCompanyTenant for {company_id} failed: {e}");
            tracing::warn!(error = %e, company_id = %company_id, "provisioner: RecordCompanyTenant rejected/failed");
        }
    }
    Ok(())
}

/// `GET /v1/events/consume?mode=auto` - byte-for-byte the same shape as
/// `alerter.rs`'s own `consume` (event type name unused, kept for parity
/// with every other binary's identical helper), minus the timestamp this
/// binary has no use for.
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
    Ok(body.events.into_iter().map(|e| (e.payload, e.event_type)).collect())
}

/// `POST /v1/commands/trigger` - identical shape and "a rejection is
/// logged by the caller, not a transport error" contract as
/// `alerter.rs`'s own `submit_command`.
async fn submit_command(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
    payload: serde_json::Value,
) -> Result<(), String> {
    let response = client
        .post(format!("{base_url}/v1/commands/trigger"))
        .bearer_auth(token)
        .json(&serde_json::json!({ "payload": payload }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let body: serde_json::Value = response.json().await.map_err(|e| e.to_string())?;
    if body["accepted"].as_bool() == Some(true) {
        Ok(())
    } else {
        Err(format!(
            "rejected: {}",
            body["rejectionReason"].as_str().unwrap_or("unknown")
        ))
    }
}

/// The real side effect: a `createBoundedContextFromTemplate` GraphQL
/// mutation, superadmin-authenticated (`config.superadmin_subject`'s own
/// freshly-signed JWT - see this file's own module doc comment on why
/// that's local-JWKS-only), granting `config.tenant_admin_role_id`
/// `ADMIN` access on the new tenant in the same call. `tenant_name`/
/// `role_id` travel as GraphQL `variables`, never interpolated into the
/// query document - see this file's own module doc comment on why that
/// matters here specifically (`company_id`, and so `tenant_name`, is
/// ultimately caller-controlled).
async fn provision_tenant(client: &reqwest::Client, config: &Config, tenant_name: &str) -> Result<(), String> {
    let query = r#"
        mutation ProvisionTenant($template: String!, $name: String!, $roleId: ID!) {
            createBoundedContextFromTemplate(
                template: $template
                name: $name
                roleId: $roleId
                level: ADMIN
                canReadSensitive: false
            ) {
                name
                status
            }
        }
    "#;
    let jwt = sign_jwt(&config.superadmin_subject);
    let response = client
        .post(format!("{}/graphql", config.base_url))
        .bearer_auth(jwt)
        .json(&serde_json::json!({
            "query": query,
            "variables": {
                "template": skilj_helpdesk::helpdesk::BOUNDED_CONTEXT,
                "name": tenant_name,
                "roleId": config.tenant_admin_role_id,
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
    if body["data"]["createBoundedContextFromTemplate"]["status"] != "ACTIVE" {
        return Err(format!("unexpected response: {body:?}"));
    }
    Ok(())
}
