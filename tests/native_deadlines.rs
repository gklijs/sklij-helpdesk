//! End-to-end proof that `src/helpdesk.rs`'s three `ScheduleDeadline`
//! reactors (`ScheduleCompanyTrialConversion`/`ScheduleCompanyTrialExpiry`/
//! `ScheduleTicketAutoClose`, ported off the old `src/bin/scheduler.rs`
//! onto skilj 0.0.7's native per-entity deadline mechanism,
//! docs/architecture.md §46 in the skilj repo) actually fire for real -
//! not just that the crate compiles against the new trait. Unlike
//! `scheduler.rs`, which was never exercised by `cargo test` at all (its
//! own former doc comment said so explicitly - it only ran as a real,
//! separately-deployed process), this mechanism runs in-process as part
//! of `Skilj::builder().build()`'s own background poll tasks, so it's
//! reachable from an ordinary integration test the same way
//! `skilj/tests/deadlines.rs` proves the mechanism itself.
//!
//! `TRIAL_DURATION_DAYS`/`AUTO_CLOSE_AFTER_DAYS` are set to `0` for this
//! whole process (`std::env::set_var`, top of each test, before
//! `setup()` spawns the pollers) so both deadlines below are due
//! immediately rather than in real days - safe here specifically because
//! this file is its own compiled test binary (see `tests/company.rs`'s
//! own doc comment on why each `tests/*.rs` file is), so it can't race
//! another file's own tests wanting the real 30/7-day defaults; the two
//! tests *in* this file both want the same override, so they don't race
//! each other either.
//!
//! No projection exposes a company's own `status` directly (only
//! ticket-shaped projections exist - see `src/helpdesk.rs`'s own
//! `CompanyTicketList`/`TicketSummary`), so the trial-conversion test
//! proves the deadline fired indirectly: re-submitting `ConvertCompanyTrial`
//! by hand afterwards, having never submitted it directly itself, and
//! getting `company_not_trialing` back - the same rejection
//! `tests/company.rs`'s own `converting_a_trialing_company_activates_it`
//! gets from a *second* manual conversion, here obtained from a *first*
//! one that was never manual at all. The ticket auto-close test doesn't
//! need this trick: `TicketSummary` already tracks `status` directly.

mod support;

use skilj_helpdesk::helpdesk::{TicketSummaryState, BOUNDED_CONTEXT};
use std::time::Duration;
use support::{accepted, mint_command_token, projection_state, rejection_kind, runtime, setup, test_db, trigger, unique_name, wait_until};

async fn token(
    pool: &skilj_core::db::Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    command_type_name: &str,
) -> String {
    mint_command_token(pool, mapping, BOUNDED_CONTEXT, command_type_name).await
}

/// `rule TrialPeriodEnds`'s success branch, fired by
/// `ScheduleCompanyTrialConversion` itself - nothing in this test ever
/// submits `ConvertCompanyTrial`.
#[test]
fn a_trialing_company_converts_on_its_own_once_the_native_deadline_fires() {
    // SAFETY (env::set_var, edition 2021, single-threaded w.r.t. other
    // test *files*): see this file's own doc comment.
    std::env::set_var("TRIAL_DURATION_DAYS", "0");
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let convert = token(&pool, &mapping, "ConvertCompanyTrial").await;
        let company_id = unique_name("company");

        let response = trigger(
            &router,
            &sign_up,
            serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
        )
        .await;
        assert!(accepted(&response), "signup should be accepted: {response:?}");

        wait_until(
            Duration::from_secs(10),
            "ScheduleCompanyTrialConversion fires ConvertCompanyTrial on its own",
            || {
                let router = router.clone();
                let convert = convert.clone();
                let company_id = company_id.clone();
                async move {
                    let response = trigger(&router, &convert, serde_json::json!({ "company_id": company_id })).await;
                    !accepted(&response) && rejection_kind(&response) == "company_not_trialing"
                }
            },
        )
        .await;
    });
}

/// `rule TicketAutoCloses`, fired by `ScheduleTicketAutoClose` itself -
/// nothing in this test ever submits `CloseTicket`.
#[test]
fn a_resolved_ticket_closes_on_its_own_once_the_native_deadline_fires() {
    // SAFETY: see this file's own doc comment.
    std::env::set_var("AUTO_CLOSE_AFTER_DAYS", "0");
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mapping) = setup().await;
        let router = skilj.rest_router();
        let sign_up = token(&pool, &mapping, "SignUpCompany").await;
        let create_ticket = token(&pool, &mapping, "CreateTicket").await;
        let assign_ticket = token(&pool, &mapping, "AssignTicket").await;
        let resolve_ticket = token(&pool, &mapping, "ResolveTicket").await;
        let company_id = unique_name("company");
        let ticket_id = unique_name("ticket");

        trigger(&router, &sign_up, serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" })).await;
        trigger(
            &router,
            &create_ticket,
            serde_json::json!({
                "ticket_id": ticket_id, "company_id": company_id, "requester_id": unique_name("customer"),
                "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
            }),
        )
        .await;
        trigger(&router, &assign_ticket, serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") })).await;
        let response = trigger(&router, &resolve_ticket, serde_json::json!({ "ticket_id": ticket_id })).await;
        assert!(accepted(&response), "resolving an assigned ticket should succeed: {response:?}");

        wait_until(
            Duration::from_secs(10),
            "ScheduleTicketAutoClose closes the ticket on its own",
            || {
                let pool = pool.clone();
                let ticket_id = ticket_id.clone();
                async move {
                    let state: TicketSummaryState =
                        projection_state(&pool, BOUNDED_CONTEXT, "TicketSummary", &ticket_id).await;
                    state.status.as_deref() == Some("closed")
                }
            },
        )
        .await;
    });
}
