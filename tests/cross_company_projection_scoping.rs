//! Regression test for a real finding from a security review of this
//! crate: `skilj-graphql`'s `projection(boundedContext, name, key)`
//! query was gated only by "does the caller hold any active
//! `RoleAccessMapping` on this bounded context" - never whether the
//! specific instance queried belonged to that caller. Since every
//! company here shares one `helpdesk` bounded context, any
//! authenticated customer could read `TicketSummary`/`CompanyTicketList`
//! (and `TicketInternalNotes`) for a company they had nothing to do
//! with.
//!
//! Fixed in the sibling `skilj` repo (`docs/architecture.md` §23):
//! `RoleAccessMapping` gained `scope: Option<String>`, and a projection
//! can declare `OWNER_TAG_KEY` naming which of its own consuming
//! events' tags is its "owner" dimension - a scoped grant is rejected
//! reading any instance whose derived owner doesn't match, fail-closed
//! on an instance whose owner can't be derived at all (a never-touched
//! key included). Adopted here: `TicketSummary`/`CompanyTicketList`/
//! `TicketInternalNotes` all declare `OWNER_TAG_KEY = Some("company")`
//! (see each one's own doc comment in `src/helpdesk.rs`), and
//! `server.rs`'s demo customer Role is scoped to its own company.
//!
//! This file proves the mechanism actually closes the gap end to end,
//! over real GraphQL requests (not `support::projection_state`'s direct
//! DB read, which bypasses this check entirely) - not just that the new
//! fields compile. See `TicketInternalNotes`'s own doc comment in
//! `src/helpdesk.rs` for what this does *not* close: a customer scoped
//! to their own company can still read that company's own internal
//! notes, a different (role-type, not tenancy) axis of the same
//! original finding, left open on purpose rather than reached for
//! encryption infrastructure this crate has never provisioned.

mod support;

use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use support::{graphql_request, mint_command_token, seed_role, seed_scoped_mapping, setup_graphql, sign_jwt, test_db, trigger, unique_name};

fn projection_query(name: &str, key: &str, graphql_type: &str, field: &str) -> String {
    format!(
        r#"query {{ projection(boundedContext: {BOUNDED_CONTEXT:?}, name: {name:?}, key: {key:?}) {{ ... on {graphql_type} {{ {field} }} }} }}"#
    )
}

/// `true` when the response is a clean, successful answer (no GraphQL
/// error, and the projection field actually came back) - `false` for
/// both a hard GraphQL error (`GrantScopeMismatch`, `GrantNotActive`,
/// ...) and the softer "the field is just null" shape a caller with no
/// error but no data would also see, so a caller of this only has to
/// check one thing either way.
fn succeeded(response: &serde_json::Value, field: &str) -> bool {
    response.get("errors").is_none() && !response["data"]["projection"][field].is_null()
}

#[test]
fn a_customer_scoped_to_one_company_cannot_read_another_companys_projections() {
    support::runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, admin_mapping, _admin_jwt) = setup_graphql().await;
        let router = skilj.graphql_router().await.unwrap();
        let rest_router = skilj.rest_router();

        let sign_up = mint_command_token(&pool, &admin_mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
        let create_ticket = mint_command_token(&pool, &admin_mapping, BOUNDED_CONTEXT, "CreateTicket").await;
        let add_note = mint_command_token(&pool, &admin_mapping, BOUNDED_CONTEXT, "AddInternalNote").await;

        let company_a = unique_name("company-a");
        let company_b = unique_name("company-b");
        let ticket_a = unique_name("ticket-a");
        let ticket_b = unique_name("ticket-b");
        for (company, ticket) in [(&company_a, &ticket_a), (&company_b, &ticket_b)] {
            trigger(
                &rest_router,
                &sign_up,
                serde_json::json!({ "company_id": company, "name": company, "contact_email": format!("{company}@example.test") }),
            )
            .await;
            trigger(
                &rest_router,
                &create_ticket,
                serde_json::json!({
                    "ticket_id": ticket, "company_id": company, "requester_id": unique_name("customer"),
                    "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
                }),
            )
            .await;
        }
        // A note on *company A's own* ticket, so the internal-notes
        // check below has real content to (fail to) find, not just an
        // always-empty projection either way.
        trigger(
            &rest_router,
            &add_note,
            serde_json::json!({ "ticket_id": ticket_a, "staff_id": unique_name("staff"), "note": "internal only" }),
        )
        .await;

        // A customer scoped to company A only, and an unscoped staff
        // Role - `seed_admin`'s own shape (used for `admin_mapping`
        // above) is already unscoped, but this is its own distinct Role/
        // JWT so a staff-reachability check isn't just reusing the
        // bootstrap admin coincidentally.
        let customer_a = seed_role(&pool, "customer-a").await;
        let customer_a_jwt = sign_jwt(&customer_a.external_subject);
        seed_scoped_mapping(&pool, &customer_a, Some(company_a.clone())).await;

        let staff = seed_role(&pool, "staff").await;
        let staff_jwt = sign_jwt(&staff.external_subject);
        seed_scoped_mapping(&pool, &staff, None).await;

        // --- TicketSummary (keyed by ticket_id) ---
        let read_a = graphql_request(
            &router,
            &customer_a_jwt,
            &projection_query("TicketSummary", &ticket_a, "helpdesk_TicketSummary", "status"),
        )
        .await;
        assert!(succeeded(&read_a, "status"), "company A's own customer should read company A's own ticket: {read_a:?}");

        let read_b = graphql_request(
            &router,
            &customer_a_jwt,
            &projection_query("TicketSummary", &ticket_b, "helpdesk_TicketSummary", "status"),
        )
        .await;
        assert!(
            !succeeded(&read_b, "status"),
            "company A's own customer must NOT read company B's ticket - the vulnerability this test regression-guards against: {read_b:?}"
        );

        let staff_read_b = graphql_request(
            &router,
            &staff_jwt,
            &projection_query("TicketSummary", &ticket_b, "helpdesk_TicketSummary", "status"),
        )
        .await;
        assert!(
            succeeded(&staff_read_b, "status"),
            "unscoped staff should still read every company's own ticket: {staff_read_b:?}"
        );

        // --- CompanyTicketList (keyed by company_id itself) ---
        let list_a = graphql_request(
            &router,
            &customer_a_jwt,
            &projection_query("CompanyTicketList", &company_a, "helpdesk_CompanyTicketList", "tickets"),
        )
        .await;
        assert!(succeeded(&list_a, "tickets"), "company A's own customer should read company A's own list: {list_a:?}");

        let list_b = graphql_request(
            &router,
            &customer_a_jwt,
            &projection_query("CompanyTicketList", &company_b, "helpdesk_CompanyTicketList", "tickets"),
        )
        .await;
        assert!(
            !succeeded(&list_b, "tickets"),
            "company A's own customer must NOT read company B's ticket list: {list_b:?}"
        );

        // --- TicketInternalNotes: the cross-company half this pass
        // closes - company A's customer reading company B's notes must
        // fail. (Company A's customer reading company A's *own* notes
        // is deliberately NOT asserted either way here - that's the
        // still-open, different axis this file's own module doc comment
        // and TicketInternalNotes's own doc comment in helpdesk.rs both
        // describe; asserting it would either lock in a known gap as
        // "expected" or fail this test on something this pass never
        // claimed to fix.)
        let notes_b = graphql_request(
            &router,
            &customer_a_jwt,
            &projection_query("TicketInternalNotes", &ticket_b, "helpdesk_TicketInternalNotes", "notes"),
        )
        .await;
        assert!(
            !succeeded(&notes_b, "notes"),
            "company A's own customer must NOT read company B's internal notes: {notes_b:?}"
        );

        let staff_notes_a = graphql_request(
            &router,
            &staff_jwt,
            &projection_query("TicketInternalNotes", &ticket_a, "helpdesk_TicketInternalNotes", "notes"),
        )
        .await;
        assert!(
            succeeded(&staff_notes_a, "notes"),
            "unscoped staff should still read internal notes for any company's ticket: {staff_notes_a:?}"
        );
    });
}
