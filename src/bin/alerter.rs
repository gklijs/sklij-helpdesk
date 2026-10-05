//! The alerting service: a separately-deployable unit, on purpose - see
//! `specs/skilj-helpdesk.allium`'s own resolved design note on why this
//! is a plain consumer of skilj's existing REST event feed rather than
//! anything built into skilj itself, and `src/alerting.rs` for the pure
//! decision logic this binary just drives.
//!
//! Talks to a *running* skilj server over HTTP - `GET
//! /v1/events/consume?mode=auto`, server-tracked/auto-advancing
//! (docs/architecture.md §7.4), the "stateless worker" row of that
//! table. Not exercised by `cargo test` (that would mean spawning and
//! tearing down a real listening server, a different kind of test than
//! this crate's other integration tests, which drive `Skilj::rest_router()`
//! in-process via `tower::ServiceExt::oneshot`); `src/alerting.rs`'s own
//! tests, plus `tests/alerting_feed.rs`'s `urgent_ticket_creation_is_visible_on_the_event_feed_and_triggers_an_alert`
//! (which exercises the exact same consume-and-decode path this binary
//! uses, just in-process), are where the real coverage is.
//!
//! **Two jobs now, not one.** `rule UrgentTicketNeedsImmediateAttention`
//! (react to `TicketCreated`, page immediately) is unchanged from the
//! original pass. `rule TicketBecomesOverdue` is newly finished here -
//! `src/alerting.rs`'s own doc comment used to note its trigger
//! (`is_overdue`) was written and tested but "not wired up this pass."
//! Finishing it turned out to mean more than printing an alert: see
//! `skilj_helpdesk::helpdesk::TicketEscalated`'s own doc comment for why
//! this now submits a real `EscalateTicket` command (a documented,
//! deliberate extension of the spec, not just the original console-only
//! trigger) - `tick` below tracks each unhandled ticket's own age via a
//! tracked-state-plus-sweep shape, the same one `engagement-watcher.rs`
//! independently needs for its own "gone quiet" rule (a rolling window,
//! not a one-shot deadline - see `src/activity_scheduling.rs`'s own doc
//! comment for why that one still needs hand-rolled polling rather than
//! skilj's native `ScheduleDeadline`, unlike `helpdesk.rs`'s own trial/
//! auto-close reactors).
//!
//! **Restart safety.** `mode=auto`'s cursor is server-tracked and
//! non-replayable (docs/architecture.md §7.4) - once an event's been
//! served, it's gone from this token's own stream for good. That's
//! fine for `send_alert` below, which needs no memory at all, but
//! `rule TicketBecomesOverdue` is a *sweep* over "every currently
//! unhandled ticket," not a per-event reaction - so if `state` only
//! ever lived in this process's own memory, a restart would silently
//! and permanently drop every ticket that was already open before it,
//! forever, unless some *other* event happened to touch that same
//! ticket again later. That's a materially bigger loss than the design
//! note's own accepted "occasional missed events on crash" - that
//! tradeoff is about a handful of events landing during the crash
//! window, not the entire pre-crash world going dark. `load_state`/
//! `save_state` below close that gap: `state` is checkpointed to a
//! local JSON file after every tick (cheap - it's a handful of ticket
//! ids), and reloaded on startup if present, so a restart resumes
//! within one `POLL_INTERVAL` of where it left off instead of forgetting
//! everything. `engagement-watcher.rs` has the identical fix, for the
//! identical reason, over its own "gone quiet" sweep.
//!
//! **Phase 4: multi-tenant awareness.** When `TICKET_ROUTING=tenant`
//! is on, ticket events live in per-company tenant contexts, not in the
//! shared `helpdesk` context. The alerter now discovers tenants by
//! reading `CompanyTenantProvisioned` events off the shared context's
//! own REST feed (which is why `CompanyTenantProvisioned` carries
//! `event_read_allowed = true` in `helpdesk.rs`), then mints per-tenant
//! `EventReadToken`s for each ticket event type and a per-tenant
//! `EscalateTicket` `CommandToken` via skilj's own
//! `createEventReadToken`/`createCommandToken` GraphQL mutations,
//! signed as the same superadmin identity the provisioner and
//! lifecycle-replicator already use. Token mints are cached in memory
//! for the process lifetime (one round trip per tenant, not per event);
//! discovered tenant names are checkpointed to the state file so a
//! restart re-provisions tokens for known tenants without re-scanning.
//! The shared-context tokens from env vars still cover companies
//! without a tenant, so the fallback path is unchanged. When escalating
//! a ticket, the alerter picks the right `EscalateTicket` token by
//! matching the ticket to its tenant (tracked per `TicketCreated`).
//!
//! Configuration (env vars, deliberately minimal - no config-loading
//! crate, matching skilj's own §2.4 choice):
//!   SKILJ_BASE_URL               - default "http://localhost:3000"
//!   UNHANDLED_ALERT_AFTER_HOURS  - default 4 (matches the spec's
//!                                  `config.unhandled_alert_after`) - set
//!                                  to `0` to escalate overdue tickets
//!                                  immediately, for a demo.
//!   ALERTER_STATE_FILE           - default "alerter-state.json"
//!                                  (relative to CWD - a real deployment
//!                                  should point this at a persistent
//!                                  volume, or this binary is back to
//!                                  losing state on every restart/
//!                                  redeploy) - set to an empty string
//!                                  to disable checkpointing entirely
//!                                  and go back to pure in-memory state.
//!   Six EventReadTokens ("id.secret"), each this binary's own - see
//!   `server.rs`'s own `ALERTER_EVENT_TYPES` doc comment:
//!     TICKET_CREATED_TOKEN, TICKET_RESOLVED_TOKEN, TICKET_REOPENED_TOKEN,
//!     TICKET_CLOSED_TOKEN, TICKET_ESCALATED_TOKEN, TICKETS_MERGED_TOKEN
//!   One CommandToken:
//!     ESCALATE_TICKET_TOKEN
//!   All six EventReadTokens and the CommandToken above are for the
//!   *shared* `helpdesk` context - they cover companies without a
//!   tenant. Per-tenant tokens are minted on demand and cached in
//!   memory.
//!   TICKET_ROUTING                - optional; set to "tenant" to enable
//!                                  Phase 4 multi-tenant discovery and
//!                                  polling. Requires the two tokens
//!                                  below.
//!   COMPANY_TENANT_PROVISIONED_TOKEN - optional; required when
//!                                  TICKET_ROUTING=tenant. EventReadToken
//!                                  for `CompanyTenantProvisioned` on
//!                                  the shared context, used to discover
//!                                  which companies have been provisioned
//!                                  a tenant.
//!   ALERTER_SUPERADMIN_SUBJECT    - optional; required when
//!                                  TICKET_ROUTING=tenant. The bootstrap
//!                                  admin Role's own `external_subject`
//!                                  (same one `server.rs` prints for the
//!                                  provisioner and lifecycle-replicator),
//!                                  which this binary signs its own
//!                                  short-lived JWT for, local-JWKS-
//!                                  shortcut only, to call
//!                                  `createEventReadToken`/`createCommandToken`
//!                                  on each tenant.
//!   SLACK_WEBHOOK_URL             - optional; unset means console-only
//!                                  (the original behaviour, and still
//!                                  what every alert does regardless).
//!
//! What actually happens on an alert was deliberately just a `println!`,
//! the real channel (email, Slack, PagerDuty, ...) being exactly the
//! piece `specs/skilj-helpdesk.allium`'s Excludes section leaves open:
//! "the alerting service's concern, not this spec's". `send_alert`
//! below is now a *worked example* of swapping that placeholder for a
//! real one (Slack's own incoming-webhook API - a POST of `{"text":
//! ...}`, no SDK needed), proving the seam the doc comment above used
//! to only describe actually works - console output stays unconditional
//! either way, Slack is additive when `SLACK_WEBHOOK_URL` is set. A
//! second real channel (PagerDuty, say) would plug in the exact same
//! way, right alongside it.
//!
//! Telemetry: `skilj_helpdesk::telemetry::init` (see that module's own
//! doc comment) as service `"skilj-helpdesk-alerter"` - same OTLP
//! opt-in as `server.rs`. This binary already loops forever with no
//! graceful-shutdown path, so `_telemetry` below is just kept alive for
//! `main`'s own lifetime rather than explicitly torn down; the OTLP
//! batch exporters still flush periodically on their own.

use chrono::{DateTime, Utc};
use jsonwebtoken::{EncodingKey, Header};
use serde::{Deserialize, Serialize};
use serde_json::json;
use skilj_helpdesk::alerting::{evaluate_ticket_created, is_overdue};
use skilj_helpdesk::helpdesk::TicketCreatedPayload;
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::Duration;

const POLL_INTERVAL: Duration = Duration::from_secs(5);

/// Every ticket event type this alerter consumes, by wire name. Used both
/// for the shared-context tokens (from env vars) and for minting the
/// same set against each tenant via GraphQL. A single source of truth so
/// the two never drift.
const ALERTER_EVENT_TYPES: &[&str] = &[
    "TicketCreated",
    "TicketResolved",
    "TicketReopened",
    "TicketClosed",
    "TicketEscalated",
    "TicketsMerged",
];

/// The one command type this alerter submits - `EscalateTicket`. Same
/// single-name-as-constant reasoning `helpdesk.rs` gives for
/// `RECORD_TENANT_LIFECYCLE_COMMAND`: a `CommandToken` is minted by type
/// name, not by Rust type, so the literal lives in one place.
const ESCALATE_TICKET_COMMAND: &str = "EscalateTicket";

// --- local JWKS/JWT shortcut - same test key material as every other
// binary in this crate (server.rs, provisioner.rs, lifecycle-replicator.rs,
// tests/support/mod.rs). Never a real secret. Duplicated rather than
// shared across the binary boundary, same reasoning those files give. ---

const TEST_PRIVATE_KEY_PEM: &str = "-----BEGIN PRIVATE KEY-----
MIIEvQIBATANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQDPHVFsUHiWXSbG
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
-----END PRIVATE KEY-----";
const TEST_KID: &str = "test-key-1";
const TEST_ISSUER: &str = "https://idp.example.test/";
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
    ticket_created_token: String,
    ticket_resolved_token: String,
    ticket_reopened_token: String,
    ticket_closed_token: String,
    ticket_escalated_token: String,
    tickets_merged_token: String,
    escalate_ticket_token: String,
    unhandled_alert_after: chrono::Duration,
    /// `None` when `ALERTER_STATE_FILE` is set to an empty string -
    /// checkpointing opted out of, back to pure in-memory `state`.
    state_file: Option<PathBuf>,
    /// `None` when `SLACK_WEBHOOK_URL` is unset - `send_alert` below
    /// then stays console-only, exactly the original behaviour.
    slack_webhook_url: Option<String>,
    /// `Some` when `TICKET_ROUTING=tenant` *and* `ALERTER_SUPERADMIN_SUBJECT`
    /// and `COMPANY_TENANT_PROVISIONED_TOKEN` are set - enables Phase 4
    /// multi-tenant discovery and polling. `None` otherwise, and the
    /// alerter reads only the shared context, exactly as before Phase 4.
    multi_tenant: Option<MultiTenantConfig>,
}

struct MultiTenantConfig {
    superadmin_subject: String,
    company_tenant_provisioned_token: String,
}

impl Config {
    fn from_env() -> Self {
        let required = |name: &str| {
            std::env::var(name).unwrap_or_else(|_| {
                eprintln!("{name} must be set");
                std::process::exit(1);
            })
        };
        let hours: i64 = std::env::var("UNHANDLED_ALERT_AFTER_HOURS")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4);
        let state_file = match std::env::var("ALERTER_STATE_FILE") {
            Ok(s) if s.is_empty() => None,
            Ok(s) => Some(PathBuf::from(s)),
            Err(_) => Some(PathBuf::from("alerter-state.json")),
        };
        let routing_mode = skilj_helpdesk::routing::RoutingMode::from_env_value(
            std::env::var("TICKET_ROUTING").ok().as_deref(),
        );
        let superadmin_subject = std::env::var("ALERTER_SUPERADMIN_SUBJECT").ok();
        let company_tenant_provisioned_token =
            std::env::var("COMPANY_TENANT_PROVISIONED_TOKEN").ok();
        let multi_tenant = match (
            routing_mode,
            superadmin_subject,
            &company_tenant_provisioned_token,
        ) {
            (skilj_helpdesk::routing::RoutingMode::Tenant, Some(subj), Some(_)) => {
                Some(MultiTenantConfig {
                    superadmin_subject: subj,
                    company_tenant_provisioned_token: company_tenant_provisioned_token.unwrap(),
                })
            }
            _ => None,
        };
        Config {
            base_url: std::env::var("SKILJ_BASE_URL")
                .unwrap_or_else(|_| "http://localhost:3000".to_string()),
            ticket_created_token: required("TICKET_CREATED_TOKEN"),
            ticket_resolved_token: required("TICKET_RESOLVED_TOKEN"),
            ticket_reopened_token: required("TICKET_REOPENED_TOKEN"),
            ticket_closed_token: required("TICKET_CLOSED_TOKEN"),
            ticket_escalated_token: required("TICKET_ESCALATED_TOKEN"),
            tickets_merged_token: required("TICKETS_MERGED_TOKEN"),
            escalate_ticket_token: required("ESCALATE_TICKET_TOKEN"),
            unhandled_alert_after: chrono::Duration::hours(hours),
            state_file,
            slack_webhook_url: std::env::var("SLACK_WEBHOOK_URL")
                .ok()
                .filter(|s| !s.is_empty()),
            multi_tenant,
        }
    }
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    /// ticket_id -> its own original `TicketCreated` timestamp - read
    /// off skilj's own event metadata, not a payload field, since
    /// `TicketCreatedPayload` carries no timestamp of its own. Kept
    /// forever, even past resolution: a reopened ticket's age is still
    /// measured from its own original creation, never reset.
    created_at: HashMap<String, DateTime<Utc>>,
    /// ticket_id -> company_id, populated alongside `created_at` - so an
    /// overdue-escalation alert (unlike an urgent-on-creation one, which
    /// already has this straight off `TicketCreated`'s own payload) can
    /// still report which company it's for.
    company_id: HashMap<String, String>,
    /// ticket_id -> the tenant this ticket lives in, if known. `None`
    /// means the shared `helpdesk` context (either no tenant was
    /// provisioned for the company, or the cutover isn't routing there
    /// yet). This is what decides which `EscalateTicket` token gets
    /// used when escalating - the command must land in the same context
    /// the ticket does.
    tenant_for_ticket: HashMap<String, Option<String>>,
    /// `specs/skilj-helpdesk.allium`'s own
    /// `unhandled: status not in {resolved, closed}` derived field, tracked
    /// directly - `merged` counts as handled too, for the same "nothing
    /// left to do" reason `TicketFacts`'s own catch-all treatment in
    /// `helpdesk.rs` gives it.
    unhandled: HashSet<String>,
    /// Ticket ids already escalated (this alerter's own submission, or
    /// read back off the same `TicketEscalated` stream another instance
    /// produced) - stops resubmitting `EscalateTicket` every poll once
    /// it's already been done.
    escalated: HashSet<String>,
    /// Tenant bounded-context names this process has learned about via
    /// `CompanyTenantProvisioned` events. Persisted to the state file
    /// so a restart re-mints tokens for known tenants without needing
    /// to re-scan the shared feed from the beginning (the `mode=auto`
    /// cursor on that token has already advanced past historical
    /// events).
    discovered_tenants: HashSet<String>,
}

/// Per-tenant credential cache, minted on demand via skilj's own
/// `createEventReadToken`/`createCommandToken` GraphQL mutations, signed
/// as the configured superadmin identity. Process-local by design - see
/// the "Phase 4" section of this file's own module doc comment. Not
/// persisted: a restart re-mints, which costs one round trip per tenant
/// per restart and nothing else.
struct TenantTokenCache {
    base_url: String,
    superadmin_subject: String,
    /// tenant_name -> (per-event-type event read tokens, EscalateTicket command token)
    cache: HashMap<String, TenantTokens>,
}

#[derive(Clone)]
struct TenantTokens {
    event_tokens: HashMap<String, String>,
    escalate_ticket_token: String,
}

impl TenantTokenCache {
    fn new(base_url: &str, superadmin_subject: &str) -> Self {
        TenantTokenCache {
            base_url: base_url.to_string(),
            superadmin_subject: superadmin_subject.to_string(),
            cache: HashMap::new(),
        }
    }

    /// Mint all tenant-scoped tokens for `tenant_name` in one batch, cache,
    /// and return a clone. A failed mint leaves the cache untouched, so a
    /// transient GraphQL failure doesn't permanently strand a tenant.
    async fn get_or_mint(
        &mut self,
        client: &reqwest::Client,
        tenant_name: &str,
    ) -> Result<TenantTokens, String> {
        if let Some(tokens) = self.cache.get(tenant_name) {
            return Ok(tokens.clone());
        }
        let jwt = sign_jwt(&self.superadmin_subject);
        let mut event_tokens = HashMap::with_capacity(ALERTER_EVENT_TYPES.len());
        for event_type in ALERTER_EVENT_TYPES {
            let token =
                mint_graphql_event_token(client, &self.base_url, &jwt, tenant_name, event_type)
                    .await?;
            event_tokens.insert(event_type.to_string(), token);
        }
        let escalate_ticket_token = mint_graphql_command_token(
            client,
            &self.base_url,
            &jwt,
            tenant_name,
            ESCALATE_TICKET_COMMAND,
        )
        .await?;
        let tokens = TenantTokens {
            event_tokens,
            escalate_ticket_token,
        };
        self.cache.insert(tenant_name.to_string(), tokens.clone());
        Ok(tokens)
    }
}

#[tokio::main]
async fn main() {
    let _telemetry = skilj_helpdesk::telemetry::init("skilj-helpdesk-alerter");

    let config = Config::from_env();
    let client = reqwest::Client::new();
    let mut state = match &config.state_file {
        Some(path) => load_state(path),
        None => State::default(),
    };
    let mut tenant_cache = match &config.multi_tenant {
        Some(mt) => Some(TenantTokenCache::new(
            &config.base_url,
            &mt.superadmin_subject,
        )),
        None => None,
    };
    println!(
        "alerter: polling {} every {POLL_INTERVAL:?} (escalating tickets unhandled for {:?})",
        config.base_url, config.unhandled_alert_after
    );
    if config.multi_tenant.is_some() {
        println!(
            "alerter: multi-tenant mode ON (TICKET_ROUTING=tenant) - discovering and polling per-company \
             tenant feeds alongside the shared context"
        );
    }

    loop {
        if let Err(e) = tick(&client, &config, &mut state, &mut tenant_cache).await {
            eprintln!("alerter: poll failed, will retry: {e}");
            tracing::warn!(error = %e, "alerter: poll failed, will retry");
        }
        if let Some(path) = &config.state_file {
            save_state(path, &state);
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn load_state(path: &std::path::Path) -> State {
    match std::fs::read_to_string(path) {
        Ok(contents) => match serde_json::from_str(&contents) {
            Ok(state) => {
                println!("alerter: resumed tracking state from {}", path.display());
                state
            }
            Err(e) => {
                eprintln!(
                    "alerter: {} exists but couldn't be parsed ({e}) - starting fresh",
                    path.display()
                );
                State::default()
            }
        },
        Err(_) => State::default(),
    }
}

fn save_state(path: &std::path::Path, state: &State) {
    let tmp = path.with_extension("json.tmp");
    let write = std::fs::write(
        &tmp,
        serde_json::to_vec(state).expect("State always serializes"),
    )
    .and_then(|()| std::fs::rename(&tmp, path));
    if let Err(e) = write {
        eprintln!(
            "alerter: couldn't checkpoint state to {}: {e}",
            path.display()
        );
    }
}

async fn tick(
    client: &reqwest::Client,
    config: &Config,
    state: &mut State,
    tenant_cache: &mut Option<TenantTokenCache>,
) -> Result<(), reqwest::Error> {
    // --- Phase 4: discover new tenants from the shared context ---
    if let Some(mt) = &config.multi_tenant {
        for (_, payload, _) in consume(
            client,
            &config.base_url,
            &mt.company_tenant_provisioned_token,
        )
        .await?
        {
            if let Some(tenant_name) = payload["tenant_name"].as_str() {
                state.discovered_tenants.insert(tenant_name.to_string());
            }
        }
    }

    // --- shared context feeds (companies without a tenant) ---
    process_ticket_created(client, config, state, None, &config.ticket_created_token).await?;
    process_state_update(
        client,
        config,
        state,
        None,
        &config.ticket_resolved_token,
        |state, ticket_id| {
            state.unhandled.remove(ticket_id);
        },
    )
    .await?;
    process_state_update(
        client,
        config,
        state,
        None,
        &config.ticket_reopened_token,
        |state, ticket_id| {
            state.unhandled.insert(ticket_id.to_string());
        },
    )
    .await?;
    process_state_update(
        client,
        config,
        state,
        None,
        &config.ticket_closed_token,
        |state, ticket_id| {
            state.unhandled.remove(ticket_id);
        },
    )
    .await?;
    process_ticket_escalated(client, config, state, None, &config.ticket_escalated_token).await?;
    process_tickets_merged(client, config, state, None, &config.tickets_merged_token).await?;

    // --- per-tenant feeds (Phase 4: multi-tenant) ---
    if let Some(cache) = tenant_cache.as_mut() {
        // Snapshot discovered tenants so we can iterate while mutating state.
        let tenants: Vec<String> = state.discovered_tenants.iter().cloned().collect();
        for tenant_name in tenants {
            let tokens = match cache.get_or_mint(client, &tenant_name).await {
                Ok(t) => t,
                Err(e) => {
                    eprintln!("alerter: failed to mint tokens for tenant {tenant_name}: {e}");
                    continue;
                }
            };
            let src = Some(tenant_name.as_str());
            process_ticket_created(
                client,
                config,
                state,
                src,
                &tokens.event_tokens["TicketCreated"],
            )
            .await?;
            process_state_update(
                client,
                config,
                state,
                src,
                &tokens.event_tokens["TicketResolved"],
                |state, ticket_id| {
                    state.unhandled.remove(ticket_id);
                },
            )
            .await?;
            process_state_update(
                client,
                config,
                state,
                src,
                &tokens.event_tokens["TicketReopened"],
                |state, ticket_id| {
                    state.unhandled.insert(ticket_id.to_string());
                },
            )
            .await?;
            process_state_update(
                client,
                config,
                state,
                src,
                &tokens.event_tokens["TicketClosed"],
                |state, ticket_id| {
                    state.unhandled.remove(ticket_id);
                },
            )
            .await?;
            process_ticket_escalated(
                client,
                config,
                state,
                src,
                &tokens.event_tokens["TicketEscalated"],
            )
            .await?;
            process_tickets_merged(
                client,
                config,
                state,
                src,
                &tokens.event_tokens["TicketsMerged"],
            )
            .await?;
        }
    }

    // --- act on the deadline: rule TicketBecomesOverdue ---
    let now = Utc::now();
    let due: Vec<String> = state
        .unhandled
        .iter()
        .filter(|ticket_id| !state.escalated.contains(ticket_id.as_str()))
        .filter_map(|ticket_id| {
            let created_at = state.created_at.get(ticket_id)?;
            is_overdue(*created_at, now, config.unhandled_alert_after).then(|| ticket_id.clone())
        })
        .collect();
    for ticket_id in due {
        // Phase 4: route the escalation command to the tenant the ticket
        // lives in, if it has one. REST derives its destination from the
        // token's own bounded context, so using a tenant-scoped token is
        // what lands the command in the tenant.
        let tenant = state.tenant_for_ticket.get(&ticket_id).cloned().flatten();
        let escalate_token: &str = if let Some(ref tenant_name) = tenant {
            if let Some(cache) = tenant_cache.as_ref() {
                match cache.cache.get(tenant_name) {
                    Some(t) => &t.escalate_ticket_token,
                    None => {
                        eprintln!(
                            "alerter: no EscalateTicket token for tenant {tenant_name}, \
                             falling back to shared"
                        );
                        &config.escalate_ticket_token
                    }
                }
            } else {
                &config.escalate_ticket_token
            }
        } else {
            &config.escalate_ticket_token
        };
        match submit_command(
            client,
            &config.base_url,
            escalate_token,
            serde_json::json!({ "ticket_id": ticket_id }),
        )
        .await
        {
            Ok(()) => {
                send_alert(
                    client,
                    config.slack_webhook_url.as_deref(),
                    &skilj_helpdesk::alerting::Alert {
                        ticket_id: ticket_id.clone(),
                        company_id: state
                            .company_id
                            .get(&ticket_id)
                            .cloned()
                            .unwrap_or_default(),
                        reason: skilj_helpdesk::alerting::AlertReason::Overdue,
                    },
                )
                .await;
                state.escalated.insert(ticket_id);
            }
            Err(e) => {
                eprintln!("alerter: EscalateTicket for ticket {ticket_id} failed: {e}");
                tracing::warn!(error = %e, ticket_id = %ticket_id, "alerter: EscalateTicket rejected/failed");
            }
        }
    }

    Ok(())
}

/// Process a batch of `TicketCreated` events from one source (shared
/// context or a tenant). Tags each ticket with `source` so the overdue
/// sweep above can pick the right `EscalateTicket` token.
async fn process_ticket_created(
    client: &reqwest::Client,
    config: &Config,
    state: &mut State,
    source: Option<&str>,
    token: &str,
) -> Result<(), reqwest::Error> {
    for (_, payload, created_at) in consume(client, &config.base_url, token).await? {
        match serde_json::from_value::<TicketCreatedPayload>(payload) {
            Ok(p) => {
                state.created_at.insert(p.ticket_id.clone(), created_at);
                state
                    .company_id
                    .insert(p.ticket_id.clone(), p.company_id.clone());
                state
                    .tenant_for_ticket
                    .insert(p.ticket_id.clone(), source.map(str::to_string));
                state.unhandled.insert(p.ticket_id.clone());
                if let Some(alert) = evaluate_ticket_created(&p) {
                    send_alert(client, config.slack_webhook_url.as_deref(), &alert).await;
                }
            }
            Err(e) => eprintln!("alerter: couldn't decode TicketCreated payload: {e}"),
        }
    }
    Ok(())
}

/// Process one event type that only updates ticket state (resolve,
/// reopen, close) - removes/adds the ticket from `unhandled` based on
/// `op`. Generic over the specific mutation because the three cases are
/// identical in shape (consume one event type, touch the `unhandled`
/// set), differing only in which set operation runs.
async fn process_state_update<F>(
    client: &reqwest::Client,
    config: &Config,
    state: &mut State,
    _source: Option<&str>,
    token: &str,
    op: F,
) -> Result<(), reqwest::Error>
where
    F: Fn(&mut State, &str),
{
    for (_, payload, _) in consume(client, &config.base_url, token).await? {
        if let Some(ticket_id) = payload["ticket_id"].as_str() {
            op(state, ticket_id);
        }
    }
    Ok(())
}

async fn process_ticket_escalated(
    client: &reqwest::Client,
    config: &Config,
    state: &mut State,
    _source: Option<&str>,
    token: &str,
) -> Result<(), reqwest::Error> {
    for (_, payload, _) in consume(client, &config.base_url, token).await? {
        if let Some(ticket_id) = payload["ticket_id"].as_str() {
            state.escalated.insert(ticket_id.to_string());
        }
    }
    Ok(())
}

async fn process_tickets_merged(
    client: &reqwest::Client,
    config: &Config,
    state: &mut State,
    _source: Option<&str>,
    token: &str,
) -> Result<(), reqwest::Error> {
    for (_, payload, _) in consume(client, &config.base_url, token).await? {
        if let Some(duplicate_ticket_id) = payload["duplicate_ticket_id"].as_str() {
            state.unhandled.remove(duplicate_ticket_id);
        }
    }
    Ok(())
}

/// One `GET /v1/events/consume?mode=auto` call for one token, decoded
/// down to what this binary needs: each event's type name (unused by
/// most call sites - each token is already scoped to one event type -
/// kept for parity with `engagement-watcher.rs`'s identical helper),
/// payload, and when skilj itself recorded it. Byte-for-byte the same
/// shape as `engagement-watcher.rs`'s own `consume` - not shared between
/// the two binaries since each is its own deployable unit with no common
/// library boundary between them worth introducing for one helper.
async fn consume(
    client: &reqwest::Client,
    base_url: &str,
    token: &str,
) -> Result<Vec<(String, serde_json::Value, DateTime<Utc>)>, reqwest::Error> {
    #[derive(serde::Deserialize)]
    struct ConsumeResponse {
        events: Vec<EventDto>,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct EventDto {
        event_type: String,
        payload: serde_json::Value,
        metadata: Metadata,
    }
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Metadata {
        created_at: DateTime<Utc>,
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
        .map(|e| (e.event_type, e.payload, e.metadata.created_at))
        .collect())
}

/// One `POST /v1/commands/trigger` call - identical shape and "a
/// rejection is logged by the caller, not a transport error" contract as
/// `engagement-watcher.rs`'s own `submit_command`.
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

/// Call `createEventReadToken` on the skilj GraphQL surface, authenticated
/// as the superadmin identity, to mint a per-tenant `EventReadToken`.
/// Returns the `"id.secret"` credential string. Byte-for-byte the same
/// shape as `lifecycle-replicator.rs`'s own `get_or_mint`, adapted for
/// the event-read-token variant of the mutation (deliberately
/// duplicated rather than shared across binaries, the same convention the
/// test key material follows).
async fn mint_graphql_event_token(
    client: &reqwest::Client,
    base_url: &str,
    jwt: &str,
    bounded_context: &str,
    event_type_name: &str,
) -> Result<String, String> {
    let query = r#"
        mutation MintEventToken($bc: String!, $eventTypeName: String!) {
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
        .json(&json!({
            "query": query,
            "variables": {
                "bc": bounded_context,
                "eventTypeName": event_type_name,
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

/// Call `createCommandToken` on the skilj GraphQL surface - identical
/// shape to `lifecycle-replicator.rs`'s own `get_or_mint` mutation body
/// (deliberately duplicated rather than shared across binaries, the same
/// convention the test key material follows).
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
        .json(&json!({
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

/// Console output (unconditional) plus, when `webhook_url` is set, a
/// real Slack post - the worked example this file's own module doc
/// comment describes. A rejected/unreachable webhook is logged and
/// swallowed, not propagated: a broken Slack integration should degrade
/// this binary back to console-only, never take the whole poll loop
/// down (same "a failed checkpoint shouldn't stop `tick`" reasoning
/// `save_state` already gets).
async fn send_alert(
    client: &reqwest::Client,
    webhook_url: Option<&str>,
    alert: &skilj_helpdesk::alerting::Alert,
) {
    println!(
        "ALERT [{:?}]: ticket {} (company {}) needs a lead's attention",
        alert.reason, alert.ticket_id, alert.company_id
    );
    tracing::info!(
        reason = ?alert.reason,
        ticket_id = %alert.ticket_id,
        company_id = %alert.company_id,
        "alerter: paged a lead"
    );

    let Some(webhook_url) = webhook_url else {
        return;
    };
    // Slack's own incoming-webhook contract: a bare `{"text": ...}`
    // POST, no SDK, no auth beyond the URL itself being the secret -
    // see https://api.slack.com/messaging/webhooks. `mrkdwn` (Slack's
    // own dialect, not real Markdown) for the ticket id, so it renders
    // as inline code in the channel rather than plain text.
    //
    // `escape_slack_text` on both fields is load-bearing, not
    // decoration: `ticket_id`/`company_id` are plain, unvalidated
    // `String`s a customer fully controls via an ordinary `CreateTicket`
    // request (`helpdesk_rs`'s own `CreateTicketPayload`) - without
    // escaping, a `ticket_id` containing Slack's own link/mention
    // syntax (backtick-then-`<!channel>`, or a masked `<https://`
    // evil.example|...>` link) would render *live* in the support
    // team's own trusted alerting channel: a real message-injection/
    // phishing vector a security review caught, not a formatting nicety.
    let text = format!(
        "*[{:?}]* ticket `{}` (company `{}`) needs a lead's attention",
        alert.reason,
        escape_slack_text(&alert.ticket_id),
        escape_slack_text(&alert.company_id)
    );
    let result = client
        .post(webhook_url)
        .json(&serde_json::json!({ "text": text }))
        .send()
        .await
        .and_then(|response| response.error_for_status());
    if let Err(e) = result {
        eprintln!("alerter: failed to post Slack alert: {e}");
        tracing::warn!(error = %e, ticket_id = %alert.ticket_id, "alerter: failed to post Slack alert");
    }
}

/// Slack's own documented escaping for text sent through its API
/// (https://api.slack.com/reference/surfaces/formatting#escaping) -
/// `&` first, so it doesn't double-escape the entities `<`/`>` just
/// became. This is what stops a caller-controlled string from being
/// interpreted as Slack's own link/mention syntax (`<...>`) once it
/// lands in a `text` field - see `send_alert`'s own call site for why
/// that matters here specifically.
fn escape_slack_text(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
