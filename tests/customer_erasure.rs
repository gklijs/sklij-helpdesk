//! GDPR erasure of one customer, end to end. Everything a customer wrote
//! or was asked - title, description, the conversation, their rating
//! comment - and their contact details are encrypted under that
//! customer's key (`helpdesk::CUSTOMER_SUBJECT`, keyed by `requester_id`),
//! and skilj's own `forgetSubject` mutation destroys the key.
//!
//! Before: the customer reads their own `CustomerTickets` row in
//! plaintext through their IdP subject alone, staff through
//! `can_read_sensitive`, and another customer of the same company only as
//! ciphertext. After: nobody can read it, in events or in the projection,
//! and a pending auto-close deadline naming the customer is resolved as
//! `forgotten` with its payload cleared (skilj 0.0.9, §132). A second
//! customer's data and deadline are left alone.
//!
//! Runs with the real 7-day `auto_close_after`, like
//! `tests/ticket_auto_close_cancel.rs`, so the deadlines stay pending
//! until the erasure. Reads `events`/`commands`/`deadlines` directly for
//! what's stored; reads through GraphQL for what a caller sees.

mod support;

use skilj_core::access_control::{AccessLevel, RoleAccessMapping, RoleStatus};
use skilj_core::db;
use skilj_helpdesk::helpdesk::{BOUNDED_CONTEXT, CUSTOMER_SUBJECT};
use std::time::Duration;
use support::{
    accepted, graphql_request, mint_command_token, runtime, seed_role, seed_scoped_mapping,
    setup_graphql, sign_jwt, test_db, test_now, trigger, unique_name, wait_until,
};

/// A customer with an IdP identity of their own: their `requester_id` is
/// their Role's `external_subject`, as in the real frontend.
struct Customer {
    requester_id: String,
    jwt: String,
    ticket_id: String,
    name: String,
    email: String,
}

impl Customer {
    async fn seed(pool: &db::Pool, company_id: &str, label: &str) -> Self {
        let role = seed_role(pool, "customer").await;
        seed_scoped_mapping(
            pool,
            &role,
            AccessLevel::Write,
            Some(company_id.to_string()),
        )
        .await;
        Customer {
            jwt: sign_jwt(&role.external_subject),
            requester_id: role.external_subject,
            ticket_id: unique_name(&format!("ticket-{label}")),
            name: format!("{label} Doe"),
            email: format!("{label}@customer.example"),
        }
    }

    fn title(&self) -> String {
        format!("{} can't log in", self.name)
    }

    /// Every piece of customer text this test files, as the frontend
    /// would see it in `CustomerTickets`.
    fn texts(&self) -> [String; 5] {
        [
            self.title(),
            format!("{} description", self.name),
            format!("which browser, {}?", self.name),
            format!("{} uses Firefox", self.name),
            format!("thanks from {}", self.name),
        ]
    }
}

/// The stored `payload` of this ticket's one `table` row of `type_name`.
async fn stored_payload(
    pool: &db::Pool,
    table: &str,
    type_column: &str,
    type_name: &str,
    ticket_id: &str,
) -> serde_json::Value {
    let (payload,): (String,) = sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT payload FROM \"bc_{BOUNDED_CONTEXT}\".{table} \
         WHERE {type_column} = $1 AND payload::jsonb->>'ticket_id' = $2"
    )))
    .bind(type_name)
    .bind(ticket_id)
    .fetch_one(pool)
    .await
    .unwrap();
    serde_json::from_str(&payload).unwrap()
}

/// This ticket's `ScheduleTicketAutoClose` rows, as `(status, payload)`.
async fn auto_close_deadlines(pool: &db::Pool, ticket_id: &str) -> Vec<(String, String)> {
    let ticket_tag = serde_json::json!([{ "key": "ticket", "value": ticket_id }]).to_string();
    sqlx::query_as(sqlx::AssertSqlSafe(format!(
        "SELECT status, payload FROM \"bc_{BOUNDED_CONTEXT}\".deadlines \
         WHERE schedule_name = 'ScheduleTicketAutoClose' AND tags @> $1::jsonb"
    )))
    .bind(ticket_tag)
    .fetch_all(pool)
    .await
    .unwrap()
}

/// `TicketCreated` for this ticket, as `queryEvents` renders it to `jwt`.
async fn rendered_ticket_created(
    router: &axum::Router,
    jwt: &str,
    ticket_id: &str,
) -> serde_json::Value {
    let query = format!(
        "query {{ queryEvents(boundedContext: {BOUNDED_CONTEXT:?}, eventTypes: [\"TicketCreated\"]) {{ payload }} }}"
    );
    let response = graphql_request(router, jwt, &query).await;
    assert!(
        response.get("errors").is_none(),
        "queryEvents failed: {response:?}"
    );
    response["data"]["queryEvents"]
        .as_array()
        .unwrap()
        .iter()
        .map(|event| {
            serde_json::from_str::<serde_json::Value>(event["payload"].as_str().unwrap()).unwrap()
        })
        .find(|payload| payload["ticket_id"] == ticket_id)
        .unwrap_or_else(|| panic!("no TicketCreated for {ticket_id}"))
}

/// `customer`'s `CustomerTickets` row as `jwt` reads it, flattened to the
/// strings a ticket view would show.
async fn rendered_content(router: &axum::Router, jwt: &str, customer: &Customer) -> Vec<String> {
    let query = format!(
        "query {{ projection(boundedContext: {BOUNDED_CONTEXT:?}, name: \"CustomerTickets\", key: {:?}) {{ ... on helpdesk_CustomerTickets {{ tickets }} }} }}",
        customer.requester_id
    );
    let response = graphql_request(router, jwt, &query).await;
    assert!(
        response.get("errors").is_none(),
        "CustomerTickets read failed: {response:?}"
    );
    let tickets: serde_json::Value =
        serde_json::from_str(response["data"]["projection"]["tickets"].as_str().unwrap()).unwrap();
    let ticket = &tickets[&customer.ticket_id];
    let mut texts = vec![
        ticket["title"].as_str().unwrap().to_string(),
        ticket["description"].as_str().unwrap().to_string(),
    ];
    for message in ticket["messages"].as_array().unwrap() {
        texts.push(message["text"].as_str().unwrap().to_string());
    }
    texts.push(ticket["rating_comment"].as_str().unwrap().to_string());
    texts
}

fn plaintext(texts: &[String], customer: &Customer) -> bool {
    texts == customer.texts()
}

fn none_readable(texts: &[String], customer: &Customer) -> bool {
    texts.len() == customer.texts().len() && texts.iter().all(|t| !customer.texts().contains(t))
}

#[test]
fn forgetting_a_customer_erases_their_data_and_pending_auto_close() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, admin, admin_jwt) = setup_graphql().await;
        let rest = skilj.rest_router();
        let graphql = skilj.graphql_router().await.unwrap();

        // Staff: may read sensitive fields. `seed_admin`'s own mapping
        // can't, so it never sees plaintext either way.
        let staff_role = seed_role(&pool, "staff").await;
        db::insert_role_access_mapping(
            &pool,
            &RoleAccessMapping {
                role: staff_role.clone(),
                bounded_context: admin.bounded_context.clone(),
                level: AccessLevel::Admin,
                can_read_sensitive: true,
                scope: None,
                status: RoleStatus::Active,
                created_at: test_now(),
                revoked_at: None,
            },
        )
        .await
        .unwrap();
        let staff_jwt = sign_jwt(&staff_role.external_subject);

        let mut tokens = std::collections::HashMap::new();
        for command in [
            "SignUpCompany",
            "CreateTicket",
            "AssignTicket",
            "RequestInfoFromCustomer",
            "CustomerRespondsToTicket",
            "ResolveTicket",
            "RateTicket",
        ] {
            tokens.insert(
                command,
                mint_command_token(&pool, &admin, BOUNDED_CONTEXT, command).await,
            );
        }
        let company_id = unique_name("company");
        let response = trigger(
            &rest,
            &tokens["SignUpCompany"],
            serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
        )
        .await;
        assert!(accepted(&response), "{response:?}");

        let forgotten = Customer::seed(&pool, &company_id, "jane").await;
        let kept = Customer::seed(&pool, &company_id, "john").await;
        for customer in [&forgotten, &kept] {
            let [title, description, question, answer, comment] = customer.texts();
            let (ticket_id, requester_id) = (&customer.ticket_id, &customer.requester_id);
            for (command, payload) in [
                (
                    "CreateTicket",
                    serde_json::json!({
                        "ticket_id": ticket_id, "company_id": company_id,
                        "requester_id": requester_id, "logged_by_staff_id": null,
                        "title": title, "description": description, "priority": "low",
                        "requester_name": customer.name, "requester_email": customer.email,
                    }),
                ),
                (
                    "AssignTicket",
                    serde_json::json!({ "ticket_id": ticket_id, "staff_id": "staff-1" }),
                ),
                (
                    "RequestInfoFromCustomer",
                    serde_json::json!({
                        "ticket_id": ticket_id, "staff_id": "staff-1", "message": question,
                        "requester_id": requester_id,
                    }),
                ),
                (
                    "CustomerRespondsToTicket",
                    serde_json::json!({ "ticket_id": ticket_id, "requester_id": requester_id, "message": answer }),
                ),
                ("ResolveTicket", serde_json::json!({ "ticket_id": ticket_id })),
                (
                    "RateTicket",
                    serde_json::json!({
                        "ticket_id": ticket_id, "rating": 5, "comment": comment,
                        "requester_id": requester_id,
                    }),
                ),
            ] {
                let response = trigger(&rest, &tokens[command], payload).await;
                assert!(
                    accepted(&response),
                    "{command} should be accepted: {response:?}"
                );
            }
        }

        // Stored encrypted, in every event and in the command that made
        // it; `requester_id` stays plaintext, it's what the key is found by.
        for customer in [&forgotten, &kept] {
            let [title, description, question, answer, comment] = customer.texts();
            for (table, type_column, type_name, fields) in [
                (
                    "events",
                    "event_type_name",
                    "TicketCreated",
                    vec![
                        ("title", title.clone()),
                        ("description", description.clone()),
                        ("requester_name", customer.name.clone()),
                        ("requester_email", customer.email.clone()),
                    ],
                ),
                (
                    "commands",
                    "command_type_name",
                    "CreateTicket",
                    vec![
                        ("title", title.clone()),
                        ("requester_email", customer.email.clone()),
                    ],
                ),
                ("events", "event_type_name", "TicketInfoRequested", vec![("message", question.clone())]),
                ("commands", "command_type_name", "RequestInfoFromCustomer", vec![("message", question.clone())]),
                ("events", "event_type_name", "TicketCustomerResponded", vec![("message", answer.clone())]),
                ("commands", "command_type_name", "CustomerRespondsToTicket", vec![("message", answer.clone())]),
                ("events", "event_type_name", "TicketRated", vec![("comment", comment.clone())]),
                ("commands", "command_type_name", "RateTicket", vec![("comment", comment.clone())]),
            ] {
                let stored =
                    stored_payload(&pool, table, type_column, type_name, &customer.ticket_id).await;
                assert_eq!(stored["requester_id"], customer.requester_id.as_str());
                for (field, plain) in fields {
                    assert!(stored[field].is_string(), "{type_name}.{field}: {stored}");
                    assert_ne!(stored[field], plain.as_str(), "{type_name}.{field}: {stored}");
                }
            }
        }

        // The queue the whole company reads holds none of it. Async
        // (issue #15), so wait until it has folded the forgotten
        // customer's last event (`TicketRated`) before checking.
        let queue_query = format!(
            "query {{ projection(boundedContext: {BOUNDED_CONTEXT:?}, name: \"CompanyActiveTickets\", key: {company_id:?}) {{ ... on helpdesk_CompanyActiveTickets {{ tickets }} }} }}"
        );
        let read_queue = || async {
            let response = graphql_request(&graphql, &staff_jwt, &queue_query).await;
            response["data"]["projection"]["tickets"].as_str().map(str::to_owned)
        };
        wait_until(Duration::from_secs(10), "CompanyActiveTickets catch-up", || async {
            read_queue().await.is_some_and(|queue| {
                serde_json::from_str::<serde_json::Value>(&queue).unwrap()[&forgotten.ticket_id]["rating"]
                    .is_number()
            })
        })
        .await;
        let queue = read_queue().await.unwrap();
        let queue = queue.as_str();
        assert!(queue.contains(&forgotten.ticket_id), "{queue}");
        for text in forgotten.texts() {
            assert!(!queue.contains(&text), "{text:?} in {queue}");
        }

        // Readable to the customer themselves and to staff...
        assert!(plaintext(&rendered_content(&graphql, &forgotten.jwt, &forgotten).await, &forgotten));
        assert!(plaintext(&rendered_content(&graphql, &staff_jwt, &forgotten).await, &forgotten));
        let before = rendered_ticket_created(&graphql, &staff_jwt, &forgotten.ticket_id).await;
        assert_eq!(before["requester_name"], forgotten.name.as_str());
        assert_eq!(before["requester_email"], forgotten.email.as_str());
        // ...and not to another customer of the same company, nor to an
        // admin without the sensitive-read grant.
        assert!(none_readable(&rendered_content(&graphql, &kept.jwt, &forgotten).await, &forgotten));
        let unprivileged = rendered_ticket_created(&graphql, &admin_jwt, &forgotten.ticket_id).await;
        assert_ne!(unprivileged["requester_email"], forgotten.email.as_str());

        // Each resolution has a pending auto-close naming its customer.
        for customer in [&forgotten, &kept] {
            wait_until(
                Duration::from_secs(10),
                "the auto-close deadline is scheduled",
                || {
                    let pool = pool.clone();
                    let ticket_id = customer.ticket_id.clone();
                    async move { auto_close_deadlines(&pool, &ticket_id).await.len() == 1 }
                },
            )
            .await;
            let (status, payload) = auto_close_deadlines(&pool, &customer.ticket_id)
                .await
                .remove(0);
            assert_eq!(status, "pending");
            let payload: serde_json::Value = serde_json::from_str(&payload).unwrap();
            assert_eq!(payload["requester_id"], customer.requester_id.as_str());
        }

        let erase = format!(
            "mutation {{ forgetSubject(boundedContext: {BOUNDED_CONTEXT:?}, subjectKey: {CUSTOMER_SUBJECT:?}, subjectValue: {:?}) {{ status subjectValue }} }}",
            forgotten.requester_id
        );
        let response = graphql_request(&graphql, &admin_jwt, &erase).await;
        assert!(
            response.get("errors").is_none(),
            "forgetSubject failed: {response:?}"
        );
        assert_eq!(response["data"]["forgetSubject"]["status"], "DESTROYED");
        assert_eq!(
            response["data"]["forgetSubject"]["subjectValue"],
            forgotten.requester_id.as_str()
        );

        // Unreadable now, to the customer and to staff alike.
        assert!(none_readable(&rendered_content(&graphql, &forgotten.jwt, &forgotten).await, &forgotten));
        assert!(none_readable(&rendered_content(&graphql, &staff_jwt, &forgotten).await, &forgotten));
        let after = rendered_ticket_created(&graphql, &staff_jwt, &forgotten.ticket_id).await;
        assert_eq!(after["requester_id"], forgotten.requester_id.as_str());
        assert_ne!(after["requester_name"], forgotten.name.as_str());
        assert_ne!(after["requester_email"], forgotten.email.as_str());
        // The other customer is untouched.
        assert!(plaintext(&rendered_content(&graphql, &kept.jwt, &kept).await, &kept));
        assert!(plaintext(&rendered_content(&graphql, &staff_jwt, &kept).await, &kept));
        let other = rendered_ticket_created(&graphql, &staff_jwt, &kept.ticket_id).await;
        assert_eq!(other["requester_email"], kept.email.as_str());

        // The forgotten customer's auto-close will never fire and holds
        // nothing about them; the other customer's is untouched.
        assert_eq!(
            auto_close_deadlines(&pool, &forgotten.ticket_id).await,
            vec![("forgotten".to_string(), "{}".to_string())]
        );
        let kept_deadlines = auto_close_deadlines(&pool, &kept.ticket_id).await;
        assert_eq!(kept_deadlines.len(), 1);
        assert_eq!(kept_deadlines[0].0, "pending");

        // There is nothing left to forget.
        let response = graphql_request(&graphql, &admin_jwt, &erase).await;
        assert!(
            response.get("errors").is_some(),
            "a second forgetSubject should fail: {response:?}"
        );
    });
}
