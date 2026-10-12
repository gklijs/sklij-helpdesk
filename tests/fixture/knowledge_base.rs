//! The `knowledge_base` bounded context: every `decide()`, both
//! projections, and the `TicketResolved -> SuggestArticle` route.

use serde_json::json;
use skilj::CrossContextRoute;
use skilj_helpdesk::helpdesk;
use skilj_helpdesk::knowledge_base::*;
use skilj_test_fixture::command::GivenEvents;

use crate::events::spec;

fn suggested(ticket_id: &str) -> KnowledgeBaseEvent {
    KnowledgeBaseEvent::ArticleSuggested(ArticleSuggestedPayload {
        ticket_id: ticket_id.into(),
        company_id: "acme".into(),
        suggested_at: "2026-10-12T00:00:00Z".into(),
    })
}

fn dismissed(ticket_id: &str) -> KnowledgeBaseEvent {
    KnowledgeBaseEvent::SuggestionDismissed(SuggestionDismissedPayload {
        ticket_id: ticket_id.into(),
        reason: None,
    })
}

fn drafted(article_id: &str, from_ticket_id: Option<&str>, title: &str) -> KnowledgeBaseEvent {
    KnowledgeBaseEvent::ArticleDrafted(ArticleDraftedPayload {
        article_id: article_id.into(),
        from_ticket_id: from_ticket_id.map(Into::into),
        title: title.into(),
        body: format!("{title} body"),
    })
}

fn edited(article_id: &str, title: &str) -> KnowledgeBaseEvent {
    KnowledgeBaseEvent::ArticleEdited(ArticleEditedPayload {
        article_id: article_id.into(),
        title: title.into(),
        body: format!("{title} body"),
    })
}

fn published(article_id: &str, title: &str, revision: u32) -> KnowledgeBaseEvent {
    KnowledgeBaseEvent::ArticlePublished(ArticlePublishedPayload {
        article_id: article_id.into(),
        title: title.into(),
        body: format!("{title} body"),
        revision,
    })
}

fn draft(article_id: &str, from_ticket_id: Option<&str>) -> DraftArticlePayload {
    DraftArticlePayload {
        article_id: article_id.into(),
        from_ticket_id: from_ticket_id.map(Into::into),
        title: "Resetting your password".into(),
        body: "Use the link on the login page.".into(),
    }
}

// --- suggestions ---

#[test]
fn a_ticket_is_suggested_once() {
    let payload = || SuggestArticlePayload {
        ticket_id: "t-1".into(),
        company_id: "acme".into(),
        suggested_at: "2026-10-12T00:00:00Z".into(),
    };
    GivenEvents::<SuggestArticle>::new()
        .when(payload())
        .then_accepted(vec![spec(
            "ArticleSuggested",
            json!({ "ticket_id": "t-1", "company_id": "acme", "suggested_at": "2026-10-12T00:00:00Z" }),
        )]);
    // A second resolution of the same ticket, or a replayed route.
    for later in [suggested("t-1"), dismissed("t-1")] {
        GivenEvents::<SuggestArticle>::new()
            .event(suggested("t-1"))
            .event(later)
            .when(payload())
            .then_rejected("already_suggested");
    }
}

#[test]
fn only_a_pending_suggestion_can_be_dismissed() {
    let dismiss = || DismissSuggestionPayload {
        ticket_id: "t-1".into(),
        reason: Some("one-off billing glitch".into()),
    };
    GivenEvents::<DismissSuggestion>::new()
        .event(suggested("t-1"))
        .when(dismiss())
        .then_accepted(vec![spec(
            "SuggestionDismissed",
            json!({ "ticket_id": "t-1", "reason": "one-off billing glitch" }),
        )]);
    GivenEvents::<DismissSuggestion>::new()
        .when(dismiss())
        .then_rejected("suggestion_not_found");
    GivenEvents::<DismissSuggestion>::new()
        .event(suggested("t-1"))
        .event(dismissed("t-1"))
        .when(dismiss())
        .then_rejected("suggestion_already_dismissed");
    GivenEvents::<DismissSuggestion>::new()
        .event(suggested("t-1"))
        .event(drafted("a-1", Some("t-1"), "Title"))
        .when(dismiss())
        .then_rejected("suggestion_already_drafted");
}

// --- drafting ---

#[test]
fn drafting_from_a_pending_suggestion_accepts_it() {
    GivenEvents::<DraftArticle>::new()
        .event(suggested("t-1"))
        .when(draft("a-1", Some("t-1")))
        .then_accepted(vec![spec(
            "ArticleDrafted",
            json!({
                "article_id": "a-1",
                "from_ticket_id": "t-1",
                "title": "Resetting your password",
                "body": "Use the link on the login page.",
            }),
        )]);
}

#[test]
fn an_article_can_be_drafted_from_scratch() {
    GivenEvents::<DraftArticle>::new()
        .when(draft("a-1", None))
        .then(crate::events::assert_accepted);
}

#[test]
fn drafting_is_rejected_for_a_suggestion_that_is_not_pending() {
    GivenEvents::<DraftArticle>::new()
        .when(draft("a-1", Some("t-1")))
        .then_rejected("suggestion_not_found");
    GivenEvents::<DraftArticle>::new()
        .event(suggested("t-1"))
        .event(dismissed("t-1"))
        .when(draft("a-1", Some("t-1")))
        .then_rejected("suggestion_already_dismissed");
    GivenEvents::<DraftArticle>::new()
        .event(suggested("t-1"))
        .event(drafted("a-0", Some("t-1"), "Earlier"))
        .when(draft("a-1", Some("t-1")))
        .then_rejected("suggestion_already_drafted");
}

#[test]
fn drafting_needs_a_new_id_and_a_title() {
    GivenEvents::<DraftArticle>::new()
        .event(drafted("a-1", None, "Existing"))
        .when(draft("a-1", None))
        .then_rejected("article_exists");
    GivenEvents::<DraftArticle>::new()
        .when(DraftArticlePayload {
            title: "  ".into(),
            ..draft("a-1", None)
        })
        .then_rejected("empty_title");
}

// --- editing and publishing ---

#[test]
fn only_an_existing_article_can_be_edited() {
    let edit = || EditArticlePayload {
        article_id: "a-1".into(),
        title: "New title".into(),
        body: "New body".into(),
    };
    GivenEvents::<EditArticle>::new()
        .event(drafted("a-1", None, "Old"))
        .when(edit())
        .then_accepted(vec![spec(
            "ArticleEdited",
            json!({ "article_id": "a-1", "title": "New title", "body": "New body" }),
        )]);
    GivenEvents::<EditArticle>::new()
        .when(edit())
        .then_rejected("article_not_found");
}

#[test]
fn publishing_copies_the_current_text_and_counts_revisions() {
    let publish = || PublishArticlePayload {
        article_id: "a-1".into(),
    };
    GivenEvents::<PublishArticle>::new()
        .event(drafted("a-1", None, "First"))
        .event(edited("a-1", "Second"))
        .when(publish())
        .then_accepted(vec![spec(
            "ArticlePublished",
            json!({ "article_id": "a-1", "title": "Second", "body": "Second body", "revision": 1 }),
        )]);
    // An edit after publication is a new revision to publish.
    GivenEvents::<PublishArticle>::new()
        .event(drafted("a-1", None, "First"))
        .event(published("a-1", "First", 1))
        .event(edited("a-1", "Second"))
        .when(publish())
        .then_accepted(vec![spec(
            "ArticlePublished",
            json!({ "article_id": "a-1", "title": "Second", "body": "Second body", "revision": 2 }),
        )]);
}

#[test]
fn publishing_is_rejected_with_nothing_new_or_no_article() {
    let publish = || PublishArticlePayload {
        article_id: "a-1".into(),
    };
    GivenEvents::<PublishArticle>::new()
        .event(drafted("a-1", None, "First"))
        .event(published("a-1", "First", 1))
        .when(publish())
        .then_rejected("nothing_to_publish");
    GivenEvents::<PublishArticle>::new()
        .when(publish())
        .then_rejected("article_not_found");
}

// --- route ---

#[test]
fn every_resolution_is_routed_as_a_suggestion() {
    let suggestion =
        HelpdeskResolutionToArticleSuggestion::route(&helpdesk::TicketResolvedPayload {
            ticket_id: "t-1".into(),
            company_id: "acme".into(),
            resolution: Some(2),
            requester_id: Some("customer-1".into()),
            assignee_id: None,
        })
        .expect("every resolution is routed");
    assert_eq!(suggestion.ticket_id, "t-1");
    assert_eq!(suggestion.company_id, "acme");
    assert!(chrono::DateTime::parse_from_rfc3339(&suggestion.suggested_at).is_ok());
}

// --- projections ---

#[test]
fn the_review_queue_drops_handled_suggestions_and_tracks_article_status() {
    skilj_test_fixture::projection::GivenEvents::<KnowledgeBaseReview>::new()
        .event(suggested("t-1"))
        .event(suggested("t-2"))
        .event(suggested("t-3"))
        .event(dismissed("t-2"))
        .event(drafted("a-1", Some("t-1"), "First"))
        .event(drafted("a-2", None, "Scratch"))
        .event(published("a-2", "Scratch", 1))
        .event(edited("a-2", "Scratch v2"))
        .then(|state| {
            let pending: Vec<_> = state
                .pending_suggestions
                .iter()
                .map(|s| s.ticket_id.as_str())
                .collect();
            assert_eq!(pending, ["t-3"]);
            assert_eq!(state.articles.len(), 2);
            assert_eq!(state.articles[0].status, "draft");
            assert_eq!(state.articles[0].from_ticket_id.as_deref(), Some("t-1"));
            assert_eq!(state.articles[1].status, "edited");
            assert_eq!(state.articles[1].title, "Scratch v2");
            assert_eq!(state.articles[1].revision, 1);
        });
}

#[test]
fn published_articles_show_only_the_last_published_text() {
    skilj_test_fixture::projection::GivenEvents::<PublishedArticles>::new()
        .event(published("a-1", "First", 1))
        .event(published("a-2", "Other", 1))
        .event(published("a-1", "Second", 2))
        .then(|state| {
            assert_eq!(state.articles.len(), 2);
            assert_eq!(state.articles[0].title, "Second");
            assert_eq!(state.articles[0].revision, 2);
            assert_eq!(state.articles[1].article_id, "a-2");
        });
}
