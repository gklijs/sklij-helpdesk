//! Phase 4 completion test: the multi-tenant alerter discovers a company's
//! tenant from `CompanyTenantProvisioned`, mints its own per-tenant
//! `EventReadToken`s/`CommandToken`s via GraphQL, polls that tenant's own
//! `TicketCreated` feed, and routes `EscalateTicket` into the tenant - all as
//! the real compiled `alerter` binary, driven against a real socket, rather
//! than the in-process decision logic `tests/alerting_feed.rs` covers.
//!
//! This is the end-to-end proof Phase 4 set out to enable: a ticket created in
//! a tenant (not the shared `helpdesk` context) gets escalated by the alerter
//! into that same tenant. The routing is implicit - REST derives its
//! destination bounded context from the `CommandToken`'s own
//! `command_type.bounded_context` (`skilj-rest`'s `post_commands_trigger`),
//! so the alerter just has to pick the right token.
//!
//! ## Test setup strategy
//!
//! `createBoundedContextFromTemplate` grants `roleId` Admin access on the new
//! tenant in the same call (`@guarantee AccessGrantedWithCreation`). The
//! alerter's `ALERTER_SUPERADMIN_SUBJECT` JWT must resolve to a `Role` holding
//! that Admin mapping - otherwise `createEventReadToken`/`createCommandToken`
//! on the tenant fails `GrantNotActive` inside `require_admin_mapping`. So the
//! superadmin `Role` seeded here *is* the `roleId` passed to
//! `createBoundedContextFromTemplate`, giving it Admin access on the tenant
//! directly.
//!
//! `CompanyTenantProvisioned` (the shared-context event the alerter reads to
//! discover tenants) is only minted after the tenant already exists - so the
//! alerter's first discovery poll finds a tenant it can actually mint tokens
//! for, not one that's still being provisioned.

mod support;

use skilj_core::db;
use skilj_helpdesk::helpdesk::{TicketSummaryState, TenantDirectoryState, BOUNDED_CONTEXT};
use std::path::Path;
use std::time::Duration;
use support::{
    accepted, graphql_request, mint_alerter_tokens, mint_command_token, projection_state, runtime,
    seed_superadmin, serve_for_real, setup_graphql, sign_jwt, spawn_alerter, test_db, trigger,
    unique_name, wait_until,
};

#[test]
fn multi_tenant_alerter_discovers_tenant_and_routes_escalation_there() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping, _admin_jwt) = setup_graphql().await;
        let rest_router = skilj.rest_router();
        let graphql_router = skilj.graphql_router().await.unwrap();
        let router = rest_router.merge(graphql_router);
        let base_url = serve_for_real(router.clone()).await;

        // --- superadmin identity: seeds the superadmin Role, uses it as
        // both the createBoundedContextFromTemplate caller AND the roleId
        // granted Admin on every tenant it creates - so its JWT can mint
        // per-tenant tokens via GraphQL (require_admin_mapping). ---
        let superadmin = seed_superadmin(&pool).await;
        let superadmin_jwt = sign_jwt(&superadmin.external_subject);
        let tenant_name = unique_name("tenant");

        // --- sign up a company in the shared context ---
        let sign_up =
            mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
        let company_id = unique_name("company");
        trigger(
            &router,
            &sign_up,
            serde_json::json!({
                "company_id": company_id,
                "name": "Acme",
                "contact_email": "a@acme.example",
            }),
        )
        .await;

        // --- create the tenant bounded context, granting the superadmin
        // Admin access on it (roleId = superadmin.id) ---
        let mutation = format!(
            r#"mutation {{
                createBoundedContextFromTemplate(
                    template: {template:?}
                    name: {tenant_name:?}
                    roleId: {role_id:?}
                    level: ADMIN
                    canReadSensitive: true
                ) {{
                    name
                    status
                }}
            }}"#,
            template = BOUNDED_CONTEXT,
            tenant_name = tenant_name,
            role_id = superadmin.id,
        );
        let response = graphql_request(&router, &superadmin_jwt, &mutation).await;
        assert!(
            response.get("errors").is_none(),
            "createBoundedContextFromTemplate should succeed: {response:?}"
        );
        assert_eq!(
            response["data"]["createBoundedContextFromTemplate"]["status"],
            "ACTIVE"
        );

        // --- record the tenant in the shared context -> CompanyTenantProvisioned ---
        let record_tenant =
            mint_command_token(&pool, &mapping, BOUNDED_CONTEXT, "RecordCompanyTenant").await;
        let response = trigger(
            &router,
            &record_tenant,
            serde_json::json!({ "company_id": company_id, "tenant_name": tenant_name }),
        )
        .await;
        assert!(
            accepted(&response),
            "recording the provisioned tenant should succeed: {response:?}"
        );

        // Verify TenantDirectory picked up the mapping - the alerter reads
        // CompanyTenantProvisioned off the same shared feed, so the event
        // must be durable before we start it.
        let resolved: TenantDirectoryState =
            projection_state(&pool, BOUNDED_CONTEXT, "TenantDirectory", &company_id).await;
        assert_eq!(
            resolved.tenant_name.as_deref(),
            Some(tenant_name.as_str()),
            "CompanyTenantProvisioned must record the tenant in TenantDirectory before the alerter starts"
        );

        // --- superadmin's own Admin mapping on the tenant ---
        let tenant_mapping = db::get_active_role_access_mapping(&pool, &superadmin.id, &tenant_name)
            .await
            .unwrap()
            .expect("createBoundedContextFromTemplate should have granted superadmin Admin on the tenant");

        // --- mirror the company's lifecycle into the tenant so CreateTicket
        // can pass its company_status guard ---
        let record_lifecycle =
            mint_command_token(&pool, &tenant_mapping, &tenant_name, "RecordTenantLifecycle").await;
        let response = trigger(
            &router,
            &record_lifecycle,
            serde_json::json!({
                "company_id": company_id,
                "status": "trialing",
                "source_event_type": "CompanySignedUp",
            }),
        )
        .await;
        assert!(
            accepted(&response),
            "mirroring a signup should succeed: {response:?}"
        );

        // --- create a ticket in the tenant ---
        let create_ticket =
            mint_command_token(&pool, &tenant_mapping, &tenant_name, "CreateTicket").await;
        let ticket_id = unique_name("ticket");
        trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id,
                "company_id": company_id,
                "requester_id": unique_name("customer"),
                "logged_by_staff_id": null,
                "title": "multi-tenant escalation",
                "description": "d",
                "priority": "low",
            }),
        )
        .await;

        // --- start the real alerter binary in multi-tenant mode ---
        let tokens = mint_alerter_tokens(&pool, &mapping, BOUNDED_CONTEXT).await;

        let tmp = std::env::temp_dir();
        let state_file = tmp.join(format!("{}.json", unique_name("multitenant-alerter-state")));
        let stderr_log = tmp.join(format!("{}.log", unique_name("multitenant-alerter-stderr")));
        struct Cleanup<'a>(&'a [&'a Path]);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                for path in self.0 {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        let _cleanup = Cleanup(&[&state_file, &stderr_log]);
        let state_file_str = state_file.to_str().unwrap();

        // TICKET_ROUTING=tenant activates multi-tenant discovery.
        // ALERTER_SUPERADMIN_SUBJECT is overridden to the superadmin's
        // external_subject (the identity with Admin on the tenant), not the
        // default admin-subject mint_alerter_tokens wired up.
        // UNHANDLED_ALERT_AFTER_HOURS=0 makes every open ticket immediately
        // overdue, so the alerter escalates without waiting.
        let alerter = spawn_alerter(
            &base_url,
            &tokens,
            &stderr_log,
            &[
                ("ALERTER_STATE_FILE", state_file_str),
                ("TICKET_ROUTING", "tenant"),
                ("ALERTER_SUPERADMIN_SUBJECT", &superadmin.external_subject),
                ("UNHANDLED_ALERT_AFTER_HOURS", "0"),
            ],
        );

        // The alerter must escalate the ticket INTO the tenant context -
        // TenantEscalated is checked on tenant_name, not BOUNDED_CONTEXT.
        wait_until(Duration::from_secs(30), "the multi-tenant alerter to escalate the ticket", || {
            let pool = pool.clone();
            let ticket_id = ticket_id.clone();
            let tenant_name = tenant_name.clone();
            async move {
                let summary: TicketSummaryState =
                    projection_state(&pool, &tenant_name, "TicketSummary", &ticket_id).await;
                summary.escalated
            }
        })
        .await;

        drop(alerter);

        // Final assertion: the tenant's own TicketSummary shows escalated.
        let summary: TicketSummaryState =
            projection_state(&pool, &tenant_name, "TicketSummary", &ticket_id).await;
        assert!(
            summary.escalated,
            "the multi-tenant alerter should have escalated the ticket in the tenant context: {summary:?}"
        );
    });
}
