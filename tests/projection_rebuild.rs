//! Changing a live projection without downtime: `TicketSummary` gains
//! `first_responder_staff_id` while tickets already exist, and the
//! rollout goes through `rebuildProjection`/`discardProjectionRebuild`
//! (skilj-graphql's `TypeRegistration` surface) - README's "Changing a
//! projection: zero-downtime rebuilds" tells the same story.
//!
//! One test, not one per mutation: both phases need the real
//! `"helpdesk"` bounded context with a `TicketSummary` registered by an
//! *older* build, and only one shape of it can be live at a time. It
//! runs in a database of its own (`fresh_database`): under
//! `DATABASE_URL` every test binary shares one, and any other test's
//! startup registers today's `TicketSummary` there first.
//!
//! What the switch-over is checked against: the old state stays what
//! every read sees while the rebuild builds, and the rebuilt state
//! replaces it in one step. The test holds the bounded context's
//! `sequence` row lock to keep the background catch-up from promoting
//! the rebuild until it has looked (`promote_projection_rebuild` takes
//! that lock first, docs/architecture.md §156 in the skilj repo; folding
//! into the rebuild doesn't need it). Without the lock, a 500ms catch-up
//! tick could promote between two reads.

mod support;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use skilj::Skilj;
use skilj_core::db::{self, Pool};
use skilj_core::plugin::Projection;
use skilj_helpdesk::helpdesk::{HelpdeskEvent, TicketSummary, TicketSummaryState, BOUNDED_CONTEXT};
use skilj_helpdesk::APPLICATION_VERSION;
use std::time::Duration;
use support::{
    accepted, fresh_database, graphql_request, mint_command_token, projection_state, runtime,
    seed_admin, serve_jwks, sign_jwt, test_idp_config, test_master_key, trigger, unique_name,
    wait_until,
};

/// `TicketSummaryState` as the previous build declares it: everything
/// but `first_responder_staff_id`. Renamed in its schema so the stored
/// schema differs from today's by that one field only, the way a real
/// older build's would.
#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
#[schemars(rename = "TicketSummaryState")]
struct StateBeforeFirstResponder {
    status: Option<String>,
    priority: Option<String>,
    assigned_staff_id: Option<String>,
    escalated: bool,
    rating: Option<u8>,
}

/// `TicketSummary` as the previous build declares it - same name, same
/// events, same fold, minus the new field.
struct TicketSummaryBeforeFirstResponder;

impl Projection for TicketSummaryBeforeFirstResponder {
    type State = StateBeforeFirstResponder;
    type Event = HelpdeskEvent;
    const NAME: &'static str = TicketSummary::NAME;
    const OWNER_TAG_KEY: Option<&'static str> = TicketSummary::OWNER_TAG_KEY;
    fn consumed_event_types() -> Vec<&'static str> {
        TicketSummary::consumed_event_types()
    }
    fn sync() -> bool {
        TicketSummary::sync()
    }
    fn keys(event: &Self::Event) -> Vec<String> {
        TicketSummary::keys(event)
    }
    fn project(state: &mut Self::State, event: &Self::Event, key: &str) {
        let mut current = TicketSummaryState {
            status: state.status.take(),
            priority: state.priority.take(),
            assigned_staff_id: state.assigned_staff_id.take(),
            escalated: state.escalated,
            rating: state.rating,
            first_responder_staff_id: None,
        };
        TicketSummary::project(&mut current, event, key);
        *state = StateBeforeFirstResponder {
            status: current.status,
            priority: current.priority,
            assigned_staff_id: current.assigned_staff_id,
            escalated: current.escalated,
            rating: current.rating,
        };
    }
}

const OLDER: u64 = APPLICATION_VERSION - 1;

async fn start(
    database_url: &str,
    subject: &str,
    jwks_url: &str,
    version: u64,
) -> (Skilj, skilj::ReconciliationReport) {
    let builder = skilj_helpdesk::register(Skilj::builder(database_url.to_string()))
        .encryption_master_key(test_master_key())
        .reconciliation_role(subject.to_string())
        .identity_provider(test_idp_config(jwks_url))
        .application_version(version);
    // `register()`'s `auto_register()` already added today's
    // `TicketSummary`; the builder keys projections by name, so this
    // replaces it rather than adding a second one.
    let builder = if version == OLDER {
        builder
            .bounded_context(BOUNDED_CONTEXT)
            .projection::<TicketSummaryBeforeFirstResponder>()
    } else {
        builder
    };
    builder.build().await.expect("the build starts")
}

async fn stop(skilj: Skilj) {
    let report = skilj.shutdown(Duration::from_secs(10)).await;
    assert!(report.aborted.is_empty(), "{report:?}");
}

async fn graphql(skilj: &Skilj, jwt: &str, query: &str) -> Value {
    let router = skilj.graphql_router().await.unwrap();
    let response = graphql_request(&router, jwt, query).await;
    assert!(
        response.get("errors").is_none(),
        "expected no GraphQL errors, got {response:?}"
    );
    response["data"].clone()
}

/// `TicketSummary`'s row in `projections(boundedContext:)`: the live
/// shape plus whichever rebuild is pending or building.
async fn ticket_summary_registration(skilj: &Skilj, jwt: &str) -> Value {
    let data = graphql(
        skilj,
        jwt,
        &format!(
            "query {{ projections(boundedContext: {BOUNDED_CONTEXT:?}) {{ name schemaVersion \
             pendingRebuild {{ status schemaVersion }} \
             buildingRebuild {{ status schemaVersion caughtUpTo }} }} }}"
        ),
    )
    .await;
    data["projections"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == TicketSummary::NAME)
        .expect("TicketSummary is registered")
        .clone()
}

/// The live `TicketSummary` state for one ticket, as stored - read
/// without a type so a field the stored shape doesn't have shows up as
/// absent rather than as a default.
async fn live_state(pool: &Pool, ticket_id: &str) -> Value {
    projection_state(pool, BOUNDED_CONTEXT, TicketSummary::NAME, ticket_id).await
}

async fn rebuild_state(pool: &Pool, ticket_id: &str) -> Option<Value> {
    db::get_projection_rebuild_state(pool, BOUNDED_CONTEXT, TicketSummary::NAME, ticket_id)
        .await
        .unwrap()
        .map(|json| serde_json::from_str(&json).unwrap())
}

/// Commands through one build's REST router, with tokens minted once -
/// a `CommandToken` lives in the database, so every build accepts it.
struct Staff {
    sign_up: String,
    create_ticket: String,
    assign_ticket: String,
    request_info: String,
    customer_responds: String,
}

impl Staff {
    async fn mint(pool: &Pool, mapping: &skilj_core::access_control::RoleAccessMapping) -> Self {
        let mint = |name| mint_command_token(pool, mapping, BOUNDED_CONTEXT, name);
        Staff {
            sign_up: mint("SignUpCompany").await,
            create_ticket: mint("CreateTicket").await,
            assign_ticket: mint("AssignTicket").await,
            request_info: mint("RequestInfoFromCustomer").await,
            customer_responds: mint("CustomerRespondsToTicket").await,
        }
    }
}

async fn submit(skilj: &Skilj, token: &str, payload: Value) {
    let response = trigger(&skilj.rest_router(), token, payload).await;
    assert!(accepted(&response), "{response:?}");
}

/// Signs up a company and opens, assigns and gets a first reply on one
/// ticket from `staff_id`, all through `skilj`. Returns the ticket's id
/// and requester.
async fn answered_ticket(
    skilj: &Skilj,
    staff: &Staff,
    company_id: &str,
    staff_id: &str,
) -> (String, String) {
    let ticket_id = unique_name("ticket");
    let requester_id = unique_name("customer");
    submit(
        skilj,
        &staff.create_ticket,
        serde_json::json!({
            "ticket_id": ticket_id, "company_id": company_id, "requester_id": requester_id,
            "logged_by_staff_id": null, "title": "t", "description": "d", "priority": "low",
        }),
    )
    .await;
    submit(
        skilj,
        &staff.assign_ticket,
        serde_json::json!({ "ticket_id": ticket_id, "staff_id": staff_id }),
    )
    .await;
    request_info(skilj, staff, &ticket_id, &requester_id, staff_id).await;
    (ticket_id, requester_id)
}

async fn request_info(
    skilj: &Skilj,
    staff: &Staff,
    ticket_id: &str,
    requester_id: &str,
    staff_id: &str,
) {
    submit(
        skilj,
        &staff.request_info,
        serde_json::json!({
            "ticket_id": ticket_id, "staff_id": staff_id,
            "message": "Which version?", "requester_id": requester_id,
        }),
    )
    .await;
}

async fn customer_responds(skilj: &Skilj, staff: &Staff, ticket_id: &str, requester_id: &str) {
    submit(
        skilj,
        &staff.customer_responds,
        serde_json::json!({
            "ticket_id": ticket_id, "requester_id": requester_id, "message": "2.1",
        }),
    )
    .await;
}

/// The rollout, step by step:
///
/// 1. The older build runs; a ticket gets its first reply from `alice`.
/// 2. Today's build starts. Its `TicketSummary` schema differs, so
///    reconciliation stages a rebuild (`pendingRebuild`) instead of
///    touching the live projection.
/// 3. The deploy is rolled back: `discardProjectionRebuild` drops the
///    staged rebuild, and the live projection is as it was.
/// 4. Today's build is deployed again and stages the rebuild again.
///    Until it runs, the live state is the old one, folded forward by the
///    new code from here on: `bob`'s later reply is the first one *it*
///    ever saw, so it names him - wrong, and the reason for a rebuild.
/// 5. `rebuildProjection` replays every event into a new state beside
///    the live one, which reads keep using until the replay has caught up.
/// 6. The rebuilt state replaces the live one in one step: `alice`.
#[test]
fn a_projection_change_is_discarded_then_rebuilt_and_switched_over() {
    runtime().block_on(async {
        let Some((database_url, pool)) = fresh_database("projection_rebuild").await else {
            return;
        };
        let mapping = seed_admin(&pool).await;
        let subject = mapping.role.external_subject.clone();
        let jwks_url = serve_jwks().await;
        let jwt = sign_jwt(&subject);

        // 1. Before the deploy.
        let (old, _) = start(&database_url, &subject, &jwks_url, OLDER).await;
        let staff = Staff::mint(&pool, &mapping).await;
        let company_id = unique_name("company");
        submit(
            &old,
            &staff.sign_up,
            serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
        )
        .await;
        let (ticket_id, requester_id) = answered_ticket(&old, &staff, &company_id, "alice").await;
        customer_responds(&old, &staff, &ticket_id, &requester_id).await;
        let before = live_state(&pool, &ticket_id).await;
        assert_eq!(before["status"], "in_progress");
        assert!(before.get("first_responder_staff_id").is_none(), "{before}");
        let old_schema_version = ticket_summary_registration(&old, &jwt).await["schemaVersion"]
            .as_i64()
            .unwrap();

        // 2. Today's build starts alongside it.
        let (new, report) = start(&database_url, &subject, &jwks_url, APPLICATION_VERSION).await;
        assert!(report.kept_newer.is_empty(), "{report:?}");
        let registration = ticket_summary_registration(&new, &jwt).await;
        assert_eq!(registration["schemaVersion"], old_schema_version);
        assert_eq!(registration["pendingRebuild"]["status"], "PENDING");
        assert_eq!(
            registration["pendingRebuild"]["schemaVersion"],
            old_schema_version + 1
        );
        assert!(registration["buildingRebuild"].is_null(), "{registration}");
        assert_eq!(live_state(&pool, &ticket_id).await, before);

        // 3. Rolled back: today's build goes away, and so does its
        // staged rebuild.
        stop(new).await;
        let discarded = graphql(
            &old,
            &jwt,
            &format!(
                "mutation {{ discardProjectionRebuild(boundedContext: {BOUNDED_CONTEXT:?}, \
                 name: \"TicketSummary\") {{ status schemaVersion }} }}"
            ),
        )
        .await;
        assert_eq!(discarded["discardProjectionRebuild"]["status"], "PENDING");
        let registration = ticket_summary_registration(&old, &jwt).await;
        assert_eq!(registration["schemaVersion"], old_schema_version);
        assert!(registration["pendingRebuild"].is_null(), "{registration}");
        assert_eq!(live_state(&pool, &ticket_id).await, before);

        // 4. Deployed again, and this time the older build is retired:
        // a rebuild is folded by whichever instance's catch-up gets to it
        // first, with that instance's own code, so an older build still
        // running would fold it into the old shape.
        let (new, report) = start(&database_url, &subject, &jwks_url, APPLICATION_VERSION).await;
        assert!(report.kept_newer.is_empty(), "{report:?}");
        stop(old).await;
        assert_eq!(
            ticket_summary_registration(&new, &jwt).await["pendingRebuild"]["status"],
            "PENDING"
        );
        request_info(&new, &staff, &ticket_id, &requester_id, "bob").await;
        let (fresh_ticket_id, _) = answered_ticket(&new, &staff, &company_id, "carol").await;
        assert_eq!(
            live_state(&pool, &ticket_id).await["first_responder_staff_id"],
            "bob",
            "the live state never saw alice's reply - only a rebuild can"
        );
        assert_eq!(
            live_state(&pool, &fresh_ticket_id).await["first_responder_staff_id"],
            "carol"
        );

        // 5. Start the rebuild, with promotion held back until the test
        // has looked at both states.
        let mut hold = pool.begin().await.unwrap();
        sqlx::query(r#"SELECT next_value FROM "bc_helpdesk".sequence FOR UPDATE"#)
            .execute(&mut *hold)
            .await
            .unwrap();
        let started = graphql(
            &new,
            &jwt,
            &format!(
                "mutation {{ rebuildProjection(boundedContext: {BOUNDED_CONTEXT:?}, \
                 name: \"TicketSummary\") {{ status schemaVersion }} }}"
            ),
        )
        .await;
        assert_eq!(started["rebuildProjection"]["status"], "BUILDING");
        wait_until(
            Duration::from_secs(30),
            "the rebuild to replay the whole history",
            || async {
                rebuild_state(&pool, &fresh_ticket_id)
                    .await
                    .is_some_and(|state| state["first_responder_staff_id"] == "carol")
            },
        )
        .await;
        let rebuilt = rebuild_state(&pool, &ticket_id).await.unwrap();
        assert_eq!(rebuilt["first_responder_staff_id"], "alice");
        assert_eq!(rebuilt["status"], "waiting_on_customer");
        assert_eq!(
            live_state(&pool, &ticket_id).await["first_responder_staff_id"],
            "bob",
            "reads must keep the live state until the switch-over"
        );
        let registration = ticket_summary_registration(&new, &jwt).await;
        assert_eq!(registration["schemaVersion"], old_schema_version);
        assert_eq!(registration["buildingRebuild"]["status"], "BUILDING");
        assert!(registration["pendingRebuild"].is_null(), "{registration}");

        // 6. Let the catch-up promote it.
        hold.rollback().await.unwrap();
        wait_until(
            Duration::from_secs(30),
            "the rebuild to be promoted",
            || async {
                live_state(&pool, &ticket_id).await["first_responder_staff_id"] == "alice"
            },
        )
        .await;
        let after = live_state(&pool, &ticket_id).await;
        assert_eq!(after, rebuilt, "the rebuilt state is what went live");
        assert_eq!(
            live_state(&pool, &fresh_ticket_id).await["first_responder_staff_id"],
            "carol"
        );
        let registration = ticket_summary_registration(&new, &jwt).await;
        assert_eq!(registration["schemaVersion"], old_schema_version + 1);
        assert!(registration["buildingRebuild"].is_null(), "{registration}");
        assert!(registration["pendingRebuild"].is_null(), "{registration}");

        // The new field reaches the projection's GraphQL type at the
        // switch-over, no restart needed: promotion notifies every instance
        // to rebuild its GraphQL schema, the way a registration does. That
        // notify is delivered asynchronously, hence the wait.
        let query = format!(
            r#"query {{ projection(boundedContext: {BOUNDED_CONTEXT:?}, name: "TicketSummary", key: {ticket_id:?}) {{ ... on helpdesk_TicketSummary {{ status firstResponderStaffId }} }} }}"#
        );
        wait_until(
            Duration::from_secs(30),
            "the promoted field to be queryable",
            || async {
                let response =
                    graphql_request(&new.graphql_router().await.unwrap(), &jwt, &query).await;
                response["data"]["projection"]["firstResponderStaffId"] == "alice"
            },
        )
        .await;

        // A restart finds nothing left to rebuild.
        stop(new).await;
        let (new, report) = start(&database_url, &subject, &jwks_url, APPLICATION_VERSION).await;
        assert!(report.kept_newer.is_empty(), "{report:?}");
        let registration = ticket_summary_registration(&new, &jwt).await;
        assert_eq!(registration["schemaVersion"], old_schema_version + 1);
        assert!(registration["pendingRebuild"].is_null(), "{registration}");
        let data = graphql(&new, &jwt, &query).await;
        assert_eq!(data["projection"]["firstResponderStaffId"], "alice");
    });
}
