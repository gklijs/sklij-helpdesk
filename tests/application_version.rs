//! Two builds of this crate side by side against one database - the
//! rolling deploy `skilj_helpdesk::APPLICATION_VERSION`'s own doc comment
//! describes. The older build predates CSAT: it has no `TicketRated`
//! event type, and its `TicketSummary` doesn't consume one. The newer
//! build is this crate as it is today.
//!
//! Each startup runs against a fresh, uniquely named bounded context
//! rather than the shared `"helpdesk"` one every other test in this
//! binary uses: the whole point is to register *different* shapes under
//! the same names, which would race those tests' own registrations.
//! Registering by hand with `.event_type::<T>()`/`.projection::<T>()`
//! (not `auto_register()`, which always picks each type's own
//! `BOUNDED_CONTEXT`) is what lets real helpdesk types land there.

mod support;

use skilj::{EventType, Skilj};
use skilj_core::access_control::AccessLevel;
use skilj_core::bootstrap::ContextCreator;
use skilj_core::db::{self, PgPoolOptions, RegistrationTable};
use skilj_core::event_store::{BoundedContext, BoundedContextStatus};
use skilj_core::plugin::Projection;
use skilj_helpdesk::helpdesk::{
    HelpdeskEvent, TicketAssigned, TicketClosed, TicketCreated, TicketCustomerResponded,
    TicketEscalated, TicketInfoRequested, TicketRated, TicketReopened, TicketResolved,
    TicketSummary, TicketSummaryState, TicketsMerged,
};
use skilj_helpdesk::APPLICATION_VERSION;
use support::{runtime, seed_mapping_for, seed_role, test_db, test_now, unique_name};

/// `TicketSummary` as the older build declares it - identical except
/// that `TicketRated` isn't among its consumed event types yet.
struct TicketSummaryBeforeCsat;

impl Projection for TicketSummaryBeforeCsat {
    type State = TicketSummaryState;
    type Event = HelpdeskEvent;
    const NAME: &'static str = TicketSummary::NAME;
    const OWNER_TAG_KEY: Option<&'static str> = TicketSummary::OWNER_TAG_KEY;
    fn consumed_event_types() -> Vec<&'static str> {
        TicketSummary::consumed_event_types()
            .into_iter()
            .filter(|name| *name != TicketRated::NAME)
            .collect()
    }
    fn sync() -> bool {
        TicketSummary::sync()
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        TicketSummary::keys(event)
    }
    fn project(state: &mut Self::State, event: &Self::Event, key: &str) {
        TicketSummary::project(state, event, key)
    }
}

const OLDER: u64 = APPLICATION_VERSION - 1;

fn builder(database_url: &str, bc: &str, subject: &str, version: u64) -> skilj::SkiljBuilder {
    Skilj::builder(database_url.to_string())
        .pool_options(PgPoolOptions::new().max_connections(2))
        .bounded_context(bc.to_string())
        .reconciliation_role(subject.to_string())
        .application_version(version)
        // Every event type `TicketSummary` consumes in both builds.
        .event_type::<TicketCreated>()
        .event_type::<TicketAssigned>()
        .event_type::<TicketResolved>()
        .event_type::<TicketReopened>()
        .event_type::<TicketInfoRequested>()
        .event_type::<TicketCustomerResponded>()
        .event_type::<TicketClosed>()
        .event_type::<TicketEscalated>()
        .event_type::<TicketsMerged>()
}

async fn start_older(
    database_url: &str,
    bc: &str,
    subject: &str,
) -> (Skilj, skilj::ReconciliationReport) {
    builder(database_url, bc, subject, OLDER)
        .projection::<TicketSummaryBeforeCsat>()
        .build()
        .await
        .expect("the older build starts")
}

async fn start_newer(
    database_url: &str,
    bc: &str,
    subject: &str,
) -> (Skilj, skilj::ReconciliationReport) {
    builder(database_url, bc, subject, APPLICATION_VERSION)
        .event_type::<TicketRated>()
        .projection::<TicketSummary>()
        .build()
        .await
        .expect("the newer build starts")
}

async fn consumed_by_ticket_summary(pool: &db::Pool, bc: &str) -> Vec<String> {
    let mut names: Vec<String> = db::get_projection(pool, bc, TicketSummary::NAME)
        .await
        .unwrap()
        .expect("TicketSummary is registered")
        .consumed_event_types
        .into_iter()
        .map(|event_type| event_type.name)
        .collect();
    names.sort();
    names
}

fn sorted(names: Vec<&str>) -> Vec<String> {
    let mut names: Vec<String> = names.into_iter().map(String::from).collect();
    names.sort();
    names
}

/// The full rollout: old build running, new build starts, an old
/// instance restarts mid-rollout, then the new build restarts. Neither
/// build ever undoes the other's registrations, and the older build's
/// restart lists everything it declares in `kept_newer`.
#[test]
fn an_older_build_restarting_mid_rollout_keeps_the_newer_builds_registrations() {
    runtime().block_on(async {
        let Some((database_url, pool)) = test_db().await else {
            return;
        };
        let bc = unique_name("rolling");
        db::insert_bounded_context(
            &pool,
            &BoundedContext {
                name: bc.clone(),
                status: BoundedContextStatus::Active,
                created_at: test_now(),
                created_by: ContextCreator::SystemCreator,
                template: None,
            },
        )
        .await
        .unwrap();
        let role = seed_role(&pool, "rolling-deploy").await;
        seed_mapping_for(&pool, &role, &bc, AccessLevel::Admin, None).await;
        let subject = role.external_subject.clone();

        let new_shape = sorted(TicketSummary::consumed_event_types());
        let old_shape = sorted(TicketSummaryBeforeCsat::consumed_event_types());
        assert_ne!(new_shape, old_shape);

        // Before the deploy: only the older build has ever run.
        let (_old, report) = start_older(&database_url, &bc, &subject).await;
        assert!(report.kept_newer.is_empty(), "{report:?}");
        assert_eq!(consumed_by_ticket_summary(&pool, &bc).await, old_shape);

        // The newer build rolls out and upgrades the registrations.
        let (_new, report) = start_newer(&database_url, &bc, &subject).await;
        assert!(report.kept_newer.is_empty(), "{report:?}");
        assert_eq!(consumed_by_ticket_summary(&pool, &bc).await, new_shape);

        // An old instance restarts mid-rollout: every row it declares was
        // stamped by the newer build, so it keeps all of them as they are.
        let (_old_again, report) = start_older(&database_url, &bc, &subject).await;
        let mut kept = report.kept_newer.clone();
        kept.sort();
        let mut expected: Vec<String> = old_shape
            .iter()
            .map(|name| format!("{bc}/{name}"))
            .collect();
        expected.push(format!("{bc}/{}", TicketSummary::NAME));
        expected.sort();
        assert_eq!(kept, expected);
        assert!(report.registered.is_empty(), "{report:?}");
        assert_eq!(
            consumed_by_ticket_summary(&pool, &bc).await,
            new_shape,
            "the older build reverted TicketSummary's consumed event types"
        );
        assert!(
            db::get_event_type(&pool, &bc, TicketRated::NAME)
                .await
                .unwrap()
                .is_some(),
            "the older build removed the newer build's TicketRated"
        );

        // The newer build restarts too, and finds nothing to undo.
        let (_new_again, report) = start_newer(&database_url, &bc, &subject).await;
        assert!(report.kept_newer.is_empty(), "{report:?}");
        assert_eq!(consumed_by_ticket_summary(&pool, &bc).await, new_shape);
        for (table, name) in [
            (RegistrationTable::Projections, TicketSummary::NAME),
            (RegistrationTable::EventTypes, TicketRated::NAME),
            (RegistrationTable::EventTypes, TicketCreated::NAME),
        ] {
            assert_eq!(
                db::registration_version(&pool, &bc, table, name)
                    .await
                    .unwrap(),
                Some(APPLICATION_VERSION as i64),
                "{name} should carry the newer build's stamp"
            );
        }
    });
}
