//! Tenant email and explicit cross-company staff access, with real PostgreSQL,
//! NATS, HTTP and bridge restarts. Kept in a separate binary so its live
//! application pools do not accumulate with the reconciler's eight fixtures.
mod support;
use skilj_core::{access_control::AccessLevel, db};
use skilj_helpdesk::{helpdesk::BOUNDED_CONTEXT, tenant_access::reconcile_company_access};
use std::sync::LazyLock;
use support::{
    accepted, graphql_request, mint_command_token, runtime, seed_role, seed_scoped_mapping,
    seed_superadmin, setup_graphql, sign_jwt, test_db, trigger, unique_name,
};
static EXCLUSIVE: LazyLock<tokio::sync::Mutex<()>> = LazyLock::new(|| tokio::sync::Mutex::new(()));
async fn exclusive() -> tokio::sync::MutexGuard<'static, ()> {
    EXCLUSIVE.lock().await
}
struct Fixture {
    company_id: String,
    tenant_name: String,
    ops_role: skilj_core::access_control::Role,
}

/// Signs a company up, records a tenant for it, and creates that tenant
/// for real - the same three steps production makes, so the reconciler
/// has something realistic to reconcile.
async fn fixture() -> (axum::Router, axum::Router, db::Pool, Fixture) {
    let (skilj, pool, _mapping, _jwt) = setup_graphql().await;
    let graphql_router = skilj.graphql_router().await.unwrap();
    let rest_router = skilj.rest_router();

    let sign_up = mint_command_token(&pool, &_mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
    let record_tenant =
        mint_command_token(&pool, &_mapping, BOUNDED_CONTEXT, "RecordCompanyTenant").await;
    // Deliberately a `company_id` that the old
    // `format!("company-{company_id}")` scheme could not have named:
    // `unique_name`'s underscore+digest form contains characters a
    // bounded context name forbids, which is exactly why
    // `routing::tenant_name_for` now derives this instead. Deriving the
    // same way the provisioner does is also what keeps this test's tenant
    // name and production's the same string.
    let company_id = format!("acme-corp-{}", unique_name("x"));
    let tenant_name = skilj_helpdesk::routing::tenant_name_for(&company_id);
    trigger(
        &rest_router,
        &sign_up,
        serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
    )
    .await;
    let response = trigger(
        &rest_router,
        &record_tenant,
        serde_json::json!({ "company_id": company_id, "tenant_name": tenant_name }),
    )
    .await;
    assert!(
        accepted(&response),
        "recording the tenant should succeed: {response:?}"
    );

    // The ops identity: superadmin, so it may both create the tenant and
    // act as the reconciler's caller. This is deliberately the *same*
    // role, mirroring `server.rs`'s own bootstrap where one role holds
    // both.
    let ops_role = seed_superadmin(&pool).await;
    let ops_jwt = sign_jwt(&ops_role.external_subject);
    let mutation = format!(
        r#"mutation {{
            createBoundedContextFromTemplate(
                template: {template:?} name: {tenant_name:?} roleId: {role_id:?}
                level: ADMIN canReadSensitive: true
            ) {{ name }}
        }}"#,
        template = BOUNDED_CONTEXT,
        tenant_name = tenant_name,
        role_id = ops_role.id,
    );
    let response = graphql_request(&graphql_router, &ops_jwt, &mutation).await;
    assert!(
        response.get("errors").is_none(),
        "createBoundedContextFromTemplate should succeed: {response:?}"
    );

    (
        graphql_router,
        rest_router,
        pool,
        Fixture {
            company_id,
            tenant_name,
            ops_role,
        },
    )
}

#[test]
fn cross_company_staff_is_opted_in_discoverable_and_revocable() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let _guard = exclusive().await;
        let (_graphql, _rest, pool, fixture) = fixture().await;
        let staff = seed_role(&pool, "staff").await;
        let mut authority = seed_scoped_mapping(&pool, &staff, AccessLevel::Write, None).await;
        // Holding staff alone must not silently broaden the grant.
        let before = reconcile_company_access(
            &pool,
            &fixture.ops_role,
            &fixture.company_id,
            &fixture.tenant_name,
        )
        .await;
        assert!(before.granted.is_empty(), "{before:?}");
        db::revoke_active_role_access_mapping(
            &pool,
            &staff.id,
            BOUNDED_CONTEXT,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        authority.capabilities.insert("cross-company".into());
        db::insert_role_access_mapping(&pool, &authority)
            .await
            .unwrap();
        let outcome = reconcile_company_access(
            &pool,
            &fixture.ops_role,
            &fixture.company_id,
            &fixture.tenant_name,
        )
        .await;
        assert_eq!(outcome.granted, vec![staff.id.clone()], "{outcome:?}");
        let directory = skilj_helpdesk::company_directory::authorized_companies(
            &pool,
            &staff,
            skilj_helpdesk::routing::RoutingMode::Tenant,
        )
        .await
        .unwrap();
        assert!(directory
            .iter()
            .any(|entry| entry.company_id == fixture.company_id
                && entry.bounded_context == fixture.tenant_name));
        // Discovery is authenticated independently of GraphQL. A valid
        // signature for an unknown role must not turn into anonymous access.
        let jwks_url = support::serve_jwks().await;
        let idp = support::test_idp_config(&jwks_url);
        let router = skilj_helpdesk::company_directory::router(std::sync::Arc::new(
            skilj_helpdesk::company_directory::DirectoryState {
                pool: pool.clone(),
                keys: skilj_core::access_control::JwksCache::new(idp.jwks_endpoint.clone()),
                idp,
                mode: skilj_helpdesk::routing::RoutingMode::Tenant,
            },
        ));
        use tower::ServiceExt;
        for token in [None, Some(sign_jwt("unknown-subject"))] {
            let mut request = axum::http::Request::builder().uri("/v1/helpdesk/companies");
            if let Some(token) = token {
                request = request.header("authorization", format!("Bearer {token}"));
            }
            let response = router
                .clone()
                .oneshot(request.body(axum::body::Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), axum::http::StatusCode::UNAUTHORIZED);
        }
        let response = router
            .oneshot(
                axum::http::Request::builder()
                    .uri("/v1/helpdesk/companies")
                    .header(
                        "authorization",
                        format!("Bearer {}", sign_jwt(&staff.external_subject)),
                    )
                    .body(axum::body::Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        db::revoke_active_role_access_mapping(
            &pool,
            &staff.id,
            BOUNDED_CONTEXT,
            chrono::Utc::now(),
        )
        .await
        .unwrap();
        let revoked = reconcile_company_access(
            &pool,
            &fixture.ops_role,
            &fixture.company_id,
            &fixture.tenant_name,
        )
        .await;
        assert_eq!(revoked.revoked, vec![staff.id.clone()]);
        let directory = skilj_helpdesk::company_directory::authorized_companies(
            &pool,
            &staff,
            skilj_helpdesk::routing::RoutingMode::Tenant,
        )
        .await
        .unwrap();
        assert!(!directory
            .iter()
            .any(|entry| entry.company_id == fixture.company_id));
    });
}

#[test]
fn tenant_email_creates_replies_and_redelivers_without_splitting_history() {
    runtime().block_on(async {
        let Some((database_url, _)) = test_db().await else { return; };
        let _guard = exclusive().await;
        use testcontainers_modules::{nats::{Nats, NatsServerCmd}, testcontainers::{runners::AsyncRunner, ImageExt}};
        use skilj_helpdesk::email_channel::{self, InboundEmail};
        use std::time::Duration;
        let nats = match Nats::default().with_cmd(&NatsServerCmd::default().with_jetstream()).start().await {
            Ok(container) => container,
            Err(error) => { eprintln!("skipping: NATS unavailable: {error}"); return; }
        };
        let nats_url = format!("nats://{}:{}", nats.get_host().await.unwrap(), nats.get_host_port_ipv4(4222).await.unwrap());
        let (_other_graphql, _other_rest, _other_pool, other) = fixture().await;
        let (_graphql, rest, pool, fixture) = fixture().await;
        let mapping = db::get_active_role_access_mapping(&pool, &fixture.ops_role.id, &fixture.tenant_name).await.unwrap().unwrap();
        let mirror = mint_command_token(&pool, &mapping, &fixture.tenant_name, "RecordTenantLifecycle").await;
        assert!(accepted(&trigger(&rest, &mirror, serde_json::json!({
            "company_id": fixture.company_id, "status": "trialing", "source_event_type": "CompanySignedUp"
        })).await));
        let base_url = support::serve_for_real(rest.clone()).await;
        let spawn_bridge = || support::KillOnDrop(std::process::Command::new(env!("CARGO_BIN_EXE_email-bridge"))
            .env("DATABASE_URL", &database_url).env("NATS_URL", &nats_url)
            .env("SKILJ_BASE_URL", &base_url).env("TICKET_ROUTING", "tenant")
            .env("EMAIL_BRIDGE_ROLE_ID", &fixture.ops_role.id)
            .env("EMAIL_BRIDGE_TOKEN_SEED", "a".repeat(64))
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().unwrap());
        let mut bridge = spawn_bridge();
        let jetstream = async_nats::jetstream::new(async_nats::connect(&nats_url).await.unwrap());
        email_channel::ensure_stream(&jetstream).await.unwrap();
        let first = InboundEmail { message_id: unique_name("email"), from_address: "alice@example.com".into(), from_name: None,
            to_address: format!("support+{}@help.example", fixture.company_id), subject: "Tenant email".into(), body: "Help".into() };
        let send = |email: InboundEmail| { let jetstream = jetstream.clone(); async move {
            jetstream.publish(email_channel::RECEIVED_SUBJECT, serde_json::to_vec(&email).unwrap().into()).await.unwrap().await.unwrap();
        }};
        let ticket_id = email_channel::ticket_id_for(&first.message_id);
        let status = || { let pool = pool.clone(); let tenant = fixture.tenant_name.clone(); let ticket = ticket_id.clone(); async move {
            support::projection_state::<skilj_helpdesk::helpdesk::TicketSummaryState>(&pool, &tenant, "TicketSummary", &ticket).await.status
        }};
        send(first.clone()).await;
        support::wait_until(Duration::from_secs(30), "tenant email ticket", || async { status().await.as_deref() == Some("open") }).await;
        // The other tenant and shared context have no copy of this ticket.
        for context in [BOUNDED_CONTEXT, other.tenant_name.as_str()] {
            let state: skilj_helpdesk::helpdesk::TicketSummaryState = support::projection_state(&pool, context, "TicketSummary", &ticket_id).await;
            assert!(state.status.is_none());
        }
        let assign = mint_command_token(&pool, &mapping, &fixture.tenant_name, "AssignTicket").await;
        let ask = mint_command_token(&pool, &mapping, &fixture.tenant_name, "RequestInfoFromCustomer").await;
        assert!(accepted(&trigger(&rest, &assign, serde_json::json!({"ticket_id": ticket_id, "staff_id": "email-staff"})).await));
        assert!(accepted(&trigger(&rest, &ask, serde_json::json!({"ticket_id": ticket_id, "staff_id": "email-staff", "requester_id": "email:alice@example.com", "message": "Which browser?"})).await));
        let reply = InboundEmail { message_id: unique_name("reply"), to_address: format!("ticket+{ticket_id}@help.example"), body: "Chromium".into(), ..first.clone() };
        send(reply.clone()).await;
        support::wait_until(Duration::from_secs(30), "reply in its original tenant", || async { status().await.as_deref() == Some("in_progress") }).await;
        // Restart, then redeliver the same reply while the ticket is waiting
        // again. A broken identity would consume the new question's reply.
        bridge.0.kill().unwrap(); bridge.0.wait().unwrap();
        assert!(accepted(&trigger(&rest, &ask, serde_json::json!({"ticket_id": ticket_id, "staff_id": "email-staff", "requester_id": "email:alice@example.com", "message": "Which version?"})).await));
        let _restarted = spawn_bridge();
        send(reply).await;
        send(first.clone()).await;
        // Marker processed after duplicates, without relying on a sleep.
        let marker = InboundEmail { message_id: unique_name("marker"), ..first };
        let marker_id = email_channel::ticket_id_for(&marker.message_id);
        send(marker).await;
        support::wait_until(Duration::from_secs(30), "ordered marker", || async {
            let state: skilj_helpdesk::helpdesk::TicketSummaryState = support::projection_state(&pool, &fixture.tenant_name, "TicketSummary", &marker_id).await;
            state.status.is_some()
        }).await;
        assert_eq!(status().await.as_deref(), Some("waiting_on_customer"), "the old reply must not answer the new question");
        // Revoked service mappings are checked even for already minted tokens.
        db::revoke_active_role_access_mapping(&pool, &fixture.ops_role.id, &fixture.tenant_name, chrono::Utc::now()).await.unwrap();
        let service = skilj_helpdesk::tenant_email::TenantEmail { pool: pool.clone(), role_id: fixture.ops_role.id.clone(), token_seed: "a".repeat(64), base_url, http: reqwest::Client::new() };
        let command = email_channel::route(&InboundEmail { message_id: unique_name("denied"), from_address: "alice@example.com".into(), from_name: None,
            to_address: format!("support+{}@help.example", fixture.company_id), subject: "Denied".into(), body: "No access".into() }).unwrap();
        assert!(service.mapping(&fixture.tenant_name, &command).await.is_err());
    });
}
