//! Narrow application read adapter for the dashboard (#102, #103).
//! Resolves the company's route on the server, rechecks the active grant and
//! projection owners, and opens only declared customer text with that subject's
//! data key. No ciphertext is returned and availability is never guessed from
//! text shape. Pages contain 25 summaries and only explicitly opened details.
use crate::{
    company_directory::{self, DirectoryState},
    helpdesk::{BOUNDED_CONTEXT, CUSTOMER_SUBJECT},
    routing::RoutingMode,
};
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode},
    routing::post,
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use skilj_core::{
    db,
    encryption::{decrypt_leaf, DataKey, KeyService},
    projections::{self, ProjectionAccessScope},
};
use std::{collections::HashMap, sync::Arc, time::Duration};

pub const PAGE_SIZE: usize = 25;
pub const MESSAGE_PAGE_SIZE: usize = 20;
const TEXT_LIMIT: usize = 16_000;
pub struct ReadState {
    pub directory: Arc<DirectoryState>,
    pub keys: KeyService,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketRead {
    pub company_id: String,
    #[serde(default)]
    pub offset: usize,
    pub wait_for_sequence: Option<i64>,
    #[serde(default)]
    pub details: HashMap<String, usize>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TicketPage {
    pub tickets: HashMap<String, Value>,
    pub contents: HashMap<String, Value>,
    pub has_more: bool,
}
type Failure = (StatusCode, String);
fn denied() -> Failure {
    (StatusCode::FORBIDDEN, "ticket access unavailable".into())
}
fn internal(error: impl std::fmt::Display) -> Failure {
    tracing::error!(%error, "ticket read failed");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        "couldn't load tickets".into(),
    )
}
pub fn router(state: Arc<ReadState>) -> Router {
    Router::new()
        .route("/v1/helpdesk/tickets", post(read))
        .with_state(state)
}
async fn read(
    State(state): State<Arc<ReadState>>,
    headers: HeaderMap,
    Json(request): Json<TicketRead>,
) -> Result<Json<TicketPage>, Failure> {
    let role = company_directory::authenticate(&state.directory, &headers).await?;
    read_page(&state, &role, request).await.map(Json)
}

pub async fn read_page(
    state: &ReadState,
    role: &skilj_core::access_control::Role,
    request: TicketRead,
) -> Result<TicketPage, Failure> {
    if request.details.len() > PAGE_SIZE
        || request.offset > i32::MAX as usize
        || request.details.values().any(|n| *n > i32::MAX as usize)
    {
        return Err((StatusCode::BAD_REQUEST, "invalid ticket page".into()));
    }
    let pool = &state.directory.pool;
    let tenant = if state.directory.mode == RoutingMode::Tenant {
        db::get_projection_state(
            pool,
            BOUNDED_CONTEXT,
            "TenantDirectory",
            &request.company_id,
        )
        .await
        .map_err(internal)?
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|v| v["tenant_name"].as_str().map(str::to_owned))
    } else {
        None
    };
    let context = tenant.as_deref().unwrap_or(BOUNDED_CONTEXT);
    let mapping = db::get_active_role_access_mapping(pool, &role.id, context)
        .await
        .map_err(internal)?
        .ok_or_else(denied)?;
    if role.status != skilj_core::access_control::RoleStatus::Active
        || mapping.bounded_context.status != skilj_core::event_store::BoundedContextStatus::Active
        || mapping
            .scope
            .as_deref()
            .is_some_and(|scope| scope != request.company_id)
    {
        return Err(denied());
    }
    let queue_projection = db::get_projection(pool, context, "CompanyActiveTickets")
        .await
        .map_err(internal)?
        .ok_or_else(denied)?;
    let content_projection = db::get_projection(pool, context, "CustomerTickets")
        .await
        .map_err(internal)?
        .ok_or_else(denied)?;
    // Respect registered projection capability gates as well as ownership.
    for projection in [&queue_projection, &content_projection] {
        projections::query_projection(
            &mapping,
            projection,
            &request.company_id,
            None,
            true,
            ProjectionAccessScope {
                declares_owner: true,
                instance_owner: Some(&request.company_id),
                requires_capability: projection.requires_capability.as_deref(),
            },
            String::new(),
        )
        .map_err(|_| denied())?;
    }
    if let Some(sequence) = request.wait_for_sequence {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        loop {
            if db::projection_caught_up_to(pool, context, "CompanyActiveTickets")
                .await
                .map_err(internal)?
                .flatten()
                .is_some_and(|s| s >= sequence)
            {
                break;
            }
            if tokio::time::Instant::now() >= deadline {
                return Err((
                    StatusCode::SERVICE_UNAVAILABLE,
                    "tickets are catching up; refresh again".into(),
                ));
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }
    // Context came from the server-owned directory, never a caller-supplied
    // schema identifier. Quote it even so; values below use bind parameters.
    let schema = format!("\"bc_{}\"", context.replace('"', "\"\""));
    let staff = mapping
        .capabilities
        .contains(crate::helpdesk::STAFF_CAPABILITY);
    let details = serde_json::to_string(&request.details).map_err(internal)?;
    // Page the queue first, then select only title + requested detail fields
    // from each customer's encrypted row. Historical tickets and the rest of
    // long conversations never leave PostgreSQL. One query, no customer fan-out.
    let sql = format!(
        r#"
        WITH page AS (
          SELECT item.key AS ticket_id, item.value AS ticket
          FROM {schema}.projection_state q,
               LATERAL jsonb_each(q.state::jsonb->'tickets') item
          WHERE q.projection_name = 'CompanyActiveTickets' AND q.key = $1 AND q.owner = $1
            AND ($2 OR item.value->>'requester_id' = $3)
          ORDER BY item.key LIMIT $4 OFFSET $5
        )
        SELECT page.ticket_id, page.ticket::text, c.owner,
          jsonb_build_object('title', c.state::jsonb->'tickets'->page.ticket_id->'title',
            'description', CASE WHEN $6::jsonb ? page.ticket_id THEN c.state::jsonb->'tickets'->page.ticket_id->'description' END,
            'rating_comment', CASE WHEN $6::jsonb ? page.ticket_id THEN c.state::jsonb->'tickets'->page.ticket_id->'rating_comment' END,
            'messages', CASE WHEN $6::jsonb ? page.ticket_id THEN (
              SELECT coalesce(jsonb_agg(m.value ORDER BY m.n), '[]'::jsonb)
              FROM jsonb_array_elements(c.state::jsonb->'tickets'->page.ticket_id->'messages') WITH ORDINALITY m(value,n)
              WHERE m.n > ($6::jsonb->>page.ticket_id)::bigint AND m.n <= ($6::jsonb->>page.ticket_id)::bigint + 20
            ) ELSE '[]'::jsonb END,
            'message_count', CASE WHEN $6::jsonb ? page.ticket_id THEN jsonb_array_length(c.state::jsonb->'tickets'->page.ticket_id->'messages') ELSE 0 END
          )::text
        FROM page LEFT JOIN {schema}.projection_state c
          ON c.projection_name = 'CustomerTickets' AND c.key = page.ticket->>'requester_id'
        ORDER BY page.ticket_id
    "#
    );
    let rows: Vec<(String, String, Option<String>, String)> =
        sqlx::query_as(sqlx::AssertSqlSafe(sql))
            .bind(&request.company_id)
            .bind(staff)
            .bind(&role.external_subject)
            .bind((PAGE_SIZE + 1) as i64)
            .bind(request.offset as i64)
            .bind(details)
            .fetch_all(pool)
            .await
            .map_err(internal)?;
    let mut page = TicketPage {
        tickets: HashMap::new(),
        contents: HashMap::new(),
        has_more: rows.len() > PAGE_SIZE,
    };
    let mut keys = HashMap::new();
    for (ticket_id, ticket, owner, raw) in rows.into_iter().take(PAGE_SIZE) {
        let ticket: Value = serde_json::from_str(&ticket).map_err(internal)?;
        let requester = ticket["requester_id"].as_str().ok_or_else(denied)?;
        let permitted = owner.as_deref() == Some(&request.company_id)
            && (mapping.can_read_sensitive || requester == role.external_subject);
        if permitted && !keys.contains_key(requester) {
            // Missing/destroyed keys and temporarily inaccessible keys share
            // the same public state. We never disclose erasure status.
            let key = db::projection_subject_data_key(
                pool,
                context,
                CUSTOMER_SUBJECT,
                requester,
                Some(&state.keys),
            )
            .await;
            keys.insert(requester.to_owned(), key.ok().flatten());
        }
        let key = if permitted {
            keys.get(requester).and_then(Option::as_ref)
        } else {
            None
        };
        let mut content: Value = serde_json::from_str(&raw).map_err(internal)?;
        let opened = request.details.contains_key(&ticket_id);
        let mut available = true;
        let mut truncated = false;
        for field in ["title", "description", "rating_comment"] {
            if let Some(cipher) = content[field].as_str() {
                let limit = if field == "title" && !opened {
                    200
                } else {
                    TEXT_LIMIT
                };
                content[field] = open_text(key, cipher, limit, &mut available, &mut truncated);
            }
        }
        if content["title"].is_null() {
            available = false;
        }
        if let Some(messages) = content["messages"].as_array_mut() {
            for message in messages {
                message["text"] = open_text(
                    key,
                    message["text"].as_str().unwrap_or_default(),
                    TEXT_LIMIT,
                    &mut available,
                    &mut truncated,
                );
            }
        }
        if content["description"].is_null() {
            content["description"] = json!("");
        }
        if !available {
            content = json!({"title": "Content unavailable", "description": "Content unavailable", "messages": [], "rating_comment": null, "message_count": 0});
        }
        content["available"] = json!(available);
        content["details_loaded"] = json!(opened);
        content["truncated"] = json!(truncated);
        page.tickets.insert(ticket_id.clone(), ticket);
        page.contents.insert(ticket_id, content);
    }
    Ok(page)
}
fn open_text(
    key: Option<&DataKey>,
    cipher: &str,
    limit: usize,
    available: &mut bool,
    truncated: &mut bool,
) -> Value {
    match key.and_then(|key| decrypt_leaf(key, cipher).ok()) {
        Some(text) => {
            *truncated |= text.chars().count() > limit;
            json!(text.chars().take(limit).collect::<String>())
        }
        None => {
            *available = false;
            Value::Null
        }
    }
}
