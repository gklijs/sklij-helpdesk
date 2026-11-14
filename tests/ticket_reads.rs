//! Real encrypted projections exercise the authorized, bounded UI adapter.
mod support;
use serde_json::{json, Value};
use skilj_core::{access_control::AccessLevel, db};
use skilj_helpdesk::{
    company_directory::DirectoryState,
    helpdesk::{BOUNDED_CONTEXT, CUSTOMER_SUBJECT},
    routing::RoutingMode,
    ticket_reads::{self, ReadState, TicketRead},
};
use std::{collections::HashMap, sync::Arc, time::Instant};
use support::*;
fn request(
    company: &str,
    details: HashMap<String, usize>,
    offset: usize,
    seq: Option<i64>,
) -> TicketRead {
    TicketRead {
        company_id: company.into(),
        offset,
        details,
        wait_for_sequence: seq,
    }
}
#[test]
fn pages_open_only_authorized_customer_text_and_bound_long_histories() {
    runtime().block_on(async {
    if test_db().await.is_none() { return; }
    let (skilj, pool, admin, _) = setup_graphql().await;
    let rest = skilj.rest_router();
    let company = unique_name("paged-company");
    let signup = mint_command_token(&pool, &admin, BOUNDED_CONTEXT, "SignUpCompany").await;
    let create = mint_command_token(&pool, &admin, BOUNDED_CONTEXT, "CreateTicket").await;
    assert!(accepted(&trigger(&rest, &signup, json!({"company_id":company,"name":"Paged", "contact_email":"p@example.test"})).await));
    let customer = seed_role(&pool, "customer").await;
    seed_scoped_mapping(&pool, &customer, AccessLevel::Write, Some(company.clone())).await;
    let staff = seed_role(&pool, "staff").await;
    let mut grant = admin.clone();
    grant.role = staff.clone(); grant.scope = Some(company.clone()); grant.can_read_sensitive = true;
    grant.capabilities.insert("staff".into());
    db::insert_role_access_mapping(&pool, &grant).await.unwrap();
    let mut seq = None;
    for i in 0..100 {
      let id = format!("{company}-{i:03}");
      let requester = if i == 0 { customer.external_subject.clone() } else { format!("{company}-customer-{i}") };
      let result = trigger(&rest, &create, json!({"ticket_id":id,"company_id":company,"requester_id":requester,"logged_by_staff_id":null,"title":"SGVsbG8=","description":"Confidential description","priority":"low"})).await;
      assert!(accepted(&result), "{result}");
      seq = result["triggeredEventSequences"].as_array().and_then(|a| a.last()).and_then(Value::as_i64);
    }
    let jwks = serve_jwks().await;
    let state = Arc::new(ReadState { directory: Arc::new(DirectoryState {pool:pool.clone(), keys:skilj_core::access_control::JwksCache::new(jwks.parse().unwrap()),idp:test_idp_config(&jwks),mode:RoutingMode::Shared}), keys:test_key_service() });
    let first_id = format!("{company}-000");
    // One customer's realistic encrypted state, enlarged to 1,000 historical
    // tickets and 1,000 messages. The queue still contains only active tickets.
    let raw = db::get_projection_state(&pool, BOUNDED_CONTEXT, "CustomerTickets", &customer.external_subject).await.unwrap().unwrap();
    let mut history: Value = serde_json::from_str(&raw).unwrap();
    let entry = history["tickets"][&first_id].clone();
    for i in 0..1000 { history["tickets"][format!("history-{i}")] = entry.clone(); }
    history["tickets"][&first_id]["messages"] = json!((0..1000).map(|_| json!({"author_id":"staff", "from_staff":true,"text":entry["description"]})).collect::<Vec<_>>());
    let historical_bytes = history.to_string().len();
    sqlx::query("UPDATE bc_helpdesk.projection_state SET state=$1 WHERE projection_name='CustomerTickets' AND key=$2")
      .bind(history.to_string()).bind(&customer.external_subject).execute(&pool).await.unwrap();
    // Compare the old serial browser read with the new adapter over real
    // localhost HTTP, using the same staff identity and encrypted database.
    ticket_reads::read_page(&state, &staff, request(&company, HashMap::new(), 0, seq)).await.unwrap();
    let base = serve_for_real(skilj.graphql_router().await.unwrap().merge(ticket_reads::router(state.clone()))).await;
    let client = reqwest::Client::new();
    let jwt = sign_jwt(&staff.external_subject);
    let start = Instant::now();
    let mut legacy_bytes = 0usize;
    for i in 0..101 {
        let (name, key) = if i == 0 { ("CompanyActiveTickets", company.clone()) }
            else if i == 1 { ("CustomerTickets", customer.external_subject.clone()) }
            else { ("CustomerTickets", format!("{company}-customer-{}", i - 1)) };
        let query = format!("query {{ projection(boundedContext: \"helpdesk\", name: {name:?}, key: {key:?}) {{ ... on helpdesk_{name} {{ tickets }} }} }}");
        let response = client.post(format!("{base}/graphql")).bearer_auth(&jwt).json(&json!({"query":query})).send().await.unwrap().text().await.unwrap();
        let json:Value=serde_json::from_str(&response).unwrap();
        assert!(json.get("errors").is_none(),"{response}");
        legacy_bytes += response.len();
    }
    let legacy_ms=start.elapsed().as_secs_f64()*1000.;
    let start=Instant::now();
    let response = client.post(format!("{base}/v1/helpdesk/tickets")).bearer_auth(&jwt)
        .json(&json!({"companyId":company,"offset":0,"details":{},"waitForSequence":seq})).send().await.unwrap();
    assert!(response.status().is_success());
    let response_bytes=response.bytes().await.unwrap().len();
    let page_http_ms=start.elapsed().as_secs_f64()*1000.;
    println!("HTTP comparison: legacy_requests=101 legacy_bytes={legacy_bytes} legacy_ms={legacy_ms:.2} page_requests=1 page_bytes={response_bytes} page_ms={page_http_ms:.2}");
    let start = Instant::now();
    let page = ticket_reads::read_page(&state, &staff, request(&company, HashMap::new(), 0, seq)).await.unwrap();
    let summary_ms = start.elapsed().as_secs_f64()*1000.;
    assert_eq!(page.tickets.len(),25); assert!(page.has_more);
    assert_eq!(page.contents[&first_id]["title"],"SGVsbG8=");
    assert_eq!(page.contents[&first_id]["description"],"");
    assert!(page.contents.values().all(|c| c["messages"].as_array().unwrap().is_empty()));
    let summary_bytes = serde_json::to_vec(&page).unwrap().len();
    let start = Instant::now();
    let detail = ticket_reads::read_page(&state, &staff, request(&company, HashMap::from([(first_id.clone(),20)]),0,seq)).await.unwrap();
    let detail_ms = start.elapsed().as_secs_f64()*1000.;
    assert_eq!(detail.contents[&first_id]["message_count"],1000);
    assert_eq!(detail.contents[&first_id]["messages"].as_array().unwrap().len(),20);
    assert_eq!(detail.contents[&first_id]["description"],"Confidential description");
    let detail_bytes=serde_json::to_vec(&detail).unwrap().len();
    assert!(detail_bytes<50_000 && summary_bytes<20_000 && historical_bytes>200_000);
    println!("ticket read measurements: customers=100 historical_tickets=1000 messages=1000 legacy_customer_bytes={historical_bytes} summary_bytes={summary_bytes} detail_bytes={detail_bytes} summary_ms={summary_ms:.2} detail_ms={detail_ms:.2}");
    let last_page=ticket_reads::read_page(&state,&staff,request(&company,HashMap::from([(first_id.clone(),0)]),75,seq)).await.unwrap();
    assert_eq!(last_page.tickets.len(),25); assert!(!last_page.has_more);
    assert!(!last_page.contents.contains_key(&first_id));
    let own = ticket_reads::read_page(&state,&customer,request(&company,HashMap::new(),0,seq)).await.unwrap();
    assert_eq!(own.tickets.len(),1); assert_eq!(own.contents[&first_id]["title"],"SGVsbG8=");
    let other = seed_role(&pool,"customer").await;
    seed_scoped_mapping(&pool,&other,AccessLevel::Read,Some(unique_name("other-company"))).await;
    assert_eq!(ticket_reads::read_page(&state,&other,request(&company,HashMap::new(),0,None)).await.err().unwrap().0, axum::http::StatusCode::FORBIDDEN);
    let restricted_staff = seed_role(&pool,"staff").await;
    seed_scoped_mapping(&pool,&restricted_staff,AccessLevel::Read,Some(company.clone())).await;
    let hidden=ticket_reads::read_page(&state,&restricted_staff,request(&company,HashMap::from([(first_id.clone(),0)]),0,seq)).await.unwrap();
    assert_eq!(hidden.tickets.len(),25);
    assert!(hidden.contents.values().all(|c| c["available"]==false && c["title"]=="Content unavailable"));
    // Erasure affects only this subject, even with a previously warm key cache.
    db::destroy_encryption_key(&pool, BOUNDED_CONTEXT, CUSTOMER_SUBJECT, &customer.external_subject, test_now()).await.unwrap();
    let erased = ticket_reads::read_page(&state,&staff,request(&company,HashMap::from([(first_id.clone(),0)]),0,seq)).await.unwrap();
    assert_eq!(erased.contents[&first_id]["available"],false);
    assert_eq!(erased.contents[&first_id]["title"],"Content unavailable");
    assert_eq!(erased.contents[&format!("{company}-001")]["title"],"SGVsbG8=");
    // Resolve a real company tenant on the server; a caller cannot substitute
    // a shared context, another company, or a projection with a missing gate.
    let tenant_company=unique_name("routed-company");
    assert!(accepted(&trigger(&rest,&signup,json!({"company_id":tenant_company,"name":"Routed","contact_email":"r@example.test"})).await));
    let tenant=unique_name("read");
    let ops=seed_superadmin(&pool).await;
    let gql=skilj.graphql_router().await.unwrap();
    let result=graphql_request(&gql,&sign_jwt(&ops.external_subject),&format!("mutation {{ createBoundedContextFromTemplate(template: \"helpdesk\", name: {tenant:?}, roleId: {:?}, level: ADMIN, canReadSensitive: true) {{ name }} }}", ops.id)).await;
    assert!(result.get("errors").is_none(),"{result}");
    let record=mint_command_token(&pool,&admin,BOUNDED_CONTEXT,"RecordCompanyTenant").await;
    assert!(accepted(&trigger(&rest,&record,json!({"company_id":tenant_company,"tenant_name":tenant})).await));
    let tenant_mapping=db::get_active_role_access_mapping(&pool,&ops.id,&tenant).await.unwrap().unwrap();
    let lifecycle=mint_command_token(&pool,&tenant_mapping,&tenant,"RecordTenantLifecycle").await;
    assert!(accepted(&trigger(&rest,&lifecycle,json!({"company_id":tenant_company,"status":"trialing","source_event_type":"CompanySignedUp"})).await));
    let tenant_create=mint_command_token(&pool,&tenant_mapping,&tenant,"CreateTicket").await;
    let created=trigger(&rest,&tenant_create,json!({"company_id":tenant_company,"ticket_id":first_id,"requester_id":ops.external_subject,"logged_by_staff_id":null,"title":"Tenant-local plaintext","description":"Tenant detail","priority":"low"})).await;
    assert!(accepted(&created));
    let tenant_seq=created["triggeredEventSequences"].as_array().and_then(|a|a.last()).and_then(Value::as_i64);
    let routed=ReadState {directory:Arc::new(DirectoryState {pool:pool.clone(),keys:skilj_core::access_control::JwksCache::new(jwks.parse().unwrap()),idp:test_idp_config(&jwks),mode:RoutingMode::Tenant}),keys:test_key_service()};
    let page=ticket_reads::read_page(&routed,&ops,request(&tenant_company,HashMap::new(),0,tenant_seq)).await.unwrap();
    assert_eq!(page.tickets.len(),1);
    assert_eq!(page.contents[&first_id]["title"],"Tenant-local plaintext");
    assert!(ticket_reads::read_page(&routed,&ops,request(&company,HashMap::new(),0,None)).await.is_err());
    db::set_projection_requires_capability(&pool,&tenant,"CustomerTickets",Some("billing")).await.unwrap();
    assert!(ticket_reads::read_page(&routed,&ops,request(&tenant_company,HashMap::new(),0,None)).await.is_err());
    // Capability and revocation changes are enforced on the next read.
    db::revoke_role_access_mapping_with_trail(&pool,&staff.id,BOUNDED_CONTEXT,test_now(),&admin.role.id).await.unwrap();
    assert!(ticket_reads::read_page(&state,&staff,request(&company,HashMap::new(),0,None)).await.is_err());
 });
}
