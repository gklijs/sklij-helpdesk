//! `TicketInternalNotes`'s own `Projection::TEAM_ONLY = Some(STAFF_TEAM)`
//! (see its doc comment in `src/helpdesk.rs`, skilj 0.0.4) only gates
//! `ProjectionQuery` - it says nothing about skilj's generic
//! `CommandQuery`/`EventQuery` (`fetchCommands`/`queryEvents` -
//! `AdminAccess`-gated, the read path skilj-inspector/skilj-tui use,
//! not anything this crate builds itself), which can still show any
//! command's or event's raw payload to a superadmin-mapped Role
//! regardless of `TEAM_ONLY`.
//!
//! `AddInternalNote`'s own `private_fields()` (command side) and
//! `TicketInternalNoteAdded`'s own identical one (event side - missed
//! in the first pass, since that event has no `event_read_allowed() =
//! true` override and the REST feed alone is blocked without it;
//! `queryEvents` doesn't check that flag at all, only
//! `fetch_events`/`consume_events` do) both close this: `staff_id`/
//! `note` declared as `PrivateFieldKind::Team(STAFF_TEAM)` redacts both
//! to `null` for an `AdminAccess`-level Role whose own `name` isn't
//! literally `STAFF_TEAM` - the identical `role_matches_required_team`
//! check `TEAM_ONLY` uses, applied per field here instead of to a whole
//! projection instance, and with the same "no superadmin bypass" shape:
//! `AccessLevel::Admin` alone clears `require_admin_mapping`'s door,
//! but the field itself still checks `Role.name` independently once
//! inside it.
//!
//! Deliberately not the same test as
//! `tests/cross_company_projection_scoping.rs`: this exercises
//! `CommandQuery`/`EventQuery` (`render_command`/`render_event`),
//! different code paths from `ProjectionQuery`/`query_projection`
//! entirely, so passing there is no evidence either surface here is
//! closed too - and the two tests below are themselves independent for
//! the identical reason relative to each other.

mod support;

use skilj_core::access_control::AccessLevel;
use skilj_helpdesk::helpdesk::{BOUNDED_CONTEXT, STAFF_TEAM};
use support::{
    graphql_request, mint_command_token, seed_role, seed_scoped_mapping, setup_graphql, sign_jwt,
    test_db, trigger, unique_name,
};

fn fetch_add_internal_note_commands() -> String {
    format!(
        r#"query {{ fetchCommands(boundedContext: {BOUNDED_CONTEXT:?}, commandTypes: ["AddInternalNote"]) }}"#
    )
}

fn query_internal_note_events() -> String {
    format!(
        r#"query {{ queryEvents(boundedContext: {BOUNDED_CONTEXT:?}, eventTypes: ["TicketInternalNoteAdded"]) {{ payload }} }}"#
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

        // A Role named `STAFF_TEAM` with the same Admin level clears
        // both `require_admin_mapping` and `PrivateFieldKind::Team`'s
        // own `role_matches_required_team` check - the redaction above
        // is the field declaration actually taking effect, not every
        // caller being denied regardless of team.
        let staff = seed_role(&pool, STAFF_TEAM).await;
        let staff_jwt = sign_jwt(&staff.external_subject);
        seed_scoped_mapping(&pool, &staff, AccessLevel::Admin, None).await;

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

/// The event-side counterpart of the test above - `TicketInternalNoteAdded`'s
/// own `private_fields()` (see its doc comment in `src/helpdesk.rs`),
/// found missing in review after the command-side one above was already
/// verified: `queryEvents` doesn't check `event_read_allowed` at all
/// (only the REST feed's `fetch_events`/`consume_events` do), so this
/// event type having no `event_read_allowed() = true` override blocks
/// the REST path but not this one - the exact gap this test regression-
/// guards.
#[test]
fn an_admin_role_off_the_staff_team_cannot_read_an_internal_notes_event_payload() {
    support::runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, admin_mapping, admin_jwt) = setup_graphql().await;
        let router = skilj.graphql_router().await.unwrap();
        let rest_router = skilj.rest_router();

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

        let response = graphql_request(&router, &admin_jwt, &query_internal_note_events()).await;
        assert!(response.get("errors").is_none(), "queryEvents as an admin-level Role should succeed: {response:?}");
        let events = response["data"]["queryEvents"]
            .as_array()
            .expect("queryEvents returns a list");
        let payload: serde_json::Value = events
            .iter()
            .map(|e| serde_json::from_str::<serde_json::Value>(e["payload"].as_str().unwrap()).unwrap())
            .find(|p| p["ticket_id"] == ticket)
            .expect("the TicketInternalNoteAdded event just appended must be in the list");
        assert!(
            payload["note"].is_null(),
            "an admin-level Role not on the staff team must NOT read TicketInternalNoteAdded's note: {payload:?}"
        );
        assert!(
            payload["staff_id"].is_null(),
            "an admin-level Role not on the staff team must NOT read TicketInternalNoteAdded's staff_id either: {payload:?}"
        );

        let staff = seed_role(&pool, STAFF_TEAM).await;
        let staff_jwt = sign_jwt(&staff.external_subject);
        seed_scoped_mapping(&pool, &staff, AccessLevel::Admin, None).await;

        let staff_response = graphql_request(&router, &staff_jwt, &query_internal_note_events()).await;
        let staff_events = staff_response["data"]["queryEvents"]
            .as_array()
            .expect("queryEvents returns a list");
        let staff_payload: serde_json::Value = staff_events
            .iter()
            .map(|e| serde_json::from_str::<serde_json::Value>(e["payload"].as_str().unwrap()).unwrap())
            .find(|p| p["ticket_id"] == ticket)
            .expect("the TicketInternalNoteAdded event must be in the list for a staff-team admin too");
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
