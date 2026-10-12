//! Golden-transcript conformance between the two command surfaces
//! (issue #37, after skilj's `docs/kafgres-learnings.md` §1): one
//! realistic helpdesk scenario is driven once over REST
//! (`POST /v1/commands/trigger`, a `CommandToken` per command type) and
//! once over GraphQL (`submitCommand`, an IdP-issued JWT per Role), each
//! into a database of its own, and what each run left behind - the
//! stored events, the commands that caused them, every projection row and
//! every snapshot row - must come out identical once the values that are
//! *meant* to differ are normalised away.
//!
//! What is normalised, and why each one is allowed to differ, is
//! catalogued in `docs/rest-graphql-differences.md`; a new difference
//! goes there first, then into `normalise_*` below. Anything not in that
//! catalogue that differs between the two runs fails the test.
//!
//! The normalised transcript is checked in as
//! `tests/golden/rest_graphql_conformance.json`, so a change in what the
//! scenario stores shows up in review as a diff of that file. Regenerate
//! it with `UPDATE_GOLDEN=1 cargo test --test rest_graphql_conformance`.

mod support;

use serde_json::{json, Value};
use skilj::Skilj;
use skilj_core::access_control::{AccessLevel, Role, RoleAccessMapping, RoleStatus};
use skilj_core::bootstrap::ContextCreator;
use skilj_core::db::{self, Pool};
use skilj_core::encryption::{decrypt_ciphertext_leaves, DataKey};
use skilj_core::event_store::{BoundedContext, BoundedContextStatus, EventOrigin};
use skilj_core::plugin::Snapshot;
use skilj_core::shared::generate_token_id;
use skilj_helpdesk::helpdesk::{TicketSnapshot, BOUNDED_CONTEXT, STAFF_TEAM};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;
use support::{
    fresh_database, graphql_request, mint_command_token, runtime, seed_role, seed_scoped_mapping,
    serve_jwks, sign_jwt, submit_command_mutation, test_idp_config, test_master_key, test_now,
    trigger, wait_until,
};

const GOLDEN: &str = "tests/golden/rest_graphql_conformance.json";
const COMPANY: &str = "company-acme";
const TICKET: &str = "ticket-login";
const DUPLICATE: &str = "ticket-login-again";
const CUSTOMER: &str = "customer-alice";
const STAFF: &str = "staff-bob";

/// Who submits a step. Over REST every step is a `CommandToken` minted
/// from one admin mapping (a token is its own per-command grant); over
/// GraphQL each is the JWT of a Role shaped like the real caller - see
/// "Who may submit" in docs/rest-graphql-differences.md.
#[derive(Clone, Copy, Debug)]
enum Actor {
    Admin,
    Staff,
    Customer,
}

/// The scenario: a company signs up, its customer files a ticket (and
/// then the same problem again), staff picks it up, notes it, asks for
/// details, the customer answers, staff escalates, merges the duplicate
/// and resolves, the customer rates it - and then tries to rate it a
/// second time, so a business rejection is part of the transcript too.
fn scenario() -> Vec<(Actor, &'static str, Value)> {
    vec![
        (
            Actor::Admin,
            "SignUpCompany",
            json!({ "company_id": COMPANY, "name": "Acme", "contact_email": "support@acme.example" }),
        ),
        (
            Actor::Customer,
            "CreateTicket",
            json!({
                "ticket_id": TICKET, "company_id": COMPANY, "requester_id": CUSTOMER,
                "logged_by_staff_id": null, "title": "Can't log in", "description": "500 on /login",
                "priority": "medium", "requester_name": "Alice", "requester_email": "alice@acme.example",
            }),
        ),
        (
            Actor::Customer,
            "CreateTicket",
            json!({
                "ticket_id": DUPLICATE, "company_id": COMPANY, "requester_id": CUSTOMER,
                "logged_by_staff_id": null, "title": "Still can't log in", "description": "same 500",
                "priority": "low", "requester_name": "Alice", "requester_email": "alice@acme.example",
            }),
        ),
        (
            Actor::Staff,
            "AssignTicket",
            json!({ "ticket_id": TICKET, "staff_id": STAFF }),
        ),
        (
            Actor::Staff,
            "AddInternalNote",
            json!({ "ticket_id": TICKET, "staff_id": STAFF, "note": "Looks like the SSO outage" }),
        ),
        (
            Actor::Staff,
            "RequestInfoFromCustomer",
            json!({ "ticket_id": TICKET, "staff_id": STAFF, "message": "Which browser?", "requester_id": CUSTOMER }),
        ),
        (
            Actor::Customer,
            "CustomerRespondsToTicket",
            json!({ "ticket_id": TICKET, "requester_id": CUSTOMER, "message": "Firefox" }),
        ),
        (
            Actor::Staff,
            "EscalateTicket",
            json!({ "ticket_id": TICKET }),
        ),
        (
            Actor::Staff,
            "MergeTickets",
            json!({ "primary_ticket_id": TICKET, "duplicate_ticket_id": DUPLICATE }),
        ),
        (
            Actor::Staff,
            "ResolveTicket",
            json!({ "ticket_id": TICKET }),
        ),
        (
            Actor::Customer,
            "RateTicket",
            json!({ "ticket_id": TICKET, "rating": 5, "comment": "Quick fix, thanks", "requester_id": CUSTOMER }),
        ),
        (
            Actor::Customer,
            "RateTicket",
            json!({ "ticket_id": TICKET, "rating": 1, "comment": "Changed my mind", "requester_id": CUSTOMER }),
        ),
    ]
}

/// One run's database, with the helpdesk context, an unscoped admin Role
/// and a `Skilj` (REST and GraphQL both) built on it. Not `support::setup`:
/// that seeds its bounded context once per binary, and this binary needs
/// it in two databases.
struct Run {
    skilj: Skilj,
    pool: Pool,
    admin: RoleAccessMapping,
}

async fn start(prefix: &str) -> Option<Run> {
    let (database_url, pool) = fresh_database(prefix).await?;
    db::insert_bounded_context(
        &pool,
        &BoundedContext {
            name: BOUNDED_CONTEXT.to_string(),
            status: BoundedContextStatus::Active,
            created_at: test_now(),
            created_by: ContextCreator::SystemCreator,
            template: None,
        },
    )
    .await
    .unwrap();
    let role = Role {
        id: generate_token_id(),
        external_subject: support::unique_name("subject"),
        name: "Test Admin".into(),
        superadmin: false,
        status: RoleStatus::Active,
        created_at: test_now(),
        revoked_at: None,
    };
    db::insert_role(&pool, &role).await.unwrap();
    let admin = seed_scoped_mapping(&pool, &role, AccessLevel::Admin, None).await;
    let jwks_url = serve_jwks().await;
    let (skilj, _report) = skilj_helpdesk::register(Skilj::builder(database_url))
        .encryption_master_key(test_master_key())
        .reconciliation_role(role.external_subject.clone())
        .identity_provider(test_idp_config(&jwks_url))
        .build()
        .await
        .unwrap();
    Some(Run { skilj, pool, admin })
}

/// What a surface answered for one step, in one shape for both: the
/// REST body and GraphQL's `submitCommand` share their field names, but
/// REST leaves out what GraphQL answers as `null`, and the sequences are
/// each database's own, so only their count is kept.
fn outcome(command: &str, answer: &Value) -> Value {
    let field = |name: &str| answer.get(name).cloned().unwrap_or(Value::Null);
    json!({
        "command": command,
        "accepted": answer["accepted"].as_bool().expect("accepted is always answered"),
        "rejection_kind": field("rejectionKind"),
        "rejection_reason": field("rejectionReason"),
        "triggered_events": answer["triggeredEventSequences"].as_array().map_or(0, Vec::len),
    })
}

/// What driving the scenario over one surface answered, step by step, and
/// who each step was submitted as - the identifier the catalogue says
/// that surface stamps as `metadata.client_id`.
struct Driven {
    outcomes: Vec<Value>,
    clients: Vec<String>,
}

async fn drive_rest(run: &Run) -> Driven {
    let router = run.skilj.rest_router();
    let mut tokens = BTreeMap::new();
    let mut outcomes = Vec::new();
    let mut clients = Vec::new();
    for (_actor, command, payload) in scenario() {
        if !tokens.contains_key(command) {
            let token = mint_command_token(&run.pool, &run.admin, BOUNDED_CONTEXT, command).await;
            tokens.insert(command, token);
        }
        let response = trigger(&router, &tokens[command], payload).await;
        clients.push(tokens[command].split('.').next().unwrap().to_string());
        outcomes.push(outcome(command, &response));
    }
    Driven { outcomes, clients }
}

async fn drive_graphql(run: &Run) -> Driven {
    let router = run.skilj.graphql_router().await.unwrap();
    let admin = sign_jwt(&run.admin.role.external_subject);
    let staff_role = seed_role(&run.pool, STAFF_TEAM).await;
    seed_scoped_mapping(&run.pool, &staff_role, AccessLevel::Write, None).await;
    let staff = sign_jwt(&staff_role.external_subject);
    let customer_role = seed_role(&run.pool, "customer").await;
    seed_scoped_mapping(
        &run.pool,
        &customer_role,
        AccessLevel::Write,
        Some(COMPANY.into()),
    )
    .await;
    let customer = sign_jwt(&customer_role.external_subject);

    let mut outcomes = Vec::new();
    let mut clients = Vec::new();
    for (actor, command, payload) in scenario() {
        let (jwt, role) = match actor {
            Actor::Admin => (&admin, &run.admin.role),
            Actor::Staff => (&staff, &staff_role),
            Actor::Customer => (&customer, &customer_role),
        };
        clients.push(role.id.clone());
        let response = graphql_request(
            &router,
            jwt,
            &submit_command_mutation(BOUNDED_CONTEXT, command, &payload),
        )
        .await;
        assert!(
            response.get("errors").is_none(),
            "{command} as {actor:?}: {response}"
        );
        outcomes.push(outcome(command, &response["data"]["submitCommand"]));
    }
    Driven { outcomes, clients }
}

/// Waits for the background snapshot catch-up to fold every ticket's last
/// event into its `TicketSnapshot` row. A command folds the events since
/// the stored row in memory and leaves the row to that task, so without
/// this a row can still be one event behind - in either run.
async fn await_snapshots(pool: &Pool, events: &[skilj_core::event_store::Event]) {
    let mut last_per_ticket = BTreeMap::new();
    for event in events {
        for tag in &event.tags {
            if let (TicketSnapshot::TAG_KEY, Some(value)) = (tag.key.as_str(), &tag.value) {
                last_per_ticket.insert(value.clone(), event.sequence);
            }
        }
    }
    let query = format!(
        "SELECT tag_value, as_of_sequence FROM {}.snapshots WHERE snapshot_name = $1 AND tag_key = $2",
        schema()
    );
    wait_until(
        Duration::from_secs(30),
        "ticket snapshots to catch up",
        || {
            let (query, last_per_ticket) = (query.clone(), &last_per_ticket);
            async move {
                let stored: BTreeMap<String, i64> =
                    sqlx::query_as::<_, (String, i64)>(sqlx::AssertSqlSafe(query))
                        .bind(TicketSnapshot::NAME)
                        .bind(TicketSnapshot::TAG_KEY)
                        .fetch_all(pool)
                        .await
                        .unwrap()
                        .into_iter()
                        .collect();
                last_per_ticket
                    .iter()
                    .all(|(ticket, last)| stored.get(ticket).is_some_and(|as_of| as_of >= last))
            }
        },
    )
    .await;
}

/// Waits for every async projection to fold the last event it consumes,
/// so the projection rows read below are final rather than mid-catch-up.
async fn await_async_projections(pool: &Pool) {
    let lagging = format!(
        "SELECT p.name FROM {schema}.projections p \
         WHERE NOT p.sync AND COALESCE(p.caught_up_to, -1) < COALESCE(( \
             SELECT MAX(e.sequence) FROM {schema}.events e \
             JOIN {schema}.projection_consumed_event_types c \
               ON c.event_type_name = e.event_type_name AND c.projection_name = p.name), -1)",
        schema = schema()
    );
    wait_until(
        Duration::from_secs(30),
        "async projections to catch up",
        || {
            let lagging = lagging.clone();
            async move {
                sqlx::query_scalar::<_, String>(sqlx::AssertSqlSafe(lagging))
                    .fetch_all(pool)
                    .await
                    .unwrap()
                    .is_empty()
            }
        },
    )
    .await;
}

fn schema() -> String {
    format!("\"bc_{BOUNDED_CONTEXT}\"")
}

/// Everything one run left behind, normalised (see the module doc), and
/// alongside it the per-event values the catalogue says differ between
/// surfaces - kept out of the comparison, checked on their own instead.
struct Transcript {
    normalised: Value,
    provenance: Vec<Provenance>,
}

#[derive(Debug)]
struct Provenance {
    client_id: String,
    correlation_id: Option<String>,
    causation_id: Option<String>,
    command_id: String,
    command_client_id: String,
    command_correlation_id: Option<String>,
    command_causation_id: Option<String>,
}

async fn transcript(run: &Run, outcomes: Vec<Value>) -> Transcript {
    await_async_projections(&run.pool).await;
    let events = db::list_events_for_bounded_context(&run.pool, BOUNDED_CONTEXT)
        .await
        .unwrap();
    await_snapshots(&run.pool, &events).await;
    let (subjects, keys) = data_keys(&run.pool).await;
    // Sequences are positions in this run's own event table; the
    // transcript names each by its place in the scenario instead.
    let position: BTreeMap<i64, usize> = events
        .iter()
        .enumerate()
        .map(|(index, event)| (event.sequence, index + 1))
        .collect();
    let at = |sequence: Option<i64>| match sequence {
        Some(sequence) if sequence >= 0 => json!(format!(
            "#{}",
            position.get(&sequence).expect("a sequence this run stored")
        )),
        _ => Value::Null,
    };
    let plaintext = |stored: &str| {
        let mut value: Value = serde_json::from_str(stored).unwrap();
        decrypt_ciphertext_leaves(&mut value, &keys);
        value
    };

    let mut normalised_events = Vec::new();
    let mut provenance = Vec::new();
    for event in &events {
        let EventOrigin::CommandTriggered { command } = &event.origin else {
            panic!("every event in this scenario comes from a command: {event:?}");
        };
        normalised_events.push(json!({
            "at": at(Some(event.sequence)),
            "event_type": event.event_type.name,
            "schema_version": event.event_type.schema_version,
            "metadata_type": event.metadata.r#type,
            "metadata_version": event.metadata.version,
            "tags": event.tags,
            "payload": plaintext(&event.payload),
            "command": {
                "command_type": command.command_type.name,
                "schema_version": command.command_type.schema_version,
                "metadata_type": command.metadata.r#type,
                "metadata_version": command.metadata.version,
                "payload": plaintext(&command.payload),
                "consistency_tags": command.consistency_tags,
                "consistency_query": command.consistency_query,
                "consistency_boundary": at(command.consistency_boundary),
            },
        }));
        provenance.push(Provenance {
            client_id: event.metadata.client_id.clone(),
            correlation_id: event.metadata.correlation_id.clone(),
            causation_id: event.metadata.causation_id.clone(),
            command_id: command.id.clone(),
            command_client_id: command.metadata.client_id.clone(),
            command_correlation_id: command.metadata.correlation_id.clone(),
            command_causation_id: command.metadata.causation_id.clone(),
        });
    }

    let projections: Vec<(String, String, String, Option<String>, i64)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT projection_name, key, state, owner, as_of_sequence FROM {}.projection_state \
             ORDER BY projection_name, key",
            schema()
        )))
        .fetch_all(&run.pool)
        .await
        .unwrap();
    let projections: Vec<Value> = projections
        .into_iter()
        .map(|(name, key, state, owner, as_of)| {
            json!({
                "projection": name, "key": key, "owner": owner,
                "as_of": at(Some(as_of)), "state": plaintext(&state),
            })
        })
        .collect();

    let snapshots: Vec<(String, String, i64, i64, String, Option<String>)> =
        sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT snapshot_name, tag_key || ':' || tag_value, snapshot_version, as_of_sequence, \
             state::text, owner FROM {}.snapshots ORDER BY snapshot_name, tag_key, tag_value",
            schema()
        )))
        .fetch_all(&run.pool)
        .await
        .unwrap();
    let snapshots: Vec<Value> = snapshots
        .into_iter()
        .map(|(name, tag, version, as_of, state, owner)| {
            json!({
                "snapshot": name, "tag": tag, "owner": owner,
                "snapshot_version": version, "as_of": at(Some(as_of)), "state": plaintext(&state),
            })
        })
        .collect();

    Transcript {
        normalised: json!({
            "outcomes": outcomes,
            "events": normalised_events,
            "encryption_subjects": subjects,
            "projections": projections,
            "snapshots": snapshots,
        }),
        provenance,
    }
}

/// Every subject this run provisioned an encryption key for, and those
/// keys, so the transcript holds plaintext: ciphertext takes a fresh nonce
/// on every write and could never match between two runs.
async fn data_keys(pool: &Pool) -> (Vec<String>, Vec<DataKey>) {
    let subjects: Vec<(String, String, String)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT subject_key, subject_value, status FROM {}.encryption_keys \
         ORDER BY subject_key, subject_value",
        schema()
    )))
    .fetch_all(pool)
    .await
    .unwrap();
    let mut keys = Vec::new();
    for (_, subject_value, _) in &subjects {
        keys.extend(
            db::list_active_data_keys_for_subject_value(
                pool,
                BOUNDED_CONTEXT,
                subject_value,
                Some(&test_master_key()),
            )
            .await
            .unwrap(),
        );
    }
    let subjects = subjects
        .into_iter()
        .map(|(key, value, status)| format!("{key}:{value} ({status})"))
        .collect();
    (subjects, keys)
}

/// The values the transcript leaves out are not free-for-all: each must
/// differ only in the way docs/rest-graphql-differences.md says.
/// `client_id` names who submitted the step (a `CommandToken`'s id over
/// REST, the Role's id over GraphQL), the same on the command and every
/// event it triggered; each command gets a correlation id of its own,
/// which its events carry; each event's cause is its command.
fn check_provenance(surface: &str, driven: &Driven, provenance: &[Provenance]) {
    let submitted_by: Vec<&String> = driven
        .outcomes
        .iter()
        .zip(&driven.clients)
        .flat_map(|(outcome, client)| {
            let triggered = outcome["triggered_events"].as_u64().unwrap() as usize;
            std::iter::repeat_n(client, triggered)
        })
        .collect();
    assert_eq!(
        submitted_by.len(),
        provenance.len(),
        "{surface}: one stored event per triggered event"
    );
    let mut correlations = BTreeMap::new();
    for (index, (event, client)) in provenance.iter().zip(submitted_by).enumerate() {
        let at = format!("{surface} event #{}", index + 1);
        assert_eq!(
            &event.client_id, client,
            "{at}: client_id is the submitter: {event:?}"
        );
        assert_eq!(event.command_client_id, event.client_id, "{at}: {event:?}");
        let correlation = event
            .correlation_id
            .as_ref()
            .unwrap_or_else(|| panic!("{at}: {event:?}"));
        assert_eq!(
            event.command_correlation_id.as_ref(),
            Some(correlation),
            "{at}: {event:?}"
        );
        assert_eq!(
            event.causation_id.as_ref(),
            Some(&event.command_id),
            "{at}: {event:?}"
        );
        assert_eq!(event.command_causation_id, None, "{at}: {event:?}");
        let command = correlations
            .entry(correlation.clone())
            .or_insert(&event.command_id);
        assert_eq!(
            *command, &event.command_id,
            "{at}: a correlation id per command: {event:?}"
        );
    }
}

/// `assert_eq!` on two ~900-line documents buries the difference; this
/// names the lines that differ instead.
fn assert_same(left: &str, right: &str, what: &str) {
    if left == right {
        return;
    }
    let differing: Vec<String> = left
        .lines()
        .zip(right.lines())
        .enumerate()
        .filter(|(_, (l, r))| l != r)
        .take(20)
        .map(|(n, (l, r))| format!("line {}:\n  - {l}\n  + {r}", n + 1))
        .collect();
    panic!(
        "{what}\n{}\n({} vs {} lines)",
        differing.join("\n"),
        left.lines().count(),
        right.lines().count()
    );
}

#[test]
fn rest_and_graphql_leave_identical_events_and_projections() {
    runtime().block_on(async {
        let Some(rest) = start("conformance_rest").await else {
            return;
        };
        let Some(graphql) = start("conformance_graphql").await else {
            return;
        };

        let rest_driven = drive_rest(&rest).await;
        let graphql_driven = drive_graphql(&graphql).await;
        let rest_transcript = transcript(&rest, rest_driven.outcomes.clone()).await;
        let graphql_transcript = transcript(&graphql, graphql_driven.outcomes.clone()).await;

        check_provenance("REST", &rest_driven, &rest_transcript.provenance);
        check_provenance("GraphQL", &graphql_driven, &graphql_transcript.provenance);

        let rest_json = serde_json::to_string_pretty(&rest_transcript.normalised).unwrap();
        let graphql_json = serde_json::to_string_pretty(&graphql_transcript.normalised).unwrap();
        assert_same(
            &rest_json,
            &graphql_json,
            "REST and GraphQL diverged beyond docs/rest-graphql-differences.md",
        );

        let golden = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(GOLDEN);
        if std::env::var_os("UPDATE_GOLDEN").is_some() {
            std::fs::create_dir_all(golden.parent().unwrap()).unwrap();
            std::fs::write(&golden, format!("{rest_json}\n")).unwrap();
        }
        let expected = std::fs::read_to_string(&golden).unwrap_or_else(|e| {
            panic!("{GOLDEN} is missing ({e}) - regenerate it with UPDATE_GOLDEN=1")
        });
        assert_same(
            expected.trim_end(),
            &rest_json,
            &format!("the transcript drifted from {GOLDEN} - if intended, regenerate it with UPDATE_GOLDEN=1 and review the diff"),
        );
    });
}
