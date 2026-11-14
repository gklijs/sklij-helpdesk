//! Forced process exits after delivery/effects, before checkpointing (#100).
mod support;
use serde_json::json;
use skilj_core::db;
use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use support::*;

fn spawn(
    binary: &str,
    base: &str,
    env: &HashMap<String, String>,
    fault: Option<&str>,
) -> KillOnDrop {
    let mut cmd = Command::new(binary);
    cmd.envs(env)
        .env("SKILJ_BASE_URL", base)
        .env("TICKET_ROUTING", "shared")
        .env_remove("OTEL_EXPORTER_OTLP_ENDPOINT")
        .env_remove("SLACK_WEBHOOK_URL")
        .env_remove("HELPDESK_CONSUMER_FAULT")
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
    if let Some(point) = fault {
        cmd.env("HELPDESK_CONSUMER_FAULT", point);
    }
    KillOnDrop(cmd.spawn().unwrap())
}
async fn crash(binary: &str, base: &str, env: &HashMap<String, String>, point: &str) {
    let mut child = spawn(binary, base, env, Some(point));
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            assert_eq!(status.code(), Some(86), "consumer did not reach {point}");
            return;
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "fault injection not reached: {point}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
async fn count(pool: &db::Pool, context: &str, event: &str, key: &str, id: &str) -> usize {
    db::list_events(pool, context, event)
        .await
        .unwrap()
        .iter()
        .filter(|e| serde_json::from_str::<serde_json::Value>(&e.payload).unwrap()[key] == id)
        .count()
}
async fn effect(pool: &db::Pool, context: &str, event: &str, key: &str, id: &str) {
    wait_until(Duration::from_secs(35), event, || async {
        count(pool, context, event, key, id).await == 1
    })
    .await;
}
fn env(pairs: &[(&str, String)]) -> HashMap<String, String> {
    pairs
        .iter()
        .map(|(k, v)| (k.to_string(), v.clone()))
        .collect()
}
fn path(dir: &Path, name: &str) -> String {
    dir.join(name).to_str().unwrap().into()
}

#[test]
fn every_application_consumer_replays_crash_windows_and_lost_checkpoints() {
    runtime().block_on(async {
    if test_db().await.is_none(){return;}
    let dir=PathBuf::from("/tmp").join(unique_name("consumer-recovery"));
    std::fs::create_dir(&dir).unwrap();
    // Other integration binaries share DATABASE_URL in CI; replay tests
    // need a clean history so the injected crash selects this fixture's event.
    let (database_url,pool)=fresh_database("consumer_recovery").await.unwrap();
    for name in ["helpdesk","activity","marketing","knowledge_base"] {
      db::insert_bounded_context(&pool,&skilj_core::event_store::BoundedContext {
        name:name.into(),status:skilj_core::event_store::BoundedContextStatus::Active,
        created_at:test_now(),created_by:skilj_core::bootstrap::ContextCreator::SystemCreator,template:None,
      }).await.unwrap();
    }
    let role=seed_role(&pool,"recovery-admin").await;
    let mappings=AllContextMappings {
      helpdesk:seed_mapping_for(&pool,&role,"helpdesk",skilj_core::access_control::AccessLevel::Admin,None).await,
      activity:seed_mapping_for(&pool,&role,"activity",skilj_core::access_control::AccessLevel::Admin,None).await,
      marketing:seed_mapping_for(&pool,&role,"marketing",skilj_core::access_control::AccessLevel::Admin,None).await,
      knowledge_base:seed_mapping_for(&pool,&role,"knowledge_base",skilj_core::access_control::AccessLevel::Admin,None).await,
    };
    let jwks=serve_jwks().await;
    let (skilj,_)=skilj_helpdesk::register(skilj::Skilj::builder(database_url))
      .key_service(test_key_service()).reconciliation_role(role.external_subject)
      .identity_provider(test_idp_config(&jwks)).build().await.unwrap();
    let admin=&mappings.helpdesk;
    sqlx::query("UPDATE roles SET superadmin=true WHERE id=$1").bind(&admin.role.id).execute(&pool).await.unwrap();
    let rest=skilj.rest_router();
    let base=serve_for_real(rest.clone().merge(skilj.graphql_router().await.unwrap())).await;
    let company=unique_name("recovery-company");
    let signup=mint_command_token(&pool,admin,BOUNDED_CONTEXT,"SignUpCompany").await;
    let sign_events=mint_event_read_token(&pool,admin,BOUNDED_CONTEXT,"CompanySignedUp").await;
    assert!(accepted(&trigger(&rest,&signup,json!({"company_id":company,"name":"Recovery","contact_email":"r@example.test"})).await));
    let provision=env(&[
      ("COMPANY_SIGNED_UP_TOKEN",sign_events.clone()),
      ("RECORD_COMPANY_TENANT_TOKEN",mint_command_token(&pool,admin,BOUNDED_CONTEXT,"RecordCompanyTenant").await),
      ("PROVISIONER_SUPERADMIN_SUBJECT",admin.role.external_subject.clone()),
      ("PROVISIONER_TENANT_ADMIN_ROLE_ID",admin.role.id.clone()),
      ("PROVISIONER_STATE_FILE",path(&dir,"provisioner.json")),
    ]);
    let bin=env!("CARGO_BIN_EXE_provisioner");
    crash(bin,&base,&provision,"after_delivery").await;
    crash(bin,&base,&provision,"after_tenant_creation").await;
    let tenant=skilj_helpdesk::routing::tenant_name_for(&company);
    assert!(db::get_bounded_context(&pool,&tenant).await.unwrap().is_some());
    assert_eq!(count(&pool,BOUNDED_CONTEXT,"CompanyTenantProvisioned","company_id",&company).await,0);
    crash(bin,&base,&provision,"after_effect").await;
    {
      let _process=spawn(bin,&base,&provision,None);
      effect(&pool,BOUNDED_CONTEXT,"CompanyTenantProvisioned","company_id",&company).await;
      wait_until(Duration::from_secs(10),"provisioner checkpoint",||async {
        std::fs::read_to_string(dir.join("provisioner.json")).unwrap_or_default().contains("CompanySignedUp")
      }).await;
    }
    std::fs::remove_file(dir.join("provisioner.json")).unwrap();
    {
      let _process=spawn(bin,&base,&provision,None);
      wait_until(Duration::from_secs(15),"rebuilt provisioning checkpoint",||async {
        std::fs::read_to_string(dir.join("provisioner.json")).unwrap_or_default().contains("CompanySignedUp")
      }).await;
      assert_eq!(count(&pool,BOUNDED_CONTEXT,"CompanyTenantProvisioned","company_id",&company).await,1);
    }
    let replicate=env(&[
      ("COMPANY_SIGNED_UP_TOKEN",sign_events),
      ("COMPANY_ACTIVATED_TOKEN",mint_event_read_token(&pool,admin,BOUNDED_CONTEXT,"CompanyActivated").await),
      ("COMPANY_EXPIRED_TOKEN",mint_event_read_token(&pool,admin,BOUNDED_CONTEXT,"CompanyExpired").await),
      ("REPLICATOR_SUPERADMIN_SUBJECT",admin.role.external_subject.clone()),
      ("REPLICATOR_STATE_FILE",path(&dir,"replicator.json")),
    ]);
    let bin=env!("CARGO_BIN_EXE_lifecycle-replicator");
    crash(bin,&base,&replicate,"after_delivery").await;
    crash(bin,&base,&replicate,"after_effect").await;
    {
      let _process=spawn(bin,&base,&replicate,None);
      effect(&pool,&tenant,"CompanyLifecycleMirrored","company_id",&company).await;
      wait_until(Duration::from_secs(10),"replicator checkpoint",||async {
        std::fs::read_to_string(dir.join("replicator.json")).unwrap_or_default().contains("CompanySignedUp")
      }).await;
      assert_eq!(count(&pool,&tenant,"CompanyLifecycleMirrored","company_id",&company).await,1);
    }
    // Conversion uses a distinct feed. Lost progress must rebuild the
    // latest lifecycle without replaying an older state over it.
    let convert=mint_command_token(&pool,admin,BOUNDED_CONTEXT,"ConvertCompanyTrial").await;
    assert!(accepted(&trigger(&rest,&convert,json!({"company_id":company})).await));
    {
      let _process=spawn(env!("CARGO_BIN_EXE_lifecycle-replicator"),&base,&replicate,None);
      wait_until(Duration::from_secs(35),"active tenant mirror",||async {
        count(&pool,&tenant,"CompanyLifecycleMirrored","company_id",&company).await==2
      }).await;
    }
    std::fs::remove_file(dir.join("replicator.json")).unwrap();
    {
      let _process=spawn(env!("CARGO_BIN_EXE_lifecycle-replicator"),&base,&replicate,None);
      wait_until(Duration::from_secs(15),"replicator rebuilt lifecycle feeds",||async {
        std::fs::read_to_string(dir.join("replicator.json")).unwrap_or_default().contains("CompanyActivated")
      }).await;
      assert_eq!(count(&pool,&tenant,"CompanyLifecycleMirrored","company_id",&company).await,2);
    }
    let quiet=unique_name("quiet-company");
    let record=mint_command_token(&pool,&mappings.activity,"activity","RecordDailyActivity").await;
    let day=(test_now()-chrono::Duration::days(30)).date_naive().and_hms_opt(0,0,0).unwrap().and_utc();
    assert!(accepted(&trigger(&rest,&record,json!({"company_id":quiet,"person_kind":"customer","person_subject":"quiet-customer","day":day})).await));
    let watch=env(&[
      ("DAILY_ACTIVITY_RECORDED_TOKEN",mint_event_read_token(&pool,&mappings.activity,"activity","DailyActivityRecorded").await),
      ("RECORD_ENGAGEMENT_DECLINE_TOKEN",mint_command_token(&pool,&mappings.activity,"activity","RecordEngagementDecline").await),
      ("ENGAGEMENT_WATCHER_STATE_FILE",path(&dir,"watcher.json")),
      ("ENGAGEMENT_WATCHER_POLL_INTERVAL_MS","20".into()),("QUIET_AFTER_DAYS","0".into()),
    ]);
    let bin=env!("CARGO_BIN_EXE_engagement-watcher");
    crash(bin,&base,&watch,"after_delivery").await;
    crash(bin,&base,&watch,"after_effect").await;
    {
      let _process=spawn(bin,&base,&watch,None);
      effect(&pool,"activity","CompanyEngagementDeclined","company_id",&quiet).await;
    }
    // Removing a stateful watcher's checkpoint rebuilds both progress and state.
    std::fs::remove_file(dir.join("watcher.json")).unwrap();
    {
      let _process=spawn(bin,&base,&watch,None);
      wait_until(Duration::from_secs(10),"watcher replay",||async {
        std::fs::read_to_string(dir.join("watcher.json")).unwrap_or_default().contains("DailyActivityRecorded")
      }).await;
      assert_eq!(count(&pool,"activity","CompanyEngagementDeclined","company_id",&quiet).await,1);
    }
    let ticket=unique_name("overdue-ticket");
    let create=mint_command_token(&pool,admin,BOUNDED_CONTEXT,"CreateTicket").await;
    assert!(accepted(&trigger(&rest,&create,json!({"ticket_id":ticket,"company_id":company,"requester_id":"recovery-customer","logged_by_staff_id":null,"title":"Recovery","description":"Recovery","priority":"low"})).await));
    let tokens=mint_alerter_tokens(&pool,admin,BOUNDED_CONTEXT).await;
    let alerts=env(&[
      ("TICKET_CREATED_TOKEN",tokens.ticket_created),("TICKET_RESOLVED_TOKEN",tokens.ticket_resolved),
      ("TICKET_REOPENED_TOKEN",tokens.ticket_reopened),("TICKET_CLOSED_TOKEN",tokens.ticket_closed),
      ("TICKET_ESCALATED_TOKEN",tokens.ticket_escalated),("TICKETS_MERGED_TOKEN",tokens.tickets_merged),
      ("ESCALATE_TICKET_TOKEN",tokens.escalate_ticket),("ALERTER_STATE_FILE",path(&dir,"alerter.json")),
      ("ALERTER_POLL_INTERVAL_MS","20".into()),("UNHANDLED_ALERT_AFTER_HOURS","0".into()),
    ]);
    let bin=env!("CARGO_BIN_EXE_alerter");
    crash(bin,&base,&alerts,"after_delivery").await;
    crash(bin,&base,&alerts,"after_effect").await;
    {
      let _process=spawn(bin,&base,&alerts,None);
      effect(&pool,BOUNDED_CONTEXT,"TicketEscalated","ticket_id",&ticket).await;
    }
    std::fs::remove_file(dir.join("alerter.json")).unwrap();
    {
      let _process=spawn(bin,&base,&alerts,None);
      wait_until(Duration::from_secs(10),"alerter replay",||async {
        std::fs::read_to_string(dir.join("alerter.json")).unwrap_or_default().contains(&ticket)
      }).await;
      assert_eq!(count(&pool,BOUNDED_CONTEXT,"TicketEscalated","ticket_id",&ticket).await,1);
    }
    // Corrupt and unwritable checkpoints fail explicitly; no empty-state fallback.
    let corrupt=dir.join("corrupt.json");std::fs::write(&corrupt,b"broken").unwrap();
    assert!(skilj_helpdesk::consumer::load::<HashMap<String,String>>(&corrupt).is_err());
    assert!(skilj_helpdesk::consumer::save(&dir.join("absent/state.json"),&alerts).is_err());
    std::fs::remove_dir_all(dir).unwrap();
 });
}
