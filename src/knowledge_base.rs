//! The "knowledge_base" bounded context: help articles, and the queue of
//! resolved tickets worth turning into one. Implements
//! `specs/knowledge_base.allium` - see that file for the domain spec, and
//! README.md's "Context map" for how it sits next to the other contexts.
//!
//! Same shape as `marketing.rs`: `helpdesk` is only ever a *source* here.
//! `HelpdeskResolutionToArticleSuggestion` (bottom of this file) reacts
//! to `helpdesk.TicketResolved` by submitting `SuggestArticle`, which -
//! like marketing's `Record*` commands - is never `rest_trigger_allowed`:
//! only the route submits it. Everything else is staff work, behind
//! `#[requires_role("staff")]` on GraphQL and a staff-held
//! `CommandToken` on REST: turn a suggestion into a draft or dismiss it,
//! edit an article, publish it.
//!
//! One suggestion per ticket, ever. The suggestion is keyed by
//! `ticket_id`, so a ticket that is reopened and resolved again is
//! rejected as `already_suggested` rather than queued a second time -
//! staff already judged that ticket once. The route's own idempotency key
//! (`"{NAME}:{source_sequence}"`) covers a redelivered *occurrence*; this
//! guard covers a second occurrence for the same ticket, and a route
//! replay after its cursor is reset.
//!
//! Drafts never reach customers. `ArticleDrafted`/`ArticleEdited` feed
//! only the staff-only `KnowledgeBaseReview`; `PublishedArticles` folds
//! nothing but `ArticlePublished`, which carries the title and body as
//! they stood when a staff member published them. Editing a published
//! article therefore leaves the live copy untouched until it is published
//! again.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use skilj::{auto_register, requires_role, CommandType, CrossContextRoute, EventType, Projection};
use skilj_core::event_store::Event;
use skilj_core::plugin::{BoundedContextEvent, Decision};
use skilj_core::shared::{CommandDecision, EventSpec, QueryItemMapping, TagMapping};

use crate::helpdesk::{self, STAFF_CAPABILITY};

pub const BOUNDED_CONTEXT: &str = "knowledge_base";

/// A suggestion is identified by the ticket it came from.
fn suggestion_tag(field: &str) -> Vec<TagMapping> {
    vec![TagMapping::new("suggestion", field)]
}

fn article_tag() -> Vec<TagMapping> {
    vec![TagMapping::new("article", "article_id")]
}

// --- events ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArticleSuggestedPayload {
    pub ticket_id: String,
    pub company_id: String,
    pub suggested_at: String,
}

pub struct ArticleSuggested;

/// `specs/knowledge_base.allium`'s `entity ArticleSuggestion`, created.
/// Readable on the event feed, like marketing's routed facts, so the
/// route's result can be observed from outside.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for ArticleSuggested {
    type Payload = ArticleSuggestedPayload;
    const NAME: &'static str = "ArticleSuggested";
    fn tag_mappings() -> Vec<TagMapping> {
        suggestion_tag("ticket_id")
    }
    fn event_read_allowed() -> bool {
        true
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SuggestionDismissedPayload {
    pub ticket_id: String,
    /// Why staff decided this ticket isn't article material, if they said.
    pub reason: Option<String>,
}

pub struct SuggestionDismissed;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for SuggestionDismissed {
    type Payload = SuggestionDismissedPayload;
    const NAME: &'static str = "SuggestionDismissed";
    fn tag_mappings() -> Vec<TagMapping> {
        suggestion_tag("ticket_id")
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArticleDraftedPayload {
    pub article_id: String,
    /// The suggestion this draft accepts, or `None` for an article staff
    /// started from scratch.
    pub from_ticket_id: Option<String>,
    pub title: String,
    pub body: String,
}

pub struct ArticleDrafted;

/// Tagged on the article and on the suggestion it accepts, so both
/// `DraftArticle`'s "is this suggestion still pending" and
/// `PublishArticle`'s "what is this article's current text" read it.
#[auto_register(BOUNDED_CONTEXT)]
impl EventType for ArticleDrafted {
    type Payload = ArticleDraftedPayload;
    const NAME: &'static str = "ArticleDrafted";
    fn tag_mappings() -> Vec<TagMapping> {
        [article_tag(), suggestion_tag("from_ticket_id")].concat()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArticleEditedPayload {
    pub article_id: String,
    pub title: String,
    pub body: String,
}

pub struct ArticleEdited;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for ArticleEdited {
    type Payload = ArticleEditedPayload;
    const NAME: &'static str = "ArticleEdited";
    fn tag_mappings() -> Vec<TagMapping> {
        article_tag()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArticlePublishedPayload {
    pub article_id: String,
    /// The text as published - copied in rather than looked up, so
    /// `PublishedArticles` never has to fold a draft to show it.
    pub title: String,
    pub body: String,
    /// 1 for the first publication, then one more per republish.
    pub revision: u32,
}

pub struct ArticlePublished;

#[auto_register(BOUNDED_CONTEXT)]
impl EventType for ArticlePublished {
    type Payload = ArticlePublishedPayload;
    const NAME: &'static str = "ArticlePublished";
    fn tag_mappings() -> Vec<TagMapping> {
        article_tag()
    }
    fn event_read_allowed() -> bool {
        true
    }
}

/// This bounded context's own hand-written event enum - same technique
/// `src/helpdesk/`'s own `HelpdeskEvent` already uses.
pub enum KnowledgeBaseEvent {
    ArticleSuggested(ArticleSuggestedPayload),
    SuggestionDismissed(SuggestionDismissedPayload),
    ArticleDrafted(ArticleDraftedPayload),
    ArticleEdited(ArticleEditedPayload),
    ArticlePublished(ArticlePublishedPayload),
}

impl BoundedContextEvent for KnowledgeBaseEvent {
    fn try_from_event(event: &Event) -> Option<Result<Self, serde_json::Error>> {
        let payload = &event.payload;
        match event.event_type.name.as_str() {
            "ArticleSuggested" => {
                Some(serde_json::from_str(payload).map(KnowledgeBaseEvent::ArticleSuggested))
            }
            "SuggestionDismissed" => {
                Some(serde_json::from_str(payload).map(KnowledgeBaseEvent::SuggestionDismissed))
            }
            "ArticleDrafted" => {
                Some(serde_json::from_str(payload).map(KnowledgeBaseEvent::ArticleDrafted))
            }
            "ArticleEdited" => {
                Some(serde_json::from_str(payload).map(KnowledgeBaseEvent::ArticleEdited))
            }
            "ArticlePublished" => {
                Some(serde_json::from_str(payload).map(KnowledgeBaseEvent::ArticlePublished))
            }
            _ => None,
        }
    }
}

// --- folds for decide() ---

/// Where one suggestion stands, folded from its own tagged history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuggestionStatus {
    Pending,
    Drafted,
    Dismissed,
}

fn suggestion_status(ticket_id: &str, events: &[KnowledgeBaseEvent]) -> Option<SuggestionStatus> {
    let mut status = None;
    for event in events {
        match event {
            KnowledgeBaseEvent::ArticleSuggested(p) if p.ticket_id == ticket_id => {
                status = Some(SuggestionStatus::Pending);
            }
            KnowledgeBaseEvent::SuggestionDismissed(p) if p.ticket_id == ticket_id => {
                status = Some(SuggestionStatus::Dismissed);
            }
            KnowledgeBaseEvent::ArticleDrafted(p)
                if p.from_ticket_id.as_deref() == Some(ticket_id) =>
            {
                status = Some(SuggestionStatus::Drafted);
            }
            _ => {}
        }
    }
    status
}

/// One article's current text, and whether that text is what customers
/// see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArticleFacts {
    pub title: String,
    pub body: String,
    pub revisions_published: u32,
    /// True when the current text has never been published - a fresh
    /// draft, or an edit since the last publication.
    pub unpublished_changes: bool,
}

fn article_facts(article_id: &str, events: &[KnowledgeBaseEvent]) -> Option<ArticleFacts> {
    let mut facts: Option<ArticleFacts> = None;
    for event in events {
        match event {
            KnowledgeBaseEvent::ArticleDrafted(p) if p.article_id == article_id => {
                facts = Some(ArticleFacts {
                    title: p.title.clone(),
                    body: p.body.clone(),
                    revisions_published: 0,
                    unpublished_changes: true,
                });
            }
            KnowledgeBaseEvent::ArticleEdited(p) if p.article_id == article_id => {
                if let Some(facts) = facts.as_mut() {
                    facts.title = p.title.clone();
                    facts.body = p.body.clone();
                    facts.unpublished_changes = true;
                }
            }
            KnowledgeBaseEvent::ArticlePublished(p) if p.article_id == article_id => {
                if let Some(facts) = facts.as_mut() {
                    facts.revisions_published = p.revision;
                    facts.unpublished_changes = false;
                }
            }
            _ => {}
        }
    }
    facts
}

fn reject(kind: &str, reason: String) -> CommandDecision {
    CommandDecision::Rejected {
        reason,
        kind: kind.into(),
    }
}

/// `empty_title` for a blank title - an article customers can't find by
/// name isn't one.
fn check_title(title: &str) -> Option<CommandDecision> {
    title
        .trim()
        .is_empty()
        .then(|| reject("empty_title", "an article needs a title".into()))
}

// --- commands ---

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct SuggestArticlePayload {
    pub ticket_id: String,
    pub company_id: String,
}

pub struct SuggestArticle;

/// `specs/knowledge_base.allium`'s `rule ArticleIsSuggested`. Only the
/// route below submits it (no `rest_trigger_allowed`); see this file's
/// doc comment for why a repeat is rejected rather than queued.
#[auto_register(BOUNDED_CONTEXT)]
impl CommandType for SuggestArticle {
    type Payload = SuggestArticlePayload;
    type Event = KnowledgeBaseEvent;
    const NAME: &'static str = "SuggestArticle";
    fn tag_mappings() -> Vec<TagMapping> {
        suggestion_tag("ticket_id")
    }
    /// Whether this ticket was ever suggested: one event at most.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![QueryItemMapping::types(&["ArticleSuggested"])
            .tagged(suggestion_tag("ticket_id"))
            .latest()]
    }
    /// `suggested_at` is the submission's own instant (`Decision::now`,
    /// skilj 0.2.0) - see the route below.
    fn decide_with(
        payload: &Self::Payload,
        decision: &Decision<'_, Self::Event>,
    ) -> CommandDecision {
        if suggestion_status(&payload.ticket_id, decision.events()).is_some() {
            return reject(
                "already_suggested",
                format!(
                    "ticket {} was already suggested as an article",
                    payload.ticket_id
                ),
            );
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "ArticleSuggested".into(),
                payload: serde_json::json!({
                    "ticket_id": payload.ticket_id,
                    "company_id": payload.company_id,
                    "suggested_at": decision.now().to_rfc3339(),
                }),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DismissSuggestionPayload {
    pub ticket_id: String,
    pub reason: Option<String>,
}

pub struct DismissSuggestion;

/// `rule StaffDismissesSuggestion`: only a pending suggestion.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for DismissSuggestion {
    type Payload = DismissSuggestionPayload;
    type Event = KnowledgeBaseEvent;
    const NAME: &'static str = "DismissSuggestion";
    fn tag_mappings() -> Vec<TagMapping> {
        suggestion_tag("ticket_id")
    }
    /// This suggestion's whole history: suggested, then drafted or
    /// dismissed - three events at most.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![QueryItemMapping::tags(suggestion_tag("ticket_id"))]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        match suggestion_status(&payload.ticket_id, matching_events) {
            None => reject(
                "suggestion_not_found",
                format!("ticket {} has no article suggestion", payload.ticket_id),
            ),
            Some(SuggestionStatus::Dismissed) => reject(
                "suggestion_already_dismissed",
                format!(
                    "the suggestion for ticket {} was already dismissed",
                    payload.ticket_id
                ),
            ),
            Some(SuggestionStatus::Drafted) => reject(
                "suggestion_already_drafted",
                format!(
                    "the suggestion for ticket {} already became an article",
                    payload.ticket_id
                ),
            ),
            Some(SuggestionStatus::Pending) => CommandDecision::Accepted {
                events: vec![EventSpec {
                    event_type: "SuggestionDismissed".into(),
                    payload: serde_json::json!({
                        "ticket_id": payload.ticket_id,
                        "reason": payload.reason,
                    }),
                }],
            },
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct DraftArticlePayload {
    pub article_id: String,
    /// The pending suggestion this draft accepts; `None` to start an
    /// article from scratch.
    pub from_ticket_id: Option<String>,
    pub title: String,
    pub body: String,
}

pub struct DraftArticle;

/// `rule StaffDraftsArticle`: a new `article_id`, and - when it accepts
/// a suggestion - a suggestion that is still pending. Accepting a
/// suggestion is drafting from it: there is no separate "accept" step,
/// and the suggestion leaves the queue with the draft.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for DraftArticle {
    type Payload = DraftArticlePayload;
    type Event = KnowledgeBaseEvent;
    const NAME: &'static str = "DraftArticle";
    fn tag_mappings() -> Vec<TagMapping> {
        [article_tag(), suggestion_tag("from_ticket_id")].concat()
    }
    /// Whether the article exists (its first event is enough), and the
    /// suggestion's history. A scratch draft has no `from_ticket_id`, so
    /// its second item derives a valueless `suggestion` tag and matches
    /// only other scratch drafts' `ArticleDrafted` - which `decide`
    /// ignores, at the cost of two concurrent scratch drafts retrying
    /// once.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![
            QueryItemMapping::types(&["ArticleDrafted"])
                .tagged(article_tag())
                .latest(),
            QueryItemMapping::types(&["ArticleSuggested", "SuggestionDismissed", "ArticleDrafted"])
                .tagged(suggestion_tag("from_ticket_id")),
        ]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        if let Some(rejected) = check_title(&payload.title) {
            return rejected;
        }
        if article_facts(&payload.article_id, matching_events).is_some() {
            return reject(
                "article_exists",
                format!("article {} already exists", payload.article_id),
            );
        }
        if let Some(ticket_id) = &payload.from_ticket_id {
            let rejected = match suggestion_status(ticket_id, matching_events) {
                Some(SuggestionStatus::Pending) => None,
                None => Some(("suggestion_not_found", "has no article suggestion")),
                Some(SuggestionStatus::Dismissed) => Some((
                    "suggestion_already_dismissed",
                    "had its suggestion dismissed",
                )),
                Some(SuggestionStatus::Drafted) => Some((
                    "suggestion_already_drafted",
                    "already had its suggestion drafted",
                )),
            };
            if let Some((kind, why)) = rejected {
                return reject(kind, format!("ticket {ticket_id} {why}"));
            }
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "ArticleDrafted".into(),
                payload: serde_json::json!({
                    "article_id": payload.article_id,
                    "from_ticket_id": payload.from_ticket_id,
                    "title": payload.title,
                    "body": payload.body,
                }),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct EditArticlePayload {
    pub article_id: String,
    pub title: String,
    pub body: String,
}

pub struct EditArticle;

/// `rule StaffEditsArticle`: any existing article, draft or published.
/// An edit to a published article is a new unpublished revision - see
/// this file's doc comment.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for EditArticle {
    type Payload = EditArticlePayload;
    type Event = KnowledgeBaseEvent;
    const NAME: &'static str = "EditArticle";
    fn tag_mappings() -> Vec<TagMapping> {
        article_tag()
    }
    /// Whether the article exists: its draft event.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![QueryItemMapping::types(&["ArticleDrafted"])
            .tagged(article_tag())
            .latest()]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        if let Some(rejected) = check_title(&payload.title) {
            return rejected;
        }
        if article_facts(&payload.article_id, matching_events).is_none() {
            return reject(
                "article_not_found",
                format!("article {} does not exist", payload.article_id),
            );
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "ArticleEdited".into(),
                payload: serde_json::json!({
                    "article_id": payload.article_id,
                    "title": payload.title,
                    "body": payload.body,
                }),
            }],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PublishArticlePayload {
    pub article_id: String,
}

pub struct PublishArticle;

/// `rule StaffPublishesArticle`: an article whose current text isn't
/// published yet. Rejected as `nothing_to_publish` otherwise, so a
/// double-click doesn't bump the revision.
#[auto_register(BOUNDED_CONTEXT)]
#[requires_role("staff")]
impl CommandType for PublishArticle {
    type Payload = PublishArticlePayload;
    type Event = KnowledgeBaseEvent;
    const NAME: &'static str = "PublishArticle";
    fn tag_mappings() -> Vec<TagMapping> {
        article_tag()
    }
    /// The article's whole history - its current text is the fold of
    /// every draft, edit and publication.
    fn consistency_query() -> Vec<QueryItemMapping> {
        vec![
            QueryItemMapping::types(&["ArticleDrafted", "ArticleEdited", "ArticlePublished"])
                .tagged(article_tag()),
        ]
    }
    fn rest_trigger_allowed() -> bool {
        true
    }
    fn decide(payload: &Self::Payload, matching_events: &[Self::Event]) -> CommandDecision {
        let Some(facts) = article_facts(&payload.article_id, matching_events) else {
            return reject(
                "article_not_found",
                format!("article {} does not exist", payload.article_id),
            );
        };
        if !facts.unpublished_changes {
            return reject(
                "nothing_to_publish",
                format!(
                    "article {} is already published as it stands",
                    payload.article_id
                ),
            );
        }
        CommandDecision::Accepted {
            events: vec![EventSpec {
                event_type: "ArticlePublished".into(),
                payload: serde_json::json!({
                    "article_id": payload.article_id,
                    "title": facts.title,
                    "body": facts.body,
                    "revision": facts.revisions_published + 1,
                }),
            }],
        }
    }
}

// --- projections ---

/// One row of the review queue: a resolved ticket nobody has acted on.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PendingSuggestion {
    pub ticket_id: String,
    pub company_id: String,
    pub suggested_at: String,
}

/// One article as staff see it, drafts included.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct ArticleOverview {
    pub article_id: String,
    pub from_ticket_id: Option<String>,
    pub title: String,
    /// `"draft"` (never published), `"published"`, or `"edited"`
    /// (published, with unpublished changes since). A plain string, for
    /// the reason `helpdesk::TicketSummaryState::priority` gives.
    pub status: String,
    pub revision: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct KnowledgeBaseReviewState {
    pub pending_suggestions: Vec<PendingSuggestion>,
    pub articles: Vec<ArticleOverview>,
}

/// `surface ArticleReview`: one shared instance (no `keys()` override),
/// the queue of pending suggestions plus every article in any state.
/// Staff-only (`REQUIRES_CAPABILITY`) - this is where drafts are visible, so a
/// customer is refused it outright. No `OWNER_TAG_KEY`: articles aren't
/// any one company's, and staff review suggestions from every company.
pub struct KnowledgeBaseReview;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for KnowledgeBaseReview {
    type State = KnowledgeBaseReviewState;
    type Event = KnowledgeBaseEvent;
    const NAME: &'static str = "KnowledgeBaseReview";
    const REQUIRES_CAPABILITY: Option<&'static str> = Some(STAFF_CAPABILITY);
    fn consumed_event_types() -> Vec<&'static str> {
        vec![
            "ArticleSuggested",
            "SuggestionDismissed",
            "ArticleDrafted",
            "ArticleEdited",
            "ArticlePublished",
        ]
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        match event {
            KnowledgeBaseEvent::ArticleSuggested(p) => {
                state.pending_suggestions.push(PendingSuggestion {
                    ticket_id: p.ticket_id.clone(),
                    company_id: p.company_id.clone(),
                    suggested_at: p.suggested_at.clone(),
                });
            }
            KnowledgeBaseEvent::SuggestionDismissed(p) => {
                state
                    .pending_suggestions
                    .retain(|s| s.ticket_id != p.ticket_id);
            }
            KnowledgeBaseEvent::ArticleDrafted(p) => {
                if let Some(ticket_id) = &p.from_ticket_id {
                    state
                        .pending_suggestions
                        .retain(|s| &s.ticket_id != ticket_id);
                }
                state.articles.push(ArticleOverview {
                    article_id: p.article_id.clone(),
                    from_ticket_id: p.from_ticket_id.clone(),
                    title: p.title.clone(),
                    status: "draft".into(),
                    revision: 0,
                });
            }
            KnowledgeBaseEvent::ArticleEdited(p) => {
                if let Some(a) = state
                    .articles
                    .iter_mut()
                    .find(|a| a.article_id == p.article_id)
                {
                    a.title = p.title.clone();
                    if a.status == "published" {
                        a.status = "edited".into();
                    }
                }
            }
            KnowledgeBaseEvent::ArticlePublished(p) => {
                if let Some(a) = state
                    .articles
                    .iter_mut()
                    .find(|a| a.article_id == p.article_id)
                {
                    a.title = p.title.clone();
                    a.status = "published".into();
                    a.revision = p.revision;
                }
            }
        }
    }
}

/// One article as customers see it: its last published text.
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct PublishedArticle {
    pub article_id: String,
    pub title: String,
    pub body: String,
    pub revision: u32,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
pub struct PublishedArticlesState {
    pub articles: Vec<PublishedArticle>,
}

/// `surface KnowledgeBase`: every published article, one shared instance.
/// No capability gate - customers read this - and it folds `ArticlePublished`
/// alone, so a draft or an unpublished edit cannot appear here by
/// construction rather than by a filter.
pub struct PublishedArticles;

#[auto_register(BOUNDED_CONTEXT)]
impl Projection for PublishedArticles {
    type State = PublishedArticlesState;
    type Event = KnowledgeBaseEvent;
    const NAME: &'static str = "PublishedArticles";
    fn consumed_event_types() -> Vec<&'static str> {
        vec!["ArticlePublished"]
    }
    fn project(state: &mut Self::State, event: &Self::Event, _key: &str) {
        let KnowledgeBaseEvent::ArticlePublished(p) = event else {
            return;
        };
        let published = PublishedArticle {
            article_id: p.article_id.clone(),
            title: p.title.clone(),
            body: p.body.clone(),
            revision: p.revision,
        };
        match state
            .articles
            .iter_mut()
            .find(|a| a.article_id == p.article_id)
        {
            Some(existing) => *existing = published,
            None => state.articles.push(published),
        }
    }
}

// --- cross-context route ---

/// `helpdesk.TicketResolved -> knowledge_base.SuggestArticle`. Every
/// resolution is routed; `SuggestArticle::decide_with` drops all but a
/// ticket's first. `TicketResolved` carries no timestamp of its own, so
/// `SuggestArticle::decide_with` stamps `suggested_at` from the
/// submission's own instant - the same choice `marketing`'s routes make.
///
/// Starts from the beginning of `helpdesk`'s history, like marketing's
/// routes: on first deploy every ticket already resolved is queued once,
/// which is a backlog staff can dismiss, not a side effect that can't be
/// taken back.
pub struct HelpdeskResolutionToArticleSuggestion;

impl CrossContextRoute for HelpdeskResolutionToArticleSuggestion {
    type Source = helpdesk::TicketResolved;
    type Target = SuggestArticle;
    const NAME: &'static str = "HelpdeskResolutionToArticleSuggestion";
    fn route(source_payload: &helpdesk::TicketResolvedPayload) -> Option<SuggestArticlePayload> {
        Some(SuggestArticlePayload {
            ticket_id: source_payload.ticket_id.clone(),
            company_id: source_payload.company_id.clone(),
        })
    }
}

#[cfg(test)]
mod required_role_tests {
    use super::*;

    /// Same pin as `helpdesk`'s own: `#[requires_role]` takes a literal.
    #[test]
    fn review_commands_require_the_staff_role() {
        for required in [
            DismissSuggestion::required_role(),
            DraftArticle::required_role(),
            EditArticle::required_role(),
            PublishArticle::required_role(),
        ] {
            assert_eq!(required, Some(helpdesk::STAFF_ROLE));
        }
        assert_eq!(SuggestArticle::required_role(), None);
    }
}
