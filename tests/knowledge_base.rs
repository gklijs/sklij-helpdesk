//! Integration tests for specs/knowledge_base.allium, against the real
//! `src/knowledge_base.rs` and the `HelpdeskResolutionToArticleSuggestion`
//! route `lib.rs`'s `register()` wires in.
//!
//! Like tests/marketing.rs, the suggestion side is driven from its
//! source: a real helpdesk ticket resolved over REST, then a wait for the
//! route's background poll task to deliver it. The staff side (dismiss,
//! draft, edit, publish) is submitted directly, over REST for the
//! accept/reject rules and over GraphQL where the caller's Role matters.
//!
//! `KnowledgeBaseReview`/`PublishedArticles` are single shared instances
//! (key `""`), and under `DATABASE_URL` every test binary shares one
//! database - and the route starts from the beginning of `helpdesk`'s
//! history - so every assertion below looks for this test's own ids
//! rather than counting rows.

mod support;

use skilj_core::access_control::AccessLevel;
use skilj_core::db::Pool;
use skilj_helpdesk::helpdesk::STAFF_ROLE;
use skilj_helpdesk::knowledge_base::{KnowledgeBaseReviewState, PublishedArticlesState};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use support::{
    accepted, consume_auto, graphql_request, mint_command_token, mint_event_read_token,
    projection_state, rejection_kind, runtime, seed_mapping_for, seed_role, setup_all_contexts,
    sign_jwt, submit_command_mutation, test_db, trigger, unique_name, wait_until,
    AllContextMappings,
};

const HELPDESK: &str = skilj_helpdesk::helpdesk::BOUNDED_CONTEXT;
const KB: &str = skilj_helpdesk::knowledge_base::BOUNDED_CONTEXT;

/// Submits `command` over REST with a fresh token and returns the
/// response, accepted or not.
async fn submit(
    pool: &Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    router: &axum::Router,
    context: &str,
    command: &str,
    payload: serde_json::Value,
) -> serde_json::Value {
    let token = mint_command_token(pool, mapping, context, command).await;
    trigger(router, &token, payload).await
}

async fn submit_ok(
    pool: &Pool,
    mapping: &skilj_core::access_control::RoleAccessMapping,
    router: &axum::Router,
    context: &str,
    command: &str,
    payload: serde_json::Value,
) {
    let response = submit(pool, mapping, router, context, command, payload).await;
    assert!(
        accepted(&response),
        "{command} should be accepted: {response:?}"
    );
}

async fn submit_rejected(
    pool: &Pool,
    mappings: &AllContextMappings,
    router: &axum::Router,
    command: &str,
    payload: serde_json::Value,
    kind: &str,
) {
    let response = submit(pool, &mappings.knowledge_base, router, KB, command, payload).await;
    assert!(
        !accepted(&response),
        "{command} should be rejected: {response:?}"
    );
    assert_eq!(rejection_kind(&response), kind, "{command}: {response:?}");
}

/// A company with one ticket taken all the way to `resolved`.
async fn resolved_ticket(
    pool: &Pool,
    mappings: &AllContextMappings,
    router: &axum::Router,
) -> (String, String) {
    let company_id = unique_name("company");
    let ticket_id = unique_name("ticket");
    let helpdesk = &mappings.helpdesk;
    submit_ok(
        pool,
        helpdesk,
        router,
        HELPDESK,
        "SignUpCompany",
        serde_json::json!({ "company_id": company_id, "name": "Acme", "contact_email": "a@acme.example" }),
    )
    .await;
    submit_ok(
        pool,
        helpdesk,
        router,
        HELPDESK,
        "CreateTicket",
        serde_json::json!({
            "ticket_id": ticket_id, "company_id": company_id,
            "requester_id": unique_name("customer"), "logged_by_staff_id": null,
            "title": "Can't reset my password", "description": "The link 404s", "priority": "medium",
        }),
    )
    .await;
    resolve(pool, mappings, router, &ticket_id).await;
    (company_id, ticket_id)
}

/// Assign then resolve.
async fn resolve(
    pool: &Pool,
    mappings: &AllContextMappings,
    router: &axum::Router,
    ticket_id: &str,
) {
    for (command, payload) in [
        (
            "AssignTicket",
            serde_json::json!({ "ticket_id": ticket_id, "staff_id": unique_name("staff") }),
        ),
        (
            "ResolveTicket",
            serde_json::json!({ "ticket_id": ticket_id }),
        ),
    ] {
        submit_ok(pool, &mappings.helpdesk, router, HELPDESK, command, payload).await;
    }
}

async fn review(pool: &Pool) -> KnowledgeBaseReviewState {
    projection_state(pool, KB, "KnowledgeBaseReview", "").await
}

async fn published(pool: &Pool) -> PublishedArticlesState {
    projection_state(pool, KB, "PublishedArticles", "").await
}

fn is_pending(state: &KnowledgeBaseReviewState, ticket_id: &str) -> bool {
    state
        .pending_suggestions
        .iter()
        .any(|s| s.ticket_id == ticket_id)
}

async fn wait_for_suggestion(pool: &Pool, ticket_id: &str) {
    wait_until(
        Duration::from_secs(10),
        "the resolved ticket to be suggested",
        || async { is_pending(&review(pool).await, ticket_id) },
    )
    .await;
}

/// Drains `read_token`'s feed (via `wait_until`, as tests/marketing.rs
/// does) until `ticket_id`'s suggestion has turned up, then returns how
/// many `ArticleSuggested` events for that ticket the feed held. Every
/// event read is counted, since `consume_auto` consumes them.
async fn suggestions_on_feed(router: &axum::Router, read_token: &str, ticket_id: &str) -> usize {
    let count = Arc::new(Mutex::new(0usize));
    wait_until(
        Duration::from_secs(10),
        "ArticleSuggested on the feed",
        || {
            let count = count.clone();
            async move {
                let consumed = consume_auto(router, read_token).await;
                let found = consumed["events"]
                    .as_array()
                    .map(|events| {
                        events
                            .iter()
                            .filter(|e| e["payload"]["ticket_id"] == ticket_id)
                            .count()
                    })
                    .unwrap_or(0);
                let mut count = count.lock().unwrap();
                *count += found;
                *count > 0
            }
        },
    )
    .await;
    // Drain whatever else is there, in case a duplicate landed later.
    for _ in 0..3 {
        let consumed = consume_auto(router, read_token).await;
        if let Some(events) = consumed["events"].as_array() {
            *count.lock().unwrap() += events
                .iter()
                .filter(|e| e["payload"]["ticket_id"] == ticket_id)
                .count();
        }
    }
    let total = *count.lock().unwrap();
    total
}

/// `rule ArticleSuggestionIsRoutedFromHelpdesk` + `rule ArticleIsSuggested`:
/// resolving a ticket in helpdesk queues it for staff in knowledge_base,
/// with the ticket's company carried over.
#[test]
fn resolving_a_ticket_routes_an_article_suggestion() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mappings) = setup_all_contexts().await;
        let router = skilj.rest_router();
        let read_suggested =
            mint_event_read_token(&pool, &mappings.knowledge_base, KB, "ArticleSuggested").await;

        let (company_id, ticket_id) = resolved_ticket(&pool, &mappings, &router).await;

        wait_for_suggestion(&pool, &ticket_id).await;
        let state = review(&pool).await;
        let suggestion = state
            .pending_suggestions
            .iter()
            .find(|s| s.ticket_id == ticket_id)
            .unwrap();
        assert_eq!(suggestion.company_id, company_id);
        assert!(chrono::DateTime::parse_from_rfc3339(&suggestion.suggested_at).is_ok());
        assert_eq!(
            suggestions_on_feed(&router, &read_suggested, &ticket_id).await,
            1
        );
    });
}

/// A ticket is suggested once, however often it is delivered: a second
/// resolution after a reopen goes through the route again and is rejected
/// by `SuggestArticle`, and so is the same payload resubmitted by hand -
/// a redelivery that the route's own idempotency key wouldn't catch.
#[test]
fn a_ticket_is_suggested_once_however_often_it_is_delivered() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mappings) = setup_all_contexts().await;
        let router = skilj.rest_router();
        let read_suggested =
            mint_event_read_token(&pool, &mappings.knowledge_base, KB, "ArticleSuggested").await;

        let (company_id, ticket_id) = resolved_ticket(&pool, &mappings, &router).await;
        wait_for_suggestion(&pool, &ticket_id).await;

        submit_ok(
            &pool,
            &mappings.helpdesk,
            &router,
            HELPDESK,
            "ReopenTicket",
            serde_json::json!({ "ticket_id": ticket_id }),
        )
        .await;
        // Reopening puts it back in progress, so it resolves directly.
        submit_ok(
            &pool,
            &mappings.helpdesk,
            &router,
            HELPDESK,
            "ResolveTicket",
            serde_json::json!({ "ticket_id": ticket_id }),
        )
        .await;
        // The route delivers in source order, so once a ticket resolved
        // after the re-resolution is suggested, the re-resolution has
        // been delivered too.
        let (_, later_ticket) = resolved_ticket(&pool, &mappings, &router).await;
        wait_for_suggestion(&pool, &later_ticket).await;

        let state = review(&pool).await;
        let ours = state
            .pending_suggestions
            .iter()
            .filter(|s| s.ticket_id == ticket_id)
            .count();
        assert_eq!(ours, 1, "the re-resolution must not queue the ticket twice");
        assert_eq!(
            suggestions_on_feed(&router, &read_suggested, &ticket_id).await,
            1
        );

        // The same payload again, as a redelivery would bring it -
        // `SuggestArticle` isn't REST-triggerable, so over GraphQL.
        let graphql = skilj.graphql_router().await.unwrap();
        let system = seed_role(&pool, "route-redelivery").await;
        seed_mapping_for(&pool, &system, KB, AccessLevel::Write, None).await;
        let response = graphql_request(
            &graphql,
            &sign_jwt(&system.external_subject),
            &submit_command_mutation(
                KB,
                "SuggestArticle",
                &serde_json::json!({
                    "ticket_id": ticket_id, "company_id": company_id,
                    "suggested_at": chrono::Utc::now().to_rfc3339(),
                }),
            ),
        )
        .await;
        assert!(response.get("errors").is_none(), "{response:?}");
        assert_eq!(response["data"]["submitCommand"]["accepted"], false);
        assert_eq!(
            response["data"]["submitCommand"]["rejectionKind"],
            "already_suggested"
        );
    });
}

/// `rule StaffDismissesSuggestion`, `StaffDraftsArticle`,
/// `StaffEditsArticle`, `StaffPublishesArticle`: one suggestion dismissed,
/// one drafted, edited and published twice - each step's rejections
/// alongside it - ending in both projections.
#[test]
fn staff_review_accepts_and_rejects_by_suggestion_and_article_state() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, mappings) = setup_all_contexts().await;
        let router = skilj.rest_router();
        let (_, dismissed) = resolved_ticket(&pool, &mappings, &router).await;
        let (_, drafted) = resolved_ticket(&pool, &mappings, &router).await;
        wait_for_suggestion(&pool, &dismissed).await;
        wait_for_suggestion(&pool, &drafted).await;
        let article_id = unique_name("article");
        let kb = &mappings.knowledge_base;

        // Dismiss one suggestion; it can't be dismissed or drafted again.
        let dismiss = serde_json::json!({ "ticket_id": dismissed, "reason": "one-off" });
        submit_ok(&pool, kb, &router, KB, "DismissSuggestion", dismiss.clone()).await;
        submit_rejected(&pool, &mappings, &router, "DismissSuggestion", dismiss, "suggestion_already_dismissed").await;
        submit_rejected(
            &pool,
            &mappings,
            &router,
            "DraftArticle",
            serde_json::json!({ "article_id": unique_name("article"), "from_ticket_id": dismissed, "title": "T", "body": "B" }),
            "suggestion_already_dismissed",
        )
        .await;
        submit_rejected(
            &pool,
            &mappings,
            &router,
            "DismissSuggestion",
            serde_json::json!({ "ticket_id": unique_name("ticket"), "reason": null }),
            "suggestion_not_found",
        )
        .await;

        // Draft from the other; the suggestion is then spent.
        let draft = serde_json::json!({
            "article_id": article_id, "from_ticket_id": drafted,
            "title": "Resetting your password", "body": "Use the link on the login page.",
        });
        submit_ok(&pool, kb, &router, KB, "DraftArticle", draft.clone()).await;
        submit_rejected(&pool, &mappings, &router, "DraftArticle", draft, "article_exists").await;
        submit_rejected(
            &pool,
            &mappings,
            &router,
            "DismissSuggestion",
            serde_json::json!({ "ticket_id": drafted, "reason": null }),
            "suggestion_already_drafted",
        )
        .await;

        // Publish; again is nothing new; edit, then publish revision 2.
        let publish = serde_json::json!({ "article_id": article_id });
        submit_ok(&pool, kb, &router, KB, "PublishArticle", publish.clone()).await;
        submit_rejected(&pool, &mappings, &router, "PublishArticle", publish.clone(), "nothing_to_publish").await;
        submit_rejected(
            &pool,
            &mappings,
            &router,
            "EditArticle",
            serde_json::json!({ "article_id": article_id, "title": " ", "body": "x" }),
            "empty_title",
        )
        .await;
        submit_ok(
            &pool,
            kb,
            &router,
            KB,
            "EditArticle",
            serde_json::json!({ "article_id": article_id, "title": "Resetting a password", "body": "Click the link." }),
        )
        .await;
        submit_ok(&pool, kb, &router, KB, "PublishArticle", publish).await;
        submit_rejected(
            &pool,
            &mappings,
            &router,
            "PublishArticle",
            serde_json::json!({ "article_id": unique_name("article") }),
            "article_not_found",
        )
        .await;

        wait_until(Duration::from_secs(10), "PublishedArticles catch-up", || async {
            published(&pool)
                .await
                .articles
                .iter()
                .any(|a| a.article_id == article_id && a.revision == 2)
        })
        .await;
        let live = published(&pool).await;
        let article = live.articles.iter().find(|a| a.article_id == article_id).unwrap();
        assert_eq!(article.title, "Resetting a password");
        assert_eq!(article.body, "Click the link.");

        wait_until(Duration::from_secs(10), "KnowledgeBaseReview catch-up", || async {
            review(&pool)
                .await
                .articles
                .iter()
                .any(|a| a.article_id == article_id && a.revision == 2)
        })
        .await;
        let state = review(&pool).await;
        assert!(!is_pending(&state, &dismissed));
        assert!(!is_pending(&state, &drafted));
        let overview = state.articles.iter().find(|a| a.article_id == article_id).unwrap();
        assert_eq!(overview.status, "published");
        assert_eq!(overview.from_ticket_id.as_deref(), Some(drafted.as_str()));
    });
}

fn projection_query(name: &str, fields: &str) -> String {
    format!(
        r#"query {{ projection(boundedContext: {KB:?}, name: {name:?}, key: "") {{ ... on {KB}_{name} {{ {fields} }} }} }}"#
    )
}

/// Customers never see a draft: the review queue is staff-only, the
/// review commands are staff-only over GraphQL, and `PublishedArticles`,
/// which customers can read, holds published text only, not a draft
/// and not an edit made after publishing.
#[test]
fn customers_read_published_articles_but_never_drafts() {
    runtime().block_on(async {
        if test_db().await.is_none() {
            return;
        }
        let (skilj, pool, _mappings) = setup_all_contexts().await;
        let graphql = skilj.graphql_router().await.unwrap();
        let company_id = unique_name("company");

        let staff = seed_role(&pool, STAFF_ROLE).await;
        seed_mapping_for(&pool, &staff, KB, AccessLevel::Write, None).await;
        let staff = sign_jwt(&staff.external_subject);
        let customer = seed_role(&pool, "customer").await;
        seed_mapping_for(&pool, &customer, KB, AccessLevel::Write, Some(company_id)).await;
        let customer = sign_jwt(&customer.external_subject);

        let live_id = unique_name("article");
        let draft_id = unique_name("article");
        let draft_title = unique_name("Secret draft");
        let unpublished_edit = unique_name("Unpublished edit");

        // A customer can't write articles, whatever their grant level.
        let customer_draft = submit_command_mutation(
            KB,
            "DraftArticle",
            &serde_json::json!({ "article_id": unique_name("article"), "from_ticket_id": null, "title": "Mine", "body": "x" }),
        );
        let response = graphql_request(&graphql, &customer, &customer_draft).await;
        assert_eq!(
            response["errors"][0]["extensions"]["code"], "insufficient_role",
            "{response:?}"
        );

        // Staff: one article published then edited, one left a draft.
        for (command, payload) in [
            ("DraftArticle", serde_json::json!({ "article_id": live_id, "from_ticket_id": null, "title": "Live title", "body": "Live body" })),
            ("PublishArticle", serde_json::json!({ "article_id": live_id })),
            ("EditArticle", serde_json::json!({ "article_id": live_id, "title": unpublished_edit, "body": "Not yet" })),
            ("DraftArticle", serde_json::json!({ "article_id": draft_id, "from_ticket_id": null, "title": draft_title, "body": "Draft body" })),
        ] {
            let response = graphql_request(&graphql, &staff, &submit_command_mutation(KB, command, &payload)).await;
            assert!(response.get("errors").is_none(), "{response:?}");
            assert_eq!(response["data"]["submitCommand"]["accepted"], true, "{command}: {response:?}");
        }
        wait_until(Duration::from_secs(10), "both projections catch up", || async {
            let review = review(&pool).await;
            published(&pool).await.articles.iter().any(|a| a.article_id == live_id)
                && review.articles.iter().any(|a| a.article_id == draft_id)
                && review.articles.iter().any(|a| a.article_id == live_id && a.status == "edited")
        })
        .await;

        // The review queue: staff yes, customer refused by the team gate.
        let review_query = projection_query("KnowledgeBaseReview", "articles");
        let response = graphql_request(&graphql, &staff, &review_query).await;
        assert!(response.get("errors").is_none(), "{response:?}");
        assert!(response.to_string().contains(&draft_title), "staff see drafts: {response:?}");
        let response = graphql_request(&graphql, &customer, &review_query).await;
        assert_eq!(
            response["errors"][0]["extensions"]["code"], "not_on_required_team",
            "a customer must not read the review queue: {response:?}"
        );

        // Published articles: the customer reads them, and only the
        // published text is there.
        let response = graphql_request(&graphql, &customer, &projection_query("PublishedArticles", "articles")).await;
        assert!(response.get("errors").is_none(), "{response:?}");
        let body = response.to_string();
        assert!(body.contains(&live_id) && body.contains("Live title"), "{response:?}");
        assert!(!body.contains(&draft_id) && !body.contains(&draft_title), "a draft leaked: {response:?}");
        assert!(!body.contains(&unpublished_edit), "an unpublished edit leaked: {response:?}");
    });
}
