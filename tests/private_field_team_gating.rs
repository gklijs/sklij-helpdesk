//! `TicketInternalNotes`'s own `Projection::TEAM_ONLY = Some("staff")`
//! (see its doc comment in `src/helpdesk.rs`, skilj 0.0.4) only gates
//! `ProjectionQuery` - it says nothing about skilj's generic
//! `CommandQuery`/`fetchCommands` (`AdminAccess`-gated, the read path
//! skilj-inspector/skilj-tui use, not anything this crate builds
//! itself), which can still show any command's raw payload to a
//! superadmin-mapped Role regardless of `TEAM_ONLY`.
//!
//! `AddInternalNote`'s own `private_fields()` closes that surface too:
//! `staff_id`/`note` declared as `PrivateFieldKind::Team("staff")`
//! (`docs/architecture.md`'s private-field writeup in the sibling
//! `skilj` repo) redacts both to `null` for an `AdminAccess`-level Role
//! whose own `name` isn't literally `"staff"` - the identical
//! `role_matches_required_team` check `TEAM_ONLY` uses, applied per
//! field here instead of to a whole projection instance, and with the
//! same "no superadmin bypass" shape: `AccessLevel::Admin` alone clears
//! `require_admin_mapping`'s door, but the field itself still checks
//! `Role.name` independently once inside it.
//!
//! Deliberately not the same test as
//! `tests/cross_company_projection_scoping.rs`: this exercises
//! `CommandQuery`/`render_command`, a different code path from
//! `ProjectionQuery`/`query_projection` entirely, so passing there is no
//! evidence this surface is closed too.

mod support;

use skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
use support::{
    graphql_request, mint_command_token, seed_admin_mapping, seed_role, setup_graphql, sign_jwt,
    test_db, trigger, unique_name,
};

fn fetch_add_internal_note_commands() -> String {
    format!(
        r#"query {{ fetchCommands(boundedContext: {BOUNDED_CONTEXT:?}, commandTypes: ["AddInternalNote"]) }}"#
    )
}

#[test]
fn an_admin_role_off_the_staff_team_cannot_read_an_internal_notes_command_payload() {
    support::runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, admin_mapping, admin_jwt) = setup_graphql().await;
        let router = skilj.graphql_router().await.unwrap();
        let rest_router = skilj.rest_router();

        // `admin_mapping`/`admin_jwt` (`seed_admin`'s own shape) is
        // `AccessLevel::Admin` with `Role.name == "Test Admin"` - clears
        // `require_admin_mapping`'s door, but isn't on the staff team.
        let sign_up = mint_command_token(&pool, &admin_mapping, BOUNDED_CONTEXT, "SignUpCompany").await;
        let create_ticket = mint_command_token(&pool, &admin_mapping, BOUNDED_CONTEXT, "CreateTicket").await;
        let add_note = mint_command_token(&pool, &admin_mapping, BOUNDED_CONTEXT, "AddInternalNote").await;

        let company = unique_name("company");
        let ticket = unique_name("ticket");
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
        let staff_id = unique_name("staff");
        trigger(
            &rest_router,
            &add_note,
            serde_json::json!({ "ticket_id": ticket, "staff_id": staff_id, "note": "internal only" }),
        )
        .await;

        let response = graphql_request(&router, &admin_jwt, &fetch_add_internal_note_commands()).await;
        assert!(response.get("errors").is_none(), "fetchCommands as an admin-level Role should succeed: {response:?}");
        let commands = response["data"]["fetchCommands"]
            .as_array()
            .expect("fetchCommands returns a list");
        let payload: serde_json::Value = commands
            .iter()
            .map(|c| serde_json::from_str::<serde_json::Value>(c.as_str().unwrap()).unwrap())
            .find(|p| p["ticket_id"] == ticket)
            .expect("the AddInternalNote command just triggered must be in the list");
        assert!(
            payload["note"].is_null(),
            "an admin-level Role not on the staff team must NOT read AddInternalNote's note: {payload:?}"
        );
        assert!(
            payload["staff_id"].is_null(),
            "an admin-level Role not on the staff team must NOT read AddInternalNote's staff_id either: {payload:?}"
        );

        // A Role named "staff" with the same Admin level clears both
        // `require_admin_mapping` and `PrivateFieldKind::Team`'s own
        // `role_matches_required_team` check - the redaction above is
        // the field declaration actually taking effect, not every
        // caller being denied regardless of team.
        let staff = seed_role(&pool, "staff").await;
        let staff_jwt = sign_jwt(&staff.external_subject);
        seed_admin_mapping(&pool, &staff).await;

        let staff_response = graphql_request(&router, &staff_jwt, &fetch_add_internal_note_commands()).await;
        let staff_commands = staff_response["data"]["fetchCommands"]
            .as_array()
            .expect("fetchCommands returns a list");
        let staff_payload: serde_json::Value = staff_commands
            .iter()
            .map(|c| serde_json::from_str::<serde_json::Value>(c.as_str().unwrap()).unwrap())
            .find(|p| p["ticket_id"] == ticket)
            .expect("the AddInternalNote command must be in the list for a staff-team admin too");
        assert_eq!(
            staff_payload["note"], "internal only",
            "an admin-level Role on the staff team should read the real note: {staff_payload:?}"
        );
        assert_eq!(
            staff_payload["staff_id"], staff_id,
            "an admin-level Role on the staff team should read the real staff_id: {staff_payload:?}"
        );
    });
}
