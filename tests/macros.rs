//! Integration tests for macros (issue #43) over the real REST surface:
//! the staff library (`CreateMacro`/`UpdateMacro`/`DeleteMacro` into
//! `CompanyMacros`), and `ApplyMacro` landing its events atomically -
//! including the one thing the fixture tests can't show, that a query
//! mixing a `TicketSnapshot` item and a plain item is accepted by skilj.

mod support;

use skilj_helpdesk::helpdesk::{
    CompanyActiveTicketsState, CompanyMacrosState, TicketSummaryState, BOUNDED_CONTEXT,
};
use support::{
    accepted, mint_command_token, projection_state, rejection_kind, runtime, setup, test_db,
    trigger, unique_name,
};

async fn token(
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    command_type_name: &str,
) -> String {
    mint_command_token(pool, mapping, BOUNDED_CONTEXT, command_type_name).await
}

fn macro_payload(
    company_id: &str,
    macro_id: &str,
    body: &str,
    status: Option<&str>,
    priority: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "company_id": company_id, "macro_id": macro_id, "staff_id": "staff-1",
        "name": "Fixed in next release", "body": body, "status": status, "priority": priority,
    })
}

#[test]
fn staff_build_up_a_company_macro_library() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create = token(&pool, &mapping, "CreateMacro").await;
        let update = token(&pool, &mapping, "UpdateMacro").await;
        let delete = token(&pool, &mapping, "DeleteMacro").await;
        let company_id = unique_name("company");
        let (m1, m2) = (unique_name("macro"), unique_name("macro"));

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        for macro_id in [&m1, &m2] {
            let response = trigger(&router, &create, macro_payload(&company_id, macro_id, "Thanks!", None, None)).await;
            assert!(accepted(&response), "{response:?}");
        }
        let response = trigger(&router, &create, macro_payload(&company_id, &m1, "again", None, None)).await;
        assert_eq!(rejection_kind(&response), "macro_already_exists");

        let response = trigger(&router, &update, macro_payload(&company_id, &m1, "Fixed in {{ticket_id}}", Some("resolved"), Some("low"))).await;
        assert!(accepted(&response), "{response:?}");
        let response = trigger(&router, &delete, serde_json::json!({ "company_id": company_id, "macro_id": m2, "staff_id": "staff-1" })).await;
        assert!(accepted(&response), "{response:?}");
        let response = trigger(&router, &update, macro_payload(&company_id, &m2, "gone", None, None)).await;
        assert_eq!(rejection_kind(&response), "macro_not_found");

        let library: CompanyMacrosState = projection_state(&pool, BOUNDED_CONTEXT, "CompanyMacros", &company_id).await;
        assert_eq!(library.macros.len(), 1, "{library:?}");
        let entry = &library.macros[&m1];
        assert_eq!(entry.body, "Fixed in {{ticket_id}}");
        assert_eq!(entry.status.as_deref(), Some("resolved"));
        assert_eq!(entry.priority.as_deref(), Some("low"));
    });
}

#[test]
fn applying_a_macro_replies_reprioritises_and_resolves_at_once() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let assign_ticket = token(&pool, &mapping, "AssignTicket").await;
        let create_macro = token(&pool, &mapping, "CreateMacro").await;
        let apply_macro = token(&pool, &mapping, "ApplyMacro").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");
        let macro_id = unique_name("macro");
        let staff_id = unique_name("staff");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(&router, &create_ticket, serde_json::json!({
            "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
            "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "high",
        })).await;
        trigger(&router, &create_macro, macro_payload(&company_id, &macro_id, "Fixed in {{ticket_id}}", Some("resolved"), Some("low"))).await;
        let apply = serde_json::json!({ "ticket_id": ticket_id, "company_id": company_id, "macro_id": macro_id, "staff_id": staff_id });

        // Open, not in progress: resolving keeps `ResolveTicket`'s rule,
        // and nothing of the macro lands.
        let response = trigger(&router, &apply_macro, apply.clone()).await;
        assert_eq!(rejection_kind(&response), "ticket_not_in_progress", "{response:?}");
        let summary: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert_eq!(summary.priority.as_deref(), Some("high"));
        assert_eq!(summary.first_responder_staff_id, None);

        trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") })).await;
        let response = trigger(&router, &apply_macro, apply.clone()).await;
        assert!(accepted(&response), "{response:?}");

        let summary: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
        assert_eq!(summary.status.as_deref(), Some("resolved"));
        assert_eq!(summary.priority.as_deref(), Some("low"));
        assert_eq!(summary.first_responder_staff_id.as_deref(), Some(staff_id.as_str()));

        // Resolved now, so the same macro is rejected again.
        let response = trigger(&router, &apply_macro, apply).await;
        assert_eq!(rejection_kind(&response), "ticket_not_in_progress", "{response:?}");
    });
}

#[test]
fn a_deleted_macro_or_another_companys_macro_cannot_be_applied() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let create_macro = token(&pool, &mapping, "CreateMacro").await;
        let delete_macro = token(&pool, &mapping, "DeleteMacro").await;
        let apply_macro = token(&pool, &mapping, "ApplyMacro").await;
        let (acme, globex) = (unique_name("company"), unique_name("company"));
        let ticket_id = unique_name("ticket");
        let (deleted, foreign) = (unique_name("macro"), unique_name("macro"));

        for company_id in [&acme, &globex] {
            trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        }
        trigger(&router, &create_ticket, serde_json::json!({
            "ticket_id": ticket_id, "company_id": acme, "requester_id": unique_name("customer"),
            "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
        })).await;
        trigger(&router, &create_macro, macro_payload(&acme, &deleted, "Hi", None, Some("urgent"))).await;
        trigger(&router, &delete_macro, serde_json::json!({ "company_id": acme, "macro_id": deleted, "staff_id": "staff-1" })).await;
        trigger(&router, &create_macro, macro_payload(&globex, &foreign, "Hi", None, Some("urgent"))).await;

        let response = trigger(&router, &apply_macro, serde_json::json!({ "ticket_id": ticket_id, "company_id": acme, "macro_id": deleted, "staff_id": "s" })).await;
        assert_eq!(rejection_kind(&response), "macro_not_found", "{response:?}");
        let response = trigger(&router, &apply_macro, serde_json::json!({ "ticket_id": ticket_id, "company_id": globex, "macro_id": foreign, "staff_id": "s" })).await;
        assert_eq!(rejection_kind(&response), "ticket_belongs_to_another_company", "{response:?}");

        // Async projection: give it a moment, then check nothing moved.
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        let queue: CompanyActiveTicketsState = projection_state(&pool, BOUNDED_CONTEXT, "CompanyActiveTickets", &acme).await;
        if let Some(entry) = queue.tickets.get(&ticket_id) {
            assert_eq!(entry.priority, "low", "a rejected macro changes nothing");
        }
    });
}

/// Resolving through a macro is `ResolveTicket`'s own decision, so its
/// `TicketResolved` carries the assignee and `StaffWorkload` drops the
/// agent's load - seen here through `AutoAssignTicket`'s next pick.
#[test]
fn resolving_through_a_macro_frees_up_the_agent_for_auto_assignment() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let assign = token(&pool, &mapping, "AssignTicket").await;
        let auto = token(&pool, &mapping, "AutoAssignTicket").await;
        let create_macro = token(&pool, &mapping, "CreateMacro").await;
        let apply_macro = token(&pool, &mapping, "ApplyMacro").await;
        let company_id = unique_name("company");
        let macro_id = unique_name("macro");
        let [a, b] = ["a", "b"].map(|n| unique_name(&format!("staff_{n}")));
        let tickets: Vec<String> = (0..4).map(|_| unique_name("ticket")).collect();

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        for ticket in &tickets {
            let response = trigger(&router, &create_ticket, serde_json::json!({
                "ticket_id": ticket, "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
            })).await;
            assert!(accepted(&response), "{response:?}");
        }
        trigger(&router, &create_macro, macro_payload(&company_id, &macro_id, "Done", Some("resolved"), None)).await;
        // a has two open tickets, b one.
        for (ticket, staff) in [(&tickets[0], &a), (&tickets[1], &a), (&tickets[2], &b)] {
            let response = trigger(&router, &assign, serde_json::json!({ "ticket_id": ticket, "staff_id": staff })).await;
            assert!(accepted(&response), "{response:?}");
        }
        // a resolves both through the macro: a 0, b 1.
        for ticket in &tickets[..2] {
            let response = trigger(&router, &apply_macro, serde_json::json!({ "ticket_id": ticket, "company_id": company_id, "macro_id": macro_id, "staff_id": a })).await;
            assert!(accepted(&response), "{response:?}");
        }
        let response = trigger(&router, &auto, serde_json::json!({ "ticket_id": tickets[3], "candidate_staff_ids": [b, a] })).await;
        assert!(accepted(&response), "{response:?}");
        let summary: TicketSummaryState = projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &tickets[3]).await;
        assert_eq!(summary.assigned_staff_id.as_ref(), Some(&a), "a's load dropped to zero");
    });
}
